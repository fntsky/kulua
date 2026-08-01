use crate::app::Command;
use crate::ipc::types::*;
use crate::notification::NotifInfo;
use crate::types::Device;

use crate::wireless_pair::WirelessPairing;
use futures::SinkExt;
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
///
/// 端口号写入 `%TEMP%/sync-daemon.port`，Drop 时自动删除。
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
///
/// 每个新连接都 spawn 一个 `handle_ipc_connection` 任务来处理帧解析和 JSON-RPC 分发。
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
        // server drop → 自动删除端口文件
    })
}

// ── 单连接处理 ──

/// 处理单条 IPC 连接：读帧 → 解析 JSON-RPC → 分发 → 写响应。
///
/// 同时监听事件源：
/// - **TCP 帧**：解析 Request 并回复 Response
/// - **Session 变更**（`session_watch.changed()`）：推送 `session.updated` 事件
/// - **剪贴板变更**（`clip_sub`）：推送 `clipboard.changed` 事件
/// - **通知事件**（`notif_sub`）：推送 `notification` 事件
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
            // ── 收到客户端帧 ──
            frame = framed.next() => {
                match frame {
                    Some(Ok((frame_type, payload))) => {
                        if on_frame(frame_type, &payload, &mut framed, &cmd_tx, &device_watch, &merged_watch, &pair_info).await.is_err() {
                            break;
                        }
                    }
                    _ => {
                        // EOF 或 I/O 错误 → 断开
                        break;
                    }
                }
            }

            // ── 合并设备列表变更事件 ──
            _ = merged_watch.changed() => {
                let devices = merged_watch.borrow().clone();
                let event = Event::DeviceUpdated { data: devices };
                if let Ok(payload) = serde_json::to_vec(&event) {
                    if framed.send((FRAME_TYPE_EVENT, payload)).await.is_err() {
                        break;
                    }
                }
            }

            // ── adb 原始设备列表变更事件 ──
            _ = device_watch.changed() => {
                let devices: Vec<Device> = device_watch.borrow().values().cloned().collect();
                let event = Event::AdbUpdated { data: devices };
                if let Ok(payload) = serde_json::to_vec(&event) {
                    if framed.send((FRAME_TYPE_EVENT, payload)).await.is_err() {
                        break;
                    }
                }
            }

            // ── session 列表变更事件 ──
            // ── session 列表变更事件 ──
            _ = session_watch.changed() => {
                let sessions = session_watch.borrow().clone();
                let event = Event::SessionUpdated {
                    data: SessionListData { sessions },
                };
                if let Ok(payload) = serde_json::to_vec(&event) {
                    if framed.send((FRAME_TYPE_EVENT, payload)).await.is_err() {
                        break;
                    }
                }
            }

            // ── 剪贴板变更事件 ──
            result = clip_sub.recv() => {
                match result {
                    Ok(text) => {
                        let event = Event::ClipboardChanged {
                            data: ClipboardData {
                                text,
                                serial: String::new(), // 后续从 session 获取来源 serial
                            },
                        };
                        if let Ok(payload) = serde_json::to_vec(&event) {
                            if framed.send((FRAME_TYPE_EVENT, payload)).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(_)) => {} // 跳过丢失消息
                }
            }

            // ── 通知事件 ──
            result = notif_sub.recv() => {
                match result {
                    Ok(notif) => {
                        let event = Event::Notification {
                            data: NotifData {
                                serial: notif.serial.clone(),
                                title: notif.title.unwrap_or_default(),
                                text: notif.body.unwrap_or_default(),
                                app: notif.package,
                            },
                        };
                        if let Ok(payload) = serde_json::to_vec(&event) {
                            if framed.send((FRAME_TYPE_EVENT, payload)).await.is_err() {
                                break;
                            }
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

/// 根据帧类型分发：Request → 解析 + 回复；其他类型暂忽略。
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
            let req: JsonRpcRequest = serde_json::from_slice(payload).map_err(|e| {
                eprintln!("IPC: JSON-RPC 解析失败: {}", e);
            })?;

            let response = dispatch_request(&req, cmd_tx, device_watch, merged_watch, pair_info).await;

            let payload = serde_json::to_vec(&response).map_err(|e| {
                eprintln!("IPC: 序列化响应失败: {}", e);
            })?;
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

// ── JSON-RPC 方法路由 ──

/// 根据 method 分发到对应的处理函数，返回 JSON-RPC 响应。
async fn dispatch_request(
    req: &JsonRpcRequest,
    cmd_tx: &mpsc::Sender<Command>,
    device_watch: &watch::Receiver<HashMap<String, Device>>,
    merged_watch: &watch::Receiver<Vec<Device>>,
    pair_info: &WirelessPairing,
) -> JsonRpcResponse {
    match req.method.as_str() {
        // 获取合并后设备列表（含 uuid/name/identity，UI 设备卡片用）
        "device.list" => {
            let devices: Vec<Device> = merged_watch.borrow().clone();
            make_result(req.id, serde_json::to_value(&devices).unwrap_or_default())
        }

        // 获取 adb 原始设备列表（含 Offline/Unauthorized，UI "ADB 连接" 页面用）
        "device.adb_list" => {
            let devices: Vec<Device> = device_watch.borrow().values().cloned().collect();
            make_result(req.id, serde_json::to_value(&devices).unwrap_or_default())
        }

        // 连接设备
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

        // 断开设备
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

        // 读取 PC 端剪贴板
        "clipboard.get" => {
            let text = match clipboard_win::get_clipboard_string() {
                Ok(t) => t,
                Err(_) => String::new(),
            };
            make_result(req.id, serde_json::json!({"text": text}))
        }

        // 获取无线配对信息（二维码数据）
        "pairing.info" => make_result(
            req.id,
            serde_json::json!({
                "dns_id": pair_info.dns_id,
                "psk": pair_info.psk,
                "wifi_string": pair_info.get_info(),
            }),
        ),

        // 更新 session 配置（通知同步 / 剪贴板同步 / 音频开关 / 音量）
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
            println!(
                "IPC: session.update: uuid={}, clipboard={}, notification={}, audio={}, volume={}",
                uuid, clipboard_sync, notification_sync, audio_sync, volume
            );
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

        // 重试 failed 墓碑 session（UI 重试按钮）
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

        // 未知方法
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

// ── 测试 ──

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn port_file_lifecycle() {
        // 唯一端口文件，避免与其他并行测试的 bind 竞争同一路径
        let port_file = std::env::temp_dir().join(format!("sync-daemon-test-{}.port", uuid::Uuid::new_v4()));
        let _ = std::fs::remove_file(&port_file);

        let server = IpcServer::bind_with_port_file(port_file.clone()).await.unwrap();
        assert!(server.port > 0);
        assert!(port_file.exists(), "bind 后端口文件应存在");

        drop(server);
        assert!(!port_file.exists(), "drop 后端口文件应删除");
    }

    #[tokio::test]
    async fn accept_returns_connected_stream() {
        let mut server = IpcServer::bind().await.unwrap();
        let addr = server.listener.local_addr().unwrap();

        let client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let server_stream = server.accept().await.unwrap();

        assert_eq!(
            client.peer_addr().unwrap(),
            server_stream.local_addr().unwrap(),
        );
    }

    /// session.retry：合法 uuid → Command::Retry 入队 + result null；非法 uuid → error
    #[tokio::test]
    async fn dispatch_session_retry() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel(32);
        let (_dev_tx, device_watch) = watch::channel(HashMap::new());
        let (_m_tx, merged_watch) = watch::channel(Vec::<Device>::new());
        let pair_info = WirelessPairing::new();

        let req = JsonRpcRequest {
            id: 42,
            method: "session.retry".into(),
            params: serde_json::json!({"uuid": "11111111-2222-3333-4444-555555555555"}),
        };
        let resp = dispatch_request(&req, &cmd_tx, &device_watch, &merged_watch, &pair_info).await;
        assert!(resp.error.is_none(), "retry 应成功: {:?}", resp.error);
        assert_eq!(resp.id, 42);

        match cmd_rx.try_recv() {
            Ok(Command::Retry(uuid)) => assert_eq!(
                uuid,
                uuid::Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap()
            ),
            other => panic!("expected Command::Retry, got {:?}", other),
        }

        // 非法 uuid → error
        let bad = JsonRpcRequest {
            id: 43,
            method: "session.retry".into(),
            params: serde_json::json!({"uuid": "not-a-uuid"}),
        };
        let resp = dispatch_request(&bad, &cmd_tx, &device_watch, &merged_watch, &pair_info).await;
        assert!(resp.error.is_some(), "非法 uuid 应报错");
        assert_eq!(resp.id, 43);
    }

    /// 发送一个正确的 JSON-RPC 请求帧，验证能收到 Response 帧
    #[tokio::test]
    async fn handle_jsonrpc_request() {
        // 准备 server
        let mut server = IpcServer::bind().await.unwrap();
        let addr = server.listener.local_addr().unwrap();

        // 构造最小 dispatch 通道
        let (_cmd_tx, _cmd_rx) = mpsc::channel(32);
        let (_dev_tx, device_watch) = watch::channel(HashMap::new());
        let (_m_tx, merged_watch) = watch::channel(Vec::<Device>::new());
        let (_session_tx, session_watch) = watch::channel(Vec::<SessionSummary>::new());
        let (clip_tx, _) = broadcast::channel(64);
        let (notif_tx, notif_rx) = broadcast::channel(64);

        // accept + handle_ipc_connection
        let join = tokio::spawn(async move {
            let stream = server.accept().await.unwrap();
            let clip_sub = clip_tx.subscribe();
            handle_ipc_connection(
                stream,
                _cmd_tx,
                device_watch,
                merged_watch,
                session_watch,
                clip_sub,
                notif_rx,
                WirelessPairing::new(),
            )
            .await;
        });

        // 客户端连接并发送 device.list 请求
        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let req = serde_json::json!({"id": 1, "method": "device.list", "params": {}});
        let payload = serde_json::to_vec(&req).unwrap();

        // 手动构造帧: [len:4][type:1][payload]
        let mut frame = Vec::new();
        frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        frame.push(FRAME_TYPE_REQUEST);
        frame.extend_from_slice(&payload);
        client.write_all(&frame).await.unwrap();

        // 读 response 帧
        let mut buf = [0u8; 4096];
        let n = client.readable().await.unwrap();
        let n = client.try_read(&mut buf).unwrap();
        assert!(n >= 5, "应收到至少 5 字节的帧头");
        let resp_len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        assert_eq!(buf[4], FRAME_TYPE_RESPONSE, "帧类型应为 Response");
        let _resp_body = &buf[5..5 + resp_len];
        // 若能解析为 JsonRpcResponse 即通过
        let _resp: JsonRpcResponse = serde_json::from_slice(&buf[5..5 + resp_len]).unwrap();
    }

    #[tokio::test]
    async fn serve_accepts_and_drops_connection() {
        let server = IpcServer::bind().await.unwrap();
        let addr = server.listener.local_addr().unwrap();
        let token = CancellationToken::new();
        let (_cmd_tx, _cmd_rx) = mpsc::channel(32);
        let (_dev_tx, device_watch) = watch::channel(HashMap::new());
        let (_m_tx, merged_watch) = watch::channel(Vec::<Device>::new());
        let (_session_tx, session_watch) = watch::channel(Vec::<SessionSummary>::new());
        let (clip_tx, _) = broadcast::channel(64);
        let (notif_tx, _) = broadcast::channel(64);

        let _handle = serve(
            server,
            token.clone(),
            _cmd_tx,
            device_watch,
            merged_watch,
            session_watch,
            clip_tx,
            notif_tx,
            WirelessPairing::new(),
        );
    }

    #[tokio::test]
    async fn serve_recovers_after_client_disconnect() {
        let server = IpcServer::bind().await.unwrap();
        let addr = server.listener.local_addr().unwrap();
        let token = CancellationToken::new();
        let (_cmd_tx, _cmd_rx) = mpsc::channel(32);
        let (_dev_tx, device_watch) = watch::channel(HashMap::new());
        let (_m_tx, merged_watch) = watch::channel(Vec::<Device>::new());
        let (_session_tx, session_watch) = watch::channel(Vec::<SessionSummary>::new());
        let (clip_tx, _) = broadcast::channel(64);
        let (notif_tx, _) = broadcast::channel(64);

        let _handle = serve(
            server,
            token.clone(),
            _cmd_tx,
            device_watch,
            merged_watch,
            session_watch,
            clip_tx,
            notif_tx,
            WirelessPairing::new(),
        );

        // 第一次连接
        let mut c1 = tokio::net::TcpStream::connect(addr).await.unwrap();
        c1.write_all(b"hello").await.unwrap();
        drop(c1);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // 第二次连接，应能再次 accept
        let c2 = tokio::net::TcpStream::connect(addr).await.unwrap();
        drop(c2);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        token.cancel();
    }
}
