use crate::types::{AdbError, Device, DeviceState};

/// 解析 `adb devices` / `adb track-devices` 输出的设备列表。
///
/// 如果第一行是 `"List of devices attached"` 则跳过，否则从第一行开始解析。
/// 每行格式为 `<serial>\t<state>`。
pub fn parse_devices(stdout: &[u8]) -> Result<Vec<Device>, AdbError> {
    let text = std::str::from_utf8(stdout).map_err(AdbError::Utf8)?;

    let devices: Vec<Device> = text
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.eq_ignore_ascii_case("List of devices attached") {
                return None;
            }

            let mut parts = line.split_whitespace();
            let raw_serial = parts.next()?.to_string();
            let state = match parts.next().unwrap_or("unknown") {
                "device" => DeviceState::Device,
                "offline" => DeviceState::Offline,
                "unauthorized" => DeviceState::Unauthorized,
                s => DeviceState::Unknown(s.to_string()),
            };
            let kind = crate::types::DeviceAddrKind::classify(&raw_serial);
            let mut identity = crate::types::DeviceIdentity::default();
            identity.set_by_kind(raw_serial.clone(), kind);
            // 初始 id 和 serial 都设为原始地址；后续通过 get-serialno 更新
            Some(Device {
                uuid: uuid::Uuid::new_v4(),
                id: raw_serial.clone(),
                serial: raw_serial,
                state,
                name: String::new(),
                identity,
            })
        })
        .collect();

    Ok(devices)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DeviceState;

    fn assert_device(d: &Device, expected_serial: &str, expected_state: DeviceState) {
        assert_eq!(d.id, expected_serial, "device id mismatch");
        assert_eq!(d.serial, expected_serial, "device serial mismatch");
        assert_eq!(d.state, expected_state, "device state mismatch");
    }

    #[test]
    fn test_parse_devices_normal() {
        let stdout = b"List of devices attached\ndevice1\tdevice\n";
        let devices = parse_devices(stdout).unwrap();
        assert_eq!(devices.len(), 1);
        assert_device(&devices[0], "device1", DeviceState::Device);
    }

    #[test]
    fn test_parse_devices_mixed_states() {
        let stdout = b"List of devices attached\ndevice1\toffline\ndevice2\tunauthorized\n";
        let devices = parse_devices(stdout).unwrap();
        assert_eq!(devices.len(), 2);
        assert_device(&devices[0], "device1", DeviceState::Offline);
        assert_device(&devices[1], "device2", DeviceState::Unauthorized);
    }

    #[test]
    fn test_parse_devices_empty() {
        let stdout = b"List of devices attached\n\n";
        let devices = parse_devices(stdout).unwrap();
        assert!(devices.is_empty());
    }

    #[test]
    fn test_parse_devices_no_devices() {
        let stdout = b"List of devices attached\n";
        let devices = parse_devices(stdout).unwrap();
        assert!(devices.is_empty());
    }

    #[test]
    fn test_parse_devices_unknown_state() {
        let stdout = b"List of devices attached\nfoo\tbar\n";
        let devices = parse_devices(stdout).unwrap();
        assert_eq!(devices.len(), 1);
        assert_device(&devices[0], "foo", DeviceState::Unknown("bar".to_string()));
    }

    #[test]
    fn test_parse_devices_with_usb_and_transport() {
        let stdout =
            b"List of devices attached\nemulator-5554\tdevice\n192.168.1.5:4321\tdevice product=XYZ model=Pixel transport_id=123\n";
        let devices = parse_devices(stdout).unwrap();
        assert_eq!(devices.len(), 2);
        assert_device(&devices[0], "emulator-5554", DeviceState::Device);
        assert_device(&devices[1], "192.168.1.5:4321", DeviceState::Device);
    }

    #[test]
    fn test_parse_devices_multiple() {
        let stdout =
            b"List of devices attached\ndevice1\tdevice\ndevice2\toffline\ndevice3\tunauthorized\ndevice4\tunknown_state\n";
        let devices = parse_devices(stdout).unwrap();
        assert_eq!(devices.len(), 4);
        assert_device(&devices[0], "device1", DeviceState::Device);
        assert_device(&devices[1], "device2", DeviceState::Offline);
        assert_device(&devices[2], "device3", DeviceState::Unauthorized);
        assert_device(
            &devices[3],
            "device4",
            DeviceState::Unknown("unknown_state".to_string()),
        );
    }

    #[test]
    fn test_parse_devices_empty_lines() {
        let stdout = b"List of devices attached\n\ndevice1\tdevice\n\n\ndevice2\toffline\n\n";
        let devices = parse_devices(stdout).unwrap();
        assert_eq!(devices.len(), 2);
        assert_device(&devices[0], "device1", DeviceState::Device);
        assert_device(&devices[1], "device2", DeviceState::Offline);
    }

    #[test]
    fn test_parse_devices_track_devices_two_devices() {
        // adb track-devices 输出的原始帧（无 "List of devices attached" 头部），
        // 两个设备之间用 \r\n 分隔。
        let payload = b"192.168.1.6:46769\tdevice\r\nadb-10AE6X05XP001TD-TNpsVn._adb-tls-connect._tcp\tdevice";
        let devices = parse_devices(payload).unwrap();
        assert_eq!(devices.len(), 2);
        assert_device(&devices[0], "192.168.1.6:46769", DeviceState::Device);
        assert_device(
            &devices[1],
            "adb-10AE6X05XP001TD-TNpsVn._adb-tls-connect._tcp",
            DeviceState::Device,
        );
    }
}
