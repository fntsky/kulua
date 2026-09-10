//! IPC 服务端。
//!
//! 帧格式：`[length:u32 LE][type:u8][payload]`，其中
//! Request / Response / Event 的 payload 为 Protobuf 编码。

use crate::app::Command;
use crate::ipc::proto::{self, event, response};
use crate::ipc::types::*;
use crate::notification::NotifInfo;
use crate::types::Device;

use super::handlers;
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

/// 统一的 RPC 错误码（-1）。
pub(super) const ERROR_CODE: i32 = -1;

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
#[allow(clippy::too_many_arguments)]
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
                let devices = merged_watch.borrow().iter().map(proto::Device::from).collect();
                let event = proto::Event {
                    data: Some(event::Payload::DeviceUpdated(proto::DeviceListData { devices })),
                };
                let payload = event.encode_to_vec();
                if framed.send((FRAME_TYPE_EVENT, payload)).await.is_err() {
                    break;
                }
            }

            _ = device_watch.changed() => {
                let devices = device_watch.borrow().values().map(proto::Device::from).collect();
                let event = proto::Event {
                    data: Some(event::Payload::AdbUpdated(proto::DeviceListData { devices })),
                };
                let payload = event.encode_to_vec();
                if framed.send((FRAME_TYPE_EVENT, payload)).await.is_err() {
                    break;
                }
            }

            _ = session_watch.changed() => {
                let sessions = session_watch.borrow().iter().map(proto::SessionSummary::from).collect();
                let event = proto::Event {
                    data: Some(event::Payload::SessionUpdated(proto::SessionListData { sessions })),
                };
                let payload = event.encode_to_vec();
                if framed.send((FRAME_TYPE_EVENT, payload)).await.is_err() {
                    break;
                }
            }

            result = clip_sub.recv() => {
                match result {
                    Ok(text) => {
                        let event = proto::Event {
                            data: Some(event::Payload::ClipboardChanged(proto::ClipboardData {
                                text,
                                serial: String::new(),
                            })),
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
                        let event = proto::Event {
                            data: Some(event::Payload::Notification(proto::NotificationData {
                                serial: notif.serial.clone(),
                                title: notif.title.unwrap_or_default(),
                                text: notif.body.unwrap_or_default(),
                                app: notif.package,
                            })),
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

            let response =
                dispatch_request(&req, cmd_tx, device_watch, merged_watch, pair_info).await;
            let payload = response.encode_to_vec();

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

// ── 方法路由 ──

/// 根据 method 分发到对应的处理函数，返回 Protobuf 响应。
///
/// 各方法自身的参数校验与命令下发见 [`super::handlers`]；此处只保留
/// 「方法名 → 处理函数」的分派骨架。
async fn dispatch_request(
    req: &proto::Request,
    cmd_tx: &mpsc::Sender<Command>,
    device_watch: &watch::Receiver<HashMap<String, Device>>,
    merged_watch: &watch::Receiver<Vec<Device>>,
    pair_info: &WirelessPairing,
) -> proto::Response {
    match req.method.as_str() {
        "device.list" => handlers::device_list(req, merged_watch),
        "device.adb_list" => handlers::device_adb_list(req, device_watch),
        "device.connect" => handlers::device_connect(req, cmd_tx).await,
        "device.disconnect" => handlers::device_disconnect(req, cmd_tx).await,
        "clipboard.get" => handlers::clipboard_get(req),
        "pairing.info" => handlers::pairing_info(req, pair_info),
        "session.update" => handlers::session_update(req, cmd_tx).await,
        "session.retry" => handlers::session_retry(req, cmd_tx).await,
        "session.start" => handlers::session_start(req, cmd_tx).await,

        // ── 融合模式 ──
        "app.list" => handlers::app_list(req, cmd_tx).await,
        "app.open" => handlers::app_open(req, cmd_tx).await,

        // ── 设置 ──
        "settings.get" => handlers::settings_get(req),
        "settings.set_autostart" => handlers::settings_set_autostart(req),
        "settings.set_scrcpy_params" => handlers::settings_set_scrcpy_params(req, cmd_tx).await,

        _ => make_error(req.id, ERROR_CODE, &format!("未知方法: {}", req.method)),
    }
}

// ── 响应构造辅助 ──

pub(super) fn make_result(id: u64, result: response::Payload) -> proto::Response {
    proto::Response {
        id,
        result: Some(result),
        error: None,
    }
}

pub(super) fn make_ok(id: u64) -> proto::Response {
    proto::Response {
        id,
        result: None,
        error: None,
    }
}

pub(super) fn make_error(id: u64, code: i32, message: &str) -> proto::Response {
    proto::Response {
        id,
        result: None,
        error: Some(proto::RpcError {
            code,
            message: message.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::proto::request;
    use crate::types::{DeviceIdentity, DeviceState};
    use tokio::sync::watch;

    fn req(id: u64, method: &str, params: Option<request::Payload>) -> proto::Request {
        proto::Request {
            id,
            method: method.to_string(),
            params,
        }
    }

    fn test_device() -> Device {
        Device {
            uuid: uuid::Uuid::new_v4(),
            id: "R58N1234567".into(),
            serial: "R58N1234567".into(),
            state: DeviceState::Device,
            name: "Pixel".into(),
            identity: DeviceIdentity::default(),
        }
    }

    #[tokio::test]
    async fn device_connect_queues_command() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel(32);
        let (_dev_tx, device_watch) = watch::channel(HashMap::new());
        let (_m_tx, merged_watch) = watch::channel(Vec::<Device>::new());
        let pair_info = WirelessPairing::new();

        let resp = dispatch_request(
            &req(
                1,
                "device.connect",
                Some(request::Payload::DeviceConnect(request::DeviceSerial {
                    serial: "192.168.1.2:5555".into(),
                })),
            ),
            &cmd_tx,
            &device_watch,
            &merged_watch,
            &pair_info,
        )
        .await;

        assert!(resp.error.is_none(), "connect 应成功: {:?}", resp.error);
        assert!(resp.result.is_none(), "connect 无结果");
        match cmd_rx.try_recv() {
            Ok(Command::Connect(serial)) => assert_eq!(serial, "192.168.1.2:5555"),
            other => panic!("expected Command::Connect, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn session_start_rejects_empty_serial() {
        let (cmd_tx, _cmd_rx) = mpsc::channel(32);
        let (_dev_tx, device_watch) = watch::channel(HashMap::new());
        let (_m_tx, merged_watch) = watch::channel(Vec::<Device>::new());
        let pair_info = WirelessPairing::new();

        let resp = dispatch_request(
            &req(
                2,
                "session.start",
                Some(request::Payload::SessionStart(request::DeviceSerial {
                    serial: String::new(),
                })),
            ),
            &cmd_tx,
            &device_watch,
            &merged_watch,
            &pair_info,
        )
        .await;

        let err = resp.error.expect("空 serial 应报错");
        assert_eq!(err.code, ERROR_CODE);
        assert!(err.message.contains("serial"));
    }

    #[tokio::test]
    async fn session_update_keeps_defaults_for_missing_options() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel(32);
        let (_dev_tx, device_watch) = watch::channel(HashMap::new());
        let (_m_tx, merged_watch) = watch::channel(Vec::<Device>::new());
        let pair_info = WirelessPairing::new();
        let uuid = "11111111-2222-3333-4444-555555555555";

        let resp = dispatch_request(
            &req(
                3,
                "session.update",
                Some(request::Payload::SessionUpdate(request::SessionUpdate {
                    uuid: uuid.into(),
                    clipboard_sync: None,
                    notification_sync: None,
                    audio_sync: None,
                    volume: None,
                })),
            ),
            &cmd_tx,
            &device_watch,
            &merged_watch,
            &pair_info,
        )
        .await;

        assert!(resp.error.is_none(), "update 应成功: {:?}", resp.error);
        match cmd_rx.try_recv() {
            Ok(Command::UpdateConfig {
                uuid: parsed,
                clipboard_sync,
                notification_sync,
                audio_sync,
                volume,
            }) => {
                assert_eq!(parsed.to_string(), uuid);
                assert!(clipboard_sync);
                assert!(notification_sync);
                assert!(!audio_sync);
                assert_eq!(volume, 80);
            }
            other => panic!("expected Command::UpdateConfig, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn device_list_returns_typed_devices() {
        let (cmd_tx, _cmd_rx) = mpsc::channel(32);
        let (_dev_tx, device_watch) = watch::channel(HashMap::new());
        let (_m_tx, merged_watch) = watch::channel(vec![test_device()]);
        let pair_info = WirelessPairing::new();

        let resp = dispatch_request(
            &req(4, "device.list", None),
            &cmd_tx,
            &device_watch,
            &merged_watch,
            &pair_info,
        )
        .await;

        match resp.result {
            Some(response::Payload::DeviceList(list)) => {
                assert_eq!(list.devices.len(), 1);
                assert_eq!(list.devices[0].serial, "R58N1234567");
                assert_eq!(list.devices[0].state, "Device");
            }
            other => panic!("expected typed DeviceList, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn unknown_method_returns_error() {
        let (cmd_tx, _cmd_rx) = mpsc::channel(32);
        let (_dev_tx, device_watch) = watch::channel(HashMap::new());
        let (_m_tx, merged_watch) = watch::channel(Vec::<Device>::new());
        let pair_info = WirelessPairing::new();

        let resp = dispatch_request(
            &req(5, "no.such.method", None),
            &cmd_tx,
            &device_watch,
            &merged_watch,
            &pair_info,
        )
        .await;

        let err = resp.error.expect("未知方法应报错");
        assert!(err.message.contains("未知方法"));
    }

    #[tokio::test]
    async fn app_list_queues_command_and_returns_result() {
        // app.list 需要同步结果：测试从命令通道取出命令并回填 oneshot 回复
        let (cmd_tx, mut cmd_rx) = mpsc::channel(32);
        let (_dev_tx, device_watch) = watch::channel(HashMap::new());
        let (_m_tx, merged_watch) = watch::channel(Vec::<Device>::new());
        let pair_info = WirelessPairing::new();
        let uuid = "11111111-2222-3333-4444-555555555555";

        let request = req(
            6,
            "app.list",
            Some(request::Payload::AppListParams(request::AppListParams {
                uuid: uuid.into(),
                force: false,
            })),
        );
        let resp_future =
            dispatch_request(&request, &cmd_tx, &device_watch, &merged_watch, &pair_info);
        tokio::pin!(resp_future);
        // dispatch 内部会等待 oneshot 回复：并发驱动请求与命令通道，避免死锁
        let resp = loop {
            tokio::select! {
                resp = &mut resp_future => break resp,
                cmd = cmd_rx.recv() => {
                    match cmd.expect("应收到 ListApps 命令") {
                        Command::ListApps {
                            uuid: parsed,
                            force,
                            reply,
                        } => {
                            assert_eq!(parsed.to_string(), uuid);
                            assert!(!force, "未指定 force 时应为 false");
                            reply
                                .send(Ok(crate::app::AppListReply {
                                    apps: vec![crate::apps::AppInfo {
                                        package_name: "com.android.settings".into(),
                                        label: "Settings".into(),
                                        system: true,
                                    }],
                                    fusion_supported: true,
                                }))
                                .expect("reply 应送达");
                        }
                        other => panic!("expected Command::ListApps, got {:?}", other),
                    }
                }
            }
        };
        match resp.result {
            Some(response::Payload::AppList(list)) => {
                assert!(list.fusion_supported);
                assert_eq!(list.apps.len(), 1);
                assert_eq!(list.apps[0].package_name, "com.android.settings");
                assert!(list.apps[0].system);
            }
            other => panic!("expected typed AppList, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn app_list_propagates_core_error() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel(32);
        let (_dev_tx, device_watch) = watch::channel(HashMap::new());
        let (_m_tx, merged_watch) = watch::channel(Vec::<Device>::new());
        let pair_info = WirelessPairing::new();
        let uuid = "11111111-2222-3333-4444-555555555555";

        let request = req(
            7,
            "app.list",
            Some(request::Payload::AppListParams(request::AppListParams {
                uuid: uuid.into(),
                force: true,
            })),
        );
        let resp_future =
            dispatch_request(&request, &cmd_tx, &device_watch, &merged_watch, &pair_info);
        tokio::pin!(resp_future);
        let resp = loop {
            tokio::select! {
                resp = &mut resp_future => break resp,
                cmd = cmd_rx.recv() => {
                    match cmd.expect("应收到 ListApps 命令") {
                        Command::ListApps { reply, .. } => {
                            reply
                                .send(Err("枚举应用失败: 超时".into()))
                                .expect("reply 应送达");
                        }
                        other => panic!("expected Command::ListApps, got {:?}", other),
                    }
                }
            }
        };
        let err = resp.error.expect("core 错误应透传");
        assert!(err.message.contains("枚举应用失败"));
    }

    #[tokio::test]
    async fn app_list_rejects_invalid_uuid() {
        let (cmd_tx, _cmd_rx) = mpsc::channel(32);
        let (_dev_tx, device_watch) = watch::channel(HashMap::new());
        let (_m_tx, merged_watch) = watch::channel(Vec::<Device>::new());
        let pair_info = WirelessPairing::new();

        let resp = dispatch_request(
            &req(
                8,
                "app.list",
                Some(request::Payload::AppListParams(request::AppListParams {
                    uuid: "not-a-uuid".into(),
                    force: false,
                })),
            ),
            &cmd_tx,
            &device_watch,
            &merged_watch,
            &pair_info,
        )
        .await;

        let err = resp.error.expect("非法 uuid 应报错");
        assert!(err.message.contains("uuid"));
    }

    #[tokio::test]
    async fn app_open_queues_command_and_returns_window_id() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel(32);
        let (_dev_tx, device_watch) = watch::channel(HashMap::new());
        let (_m_tx, merged_watch) = watch::channel(Vec::<Device>::new());
        let pair_info = WirelessPairing::new();
        let uuid = "11111111-2222-3333-4444-555555555555";

        let request = req(
            9,
            "app.open",
            Some(request::Payload::AppOpenParams(request::AppOpenParams {
                uuid: uuid.into(),
                package_name: "com.android.settings".into(),
            })),
        );
        let resp_future =
            dispatch_request(&request, &cmd_tx, &device_watch, &merged_watch, &pair_info);
        tokio::pin!(resp_future);
        let resp = loop {
            tokio::select! {
                resp = &mut resp_future => break resp,
                cmd = cmd_rx.recv() => {
                    match cmd.expect("应收到 OpenApp 命令") {
                        Command::OpenApp {
                            uuid: parsed,
                            package_name,
                            reply,
                        } => {
                            assert_eq!(parsed.to_string(), uuid);
                            assert_eq!(package_name, "com.android.settings");
                            reply.send(Ok((42, "192.168.1.5:27183".into()))).expect("reply 应送达");
                        }
                        other => panic!("expected Command::OpenApp, got {:?}", other),
                    }
                }
            }
        };
        match resp.result {
            Some(response::Payload::AppOpen(open)) => {
                assert_eq!(open.window_id, 42);
                assert_eq!(open.addr, "192.168.1.5:27183");
            }
            other => panic!("expected typed AppOpen, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn app_open_rejects_missing_package() {
        let (cmd_tx, _cmd_rx) = mpsc::channel(32);
        let (_dev_tx, device_watch) = watch::channel(HashMap::new());
        let (_m_tx, merged_watch) = watch::channel(Vec::<Device>::new());
        let pair_info = WirelessPairing::new();

        let resp = dispatch_request(
            &req(
                10,
                "app.open",
                Some(request::Payload::AppOpenParams(request::AppOpenParams {
                    uuid: "11111111-2222-3333-4444-555555555555".into(),
                    package_name: String::new(),
                })),
            ),
            &cmd_tx,
            &device_watch,
            &merged_watch,
            &pair_info,
        )
        .await;

        let err = resp.error.expect("空包名应报错");
        assert!(err.message.contains("package_name"));
    }
}
