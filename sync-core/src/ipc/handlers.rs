//! IPC 方法处理函数。
//!
//! 自 `server.rs` 拆出：`dispatch_request` 只保留「方法名 → 处理函数」的分派骨架，
//! 各方法自身的参数校验与命令下发集中在本模块，便于按方法组阅读与维护。

use crate::app::Command;
use crate::ipc::proto::{self, request, response};
use crate::types::Device;
use crate::wireless_pair::WirelessPairing;
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::sync::{mpsc, watch};

use super::server::{ERROR_CODE, make_error, make_ok, make_result};

/// `device.list`：下发融合后的设备列表。
pub(super) fn device_list(
    req: &proto::Request,
    merged_watch: &watch::Receiver<Vec<Device>>,
) -> proto::Response {
    let devices = merged_watch
        .borrow()
        .iter()
        .map(proto::Device::from)
        .collect();
    make_result(
        req.id,
        response::Payload::DeviceList(response::DeviceList { devices }),
    )
}

/// `device.adb_list`：下发 ADB 原始设备列表。
pub(super) fn device_adb_list(
    req: &proto::Request,
    device_watch: &watch::Receiver<HashMap<String, Device>>,
) -> proto::Response {
    let devices = device_watch
        .borrow()
        .values()
        .map(proto::Device::from)
        .collect();
    make_result(
        req.id,
        response::Payload::AdbList(response::DeviceList { devices }),
    )
}

/// `device.connect`：校验 serial 后向 Core 下发连接命令。
pub(super) async fn device_connect(
    req: &proto::Request,
    cmd_tx: &mpsc::Sender<Command>,
) -> proto::Response {
    let Some(request::Payload::DeviceConnect(params)) = &req.params else {
        return make_error(req.id, ERROR_CODE, "缺少 device.connect 参数");
    };
    if params.serial.is_empty() {
        return make_error(req.id, ERROR_CODE, "缺少 serial 参数");
    }
    if cmd_tx
        .send(Command::Connect(params.serial.clone()))
        .await
        .is_err()
    {
        return make_error(req.id, ERROR_CODE, "core 正在关闭");
    }
    make_ok(req.id)
}

/// `device.disconnect`：校验 serial 后向 Core 下发断开命令。
pub(super) async fn device_disconnect(
    req: &proto::Request,
    cmd_tx: &mpsc::Sender<Command>,
) -> proto::Response {
    let Some(request::Payload::DeviceDisconnect(params)) = &req.params else {
        return make_error(req.id, ERROR_CODE, "缺少 device.disconnect 参数");
    };
    if params.serial.is_empty() {
        return make_error(req.id, ERROR_CODE, "缺少 serial 参数");
    }
    if cmd_tx
        .send(Command::Disconnect(params.serial.clone()))
        .await
        .is_err()
    {
        return make_error(req.id, ERROR_CODE, "core 正在关闭");
    }
    make_ok(req.id)
}

/// `clipboard.get`：读取本机剪贴板文本（读取失败时返回空串）。
pub(super) fn clipboard_get(req: &proto::Request) -> proto::Response {
    let text = match crate::clipboard::get_text() {
        Ok(t) => t,
        Err(_) => String::new(),
    };
    make_result(
        req.id,
        response::Payload::ClipboardGet(response::ClipboardGet { text }),
    )
}

/// `pairing.info`：返回无线配对信息。
pub(super) fn pairing_info(req: &proto::Request, pair_info: &WirelessPairing) -> proto::Response {
    make_result(
        req.id,
        response::Payload::PairingInfo(response::PairingInfo {
            dns_id: pair_info.dns_id.clone(),
            psk: pair_info.psk.clone(),
            wifi_string: pair_info.get_info(),
        }),
    )
}

/// `session.update`：校验 uuid 后更新会话同步开关与音量。
pub(super) async fn session_update(
    req: &proto::Request,
    cmd_tx: &mpsc::Sender<Command>,
) -> proto::Response {
    let Some(request::Payload::SessionUpdate(params)) = &req.params else {
        return make_error(req.id, ERROR_CODE, "缺少 session.update 参数");
    };
    let clipboard_sync = params.clipboard_sync.unwrap_or(true);
    let notification_sync = params.notification_sync.unwrap_or(true);
    let audio_sync = params.audio_sync.unwrap_or(false);
    let volume = params.volume.unwrap_or(80).min(100) as u16;

    if params.uuid.is_empty() {
        return make_error(req.id, ERROR_CODE, "缺少 uuid 参数");
    }
    match params.uuid.parse::<uuid::Uuid>() {
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
                return make_error(req.id, ERROR_CODE, "core 正在关闭");
            }
            make_ok(req.id)
        }
        Err(_) => make_error(req.id, ERROR_CODE, "无效 uuid 格式"),
    }
}

/// `session.retry`：校验 uuid 后请求 Core 重试会话。
pub(super) async fn session_retry(
    req: &proto::Request,
    cmd_tx: &mpsc::Sender<Command>,
) -> proto::Response {
    let Some(request::Payload::SessionRetry(params)) = &req.params else {
        return make_error(req.id, ERROR_CODE, "缺少 session.retry 参数");
    };
    if params.uuid.is_empty() {
        return make_error(req.id, ERROR_CODE, "缺少 uuid 参数");
    }
    match params.uuid.parse::<uuid::Uuid>() {
        Ok(parsed) => {
            if cmd_tx.send(Command::Retry(parsed)).await.is_err() {
                return make_error(req.id, ERROR_CODE, "core 正在关闭");
            }
            make_ok(req.id)
        }
        Err(_) => make_error(req.id, ERROR_CODE, "无效 uuid 格式"),
    }
}

/// `session.start`：校验 serial 后请求 Core 启动会话。
pub(super) async fn session_start(
    req: &proto::Request,
    cmd_tx: &mpsc::Sender<Command>,
) -> proto::Response {
    let Some(request::Payload::SessionStart(params)) = &req.params else {
        return make_error(req.id, ERROR_CODE, "缺少 session.start 参数");
    };
    if params.serial.is_empty() {
        return make_error(req.id, ERROR_CODE, "缺少 serial 参数");
    }
    if cmd_tx
        .send(Command::StartSession(params.serial.clone()))
        .await
        .is_err()
    {
        return make_error(req.id, ERROR_CODE, "core 正在关闭");
    }
    make_ok(req.id)
}
// ── 融合模式 ──
/// `app.list`（融合模式）：枚举设备应用，等待 Core 同步回复。
pub(super) async fn app_list(
    req: &proto::Request,
    cmd_tx: &mpsc::Sender<Command>,
) -> proto::Response {
    let Some(request::Payload::AppListParams(params)) = &req.params else {
        return make_error(req.id, ERROR_CODE, "缺少 app.list 参数");
    };
    let uuid = match params.uuid.parse::<uuid::Uuid>() {
        Ok(u) => u,
        Err(_) => return make_error(req.id, ERROR_CODE, "无效 uuid 格式"),
    };
    let (tx, rx) = tokio::sync::oneshot::channel();
    if cmd_tx
        .send(Command::ListApps {
            uuid,
            force: params.force,
            reply: tx,
        })
        .await
        .is_err()
    {
        return make_error(req.id, ERROR_CODE, "core 正在关闭");
    }
    // 枚举最长 20s + 调度余量；等不到回复视为 core 异常
    match tokio::time::timeout(std::time::Duration::from_secs(30), rx).await {
        Ok(Ok(Ok(reply))) => make_result(
            req.id,
            response::Payload::AppList(response::AppList {
                apps: reply.apps.iter().map(proto::AppInfo::from).collect(),
                fusion_supported: reply.fusion_supported,
            }),
        ),
        Ok(Ok(Err(message))) => make_error(req.id, ERROR_CODE, &message),
        Ok(Err(_)) => make_error(req.id, ERROR_CODE, "core 正在关闭"),
        Err(_) => make_error(req.id, ERROR_CODE, "应用枚举超时"),
    }
}

/// `app.open`（融合模式）：拉起设备应用，等待 Core 返回窗口信息。
pub(super) async fn app_open(
    req: &proto::Request,
    cmd_tx: &mpsc::Sender<Command>,
) -> proto::Response {
    let Some(request::Payload::AppOpenParams(params)) = &req.params else {
        return make_error(req.id, ERROR_CODE, "缺少 app.open 参数");
    };
    let uuid = match params.uuid.parse::<uuid::Uuid>() {
        Ok(u) => u,
        Err(_) => return make_error(req.id, ERROR_CODE, "无效 uuid 格式"),
    };
    if params.package_name.is_empty() {
        return make_error(req.id, ERROR_CODE, "缺少 package_name 参数");
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    if cmd_tx
        .send(Command::OpenApp {
            uuid,
            package_name: params.package_name.clone(),
            reply: tx,
        })
        .await
        .is_err()
    {
        return make_error(req.id, ERROR_CODE, "core 正在关闭");
    }
    match tokio::time::timeout(std::time::Duration::from_secs(10), rx).await {
        Ok(Ok(Ok((window_id, addr)))) => make_result(
            req.id,
            response::Payload::AppOpen(response::AppOpen { window_id, addr }),
        ),
        Ok(Ok(Err(message))) => make_error(req.id, ERROR_CODE, &message),
        Ok(Err(_)) => make_error(req.id, ERROR_CODE, "core 正在关闭"),
        Err(_) => make_error(req.id, ERROR_CODE, "打开应用超时"),
    }
}
// ── 设置 ──

/// 组装设置回执（自启动状态 + 编码参数真源 `settings::read`）。
fn settings_payload(autostart_enabled: bool, autostart_supported: bool) -> response::Settings {
    let config = crate::settings::read();
    response::Settings {
        autostart_enabled,
        autostart_supported,
        video_bit_rate: config.video_bit_rate,
        video_max_size: config.video_max_size,
        video_max_fps: config.video_max_fps,
        audio_bit_rate: config.audio_bit_rate,
        audio_codec: config.audio_codec.clone(),
        icon_theme: config.icon_theme.clone(),
    }
}

/// `settings.get`：返回当前编码设置与自启动状态。
pub(super) fn settings_get(req: &proto::Request) -> proto::Response {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("daemon"));
    let state = crate::autostart::current_state(&exe);
    make_result(
        req.id,
        response::Payload::Settings(settings_payload(state.enabled, state.supported)),
    )
}

/// `settings.set_autostart`：设置开机自启动并回读状态。
pub(super) fn settings_set_autostart(req: &proto::Request) -> proto::Response {
    let Some(request::Payload::SetAutostart(params)) = &req.params else {
        return make_error(req.id, ERROR_CODE, "缺少 settings.set_autostart 参数");
    };
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("daemon"));
    match crate::autostart::set_enabled(&exe, params.enabled) {
        Ok(state) => make_result(
            req.id,
            response::Payload::Settings(settings_payload(state.enabled, state.supported)),
        ),
        Err(e) => make_error(req.id, ERROR_CODE, &format!("设置自启动失败: {e}")),
    }
}

/// `settings.set_scrcpy_params`：写入 scrcpy 编码参数并重启全部会话。
pub(super) async fn settings_set_scrcpy_params(
    req: &proto::Request,
    cmd_tx: &mpsc::Sender<Command>,
) -> proto::Response {
    let Some(request::Payload::SetScrcpyParams(params)) = &req.params else {
        return make_error(req.id, ERROR_CODE, "缺少 settings.set_scrcpy_params 参数");
    };
    let mut config = crate::settings::read();
    if let Some(v) = params.video_bit_rate {
        config.video_bit_rate = v;
    }
    if let Some(v) = params.video_max_size {
        config.video_max_size = v;
    }
    if let Some(v) = params.video_max_fps {
        config.video_max_fps = v;
    }
    if let Some(v) = params.audio_bit_rate {
        config.audio_bit_rate = v;
    }
    if let Some(v) = &params.audio_codec {
        // 白名单校验：只接受 scrcpy 支持的音频编码器
        if matches!(v.as_str(), "opus" | "aac" | "flac" | "raw") {
            config.audio_codec = v.clone();
        } else {
            return make_error(req.id, ERROR_CODE, &format!("不支持的音频编码器: {v}"));
        }
    }
    match crate::settings::write(&config) {
        Ok(()) => {
            // 通知 Core 重启所有设备会话，使新编码参数立即生效
            if cmd_tx.send(Command::RestartAllSessions).await.is_err() {
                return make_error(req.id, ERROR_CODE, "core 正在关闭");
            }
            let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("daemon"));
            let state = crate::autostart::current_state(&exe);
            make_result(
                req.id,
                response::Payload::Settings(settings_payload(state.enabled, state.supported)),
            )
        }
        Err(e) => make_error(req.id, ERROR_CODE, &format!("保存 scrcpy 参数失败: {e}")),
    }
}

/// `settings.set_icon_theme`：写入图标主题（dark/light）并回读设置。
///
/// 不重启任何会话：窗口/托盘图标由各自进程按新主题即时刷新。
pub(super) fn settings_set_icon_theme(req: &proto::Request) -> proto::Response {
    let Some(request::Payload::SetIconTheme(params)) = &req.params else {
        return make_error(req.id, ERROR_CODE, "缺少 settings.set_icon_theme 参数");
    };
    if !matches!(params.icon_theme.as_str(), "dark" | "light") {
        return make_error(
            req.id,
            ERROR_CODE,
            &format!("不支持的图标主题: {}", params.icon_theme),
        );
    }
    let mut config = crate::settings::read();
    config.icon_theme = params.icon_theme.clone();
    match crate::settings::write(&config) {
        Ok(()) => {
            let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("daemon"));
            let state = crate::autostart::current_state(&exe);
            make_result(
                req.id,
                response::Payload::Settings(settings_payload(state.enabled, state.supported)),
            )
        }
        Err(e) => make_error(req.id, ERROR_CODE, &format!("保存图标主题失败: {e}")),
    }
}
