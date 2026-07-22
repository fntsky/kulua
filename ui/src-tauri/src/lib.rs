use parking_lot::Mutex;
use tauri::{AppHandle, Emitter, Manager, State};

// ── State ──

struct AppState {
    devices: Mutex<Vec<DeviceInfo>>,
    connected: Mutex<bool>,
    pairing_info: Mutex<Option<PairingInfo>>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct PairingInfo {
    dns_id: String,
    psk: String,
    wifi_string: String,
}

#[derive(Clone, serde::Serialize)]
struct DeviceInfo {
    serial: String,
    state: String,
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

    use futures::SinkExt;
    use sync_core::ipc::types::{
        FrameCodec, FRAME_TYPE_EVENT, FRAME_TYPE_REQUEST, FRAME_TYPE_RESPONSE,
    };
    use tokio_stream::StreamExt;
    use tokio_util::codec::Framed;

    let mut framed = Framed::new(stream, FrameCodec);

    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1u64,
        "method": "device.list",
        "params": {},
    });
    if framed
        .send((FRAME_TYPE_REQUEST, serde_json::to_vec(&req).unwrap()))
        .await
        .is_err()
    {
        set_conn(&app, false);
        return;
    }

    let pair_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2u64,
        "method": "pairing.info",
        "params": {},
    });
    if framed
        .send((FRAME_TYPE_REQUEST, serde_json::to_vec(&pair_req).unwrap()))
        .await
        .is_err()
    {
        eprintln!("pairing.info request failed, continuing without QR data");
    }

    loop {
        match framed.next().await {
            Some(Ok((FRAME_TYPE_EVENT, data))) => {
                if let Ok(val) = serde_json::from_slice::<serde_json::Value>(&data) {
                    if val.get("event").and_then(|v| v.as_str()) == Some("device.updated") {
                        if let Some(devices_val) = val.get("data") {
                            if let Ok(devices) =
                                serde_json::from_value::<Vec<sync_core::types::Device>>(
                                    devices_val.clone(),
                                )
                            {
                                let infos: Vec<DeviceInfo> = devices
                                    .iter()
                                    .map(|d| DeviceInfo {
                                        serial: d.serial.clone(),
                                        state: format!("{:?}", d.state),
                                    })
                                    .collect();
                                if let Some(st) = app.try_state::<AppState>() {
                                    *st.devices.lock() = infos.clone();
                                }
                                let _ = app.emit("devices-updated", &infos);
                            }
                        }
                    }
                }
            }
            Some(Ok((FRAME_TYPE_RESPONSE, data))) => {
                if let Ok(resp) =
                    serde_json::from_slice::<sync_core::ipc::types::JsonRpcResponse>(&data)
                {
                    if let Some(result) = resp.result {
                        match resp.id {
                            1 => {
                                if let Ok(devices) =
                                    serde_json::from_value::<Vec<sync_core::types::Device>>(result)
                                {
                                    let infos: Vec<DeviceInfo> = devices
                                        .iter()
                                        .map(|d| DeviceInfo {
                                            serial: d.serial.clone(),
                                            state: format!("{:?}", d.state),
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

// ── Entry ──

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(AppState {
            devices: Mutex::new(Vec::new()),
            connected: Mutex::new(false),
            pairing_info: Mutex::new(None),
        })
        .invoke_handler(tauri::generate_handler![
            get_devices,
            get_connection_status,
            get_pairing_info,
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
