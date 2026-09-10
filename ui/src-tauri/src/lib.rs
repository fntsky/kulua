use parking_lot::Mutex;
use prost::Message;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use sync_core::ipc::proto::{self, event, request, response};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::{mpsc, oneshot};

mod fusion;

use fusion::FusionSession;

/// 通过长连接发送的 IPC 请求，响应通过 oneshot 回传。
struct IpcRequest {
    id: u64,
    frame: (u8, Vec<u8>),
    response_tx: oneshot::Sender<Result<proto::Response, String>>,
}

// ── State ──

pub(crate) struct AppState {
    devices: Mutex<Vec<DeviceInfo>>,
    connected: Mutex<bool>,
    pairing_info: Mutex<Option<PairingInfo>>,
    /// 长连接的 IPC 发送端
    ipc_tx: Mutex<Option<mpsc::Sender<IpcRequest>>>,
    /// 自增请求 ID（1、2 保留给连接建立后的初始握手）
    next_id: AtomicU64,
    /// 活跃融合窗口会话：window label → 元数据 + ctrl 发送端。
    /// UdpSession 本体被 video pump 任务独占；ctrl 指令经 channel 转发。
    fusion_sessions: Mutex<HashMap<String, FusionSession>>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct PairingInfo {
    dns_id: String,
    psk: String,
    wifi_string: String,
}

#[derive(Clone, serde::Serialize)]
struct DeviceInfo {
    uuid: String,
    serial: String,
    state: String,
    name: String,
}

/// adb 原始设备列表条目（"ADB 连接" 页面，含 Offline/Unauthorized）
#[derive(Clone, serde::Serialize)]
struct AdbDeviceInfo {
    serial: String,
    state: String,
}

/// 应用设置（自上而下经 daemon 的 `settings.*` RPC）。
#[derive(Clone, serde::Serialize)]
struct SettingsInfo {
    autostart_enabled: bool,
    autostart_supported: bool,
    video_bit_rate: u32,
    video_max_size: u32,
    video_max_fps: u32,
    audio_bit_rate: u32,
    audio_codec: String,
}

/// 发送给前端的会话列表事件载荷。
#[derive(Clone, serde::Serialize)]
struct SessionInfo {
    uuid: String,
    id: String,
    serial: String,
    name: String,
    state: String,
    session_state: String,
    audio_buffer_ms: u64,
    clipboard_sync: bool,
    notification_sync: bool,
    /// 音频目标（用户期望的开关）
    audio_enabled: bool,
    /// 音频运行态：off | starting | on | stopping | failed
    audio_state: String,
    /// 运行态为 failed 时的原因
    audio_error: String,
    volume: u32,
}

#[derive(Clone, serde::Serialize)]
struct SessionListInfo {
    sessions: Vec<SessionInfo>,
}

#[derive(Clone, serde::Serialize)]
struct ClipboardInfo {
    text: String,
    serial: String,
}

#[derive(Clone, serde::Serialize)]
struct NotificationInfo {
    serial: String,
    title: String,
    text: String,
    app: String,
}

/// 设备上的一个可启动应用（`app.list` 结果项）。
#[derive(Clone, serde::Serialize)]
struct AppInfo {
    package_name: String,
    label: String,
    system: bool,
}

/// `app.list` 的完整结果：应用列表 + 融合模式是否可用。
#[derive(Clone, serde::Serialize)]
struct AppListInfo {
    apps: Vec<AppInfo>,
    fusion_supported: bool,
}

// ── 转换辅助 ──

fn device_to_info(device: &proto::Device) -> DeviceInfo {
    DeviceInfo {
        uuid: device.uuid.clone(),
        serial: device.serial.clone(),
        state: device.state.clone(),
        name: device.name.clone(),
    }
}

fn device_to_adb_info(device: &proto::Device) -> AdbDeviceInfo {
    AdbDeviceInfo {
        serial: device.serial.clone(),
        state: device.state.clone(),
    }
}

fn session_to_info(session: &proto::SessionSummary) -> SessionInfo {
    SessionInfo {
        uuid: session.uuid.clone(),
        id: session.id.clone(),
        serial: session.serial.clone(),
        name: session.name.clone(),
        state: session.state.clone(),
        session_state: session.session_state.clone(),
        audio_buffer_ms: session.audio_buffer_ms,
        clipboard_sync: session.clipboard_sync,
        notification_sync: session.notification_sync,
        audio_enabled: session.audio_enabled,
        audio_state: session.audio_state.clone(),
        audio_error: session.audio_error.clone(),
        volume: session.volume,
    }
}

fn check_response(resp: &proto::Response) -> Result<(), String> {
    match &resp.error {
        Some(err) => Err(format!("{} (code {})", err.message, err.code)),
        None => Ok(()),
    }
}

fn update_device_cache(app: &AppHandle, infos: Vec<DeviceInfo>) {
    if let Some(state) = app.try_state::<AppState>() {
        *state.devices.lock() = infos.clone();
    }
    let _ = app.emit("devices-updated", &infos);
}

// ── Commands ──

#[tauri::command]
fn get_devices(state: State<'_, AppState>) -> Vec<DeviceInfo> {
    state.devices.lock().clone()
}

#[tauri::command]
fn get_connection_status(state: State<'_, AppState>) -> bool {
    *state.connected.lock()
}

#[tauri::command]
fn get_pairing_info(state: State<'_, AppState>) -> Option<PairingInfo> {
    state.pairing_info.lock().clone()
}

/// 通过长连接向 daemon 发送 Protobuf 请求并等待响应。
async fn ipc_request(
    state: &State<'_, AppState>,
    method: &str,
    params: Option<request::Payload>,
) -> Result<proto::Response, String> {
    use sync_core::ipc::types::FRAME_TYPE_REQUEST;

    let id = state.next_id.fetch_add(1, Ordering::SeqCst);
    let (tx, rx) = oneshot::channel();

    let req = proto::Request {
        id,
        method: method.to_string(),
        params,
    };

    let ipc_req = IpcRequest {
        id,
        frame: (FRAME_TYPE_REQUEST, req.encode_to_vec()),
        response_tx: tx,
    };

    let sender = state.ipc_tx.lock().clone().ok_or("未连接 daemon")?;
    sender.send(ipc_req).await.map_err(|_| "daemon 已断开")?;

    rx.await.map_err(|_| "daemon 已断开")?
}

#[tauri::command]
async fn update_session_config(
    state: State<'_, AppState>,
    uuid: String,
    clipboard_sync: bool,
    notification_sync: bool,
    audio_sync: bool,
    volume: u16,
) -> Result<(), String> {
    let params = request::Payload::SessionUpdate(request::SessionUpdate {
        uuid,
        clipboard_sync: Some(clipboard_sync),
        notification_sync: Some(notification_sync),
        audio_sync: Some(audio_sync),
        volume: Some(u32::from(volume)),
    });
    let resp = ipc_request(&state, "session.update", Some(params)).await?;
    check_response(&resp)?;
    Ok(())
}

/// 重试 failed 墓碑 session
#[tauri::command]
async fn retry_session(state: State<'_, AppState>, uuid: String) -> Result<(), String> {
    let params = request::Payload::SessionRetry(request::UuidParam { uuid });
    let resp = ipc_request(&state, "session.retry", Some(params)).await?;
    check_response(&resp)?;
    Ok(())
}

/// 点击 ADB 列表设备建立 session（daemon 侧校验无活跃 session 才创建）
#[tauri::command]
async fn start_session(state: State<'_, AppState>, serial: String) -> Result<(), String> {
    let params = request::Payload::SessionStart(request::DeviceSerial { serial });
    let resp = ipc_request(&state, "session.start", Some(params)).await?;
    check_response(&resp)?;
    Ok(())
}

/// adb 原始设备列表（含 Offline/Unauthorized）
#[tauri::command]
async fn get_adb_devices(state: State<'_, AppState>) -> Result<Vec<AdbDeviceInfo>, String> {
    let resp = ipc_request(&state, "device.adb_list", None).await?;
    check_response(&resp)?;
    match resp.result {
        Some(response::Payload::AdbList(list)) => {
            Ok(list.devices.iter().map(device_to_adb_info).collect())
        }
        _ => Err("daemon 返回了意外的 device.adb_list 结果".into()),
    }
}

/// 将 daemon 返回的 `response::Settings` 映射为前端结构。
fn settings_to_info(s: &response::Settings) -> SettingsInfo {
    SettingsInfo {
        autostart_enabled: s.autostart_enabled,
        autostart_supported: s.autostart_supported,
        video_bit_rate: s.video_bit_rate,
        video_max_size: s.video_max_size,
        video_max_fps: s.video_max_fps,
        audio_bit_rate: s.audio_bit_rate,
        audio_codec: s.audio_codec.clone(),
    }
}

/// 读取应用设置（含开机自启动状态 + 编码参数）。
#[tauri::command]
async fn get_settings(state: State<'_, AppState>) -> Result<SettingsInfo, String> {
    let resp = ipc_request(&state, "settings.get", None).await?;
    check_response(&resp)?;
    match resp.result {
        Some(response::Payload::Settings(s)) => Ok(settings_to_info(&s)),
        _ => Err("daemon 返回了意外的 settings.get 结果".into()),
    }
}

/// 设置是否开机自启动（写 config 真源 + 同步 HKCU Run 键）。
#[tauri::command]
async fn set_autostart(state: State<'_, AppState>, enabled: bool) -> Result<SettingsInfo, String> {
    let params = request::Payload::SetAutostart(request::SetAutostart { enabled });
    let resp = ipc_request(&state, "settings.set_autostart", Some(params)).await?;
    check_response(&resp)?;
    match resp.result {
        Some(response::Payload::Settings(s)) => Ok(settings_to_info(&s)),
        _ => Err("daemon 返回了意外的 settings.set_autostart 结果".into()),
    }
}

/// 保存编码参数（只更新提供的字段，其余保持原值）；音频编码热切换不重启会话。
#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn set_scrcpy_params(
    state: State<'_, AppState>,
    video_bit_rate: Option<u32>,
    video_max_size: Option<u32>,
    video_max_fps: Option<u32>,
    audio_bit_rate: Option<u32>,
    audio_codec: Option<String>,
) -> Result<SettingsInfo, String> {
    let params = request::Payload::SetScrcpyParams(request::SetScrcpyParams {
        video_bit_rate,
        video_max_size,
        video_max_fps,
        audio_bit_rate,
        audio_codec,
    });
    let resp = ipc_request(&state, "settings.set_scrcpy_params", Some(params)).await?;
    check_response(&resp)?;
    match resp.result {
        Some(response::Payload::Settings(s)) => Ok(settings_to_info(&s)),
        _ => Err("daemon 返回了意外的 settings.set_scrcpy_params 结果".into()),
    }
}

/// 枚举设备可启动应用（`app.list`；带 60s 缓存，force=true 强制刷新）。
#[tauri::command]
async fn get_apps(
    state: State<'_, AppState>,
    uuid: String,
    force: bool,
) -> Result<AppListInfo, String> {
    let params = request::Payload::AppListParams(request::AppListParams { uuid, force });
    let resp = ipc_request(&state, "app.list", Some(params)).await?;
    check_response(&resp)?;
    match resp.result {
        Some(response::Payload::AppList(list)) => Ok(AppListInfo {
            apps: list
                .apps
                .iter()
                .map(|a| AppInfo {
                    package_name: a.package_name.clone(),
                    label: a.label.clone(),
                    system: a.system,
                })
                .collect(),
            fusion_supported: list.fusion_supported,
        }),
        _ => Err("daemon 返回了意外的 app.list 结果".into()),
    }
}

/// 打开应用的融合窗口：daemon 拉起 kulua-server 虚拟显示器会话元数据，
/// UI 创建 WebviewWindow 并自建 video-only UDP 客户端直连 phone。
///
/// 返回 window_id；视频流由 `fusion_start` 命令启动（融合窗口 JS 加载后调用）。
#[tauri::command]
async fn open_app(
    app: AppHandle,
    state: State<'_, AppState>,
    uuid: String,
    package_name: String,
) -> Result<u64, String> {
    let params = request::Payload::AppOpenParams(request::AppOpenParams {
        uuid: uuid.clone(),
        package_name: package_name.clone(),
    });
    let resp = ipc_request(&state, "app.open", Some(params)).await?;
    check_response(&resp)?;
    match resp.result {
        Some(response::Payload::AppOpen(open)) => {
            if open.addr.is_empty() {
                return Err("daemon 未返回设备直连地址".into());
            }
            let label = format!("fusion-{}", open.window_id);
            let title = format!("{} — Kulua", package_name);

            // 融合窗口页面：/fusion.html?addr=..&port=..&package=..&window_id=..
            // addr 是 ip:port（URL 安全），包名是 Java 标识符（URL 安全），直接拼接。
            let port = open.addr.rsplit(':').next().unwrap_or("").to_string();
            let url = format!(
                "/fusion.html?addr={}&port={}&package={}&window_id={}",
                open.addr, port, package_name, open.window_id
            );

            tauri::WebviewWindowBuilder::new(&app, &label, tauri::WebviewUrl::App(url.into()))
                .title(&title)
                .inner_size(1280.0, 960.0)
                .min_inner_size(480.0, 360.0)
                .build()
                .map_err(|e| format!("创建融合窗口失败: {}", e))?;

            Ok(open.window_id)
        }
        _ => Err("daemon 返回了意外的 app.open 结果".into()),
    }
}

async fn connect_daemon(app: AppHandle) {
    let port_path = std::env::temp_dir().join("sync-daemon.port");

    let port_str = wait_for_port(&port_path, 10_000).await;
    let port: u16 = match port_str.as_deref() {
        Some(s) => match s.parse() {
            Ok(p) => p,
            Err(_) => {
                eprintln!("bad port: {s}");
                set_conn(&app, false);
                return;
            }
        },
        None => {
            eprintln!("port file not found");
            set_conn(&app, false);
            return;
        }
    };

    let addr = format!("127.0.0.1:{port}");
    let stream = match tokio::net::TcpStream::connect(&addr).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("connect daemon: {e}");
            set_conn(&app, false);
            return;
        }
    };

    println!("daemon connected {addr}");
    set_conn(&app, true);

    // ── 创建 IPC 命令通道 ──
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<IpcRequest>(32);
    if let Some(st) = app.try_state::<AppState>() {
        *st.ipc_tx.lock() = Some(cmd_tx);
    }

    use futures::SinkExt;
    use sync_core::ipc::types::{
        FrameCodec, FRAME_TYPE_EVENT, FRAME_TYPE_REQUEST, FRAME_TYPE_RESPONSE,
    };
    use tokio_stream::StreamExt;
    use tokio_util::codec::Framed;

    let mut framed = Framed::new(stream, FrameCodec);
    let mut pending: HashMap<u64, oneshot::Sender<Result<proto::Response, String>>> =
        HashMap::new();

    // ── 初始握手：device.list + pairing.info ──
    for (id, method) in [(1u64, "device.list"), (2u64, "pairing.info")] {
        let req = proto::Request {
            id,
            method: method.to_string(),
            params: None,
        };
        if framed
            .send((FRAME_TYPE_REQUEST, req.encode_to_vec()))
            .await
            .is_err()
        {
            set_conn(&app, false);
            return;
        }
    }

    // ── 事件循环 + 命令注入 ──
    loop {
        tokio::select! {
            // 收到 daemon 帧
            frame = framed.next() => {
                match frame {
                    Some(Ok((FRAME_TYPE_EVENT, data))) => {
                        if let Ok(event) = proto::Event::decode(&data[..]) {
                            match event.data {
                                Some(event::Payload::DeviceUpdated(list)) => {
                                    let infos: Vec<DeviceInfo> =
                                        list.devices.iter().map(device_to_info).collect();
                                    update_device_cache(&app, infos);
                                }
                                Some(event::Payload::AdbUpdated(list)) => {
                                    let infos: Vec<AdbDeviceInfo> =
                                        list.devices.iter().map(device_to_adb_info).collect();
                                    let _ = app.emit("adb-updated", &infos);
                                }
                                Some(event::Payload::SessionUpdated(list)) => {
                                    let data = SessionListInfo {
                                        sessions: list.sessions.iter().map(session_to_info).collect(),
                                    };
                                    let _ = app.emit("sessions-updated", &data);
                                }
                                Some(event::Payload::ClipboardChanged(data)) => {
                                    let payload = ClipboardInfo {
                                        text: data.text,
                                        serial: data.serial,
                                    };
                                    let _ = app.emit("clipboard-changed", &payload);
                                }
                                Some(event::Payload::Notification(data)) => {
                                    let payload = NotificationInfo {
                                        serial: data.serial,
                                        title: data.title,
                                        text: data.text,
                                        app: data.app,
                                    };
                                    let _ = app.emit("notification-received", &payload);
                                }
                                None => {}
                            }
                        }
                    }

                    // 响应帧：路由到 pending oneshot，或处理初始握手响应
                    Some(Ok((FRAME_TYPE_RESPONSE, data))) => {
                        if let Ok(resp) = proto::Response::decode(&data[..]) {
                            if let Some(tx) = pending.remove(&resp.id) {
                                let _ = tx.send(Ok(resp));
                            } else {
                                match (resp.id, resp.result, resp.error) {
                                    (1, Some(response::Payload::DeviceList(list)), None) => {
                                        let infos: Vec<DeviceInfo> =
                                            list.devices.iter().map(device_to_info).collect();
                                        update_device_cache(&app, infos);
                                    }
                                    (2, Some(response::Payload::PairingInfo(info)), None) => {
                                        let pairing_info = PairingInfo {
                                            dns_id: info.dns_id,
                                            psk: info.psk,
                                            wifi_string: info.wifi_string,
                                        };
                                        if let Some(st) = app.try_state::<AppState>() {
                                            *st.pairing_info.lock() = Some(pairing_info.clone());
                                        }
                                        let _ = app.emit("pairing-info-updated", &pairing_info);
                                    }
                                    (_, _, Some(err)) => {
                                        eprintln!("IPC 握手失败: {} (code {})", err.message, err.code);
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }

                    Some(Ok((_, _))) => {}
                    _ => {
                        set_conn(&app, false);
                        break;
                    }
                }
            }

            // 来自 Tauri commands 的 IPC 请求
            Some(req) = cmd_rx.recv() => {
                if framed.send(req.frame).await.is_ok() {
                    pending.insert(req.id, req.response_tx);
                }
            }
        }
    }
}

async fn wait_for_port(path: &std::path::Path, timeout_ms: u64) -> Option<String> {
    let start = std::time::Instant::now();
    loop {
        if let Ok(content) = tokio::fs::read_to_string(path).await {
            let trimmed: String = content.trim().to_string();
            if !trimmed.is_empty() {
                return Some(trimmed);
            }
        }
        if start.elapsed().as_millis() as u64 >= timeout_ms {
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

fn set_conn(app: &AppHandle, ok: bool) {
    if let Some(st) = app.try_state::<AppState>() {
        *st.connected.lock() = ok;
    }
    let _ = app.emit("connection-changed", ok);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(AppState {
            devices: Mutex::new(Vec::new()),
            connected: Mutex::new(false),
            pairing_info: Mutex::new(None),
            ipc_tx: Mutex::new(None),
            // 1/2 保留给启动握手的 device.list / pairing.info
            next_id: AtomicU64::new(3),
            fusion_sessions: Mutex::new(HashMap::new()),
        })
        .invoke_handler(tauri::generate_handler![
            get_devices,
            get_connection_status,
            get_pairing_info,
            update_session_config,
            retry_session,
            start_session,
            get_adb_devices,
            get_settings,
            set_autostart,
            set_scrcpy_params,
            get_apps,
            open_app,
            fusion::fusion_start,
            fusion::fusion_touch,
            fusion::fusion_scroll,
            fusion::fusion_key,
            fusion::fusion_text,
            fusion::fusion_back,
            fusion::fusion_resize,
            fusion::fusion_close,
        ])
        .on_window_event(|window, event| {
            // 融合窗口销毁 → 通知主窗口更新计数（UI 本地维护窗口列表）
            if matches!(event, tauri::WindowEvent::Destroyed)
                && window.label().starts_with("fusion-")
            {
                let _ = window
                    .app_handle()
                    .emit("fusion-window-closed", window.label());
                let _ = window
                    .app_handle()
                    .state::<AppState>()
                    .fusion_sessions
                    .lock()
                    .remove(window.label());
            }
        })
        .setup(|app| {
            let h = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                connect_daemon(h).await;
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
