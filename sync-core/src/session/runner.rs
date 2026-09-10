use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU64, Ordering};

use kulua_proto::generated::ctrl_msg;
use kulua_proto::session::Event;

use crate::adb_cmd::AdbOps;
use crate::notification::{self, NotifInfo};
use crate::types::Device;

use super::audio::{
    self, AudioEvent, AudioTarget, Link, Progress, SharedRuntime, send_audio, spawn_audio_task,
};
use super::deploy::deploy_and_connect;
use super::handle::{SESSION_STATE_CONNECTING, SESSION_STATE_RUNNING};

/// 剪贴板主循环退出原因。
enum SessionExit {
    Stop,
}

/// 单个设备的完整 session（UDP 直连版）。
///
/// 部署 kulua-server（phone 绑定网络 UDP 端口）→ 局域网直连（IP:port）→
/// 双向剪贴板 I/O + 通知轮询 + 音频播放（复用 same UDP 会话的多路流）。
pub struct Session {
    adb: Arc<dyn AdbOps>,
    device: Device,
    jar_path: String,
    port: u16,
    clip_sub: tokio::sync::broadcast::Receiver<String>,
    phone_clip_tx: tokio::sync::mpsc::Sender<String>,
    notif_tx: tokio::sync::mpsc::Sender<NotifInfo>,
    stop_rx: Option<tokio::sync::oneshot::Receiver<()>>,
    clipboard_enabled: Arc<AtomicBool>,
    notification_enabled: Arc<AtomicBool>,
    volume: Arc<AtomicU16>,
    session_state: Arc<AtomicU8>,
    audio_latency: Arc<AtomicU64>,
    /// 音频目标（Core 经 watch 下发；连续操作自动合并到最新值）。
    audio_target_rx: Option<tokio::sync::watch::Receiver<AudioTarget>>,
    /// 音频运行态（供 Core 推 UI）。
    audio_runtime: SharedRuntime,
    device_name_tx: Option<tokio::sync::mpsc::Sender<(String, String)>>,
    scrcpy_params: crate::settings::ScrcpyParams,
}
impl Session {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        adb: Arc<dyn AdbOps>,
        device: Device,
        jar_path: String,
        port: u16,
        clip_sub: tokio::sync::broadcast::Receiver<String>,
        phone_clip_tx: tokio::sync::mpsc::Sender<String>,
        notif_tx: tokio::sync::mpsc::Sender<NotifInfo>,
        stop_rx: tokio::sync::oneshot::Receiver<()>,
        clipboard_enabled: Arc<AtomicBool>,
        notification_enabled: Arc<AtomicBool>,
        volume: Arc<AtomicU16>,
        device_name_tx: tokio::sync::mpsc::Sender<(String, String)>,
        session_state: Arc<AtomicU8>,
        audio_latency: Arc<AtomicU64>,
        audio_target_rx: tokio::sync::watch::Receiver<AudioTarget>,
        audio_runtime: SharedRuntime,
        scrcpy_params: crate::settings::ScrcpyParams,
    ) -> Self {
        session_state.store(SESSION_STATE_CONNECTING, Ordering::SeqCst);
        Self {
            adb,
            device,
            jar_path,
            port,
            clip_sub,
            phone_clip_tx,
            notif_tx,
            stop_rx: Some(stop_rx),
            clipboard_enabled,
            notification_enabled,
            volume,
            session_state,
            audio_latency,
            audio_target_rx: Some(audio_target_rx),
            audio_runtime,
            device_name_tx: Some(device_name_tx),
            scrcpy_params,
        }
    }

    /// 运行 session 主循环：部署 kulua-server → UDP 直连 → 双向剪贴板 + 音频。
    pub async fn run(&mut self) {
        let t0 = std::time::Instant::now();
        let mut stop_rx = self.stop_rx.take().expect("run can only be called once");
        let audio_target_rx = self
            .audio_target_rx
            .take()
            .expect("audio target rx can only be taken once");
        // 会话初始音频目标（HELLO 带出）：音频链路的代次从这里（0）开始，
        // 目标变更 / 回执超时重试 / 断流自救都收敛到 Link 内部。
        let initial_target = *audio_target_rx.borrow();
        let mut link = Link::new(initial_target, std::time::Instant::now());
        let audio_runtime = self.audio_runtime.clone();
        let serial = self.device.serial.clone();

        // 1-4. 部署 kulua-server → 取设备名 → 解析直连 IP → UDP 握手（整段见 deploy.rs）。
        // 失败路径（错误日志、SESSION_STATE_FAILED、server.stop 清理）在 deploy 内原位
        // 执行，语义与拆分前一致；t0 传入以保证 [timing] 标签数值不变。
        let (server, mut udp, _peer) = match deploy_and_connect(
            self.adb.clone(),
            self.device.clone(),
            self.jar_path.clone(),
            self.port,
            self.scrcpy_params.clone(),
            self.device_name_tx.take(),
            initial_target,
            self.session_state.clone(),
            t0,
        )
        .await
        {
            Ok(v) => v,
            Err(()) => return,
        };

        // 启动通知轮询
        let notif_stop = Arc::new(AtomicBool::new(false));
        notification::spawn_notification_poller_tokio(
            self.adb.clone(),
            self.device.clone(),
            self.notif_tx.clone(),
            notif_stop.clone(),
            self.notification_enabled.clone(),
        );

        // 主循环：control（可靠双向）+ audio（尽力而为、按代次过滤）+ 心跳 + 音频目标。
        // 方案（代次/开关/编码）经 watch 直达音频任务：立即生效、不会因通道满阻塞主循环。
        let (audio_tx, audio_rx) = tokio::sync::mpsc::channel::<AudioEvent>(64);
        let (plan_tx, plan_rx) = tokio::sync::watch::channel(link.plan());
        // 已成功播放的帧计数（控制面据此判断流与恢复，任务只负责喂帧）
        let audio_frames = Arc::new(AtomicU64::new(0));
        let audio_task = spawn_audio_task(
            audio_rx,
            plan_rx,
            serial.clone(),
            self.volume.clone(),
            self.audio_latency.clone(),
            audio_frames.clone(),
        );
        audio::publish(&audio_runtime, link.runtime());

        // 握手全部完成 → running
        self.session_state
            .store(SESSION_STATE_RUNNING, Ordering::SeqCst);

        let clipboard_enabled = self.clipboard_enabled.clone();
        let phone_clip_tx = self.phone_clip_tx.clone();
        let audio_latency = self.audio_latency.clone();
        let mut tick = tokio::time::Instant::now() + audio::TICK;

        let exit_reason = loop {
            tokio::select! {
                biased;
                _ = &mut stop_rx => {
                    break SessionExit::Stop;
                }
                _ = tokio::time::sleep_until(tick) => {
                    tick = tokio::time::Instant::now() + audio::TICK;
                    let now = std::time::Instant::now();

                    // 控制面给出结论（目标变更 / 回执超时重试 / 断流自救），这里只落副作用
                    let step = link.drive(
                        now,
                        *audio_target_rx.borrow(),
                        Progress {
                            frames: audio_frames.load(Ordering::Relaxed),
                        },
                    );
                    if step.stalled {
                        audio_latency.store(0, Ordering::Relaxed);
                        eprintln!(
                            "[audio] {serial} 音频帧断流：已无声音（对端采集停摆 / 媒体链路中断 / 解码持续失败）"
                        );
                    }
                    if step.recovered {
                        eprintln!("[audio] {serial} 音频帧恢复");
                    }
                    if let Some(issue) = step.issue {
                        match send_audio(&udp, &plan_tx, &issue.plan) {
                            Ok(()) => println!(
                                "[audio] {serial} SetAudio（{}）enabled={} codec={} rev={}",
                                issue.reason.label(),
                                issue.plan.enabled,
                                issue.plan.codec,
                                issue.plan.revision
                            ),
                            Err(e) => {
                                eprintln!("[audio] {serial} SetAudio 发送失败: {e}，会话终止");
                                break SessionExit::Stop;
                            }
                        }
                    }
                    // 断流自救与回执超时都在同一拍推进（前者不需要等回执）
                    if let Some(issue) = link.poll_stall_retry(now) {
                        if send_audio(&udp, &plan_tx, &issue.plan).is_ok() {
                            println!(
                                "[audio] {serial} SetAudio（{}）enabled={} codec={} rev={}",
                                issue.reason.label(),
                                issue.plan.enabled,
                                issue.plan.codec,
                                issue.plan.revision
                            );
                        }
                    }
                    audio::publish(&audio_runtime, link.runtime());
                }
                evt = udp.recv() => {
                    match evt {
                        Some(Event::Control(msg)) => match msg.msg {
                            Some(ctrl_msg::Msg::ClipboardChanged(c)) => {
                                if clipboard_enabled.load(Ordering::SeqCst) {
                                    println!("Phone clipboard from {}: {}", serial, c.text);
                                    if phone_clip_tx.send(c.text).await.is_err() {
                                        break SessionExit::Stop;
                                    }
                                }
                            }
                            Some(ctrl_msg::Msg::MediaConfig(cfg)) => {
                                // cfg.stream：1=AUDIO 2=VIDEO（媒体流标识，与媒体端口对应）
                                // 只收当前代次的音频 config：旧代次参数集用来初始化解码器
                                // 会让新代次每帧都解不出来。
                                if cfg.stream == kulua_proto::codec::MEDIA_STREAM_AUDIO
                                    && link.plan().accepts(cfg.revision)
                                {
                                    let _ = audio_tx
                                        .send(AudioEvent::Config {
                                            revision: cfg.revision,
                                            data: cfg.data,
                                        })
                                        .await;
                                }
                            }
                            Some(ctrl_msg::Msg::AudioState(st)) => {
                                // 业务回执：传输 ACK 只代表命令送达，启停是否完成看这里
                                if link.on_state(st.revision, st.enabled, st.codec_id, &st.error) {
                                    if st.error.is_empty() {
                                        println!(
                                            "[audio] {serial} AudioState: {} codec=0x{:08x} rev={}",
                                            if st.enabled { "on" } else { "off" },
                                            st.codec_id,
                                            st.revision
                                        );
                                    } else {
                                        eprintln!(
                                            "[audio] {serial} 音频启用失败: {} (rev={})",
                                            st.error, st.revision
                                        );
                                    }
                                    audio::publish(&audio_runtime, link.runtime());
                                } else {
                                    println!(
                                        "[audio] {serial} 忽略旧代次 AudioState rev={}（当前 rev={}）",
                                        st.revision,
                                        link.plan().revision
                                    );
                                }
                            }
                            _ => {}
                        },
                        Some(Event::Audio(frame)) => {
                            let _ = audio_tx
                                .send(AudioEvent::Frame {
                                    revision: frame.revision,
                                    frame,
                                })
                                .await;
                        }
                        Some(Event::Video(_)) => {}
                        Some(Event::Error(e)) => {
                            eprintln!("[{}] UDP 会话错误: {}", serial, e);
                            break SessionExit::Stop;
                        }
                        Some(Event::Closed) | None => {
                            eprintln!("[{}] UDP 会话已关闭（对端 BYE/消失）", serial);
                            break SessionExit::Stop;
                        }
                        Some(Event::Connected) => {}
                    }
                }
                result = self.clip_sub.recv() => {
                    match result {
                        Ok(text) if clipboard_enabled.load(Ordering::SeqCst) => {
                            if let Err(e) = udp.send_ctrl(&kulua_proto::session::msgs::set_clipboard(0, false, &text)) {
                                eprintln!("[{}] set_clipboard 发送失败: {}，终止", serial, e);
                                break SessionExit::Stop;
                            }
                        }
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            break SessionExit::Stop;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            eprintln!("[{}] clip_sub lagged by {}", serial, n);
                        }
                    }
                }
            }
        };

        // ── 清理 ──
        audio_task.abort();
        notif_stop.store(true, Ordering::SeqCst);

        match exit_reason {
            SessionExit::Stop => {
                println!("[{}] 关闭 UDP 会话（BYE）", serial);
                udp.close();
            }
        }

        // BYE 只关闭本客户端；共享 server 在最后一个客户端离开后自行退出。
        server.detach();
        println!("Session {} cleaned up", serial);
    }
}
