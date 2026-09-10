use crate::{adb_cmd::AdbOps, types::Device};
use std::{
    io::{BufRead, BufReader},
    thread,
};

/// scid 高 16 位固定标记：`0x4B4C` 即 ASCII "KL"（Kulua 前缀），低 16 位为会话端口。
///
/// 每个 Kulua session 生成确定性 scid，server 参数携带（UDP 会话隔离标识，
/// 与 kill_by_scid 对齐）。不再依赖 localabstract socket 名。
pub const SCID_PREFIX: u32 = 0x4B4C_0000;

/// 由会话端口推导确定性 scid（31 位非负）。
pub fn scid_for_port(port: u16) -> u32 {
    SCID_PREFIX | u32::from(port)
}

/// scid 的 8 位小写 16 进制字符串（server 参数 `scid=<hex>`，HELLO 校验用）。
pub fn scid_hex(port: u16) -> String {
    format!("{:08x}", scid_for_port(port))
}

/// 生成按 scid 精准 kill 的 shell 脚本。
///
/// 用 `grep -l` 一次扫描全部 cmdline（匹配文件路径），而不是逐进程循环
/// `tr | grep`：无线 adb 下 toybox 每个 fork 都慢，984 个 /proc 进程的循环
/// 实测 ~26s，而单次 grep 扫描 <0.2s（快 160 倍）。匹配结果形如
/// `/proc/<pid>/cmdline`，从中提取 pid 后 kill。
/// - 跳过 `$$`（脚本自身 shell）与 `/proc/self`、`/proc/thread-self`
/// - 以 `true` 结尾保证 `adb shell` 退出码为 0
pub fn build_scid_kill_script(scid_hex: &str) -> String {
    format!(
        "for f in $(grep -l 'scid={}' /proc/[0-9]*/cmdline 2>/dev/null); do \
         p=${{f#/proc/}}; p=${{p%/cmdline}}; \
         [ \"$p\" = \"$$\" ] && continue; \
         [ \"$p\" = \"self\" ] && continue; \
         kill -9 \"$p\" 2>/dev/null; done; true",
        scid_hex
    )
}

/// 按 scid 精准杀死设备上属于本会话的 kulua-server 进程。
///
/// 替换旧的 broad kill（`grep com.genymobile.scrcpy` 全量击杀）：只匹配参数含
/// `scid=<本会话 hex>` 的进程，其它 scrcpy / 融合窗口实例不受影响。
pub fn kill_by_scid(adb: &dyn AdbOps, serial: &str, port: u16) {
    let script = build_scid_kill_script(&scid_hex(port));
    let _ = adb.run(&["-s", serial, "shell", &script]);
}

/// 解析设备当前可用于 UDP 直连的 Wi-Fi 地址。
///
/// 优先级：
/// 1. `serial` 形如 `ip:port`（adb wireless 主路径）→ 取 IP
/// 2. 其它（无 IP 的 serial，如 mDNS/USB）：取设备**默认路由网卡**的 inet 地址。
///    比直接扫 `src <ip>` 可靠——src 可能是旧的/其它网卡（tethering/VPN）IP，
///    用错 IP 会导致 UDP 直连 HELLO 频繁超时。
///
/// 返回 (ip, port)：ip 为解析到的设备地址，port 为会话端口（UDP vs 语义无差）。
pub fn resolve_device_ip(adb: &dyn AdbOps, serial: &str, port: u16) -> Option<String> {
    if let Some(ip) = serial_ip(serial) {
        return Some(format!("{ip}:{port}"));
    }
    // 无 IP 的 serial：优先按默认路由网卡取 inet 地址
    if let Some(iface) = default_route_dev(adb, serial) {
        if let Some(ip) = iface_inet_ip(adb, serial, &iface) {
            return Some(format!("{ip}:{port}"));
        }
    }
    // 兜底：扫 `ip route` 里的 `src <ip>`
    let out = adb
        .run(&["-s", serial, "shell", "ip", "route"])
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    for line in out.lines() {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        for (i, t) in tokens.iter().enumerate() {
            if *t == "src" {
                if let Some(ip) = tokens.get(i + 1) {
                    return Some(format!("{ip}:{port}"));
                }
            }
        }
    }
    None
}

/// 解析设备默认路由的出网卡名（`ip route` 中 `default ... dev <iface>`）。
fn default_route_dev(adb: &dyn AdbOps, serial: &str) -> Option<String> {
    let out = adb
        .run(&["-s", serial, "shell", "ip", "route"])
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    for line in out.lines() {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.first() != Some(&"default") {
            continue;
        }
        for (i, t) in tokens.iter().enumerate() {
            if *t == "dev" {
                if let Some(dev) = tokens.get(i + 1) {
                    return Some((*dev).to_string());
                }
            }
        }
    }
    None
}

/// 取指定网卡的 IPv4 地址（`ip -f inet addr show <iface>` 中 `inet A.B.C.D/n`）。
fn iface_inet_ip(adb: &dyn AdbOps, serial: &str, iface: &str) -> Option<String> {
    let out = adb
        .run(&[
            "-s", serial, "shell", "ip", "-f", "inet", "addr", "show", iface,
        ])
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    for line in out.lines() {
        // 形如 "    inet 192.168.1.6/24 brd ... scope global wlan0"
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.first() == Some(&"inet") {
            if let Some(addr) = tokens.get(1) {
                if let Some(ip) = addr.split('/').next() {
                    if ip.parse::<std::net::Ipv4Addr>().is_ok() {
                        return Some(ip.to_string());
                    }
                }
            }
        }
    }
    None
}

/// 从 `ip:port` 形式的 serial 提取 IP（含 IPv4/IPv6 冒号处理）。
fn serial_ip(serial: &str) -> Option<String> {
    let s = serial.trim();
    // IPv4: 192.168.1.5:5555
    if s.contains('.') {
        if let Some((ip, _)) = s.rsplit_once(':') {
            if ip.parse::<std::net::Ipv4Addr>().is_ok() {
                return Some(ip.to_string());
            }
        }
    }
    // IPv6: [::1]:5555
    if let Some(rest) = s.strip_prefix('[') {
        if let Some((ip, _)) = rest.rsplit_once("]:") {
            return Some(format!("[{ip}]"));
        }
    }
    None
}

pub struct ScrcpyServer {
    device: Device,
    process: std::process::Child,
    pub port: u16,
}

/// 判断本地 jar 是否需要 push 到设备：本地缺失 / 远程缺失 / 内容 md5 不一致。
///
/// WHY: 只看“远程存在”会跳过协议换代后的重新部署（设备上残留旧 jar → UDP
/// 直连握手超时）。用 `md5sum` 比较内容，本地重建过就重新 push，内容一致才跳过。
fn jar_needs_push(adb: &dyn AdbOps, serial: &str, local: &str, remote: &str) -> bool {
    let Ok(local_bytes) = std::fs::read(local) else {
        return true; // 本地缺失按需 push 处理（调用方随后会报 push 失败）
    };
    let remote_md5 = adb
        .run(&["-s", serial, "shell", "md5sum", remote])
        .ok()
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .split_whitespace()
                .next()
                .map(str::to_string)
        });
    let local_md5 = {
        use md5::Digest;
        format!("{:x}", md5::Md5::digest(&local_bytes))
    };
    remote_md5.as_deref() != Some(local_md5.as_str())
}

/// 远程 jar 路径（自研 kulua-server，替代官方 scrcpy-server）。
pub const REMOTE_JAR: &str = "/data/local/tmp/kulua-server.jar";

impl ScrcpyServer {
    /// 部署自研 kulua-server（单进程多显示器，UDP 直连）。
    ///
    /// 与旧版的关键区别（docs/direct-udp-protocol.md）：
    /// - 不再 `adb forward`，server 直接绑定 phone 网络 UDP 端口 `port=<n>`
    ///   （PC 端以 device IP:port 直连）
    /// - server 参数：scid（会话隔离）+ port（UDP 端口）+ audio 编码参数
    pub fn deploy_scrcpy(
        adb: &dyn AdbOps,
        device: &Device,
        local_jar: &str,
        port: u16,
        params: crate::settings::ScrcpyParams,
    ) -> Result<Self, crate::types::AdbError> {
        // 部署分步计时（KULUA_TIMING=1 时打印），定位启动慢的瓶颈
        let timing = std::env::var("KULUA_TIMING")
            .map(|v| v == "1")
            .unwrap_or(false);
        let t0 = std::time::Instant::now();
        let mark = |label: &str| {
            if timing {
                eprintln!(
                    "[timing]   deploy: {} — {}ms",
                    label,
                    t0.elapsed().as_millis()
                );
            }
        };

        // 仅清理同 scid 的陈旧进程（上次崩溃残留），不碰其它 scrcpy 实例（如融合窗口）
        kill_by_scid(adb, &device.serial, port);
        mark("kill_by_scid");

        let remote_jar = REMOTE_JAR;
        // 本地/远程内容比较（md5）：仅当缺失或内容不一致才 push。
        // WHY：不能只看“文件是否存在”——协议换代/重新构建后本地 jar 已变，
        // 若仍沿用设备上的旧 jar（如旧 TCP localabstract 版），UDP 直连会握手超时。
        let needs_push = jar_needs_push(adb, &device.serial, local_jar, remote_jar);
        mark("jar 检查");
        if needs_push {
            adb.push(device, local_jar, remote_jar)?;
        } else {
            println!("kulua-server.jar already up-to-date on device, skipping push");
        }
        mark("jar push（如需）");

        let classpath = format!("CLASSPATH={}", remote_jar);
        let mut args = vec![
            classpath,
            "app_process".to_string(),
            "/".to_string(),
            "com.kulua.server.Server".to_string(),
            format!("scid={}", scid_hex(port)),
            format!("port={}", port),
        ];
        // 音频编码参数：仅当显式配置时追加，否则 server 用默认（raw）
        if !params.audio_codec.is_empty() {
            args.push(format!("audio_codec={}", params.audio_codec));
        }
        if params.audio_bit_rate > 0 {
            args.push(format!("audio_bit_rate={}", params.audio_bit_rate));
        }
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let mut process = adb.spawn_shell(device, &arg_refs)?;
        mark("spawn app_process");
        if let Some(stderr) = process.stderr.take() {
            let serial = device.serial.clone();
            thread::spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines() {
                    match line {
                        Ok(l) => eprintln!("kulua-server[{}] stderr: {}", serial, l),
                        Err(_) => break,
                    }
                }
            });
        }
        Ok(ScrcpyServer {
            device: device.clone(),
            process,
            port,
        })
    }
    /// 非阻塞检查 server 进程是否已退出。
    /// 返回 `None` 表示仍在运行，`Some(exit_status)` 表示已退出。
    pub fn try_wait(&mut self) -> Option<std::process::ExitStatus> {
        self.process.try_wait().ok().flatten()
    }

    /// 交由设备端按客户端存活状态退出，后台回收本地 adb shell。
    pub fn detach(mut self) {
        // 不能关闭 adb shell：同一 server 可能仍承载其它客户端。
        thread::spawn(move || {
            let _ = self.process.wait();
        });
    }

    /// 强制停止 server（仅部署失败时清理远程进程和本地 adb shell）。
    pub fn stop(&mut self, adb: &dyn AdbOps) {
        // 先按 scid 精准杀远程（设备端 kulua-server），确保无论本地如何终止都不会残留
        kill_by_scid(adb, &self.device.serial, self.port);
        // 再杀本地 adb shell 进程
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scid_prefix_is_kl_marker() {
        // 高 16 位 0x4B4C 即 ASCII "KL"，用于在设备上区分 Kulua 会话与官方 scrcpy
        assert_eq!(SCID_PREFIX, 0x4B4C_0000);
    }

    #[test]
    fn scid_is_deterministic_from_port() {
        // 27183 = 0x6A2F
        assert_eq!(scid_for_port(27183), 0x4B4C_6A2F);
        assert_eq!(scid_for_port(0), SCID_PREFIX);
        assert_eq!(scid_for_port(0xFFFF), SCID_PREFIX | 0xFFFF);
        // 31 位非负，满足 server 侧 scid 约束（-1 或 0..2^31）
        assert!(scid_for_port(u16::MAX) < 1 << 31);
    }

    #[test]
    fn scid_hex_is_8_lowercase_hex_digits() {
        assert_eq!(scid_hex(27183), "4b4c6a2f");
        assert_eq!(scid_hex(0), "4b4c0000");
        assert_eq!(scid_hex(0xFFFF), "4b4cffff");
    }

    #[test]
    fn kill_script_targets_only_own_scid() {
        let script = build_scid_kill_script("4b4c6a17");
        assert!(script.contains("scid=4b4c6a17"), "应匹配本会话 scid");
        assert!(!script.contains("scid=4b4c6a18"), "不得匹配其它 scid");
        assert!(script.contains("$$"), "应跳过脚本自身 shell 进程");
        assert!(
            script.contains("/proc/[0-9]*/cmdline"),
            "应 grep 扫描 /proc cmdline（高效，非逐进程循环）"
        );
        assert!(
            !script.contains("tr '\\0'"),
            "不得用逐进程 tr|grep 循环（无线 adb 下极慢）"
        );
        assert!(script.contains("self"), "应跳过 /proc/self 与 thread-self");
        assert!(script.ends_with("true"), "应以 true 结尾保证 exit 0");
    }

    #[test]
    fn kill_script_quotes_scid() {
        // scid 必须带引号，防止设备 shell 展开/分词
        let script = build_scid_kill_script("4b4c6a17");
        assert!(script.contains("'scid=4b4c6a17'"), "scid 应被单引号包裹");
    }

    #[test]
    fn kill_script_extracts_pid_from_grep_path() {
        // 验证从 /proc/<pid>/cmdline 提取 pid 的参数展开逻辑
        let script = build_scid_kill_script("4b4c6a17");
        assert!(
            script.contains("p=${f#/proc/}") && script.contains("p=${p%/cmdline}"),
            "应从 grep 输出路径提取 pid"
        );
        assert!(script.contains("kill -9 \"$p\""), "应 kill 提取出的 pid");
    }

    #[test]
    fn serial_ip_extracts_ipv4() {
        assert_eq!(serial_ip("192.168.1.5:5555"), Some("192.168.1.5".into()));
        assert_eq!(serial_ip("10.0.0.2:4444"), Some("10.0.0.2".into()));
        assert_eq!(serial_ip("ABC123"), None);
        assert_eq!(serial_ip("2001:db8::1:5555"), None); // 纯 IPv6 无括号
    }

    #[test]
    fn serial_ip_extracts_bracketed_ipv6() {
        assert_eq!(
            serial_ip("[2001:db8::1]:5555"),
            Some("[2001:db8::1]".into())
        );
    }
}
