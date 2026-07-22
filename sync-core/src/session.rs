use crate::adb_cmd::AdbOps;
use crate::notification::{self, NotifInfo};
use crate::scrcpy;
use crate::types::{AdbError, Device};

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, oneshot};

/// 轻量 session 句柄，主循环用来发停止信号 + 等待任务结束
pub struct Handle {
    #[allow(dead_code)]
    pub device: Device,
    pub stop_tx: Option<oneshot::Sender<()>>,
    pub task: tokio::task::JoinHandle<()>,
}

impl Handle {
    pub async fn stop(&mut self, _adb: &dyn AdbOps) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        // 给 session 一点时间自行清理
        let _ = tokio::time::timeout(Duration::from_secs(3), &mut self.task).await;
    }
}

/// 启动一个设备的完整 session：部署 scrcpy → TCP 连接 → 双向剪贴板 I/O + 通知轮询
pub async fn run(
    adb: Arc<dyn AdbOps>,
    device: Device,
    jar_path: String,
    port: u16,
    mut clip_sub: broadcast::Receiver<String>,
    phone_clip_tx: mpsc::Sender<String>,
    notif_tx: mpsc::Sender<NotifInfo>,
    mut stop_rx: oneshot::Receiver<()>,
) {
    // 1. 部署 scrcpy-server（阻塞 ADB 操作 → spawn_blocking）
    let server = tokio::task::spawn_blocking({
        let adb = adb.clone();
        let device = device.clone();
        let jar_path = jar_path.clone();
        move || scrcpy::ScrcpyServer::deploy_clipboard_only(adb.as_ref(), &device, &jar_path, port)
    })
    .await;

    let mut server = match server {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            eprintln!("scrcpy deploy failed for {}: {:?}", device.serial, e);
            run_notification_only(adb, device, notif_tx, stop_rx).await;
            return;
        }
        Err(e) => {
            eprintln!("spawn_blocking panic: {}", e);
            return;
        }
    };
    println!("scrcpy-server alive on {} (port {})", device.serial, port);

    // 2. 连接 scrcpy TCP 控制通道
    let stream = loop {
        tokio::select! {
            _ = &mut stop_rx => {
                server.stop(adb.as_ref());
                return;
            }
            r = TcpStream::connect(format!("127.0.0.1:{}", port)) => {
                if let Ok(mut s) = r {
                    let mut dummy = [0u8; 1];
                    tokio::select! {
                        _ = &mut stop_rx => {
                            server.stop(adb.as_ref());
                            return;
                        }
                        rr = s.read_exact(&mut dummy) => {
                            if rr.is_ok() { break s; }
                        }
                    }
                }
            }
        }
        tokio::select! {
            _ = &mut stop_rx => {
                server.stop(adb.as_ref());
                return;
            }
            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
        }
    };

    // 3. 启动通知轮询（子任务）
    let notif_stop = Arc::new(AtomicBool::new(false));
    let np_stop = notif_stop.clone();
    let np_adb = adb.clone();
    let np_device = device.clone();
    let np_tx = notif_tx.clone();
    notification::spawn_notification_poller_tokio(np_adb, np_device, np_tx, np_stop);

    // 4. 双向剪贴板 I/O
    run_clipboard_io(stream, &mut clip_sub, &phone_clip_tx, stop_rx).await;

    // 5. 清理
    notif_stop.store(true, Ordering::SeqCst);
    server.stop(adb.as_ref());
    println!("Session {} cleaned up", device.serial);
}

/// 剪贴板服务不可用时仅启动通知轮询
async fn run_notification_only(
    adb: Arc<dyn AdbOps>,
    device: Device,
    notif_tx: mpsc::Sender<NotifInfo>,
    stop_rx: oneshot::Receiver<()>,
) {
    let stop = Arc::new(AtomicBool::new(false));
    notification::spawn_notification_poller_tokio(adb, device, notif_tx, stop.clone());

    let _ = stop_rx.await;
    stop.store(true, Ordering::SeqCst);
}

// ─── 双向剪贴板 I/O ─────────────────────────────────────

async fn run_clipboard_io(
    mut stream: TcpStream,
    clip_sub: &mut broadcast::Receiver<String>,
    phone_clip_tx: &mpsc::Sender<String>,
    mut stop_rx: oneshot::Receiver<()>,
) {
    loop {
        tokio::select! {
            biased;
            _ = &mut stop_rx => break,

            // PC→Phone：广播剪贴板 → 写入 scrcpy 控制连接
            result = clip_sub.recv() => {
                match result {
                    Ok(text) => {
                        if let Err(e) = scrcpy::send_clipboard_async(&mut stream, &text).await {
                            eprintln!("send clipboard: {}", e);
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        eprintln!("clipboard broadcast lagged by {}", n);
                    }
                }
            }

            // Phone→PC：读取设备剪贴板事件 → 发送给 Core
            result = read_phone_clipboard(&mut stream) => {
                match result {
                    Ok(Some(text)) => {
                        if phone_clip_tx.send(text).await.is_err() {
                            break;
                        }
                    }
                    Ok(None) => {}
                    Err(_) => break,
                }
            }
        }
    }
}

/// 从 scrcpy 控制连接读取一条设备剪贴板消息（200ms 超时返回 None）
async fn read_phone_clipboard(stream: &mut TcpStream) -> Result<Option<String>, AdbError> {
    let mut type_buf = [0u8; 1];
    let result = tokio::time::timeout(
        Duration::from_millis(200),
        stream.read_exact(&mut type_buf),
    )
    .await;
    match result {
        Err(_timeout) => return Ok(None),
        Ok(Err(e)) => return Err(AdbError::Io(e)),
        Ok(Ok(_)) => {}
    }

    if type_buf[0] != 0x00 {
        let mut extra = [0u8; 64];
        let n = stream.read(&mut extra).await.unwrap_or(0);
        println!(
            "scrcpy msg: type=0x{:02X}, payload ({} bytes): {:02X?}",
            type_buf[0], n, &extra[..n]
        );
        return Ok(None);
    }

    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await.map_err(AdbError::Io)?;
    let text_len = u32::from_be_bytes(len_buf) as usize;

    let mut text = vec![0u8; text_len];
    stream.read_exact(&mut text).await.map_err(AdbError::Io)?;

    let clip_text = String::from_utf8(text).map_err(|e| AdbError::Other(format!("{}", e)))?;
    println!("Phone clipboard: {}", clip_text);
    Ok(Some(clip_text))
}
