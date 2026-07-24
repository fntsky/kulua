use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::{mpsc, oneshot};

/// 通过长连接发送的 IPC 请求，响应通过 oneshot 回传。
struct IpcRequest {
    id: u64,
    frame: (u8, Vec<u8>),
    response_tx: oneshot::Sender<Result<serde_json::Value, String>>,
}

// ── State ──

struct AppState {
    devices: Mutex<Vec<DeviceInfo>>,
    connected: Mutex<bool>,
    pairing_info: Mutex<Option<PairingInfo>>,
    /// 长连接的 IPC 发送端
    ipc_tx: Mutex<Option<mpsc::Sender<IpcRequest>>>,
    /// 自增请求 ID
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

/// 通过长连接向 daemon 发送 JSON-RPC 请求并等待响应。
async fn ipc_request(
    state: &State<'_, AppState>,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    use sync_core::ipc::types::FRAME_TYPE_REQUEST;

    let id = state.next_id.fetch_add(1, Ordering::SeqCst);
    let (tx, rx) = oneshot::channel();

    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });

    let ipc_req = IpcRequest {
        id,
        frame: (FRAME_TYPE_REQUEST, serde_json::to_vec(&req).unwrap()),
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
    ) -> Result<(), String> {
        ipc_request(
            &state,
            "session.update",
            serde_json::json!({
                "uuid": uuid,
                "clipboard_sync": clipboard_sync,
                "notification_sync": notification_sync,
                "audio_sync": audio_sync,
            }),
        ).await?;
        Ok(())
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
    use sync_core::ipc::types::JsonRpcResponse;
    use tokio_stream::StreamExt;
    use tokio_util::codec::Framed;

    let mut framed = Framed::new(stream, FrameCodec);
    let mut pending: HashMap<u64, oneshot::Sender<Result<serde_json::Value, String>>> = HashMap::new();

    // ── 初始握手：device.list + pairing.info ──
    for (id, method, params) in [
        (1u64, "device.list", serde_json::json!({})),
        (2u64, "pairing.info", serde_json::json!({})),
    ] {
        let req = serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if framed.send((FRAME_TYPE_REQUEST, serde_json::to_vec(&req).unwrap())).await.is_err() {
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
                        if let Ok(val) = serde_json::from_slice::<serde_json::Value>(&data) {
                            let event_name = val.get("event").and_then(|v| v.as_str());
                            match event_name {
                                Some("device.updated") => {
                                    if let Some(devices_val) = val.get("data") {
                                        if let Ok(devices) =
                                            serde_json::from_value::<Vec<sync_core::types::Device>>(
                                                devices_val.clone(),
                                            )
                                        {
                                            let infos: Vec<DeviceInfo> = devices
                                                .iter()
                                                .map(|d| DeviceInfo {
                                                    uuid: d.uuid.to_string(),
                                                    serial: d.serial.clone(),
                                                    state: format!("{:?}", d.state),
                                                    name: d.name.clone(),
                                                })
                                                .collect();
                                            if let Some(st) = app.try_state::<AppState>() {
                                                *st.devices.lock() = infos.clone();
                                            }
                                            let _ = app.emit("devices-updated", &infos);
                                        }
                                    }
                                }
                                Some("session.updated") => {
                                    if let Some(sessions_val) = val.get("data") {
                                        if let Ok(session_data) = serde_json::from_value::<sync_core::ipc::types::SessionListData>(
                                            sessions_val.clone(),
                                        ) {
                                            let _ = app.emit("sessions-updated", &session_data);
                                        }
                                    }
                                }
                                Some("clipboard.changed") => {
                                    let _ = app.emit("clipboard-changed", &data);
                                }
                                Some("notification") => {
                                    let _ = app.emit("notification-received", &data);
                                }
                                _ => {}
                            }
                        }
                    }

                    // 响应帧：路由到 pending oneshot，或处理初始握手遗留响应
                    Some(Ok((FRAME_TYPE_RESPONSE, data))) => {
                        if let Ok(resp) = serde_json::from_slice::<JsonRpcResponse>(&data) {
                            if let Some(tx) = pending.remove(&resp.id) {
                                let result = match (resp.result, resp.error) {
                                    (Some(r), _) => Ok(r),
                                    (_, Some(e)) => Err(format!("{} (code {})", e.message, e.code)),
                                    _ => Ok(serde_json::Value::Null),
                                };
                                let _ = tx.send(result);
                            } else if let Some(result) = resp.result {
                                // 初始握手响应（id=1 或 id=2）
                                match resp.id {
                                    1 => {
                                        if let Ok(devices) =
                                            serde_json::from_value::<Vec<sync_core::types::Device>>(result)
                                        {
                                            let infos: Vec<DeviceInfo> = devices
                                                .iter()
                                                .map(|d| DeviceInfo {
                                                    uuid: d.uuid.to_string(),
                                                    serial: d.serial.clone(),
                                                    state: format!("{:?}", d.state),
                                                    name: d.name.clone(),
                                                })
                                                .collect();
                                            if let Some(st) = app.try_state::<AppState>() {
                                                *st.devices.lock() = infos.clone();
                                            }
                                            let _ = app.emit("devices-updated", &infos);
                                        }
                                    }
                                    2 => {
                                        if let Ok(info) =
                                            serde_json::from_value::<PairingInfo>(result)
                                        {
                                            if let Some(st) = app.try_state::<AppState>() {
                                                *st.pairing_info.lock() = Some(info.clone());
                                            }
                                            let _ = app.emit("pairing-info-updated", &info);
                                        }
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
            next_id: AtomicU64::new(1),
        })
        .invoke_handler(tauri::generate_handler![
            get_devices,
            get_connection_status,
            get_pairing_info,
            update_session_config,
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
