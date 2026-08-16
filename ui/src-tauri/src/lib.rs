use parking_lot::Mutex;
use prost::Message;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use sync_core::ipc::proto::{self, event, request, response};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::{mpsc, oneshot};

/// 通过长连接发送的 IPC 请求，响应通过 oneshot 回传。
struct IpcRequest {
    id: u64,
    frame: (u8, Vec<u8>),
    response_tx: oneshot::Sender<Result<proto::Response, String>>,
}

// ── State ──

struct AppState {
    devices: Mutex<Vec<DeviceInfo>>,
    connected: Mutex<bool>,
    pairing_info: Mutex<Option<PairingInfo>>,
    /// 长连接的 IPC 发送端
    ipc_tx: Mutex<Option<mpsc::Sender<IpcRequest>>>,
    /// 自增请求 ID（1、2 保留给连接建立后的初始握手）
    next_id: AtomicU64,
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
    audio_enabled: bool,
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

// ── IPC Client ──

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
        })
        .invoke_handler(tauri::generate_handler![
            get_devices,
            get_connection_status,
            get_pairing_info,
            update_session_config,
            retry_session,
            start_session,
            get_adb_devices,
        ])
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
