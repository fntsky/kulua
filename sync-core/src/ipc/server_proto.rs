//! Protobuf 版 IPC 服务端。
//!
//! 帧格式保持 `[length:u32 LE][type:u8][payload]` 不变，
//! 但 Request / Response / Event 的 payload 改为 Protobuf 编码。
//! 当前 `params` / `result` / `data` 内部仍暂时使用 JSON 字节，便于逐步迁移。

use crate::app::Command;
use crate::ipc::proto;
use crate::ipc::types::*;
use crate::notification::NotifInfo;
use crate::types::Device;

use crate::wireless_pair::WirelessPairing;
use futures::SinkExt;
use prost::Message;
use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, watch};
use tokio_stream::StreamExt;
use tokio_util::codec::Framed;
use tokio_util::sync::CancellationToken;

// ── IpcServer ──

/// TCP IPC 服务器，绑定 `127.0.0.1:0`（OS 分配端口）。
pub struct IpcServer {
    pub(crate) listener: TcpListener,
    port_file: PathBuf,
    port: u16,
}

impl IpcServer {
    /// 绑定回环地址 + 随机端口，写入默认端口文件（`%TEMP%/sync-daemon.port`）。
    pub async fn bind() -> Result<Self, io::Error> {
        Self::bind_with_port_file(std::env::temp_dir().join("sync-daemon.port")).await
    }

    /// 绑定回环地址 + 随机端口，写入指定端口文件（测试隔离用）。
    pub(crate) async fn bind_with_port_file(port_file: PathBuf) -> Result<Self, io::Error> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();

        tokio::fs::write(&port_file, port.to_string()).await?;
        println!("IPC 服务端监听 127.0.0.1:{}", port);

        Ok(Self {
            listener,
            port_file,
            port,
        })
    }

    /// 接受下一个 TCP 连接。
    pub async fn accept(&mut self) -> io::Result<TcpStream> {
        let (stream, addr) = self.listener.accept().await?;
        println!("IPC 客户端已连接: {}", addr);
        Ok(stream)
    }

    /// 当前绑定的端口号。
    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.port_file);
    }
}

// ── 服务入口 ──

/// 启动后台 accept 循环。
pub fn serve(
    mut server: IpcServer,
    token: CancellationToken,
    cmd_tx: mpsc::Sender<Command>,
    device_watch: watch::Receiver<HashMap<String, Device>>,
    merged_watch: watch::Receiver<Vec<Device>>,
    session_watch: watch::Receiver<Vec<super::types::SessionSummary>>,
    clip_tx: broadcast::Sender<String>,
    notif_tx: broadcast::Sender<NotifInfo>,
    pair_info: WirelessPairing,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = token.cancelled() => {
                    println!("IPC 服务端停止");
                    break;
                }
                result = server.accept() => {
                    match result {
                        Ok(stream) => {
                            let cmd_tx = cmd_tx.clone();
                            let dw = device_watch.clone();
                            let mw = merged_watch.clone();
                            let sw = session_watch.clone();
                            let clip_sub = clip_tx.subscribe();
                            let notif_sub = notif_tx.subscribe();
                            let pi = pair_info.clone();
                            tokio::spawn(handle_ipc_connection(
                                stream, cmd_tx, dw, mw, sw, clip_sub, notif_sub, pi,
                            ));
                        }
                        Err(e) => {
                            eprintln!("IPC accept 错误: {}", e);
                            break;
                        }
                    }
                }
            }
        }
    })
}

// ── 单连接处理 ──

#[allow(clippy::too_many_arguments)]
async fn handle_ipc_connection(
    stream: TcpStream,
    cmd_tx: mpsc::Sender<Command>,
    mut device_watch: watch::Receiver<HashMap<String, Device>>,
    mut merged_watch: watch::Receiver<Vec<Device>>,
    mut session_watch: watch::Receiver<Vec<super::types::SessionSummary>>,
    mut clip_sub: broadcast::Receiver<String>,
    mut notif_sub: broadcast::Receiver<NotifInfo>,
    pair_info: WirelessPairing,
) {
    let mut framed = Framed::new(stream, FrameCodec);

    loop {
        tokio::select! {
            frame = framed.next() => {
                match frame {
                    Some(Ok((frame_type, payload))) => {
                        if on_frame(frame_type, &payload, &mut framed, &cmd_tx, &device_watch, &merged_watch, &pair_info).await.is_err() {
                            break;
                        }
                    }
                    _ => break,
                }
            }

            _ = merged_watch.changed() => {
                let devices = merged_watch.borrow().clone();
                let event = proto::Event {
                    event: "device.updated".into(),
                    data: serde_json::to_vec(&devices).unwrap_or_default(),
                };
                let payload = event.encode_to_vec();
                if framed.send((FRAME_TYPE_EVENT, payload)).await.is_err() {
                    break;
                }
            }

            _ = device_watch.changed() => {
                let devices: Vec<Device> = device_watch.borrow().values().cloned().collect();
                let event = proto::Event {
                    event: "adb.updated".into(),
                    data: serde_json::to_vec(&devices).unwrap_or_default(),
                };
                let payload = event.encode_to_vec();
                if framed.send((FRAME_TYPE_EVENT, payload)).await.is_err() {
                    break;
                }
            }

            _ = session_watch.changed() => {
                let sessions = session_watch.borrow().clone();
                let data = SessionListData { sessions };
                let event = proto::Event {
                    event: "session.updated".into(),
                    data: serde_json::to_vec(&data).unwrap_or_default(),
                };
                let payload = event.encode_to_vec();
                if framed.send((FRAME_TYPE_EVENT, payload)).await.is_err() {
                    break;
                }
            }

            result = clip_sub.recv() => {
                match result {
                    Ok(text) => {
                        let data = ClipboardData {
                            text,
                            serial: String::new(),
                        };
                        let event = proto::Event {
                            event: "clipboard.changed".into(),
                            data: serde_json::to_vec(&data).unwrap_or_default(),
                        };
                        let payload = event.encode_to_vec();
                        if framed.send((FRAME_TYPE_EVENT, payload)).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                }
            }

            result = notif_sub.recv() => {
                match result {
                    Ok(notif) => {
                        let data = NotifData {
                            serial: notif.serial.clone(),
                            title: notif.title.unwrap_or_default(),
                            text: notif.body.unwrap_or_default(),
                            app: notif.package,
                        };
                        let event = proto::Event {
                            event: "notification".into(),
                            data: serde_json::to_vec(&data).unwrap_or_default(),
                        };
                        let payload = event.encode_to_vec();
                        if framed.send((FRAME_TYPE_EVENT, payload)).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                }
            }
        }
    }

    println!("IPC 客户端断开连接");
}

// ── 帧分发 ──

/// 根据帧类型分发：Request → 解析 Protobuf 请求并回复 Protobuf 响应。
async fn on_frame(
    frame_type: u8,
    payload: &[u8],
    framed: &mut Framed<TcpStream, FrameCodec>,
    cmd_tx: &mpsc::Sender<Command>,
    device_watch: &watch::Receiver<HashMap<String, Device>>,
    merged_watch: &watch::Receiver<Vec<Device>>,
    pair_info: &WirelessPairing,
) -> Result<(), ()> {
    match frame_type {
        FRAME_TYPE_REQUEST => {
            let req = proto::Request::decode(payload).map_err(|e| {
                eprintln!("IPC: Protobuf 请求解析失败: {}", e);
            })?;

            let params = if req.params.is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::from_slice(&req.params).map_err(|e| {
                    eprintln!("IPC: 请求 params JSON 解析失败: {}", e);
                })?
            };
            let legacy_req = JsonRpcRequest {
                id: req.id,
                method: req.method,
                params,
            };

            let response = dispatch_request(&legacy_req, cmd_tx, device_watch, merged_watch, pair_info).await;

            let mut proto_resp = proto::Response {
                id: response.id,
                result: Vec::new(),
                error: None,
            };
            if let Some(result) = response.result {
                proto_resp.result = serde_json::to_vec(&result).map_err(|e| {
                    eprintln!("IPC: 响应 result 序列化失败: {}", e);
                })?;
            } else if let Some(err) = response.error {
                proto_resp.error = Some(proto::RpcError {
                    code: err.code,
                    message: err.message,
                });
            }

            let payload = proto_resp.encode_to_vec();
            framed
                .send((FRAME_TYPE_RESPONSE, payload))
                .await
                .map_err(|_| ())?;

            Ok(())
        }

        FRAME_TYPE_AUDIO => Ok(()),

        _ => {
            eprintln!("IPC: 未知帧类型 0x{:02x}", frame_type);
            Ok(())
        }
    }
}

// ── JSON-RPC 方法路由（内部仍使用旧类型，后续逐步替换为强类型 Protobuf） ──

async fn dispatch_request(
    req: &JsonRpcRequest,
    cmd_tx: &mpsc::Sender<Command>,
    device_watch: &watch::Receiver<HashMap<String, Device>>,
    merged_watch: &watch::Receiver<Vec<Device>>,
    pair_info: &WirelessPairing,
) -> JsonRpcResponse {
    match req.method.as_str() {
        "device.list" => {
            let devices: Vec<Device> = merged_watch.borrow().clone();
            make_result(req.id, serde_json::to_value(&devices).unwrap_or_default())
        }

        "device.adb_list" => {
            let devices: Vec<Device> = device_watch.borrow().values().cloned().collect();
            make_result(req.id, serde_json::to_value(&devices).unwrap_or_default())
        }

        "device.connect" => {
            let serial = req
                .params
                .get("serial")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if serial.is_empty() {
                return make_error(req.id, -1, "缺少 serial 参数");
            }
            if cmd_tx
                .send(Command::Connect(serial.to_string()))
                .await
                .is_err()
            {
                return make_error(req.id, -1, "core 正在关闭");
            }
            make_result(req.id, serde_json::Value::Null)
        }

        "device.disconnect" => {
            let serial = req
                .params
                .get("serial")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if serial.is_empty() {
                return make_error(req.id, -1, "缺少 serial 参数");
            }
            if cmd_tx
                .send(Command::Disconnect(serial.to_string()))
                .await
                .is_err()
            {
                return make_error(req.id, -1, "core 正在关闭");
            }
            make_result(req.id, serde_json::Value::Null)
        }

        "clipboard.get" => {
            let text = match clipboard_win::get_clipboard_string() {
                Ok(t) => t,
                Err(_) => String::new(),
            };
            make_result(req.id, serde_json::json!({"text": text}))
        }

        "pairing.info" => make_result(
            req.id,
            serde_json::json!({
                "dns_id": pair_info.dns_id,
                "psk": pair_info.psk,
                "wifi_string": pair_info.get_info(),
            }),
        ),

        "session.update" => {
            let uuid = req
                .params
                .get("uuid")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let clipboard_sync = req
                .params
                .get("clipboard_sync")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let notification_sync = req
                .params
                .get("notification_sync")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let audio_sync = req
                .params
                .get("audio_sync")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let volume = req
                .params
                .get("volume")
                .and_then(|v| v.as_u64())
                .map(|v| v.min(100) as u16)
                .unwrap_or(80);
            if uuid.is_empty() {
                return make_error(req.id, -1, "缺少 uuid 参数");
            }
            match uuid.parse::<uuid::Uuid>() {
                Ok(parsed) => {
                    if cmd_tx
                        .send(Command::UpdateConfig {
                            uuid: parsed,
                            clipboard_sync,
                            notification_sync,
                            audio_sync,
                            volume,
                        })
                        .await
                        .is_err()
                    {
                        return make_error(req.id, -1, "core 正在关闭");
                    }
                    make_result(req.id, serde_json::Value::Null)
                }
                Err(_) => make_error(req.id, -1, "无效 uuid 格式"),
            }
        }

        "session.retry" => {
            let uuid = req
                .params
                .get("uuid")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if uuid.is_empty() {
                return make_error(req.id, -1, "缺少 uuid 参数");
            }
            match uuid.parse::<uuid::Uuid>() {
                Ok(parsed) => {
                    if cmd_tx.send(Command::Retry(parsed)).await.is_err() {
                        return make_error(req.id, -1, "core 正在关闭");
                    }
                    make_result(req.id, serde_json::Value::Null)
                }
                Err(_) => make_error(req.id, -1, "无效 uuid 格式"),
            }
        }

        "session.start" => {
            let serial = req
                .params
                .get("serial")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if serial.is_empty() {
                return make_error(req.id, -1, "缺少 serial 参数");
            }
            if cmd_tx
                .send(Command::StartSession(serial.to_string()))
                .await
                .is_err()
            {
                return make_error(req.id, -1, "core 正在关闭");
            }
            make_result(req.id, serde_json::Value::Null)
        }

        _ => make_error(req.id, -1, &format!("未知方法: {}", req.method)),
    }
}

// ── 响应构造辅助 ──

fn make_result(id: u64, result: serde_json::Value) -> JsonRpcResponse {
    JsonRpcResponse {
        id,
        result: Some(result),
        error: None,
    }
}

fn make_error(id: u64, code: i32, message: &str) -> JsonRpcResponse {
    JsonRpcResponse {
        id,
        result: None,
        error: Some(JsonRpcError {
            code,
            message: message.to_string(),
        }),
    }
}
