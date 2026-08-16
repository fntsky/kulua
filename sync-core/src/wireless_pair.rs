#[derive(Debug, Clone)]
pub struct WirelessPairing {
    pub dns_id: String,
    pub psk: String,
}
impl WirelessPairing {
    pub fn new() -> Self {
        let dns_id = rand_string_runes(24);
        let psk = rand_string_runes(6);
        WirelessPairing { dns_id, psk }
    }
    pub fn get_info(&self) -> String {
        format!("WIFI:T:ADB;S:{};P:{};;", self.dns_id, self.psk)
    }
}

use std::{
    sync::mpsc::{self, Receiver},
    thread,
};

use mdns_sd::{ServiceDaemon, ServiceEvent};
use rand::Rng;

const LETTER_RUNES: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";

pub fn rand_string_runes(n: usize) -> String {
    let mut rng = rand::thread_rng();

    (0..n)
        .map(|_| LETTER_RUNES[rng.gen_range(0..LETTER_RUNES.len())] as char)
        .collect()
}

use image::{DynamicImage, Luma};
use qrcode::QrCode;

#[allow(dead_code)]
pub fn generate_qr_code(info: &str) -> Result<DynamicImage, Box<dyn std::error::Error>> {
    let code = QrCode::new(info)?;

    let img = code.render::<Luma<u8>>().min_dimensions(300, 300).build();

    Ok(DynamicImage::ImageLuma8(img))
}

#[derive(Debug)]
pub enum MdnsEvent {
    PairingDiscovered {
        host: String,
        port: u16,
    },
    ConnectDiscovered {
        host: String,
        port: u16,
        fullname: String,
    },
    #[allow(dead_code)]
    Error(String),
}

pub struct MdnsHandle {
    #[allow(dead_code)]
    daemon: Option<ServiceDaemon>,
}

impl MdnsHandle {
    /// 创建一个空 handle（mDNS 不可用时占位）
    pub fn empty() -> Self {
        MdnsHandle { daemon: None }
    }

    #[allow(dead_code)]
    pub fn stop(self) {
        let _ = self.daemon;
    }
}
pub fn start_discovery(
    info: &WirelessPairing,
) -> Result<(MdnsHandle, Receiver<MdnsEvent>), Box<dyn std::error::Error>> {
    let mdns = ServiceDaemon::new()?;

    let pairing_receiver = mdns.browse("_adb-tls-pairing._tcp.local.")?;

    let connect_receiver = mdns.browse("_adb-tls-connect._tcp.local.")?;

    let dns_id = info.dns_id.clone();

    let (tx, rx) = mpsc::channel();

    //
    // pairing
    //
    {
        let tx = tx.clone();
        let dns_id = dns_id.clone();

        thread::spawn(move || {
            while let Ok(event) = pairing_receiver.recv() {
                match event {
                    ServiceEvent::ServiceResolved(info) => {
                        println!("[PAIRING]Resolved service: {:?}", info);
                        if info.get_fullname().contains(&dns_id) {
                            if let Some(addr) = info.get_addresses_v4().iter().next() {
                                let _ = tx.send(MdnsEvent::PairingDiscovered {
                                    host: addr.to_string(),
                                    port: info.get_port(),
                                });
                            }
                        }
                    }
                    _ => {}
                }
            }
        });
    }

    //
    // connect
    //
    {
        let tx = tx.clone();
        thread::spawn(move || {
            while let Ok(event) = connect_receiver.recv() {
                match event {
                    ServiceEvent::ServiceResolved(info) => {
                        println!("[CONNECT] Resolved service: {:?}", info);
                        if let Some(addr) = info.get_addresses_v4().iter().next() {
                            let _ = tx.send(MdnsEvent::ConnectDiscovered {
                                host: addr.to_string(),
                                port: info.get_port(),
                                fullname: info.get_fullname().to_string(),
                            });
                        }
                    }
                    _ => {}
                }
            }
        });
    }

    Ok((MdnsHandle { daemon: Some(mdns) }, rx))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rand_string_runes_length() {
        assert_eq!(rand_string_runes(0), "");
        assert_eq!(rand_string_runes(5).len(), 5);
        assert_eq!(rand_string_runes(100).len(), 100);
    }

    #[test]
    fn test_rand_string_runes_charset() {
        let s = rand_string_runes(1000);
        for c in s.chars() {
            assert!(
                LETTER_RUNES.contains(&(c as u8)),
                "char '{}' not in LETTER_RUNES",
                c
            );
        }
    }

    #[test]
    fn test_wireless_pairing_info_format() {
        let p = WirelessPairing::new();
        let info = p.get_info();

        assert!(
            info.starts_with("WIFI:T:ADB;S:"),
            "info should start with WIFI:T:ADB;S:"
        );
        assert!(info.ends_with(";;"), "info should end with ;;");

        let parts: Vec<&str> = info.split(';').collect();
        assert_eq!(parts.len(), 5, "splitting by ';' should yield 5 parts");
        assert_eq!(parts[0], "WIFI:T:ADB");
        assert!(
            parts[1].starts_with("S:"),
            "second part should start with S:"
        );
        assert!(
            parts[2].starts_with("P:"),
            "third part should start with P:"
        );
    }

    #[test]
    fn test_wireless_pairing_info_contains_ids() {
        let p = WirelessPairing::new();
        let info = p.get_info();

        assert!(info.contains(&p.dns_id), "info should contain dns_id");
        assert!(info.contains(&p.psk), "info should contain psk");
    }

    #[test]
    fn test_wireless_pairing_new_generates_ids() {
        let p = WirelessPairing::new();

        assert_eq!(p.dns_id.len(), 24, "dns_id should be 24 characters");
        assert_eq!(p.psk.len(), 6, "psk should be 6 characters");
    }
}
