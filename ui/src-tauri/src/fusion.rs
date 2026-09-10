//! 融合窗口（Tauri WebviewWindow + WebCodecs）命令与视频泵。
//!
//! 窗口生命周期归 UI 进程：daemon 只回传直连地址，这里自建 video-only UDP 会话
//! （HELLO → CreateDisplay → DisplayReady → START_APP），编码帧经 Tauri Channel
//! 推给 `fusion.html` 的 JS 用 WebCodecs 硬解；ctrl 注入经 mpsc 转发给 pump 任务
//! （`UdpSession` 被 pump 独占），drop 全部 ctrl_tx 即 BYE 收尾。

use kulua_proto::session::UdpSession;
use tauri::{AppHandle, Manager, State};
use tokio::sync::mpsc;

use crate::AppState;

/// 一个融合窗口的会话元数据。
///
/// `UdpSession` 本体由 video pump 任务独占（`fusion_start` 里 spawn）；
/// 这里只保留注入所需的 display_id 和 ctrl 指令发送通道——drop 全部
/// `ctrl_tx` 克隆后 pump 任务退出并发 BYE。
pub(crate) struct FusionSession {
    display_id: u32,
    ctrl_tx: mpsc::Sender<kulua_proto::generated::CtrlMsg>,
}

/// Channel 推给融合窗口 JS 的视频载荷。
///
/// `data` 为完整编码帧（Annex-B NAL units）或 config 帧（is_config=true，
/// avcC 格式 SPS/PPS，用作 WebCodecs description）。
#[derive(Clone, serde::Serialize)]
pub(crate) struct VideoFramePayload {
    data: Vec<u8>,
    pts: u64,
    is_key: bool,
    is_config: bool,
}
// ── 融合窗口命令（fusion.html 页面调用）──

/// 解析 `WxH/DPI` 显示器规格字符串。
pub(crate) fn parse_display_spec(spec: &str) -> Result<(u32, u32, u32), String> {
    let mut parts = spec.split('/');
    let dims = parts.next().unwrap_or("");
    let dpi: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(160);
    let (w, h) = dims
        .split_once('x')
        .ok_or_else(|| format!("无效显示器规格: {spec}"))?;
    let w: u32 = w.parse().map_err(|_| format!("无效宽度: {w}"))?;
    let h: u32 = h.parse().map_err(|_| format!("无效高度: {h}"))?;
    Ok((w, h, dpi))
}

/// 启动融合窗口的视频流：UDP 握手 → CreateDisplay → START_APP →
/// video pump 任务把编码帧经 Channel 推给页面 JS（WebCodecs 解码）。
///
/// 返回 display_id（输入注入用）。会话元数据存入 AppState；ctrl 指令
/// 经 channel 转发给 pump 任务，drop 发送端即触发 BYE 收尾。
#[tauri::command]
pub(crate) async fn fusion_start(
    app: AppHandle,
    state: State<'_, AppState>,
    addr: String,
    package: String,
    window_label: String,
    channel: tauri::ipc::Channel<VideoFramePayload>,
) -> Result<u32, String> {
    // 同 label 重复启动：先清旧会话（pump 任务随 ctrl_tx drop 自行收尾）
    state.fusion_sessions.lock().remove(&window_label);

    let peer: std::net::SocketAddr = addr
        .parse()
        .map_err(|e| format!("无效直连地址 {addr}: {e}"))?;
    let port = peer.port();
    let scid = sync_core::scrcpy::scid_hex(port);
    let display_spec = "1280x960/160";
    let (w, h, dpi) = parse_display_spec(display_spec)?;

    // 阻塞握手 + 建显示器（spawn_blocking：底层是同步 UDP + 重试循环）
    let create = kulua_proto::session::msgs::create_display(w, h, dpi);
    let mut session = {
        let scid = scid.clone();
        let create = create.clone();
        tokio::task::spawn_blocking(move || {
            let s = UdpSession::connect(
                peer,
                &scid,
                kulua_proto::session::ConnectOpts {
                    // 融合窗口只收视频：不开音频端口，也不要求 phone 起采集
                    audio_link: false,
                    hello_audio: false,
                    video: true,
                },
            )?;
            s.send_ctrl(&create)?;
            Ok::<_, String>(s)
        })
        .await
        .map_err(|e| format!("握手任务失败: {e}"))??
    };

    // 等 DisplayReady（可靠 ctrl 流回执）
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    let display_id = loop {
        if tokio::time::Instant::now() > deadline {
            session.close();
            return Err("等待 DisplayReady 超时".into());
        }
        match session.try_recv() {
            Some(kulua_proto::session::Event::Control(msg)) => {
                if let Some(kulua_proto::generated::ctrl_msg::Msg::DisplayReady(dr)) = msg.msg {
                    break dr.display_id;
                }
            }
            Some(kulua_proto::session::Event::Error(e)) => {
                session.close();
                return Err(format!("会话错误: {e}"));
            }
            Some(kulua_proto::session::Event::Closed) | None => {}
            _ => {}
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    };
    println!("[fusion] display ready id={display_id} codec={}", scid);

    // 立即发 START_APP（不等首帧，与旧 viewer 一致防死锁）
    let start = kulua_proto::session::msgs::start_app(display_id, &package);
    session
        .send_ctrl(&start)
        .map_err(|e| format!("START_APP 失败: {e}"))?;

    // 会话元数据入 state；UdpSession 移交 pump 任务独占
    let (ctrl_tx, mut ctrl_rx) = mpsc::channel::<kulua_proto::generated::CtrlMsg>(64);
    state.fusion_sessions.lock().insert(
        window_label.clone(),
        FusionSession {
            display_id,
            ctrl_tx,
        },
    );

    let label = window_label.clone();
    let app_for_pump = app.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::select! {
                biased;
                ctrl = ctrl_rx.recv() => match ctrl {
                    Some(msg) => {
                        if session.send_ctrl(&msg).is_err() {
                            break;
                        }
                    }
                    // 全部 ctrl_tx 被 drop（窗口关闭 / fusion_close）→ BYE 收尾
                    None => break,
                },
                evt = session.recv() => match evt {
                    Some(kulua_proto::session::Event::Video(frame)) => {
                        let is_key =
                            frame.flags & kulua_proto::codec::MEDIA_FLAG_KEYFRAME != 0;
                        let _ = channel.send(VideoFramePayload {
                            data: frame.data,
                            pts: frame.pts,
                            is_key,
                            is_config: false,
                        });
                    }
                    Some(kulua_proto::session::Event::Control(msg)) => {
                        // 视频 config（SPS/PPS，avcC 格式）走可靠流 → JS 用作 description
                        if let Some(kulua_proto::generated::ctrl_msg::Msg::MediaConfig(cfg)) =
                            msg.msg
                        {
                            if cfg.stream == kulua_proto::codec::MEDIA_STREAM_VIDEO {
                                let _ = channel.send(VideoFramePayload {
                                    data: cfg.data,
                                    pts: 0,
                                    is_key: false,
                                    is_config: true,
                                });
                            }
                        }
                    }
                    Some(kulua_proto::session::Event::Error(e)) => {
                        eprintln!("[fusion/{label}] 会话错误: {e}");
                        break;
                    }
                    Some(kulua_proto::session::Event::Closed) | None => break,
                    _ => {}
                },
            }
        }
        session.close();
        app_for_pump
            .state::<AppState>()
            .fusion_sessions
            .lock()
            .remove(&label);
        println!("[fusion/{label}] 视频会话已关闭");
    });

    Ok(display_id)
}

/// 取指定融合窗口的 (display_id, ctrl_tx) 快照；不存在报错。
pub(crate) fn fusion_ctrl(
    state: &AppState,
    window_label: &str,
) -> Result<(u32, mpsc::Sender<kulua_proto::generated::CtrlMsg>), String> {
    let sessions = state.fusion_sessions.lock();
    let fs = sessions
        .get(window_label)
        .ok_or_else(|| "融合窗口不存在或已关闭".to_string())?;
    Ok((fs.display_id, fs.ctrl_tx.clone()))
}

/// 注入触摸事件（action：0=DOWN 1=UP 2=MOVE）。
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub(crate) async fn fusion_touch(
    state: State<'_, AppState>,
    window_label: String,
    action: u32,
    x: i32,
    y: i32,
    screen_width: u32,
    screen_height: u32,
    pressure: f32,
) -> Result<(), String> {
    let (display_id, tx) = fusion_ctrl(&state, &window_label)?;
    // pointer_id = u64::MAX（鼠标约定，与旧 viewer 一致）
    let msg = kulua_proto::session::msgs::inject_touch(
        display_id,
        action,
        u64::MAX,
        x,
        y,
        screen_width,
        screen_height,
        pressure,
        0,
        0,
    );
    tx.send(msg).await.map_err(|_| "会话已关闭".to_string())
}

/// 注入滚轮事件（Android 滚动语义）。
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn fusion_scroll(
    state: State<'_, AppState>,
    window_label: String,
    x: i32,
    y: i32,
    screen_width: u32,
    screen_height: u32,
    scroll_x: f32,
    scroll_y: f32,
) -> Result<(), String> {
    let (display_id, tx) = fusion_ctrl(&state, &window_label)?;
    let msg = kulua_proto::session::msgs::inject_scroll(
        display_id,
        x,
        y,
        screen_width,
        screen_height,
        scroll_x,
        scroll_y,
        0,
    );
    tx.send(msg).await.map_err(|_| "会话已关闭".to_string())
}

/// 注入按键 keycode（action：0=DOWN 1=UP）。
#[tauri::command]
pub(crate) async fn fusion_key(
    state: State<'_, AppState>,
    window_label: String,
    action: u32,
    keycode: u32,
) -> Result<(), String> {
    let (display_id, tx) = fusion_ctrl(&state, &window_label)?;
    let msg = kulua_proto::session::msgs::inject_keycode(display_id, action, keycode, 0, 0);
    tx.send(msg).await.map_err(|_| "会话已关闭".to_string())
}

/// 注入文本（含 IME 提交串）。
#[tauri::command]
pub(crate) async fn fusion_text(
    state: State<'_, AppState>,
    window_label: String,
    text: String,
) -> Result<(), String> {
    let (display_id, tx) = fusion_ctrl(&state, &window_label)?;
    let msg = kulua_proto::session::msgs::inject_text(display_id, &text);
    tx.send(msg).await.map_err(|_| "会话已关闭".to_string())
}

/// 返回键 / 点亮屏幕（右键映射）。
#[tauri::command]
pub(crate) async fn fusion_back(
    state: State<'_, AppState>,
    window_label: String,
    action: u32,
) -> Result<(), String> {
    let (display_id, tx) = fusion_ctrl(&state, &window_label)?;
    let msg = kulua_proto::session::msgs::back_or_screen_on(display_id, action);
    tx.send(msg).await.map_err(|_| "会话已关闭".to_string())
}

/// 调整虚拟显示器分辨率（窗口 resize 防抖后调用）。
#[tauri::command]
pub(crate) async fn fusion_resize(
    state: State<'_, AppState>,
    window_label: String,
    width: u32,
    height: u32,
) -> Result<(), String> {
    let (display_id, tx) = fusion_ctrl(&state, &window_label)?;
    let msg = kulua_proto::session::msgs::resize_display(display_id, width, height);
    tx.send(msg).await.map_err(|_| "会话已关闭".to_string())
}

/// 关闭融合窗口会话（BYE + pump 任务退出 + 清理 state）。
#[tauri::command]
pub(crate) async fn fusion_close(
    state: State<'_, AppState>,
    window_label: String,
) -> Result<(), String> {
    // drop ctrl_tx 即让 pump 任务 break → session.close() 发 BYE
    state.fusion_sessions.lock().remove(&window_label);
    Ok(())
}

// ── IPC Client ──
