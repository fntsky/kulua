//! 部署 + 建连阶段：部署 kulua-server → 取设备名 → 解析直连 IP → UDP 握手。
//!
//! 从 runner.rs 的 `Session::run()` 整段抽出（纯搬家）：日志文案、KULUA_TIMING
//! 计时标签、失败即置 `SESSION_STATE_FAILED` 再 `server.stop(...)` 清理的顺序，
//! 与拆分前逐字/逐路径一致。

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use kulua_proto::generated::HelloAck;
use kulua_proto::session::{ConnectOpts, UdpSession};

use crate::adb_cmd::AdbOps;
use crate::scrcpy;
use crate::types::Device;

use super::audio::AudioTarget;
use super::handle::SESSION_STATE_FAILED;
use super::proto::audio_codec_name;

/// 部署 kulua-server → 取设备名 → 解析直连 IP → UDP 握手。
///
/// 成功返回 `(server, udp, peer)` 供主循环使用；`t0` 由调用方传入，保证
/// `[timing]` 标签数值与拆分前一致。
///
/// 失败时就地完成全部收尾：打印错误日志 → 置 `state` 为 FAILED →（若 server 已
/// 部署）`server.stop(...)` 清理远端进程与本地 adb shell，然后返回 `Err(())`。
/// 调用方无需再做失败处理，直接退出即可（早退路径与拆分前逐字一致）。
#[allow(clippy::too_many_arguments)]
pub(super) async fn deploy_and_connect(
    adb: Arc<dyn AdbOps>,
    device: Device,
    jar_path: String,
    port: u16,
    params: crate::settings::ScrcpyParams,
    mut device_name_tx: Option<tokio::sync::mpsc::Sender<(String, String)>>,
    initial_target: AudioTarget,
    state: Arc<AtomicU8>,
    t0: std::time::Instant,
) -> Result<(scrcpy::ScrcpyServer, UdpSession, SocketAddr), ()> {
    let timing = std::env::var("KULUA_TIMING")
        .map(|v| v == "1")
        .unwrap_or(false);
    let mark = |label: &str| {
        if timing {
            eprintln!("[timing] {}: {}ms", label, t0.elapsed().as_millis());
        }
    };

    let serial = device.serial.clone();

    // 1. 部署 kulua-server（phone 绑定网络 UDP 端口）
    let adb_deploy = adb.clone();
    let device_deploy = device.clone();
    let server = tokio::task::spawn_blocking(move || {
        scrcpy::ScrcpyServer::deploy_scrcpy(
            adb_deploy.as_ref(),
            &device_deploy,
            &jar_path,
            port,
            params,
        )
    })
    .await;
    mark("deploy（adb push/spawn app_process）");

    let mut server = match server {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            eprintln!("scrcpy deploy failed for {}: {:?}", device.serial, e);
            state.store(SESSION_STATE_FAILED, Ordering::SeqCst);
            return Err(());
        }
        Err(e) => {
            eprintln!("spawn_blocking panic: {}", e);
            state.store(SESSION_STATE_FAILED, Ordering::SeqCst);
            return Err(());
        }
    };
    println!("kulua-server launching on {} (udp port={})", serial, port);

    // 2. 设备名（无 adb forward 后 server 不再发元数据；getprop 获取）
    if device.name.is_empty() {
        let adb = adb.clone();
        let serial_for_closure = serial.clone();
        let tx = device_name_tx.take();
        if tx.is_some() {
            let result = tokio::task::spawn_blocking(move || {
                adb.run(&[
                    "-s",
                    &serial_for_closure,
                    "shell",
                    "getprop",
                    "ro.product.model",
                ])
            })
            .await;
            if let Ok(Ok(output)) = result {
                let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !name.is_empty() {
                    println!("Device name: {} ({})", name, serial);
                    let _ = tx.unwrap().send((serial.clone(), name)).await;
                }
            }
        }
    }
    mark("设备名 getprop");

    // 3. 解析 phone 直连地址（serial IP 优先，USB 走 ip route）
    let addr_string = match scrcpy::resolve_device_ip(adb.as_ref(), &serial, port) {
        Some(a) => a,
        None => {
            eprintln!(
                "[{}] 无法解析设备 UDP 直连地址（需要 Wi-Fi；serial/IP 均无），判死",
                serial
            );
            state.store(SESSION_STATE_FAILED, Ordering::SeqCst);
            server.stop(adb.as_ref());
            return Err(());
        }
    };
    let peer: SocketAddr = match addr_string.parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("[{}] 非法直连地址 {addr_string}: {e}", serial);
            state.store(SESSION_STATE_FAILED, Ordering::SeqCst);
            server.stop(adb.as_ref());
            return Err(());
        }
    };
    let scid = scrcpy::scid_hex(port);

    // 4. UDP 握手（含冷启动重试，同旧 TCP 重试语义）
    //
    // 音频媒体端口恒开启（audio_link=true）：音频可在会话中途热开启，接收
    // 端点必须先注册好；`hello_audio` 才是"握手时是否就让 phone 起采集"。
    let hello_audio = initial_target.enabled;
    let peer2 = peer;
    let scid2 = scid.clone();
    let udp = tokio::task::spawn_blocking(move || {
        UdpSession::connect(
            peer2,
            &scid2,
            ConnectOpts {
                audio_link: true,
                hello_audio,
                video: false,
            },
        )
    })
    .await;
    let udp = match udp {
        Ok(Ok(s)) => s,
        _ => {
            eprintln!("[{}] UDP 直连 {peer} 失败（握手超时）", serial);
            state.store(SESSION_STATE_FAILED, Ordering::SeqCst);
            server.stop(adb.as_ref());
            return Err(());
        }
    };
    mark("UDP 握手（HELLO/HELLO_ACK）");
    println!("[{}] UDP 直连已就绪 @ {peer}", serial);

    // HELLO_ACK 携带初始 codec 状态（会话初始代次 0 的实际编码）
    let hello_ack: HelloAck = udp.hello_ack().unwrap_or_default();
    let initial_codec_id = hello_ack.audio_codec;
    if initial_target.enabled {
        if initial_codec_id == 0 {
            println!("Audio stream disabled by device, continuing without audio");
        } else {
            println!(
                "Audio codec: {} (0x{:08x}) from {}",
                audio_codec_name(initial_codec_id),
                initial_codec_id,
                serial
            );
        }
    }
    mark("codec 初始信息");

    Ok((server, udp, peer))
}
