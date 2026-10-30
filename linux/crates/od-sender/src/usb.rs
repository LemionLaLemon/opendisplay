//! USB transport (PROTOCOL.md §2.2): dial the receiver's TCP port on an Apple
//! device through **usbmuxd**, the device-multiplexing daemon that ships with
//! macOS and comes with libimobiledevice on Linux and Windows.
//!
//! The sender asks usbmuxd to `Connect` to port 9000 on the chosen device;
//! after the `OK` result that socket is a transparent byte pipe, so the
//! session code above sees exactly the stream it gets over Wi-Fi. Bonjour
//! plays no role on this path, and the receiver classifies the connection as
//! "USB" for its stats because it arrives from loopback.
//!
//! Needs `usbmuxd` running (`sudo systemctl start usbmuxd` on Arch) and the
//! device's usual "Trust this computer" prompt answered once.

use std::io::Cursor;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use opendisplay_proto::DEFAULT_PORT;
use plist::{Dictionary, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tracing::debug;

/// usbmuxd's socket. libimobiledevice also honours
/// `USBMUXD_SOCKET_ADDRESS` (e.g. `unix:/var/run/usbmuxd`), which is handy
/// for tests and for a usbmuxd started by hand.
const DEFAULT_SOCKET: &str = "/var/run/usbmuxd";
/// How long usbmuxd gets to answer one request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// A plist answer is a few KB; anything past this is not one.
const MAX_RESPONSE: usize = 1 << 20;
/// Every message on the usbmuxd socket is a 16-byte header — length (with
/// the header), version, message type, tag — followed by that many bytes.
/// Current usbmuxd (1.1.x) speaks this framed form; libusbmuxd sends the same.
const HEADER: usize = 16;
const VERSION: u32 = 1;
/// Message type 8 is "the payload is a plist".
const PLIST_MESSAGE: u32 = 8;

/// A device usbmuxd knows about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// usbmuxd's handle for this attachment; changes when the cable is
    /// replugged, so it is looked up again for every dial.
    pub id: u32,
    /// The iPad/iPhone's UDID, e.g. `00008030-00165C3A1E78C02E`.
    pub serial: String,
    /// `USB` for a cable, `Network` for Wi-Fi sync.
    pub connection: String,
    pub product_id: u32,
}

impl Device {
    fn is_usb(&self) -> bool {
        self.connection.eq_ignore_ascii_case("USB")
    }
}

/// usbmuxd's socket path: `$USBMUXD_SOCKET_ADDRESS` if set, else the default.
pub fn socket_path() -> PathBuf {
    match std::env::var("USBMUXD_SOCKET_ADDRESS") {
        Ok(v) if !v.trim().is_empty() => {
            let v = v.trim();
            PathBuf::from(v.strip_prefix("unix:").unwrap_or(v))
        }
        _ => PathBuf::from(DEFAULT_SOCKET),
    }
}

/// The devices usbmuxd currently has, USB attachments included.
pub async fn devices() -> Result<Vec<Device>> {
    let socket = socket_path();
    let mut stream = UnixStream::connect(&socket).await.with_context(|| {
        format!(
            "usbmuxd is not reachable at {} (install libimobiledevice and start usbmuxd)",
            socket.display()
        )
    })?;
    let msg = {
        let mut d = base_request();
        d.insert("MessageType".into(), Value::from("ListDevices"));
        d
    };
    let reply = tokio::time::timeout(REQUEST_TIMEOUT, request(&mut stream, msg))
        .await
        .map_err(|_| anyhow!("usbmuxd did not answer ListDevices within {REQUEST_TIMEOUT:?}"))??;
    let list = reply
        .as_dictionary()
        .and_then(|d| d.get("DeviceList"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::with_capacity(list.len());
    for entry in list {
        let Some(entry) = entry.as_dictionary() else {
            continue;
        };
        let props = entry.get("Properties").and_then(|v| v.as_dictionary());
        // usbmuxd puts DeviceID at the top level and repeats it in Properties.
        let id = entry
            .get("DeviceID")
            .or_else(|| props.and_then(|p| p.get("DeviceID")))
            .and_then(int)
            .unwrap_or(0) as u32;
        let serial = props
            .and_then(|p| p.get("SerialNumber"))
            .or_else(|| props.and_then(|p| p.get("UDID")))
            .and_then(|v| v.as_string())
            .unwrap_or_default()
            .to_string();
        let connection = props
            .and_then(|p| p.get("ConnectionType"))
            .and_then(|v| v.as_string())
            .unwrap_or("USB")
            .to_string();
        let product_id = props
            .and_then(|p| p.get("ProductID"))
            .and_then(int)
            .unwrap_or(0) as u32;
        out.push(Device {
            id,
            serial,
            connection,
            product_id,
        });
    }
    Ok(out)
}

/// Open port 9000 on `selector`'s device through usbmuxd.
///
/// `selector` matches a UDID or a part of it (case-insensitively). `None`
/// picks the only USB device attached and fails with the list when there are
/// several. Returns the byte pipe plus a label for logs.
pub async fn open(selector: Option<&str>) -> Result<(UnixStream, String)> {
    let all = devices().await?;
    let usb: Vec<Device> = all.into_iter().filter(Device::is_usb).collect();
    let device = pick(&usb, selector)?.clone();
    debug!(
        "usbmuxd: connecting to port {} on {} (device {})",
        DEFAULT_PORT, device.serial, device.id
    );
    let socket = socket_path();
    let mut stream = UnixStream::connect(&socket)
        .await
        .with_context(|| format!("usbmuxd at {}", socket.display()))?;
    let msg = {
        let mut d = base_request();
        d.insert("MessageType".into(), Value::from("Connect"));
        d.insert("DeviceID".into(), Value::from(u64::from(device.id)));
        // The usbmux protocol carries this one field in network byte order.
        d.insert(
            "PortNumber".into(),
            Value::from(u64::from(DEFAULT_PORT.to_be())),
        );
        d
    };
    let reply = tokio::time::timeout(REQUEST_TIMEOUT, request(&mut stream, msg))
        .await
        .map_err(|_| anyhow!("usbmuxd did not answer Connect within {REQUEST_TIMEOUT:?}"))??;
    let dict = reply
        .as_dictionary()
        .ok_or_else(|| anyhow!("usbmuxd sent an unexpected reply: {reply:?}"))?;
    let ok = dict.get("MessageType").and_then(|v| v.as_string()) == Some("Result")
        && dict.get("Number").and_then(int) == Some(0);
    if !ok {
        let number = dict.get("Number").and_then(int).unwrap_or(-1);
        bail!(
            "usbmuxd refused port {DEFAULT_PORT} on {} (usbmux error {number}); is the OpenDisplay \
             app open and in the foreground on the device?",
            device.serial
        );
    }
    let label = format!(
        "{} (USB, {}) on port {DEFAULT_PORT}",
        if device.serial.is_empty() {
            "Apple device"
        } else {
            &device.serial
        },
        device.id
    );
    Ok((stream, label))
}

/// Pick one device: by UDID (full or part), else the only USB one.
fn pick<'a>(devices: &'a [Device], selector: Option<&str>) -> Result<&'a Device> {
    let Some(want) = selector.map(str::trim).filter(|s| !s.is_empty()) else {
        return match devices {
            [] => bail!(
                "no Apple device on a USB cable: connect the iPad/iPhone and trust this computer"
            ),
            [only] => Ok(only),
            many => bail!(
                "{} Apple devices on USB; pick one with --usb-device <UDID>:\n{}",
                many.len(),
                many.iter()
                    .map(|d| format!("  {}", describe_device(d)))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        };
    };
    let matches: Vec<&Device> = devices
        .iter()
        .filter(|d| d.serial.to_lowercase().contains(&want.to_lowercase()))
        .collect();
    match matches.as_slice() {
        [only] => Ok(only),
        [] => bail!(
            "no USB device matches {want:?}; attached:\n{}",
            if devices.is_empty() {
                "  (none)".to_string()
            } else {
                devices
                    .iter()
                    .map(|d| format!("  {}", describe_device(d)))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        ),
        many => bail!(
            "{want:?} matches {} USB devices; use the full UDID",
            many.len()
        ),
    }
}

/// One line for lists: UDID, usbmuxd handle, USB product id.
pub fn describe_device(d: &Device) -> String {
    format!(
        "{}  (device {}, product 0x{:04x}{})",
        if d.serial.is_empty() { "?" } else { &d.serial },
        d.id,
        d.product_id,
        if d.is_usb() { "" } else { ", not on USB" }
    )
}

fn base_request() -> Dictionary {
    let mut d = Dictionary::new();
    d.insert("ClientVersionString".into(), Value::from("od-sender 1"));
    d.insert("ProgName".into(), Value::from("od-sender"));
    // libimobiledevice's libusbmuxd sends this; older usbmuxd needs it.
    d.insert("kLibUSBMuxVersion".into(), Value::from(3u64));
    d
}

/// Send one plist request on `stream` and read the plist answer, both framed
/// with the 16-byte usbmuxd header. The answer may be XML or binary; both
/// parse the same way.
async fn request(stream: &mut UnixStream, request: Dictionary) -> Result<Value> {
    let mut body = Vec::new();
    Value::Dictionary(request)
        .to_writer_xml(&mut body)
        .context("encoding the usbmuxd request")?;
    let length = u32::try_from(HEADER + body.len()).context("usbmuxd request is too long")?;
    let mut framed = Vec::with_capacity(length as usize);
    framed.extend_from_slice(&length.to_le_bytes());
    framed.extend_from_slice(&VERSION.to_le_bytes());
    framed.extend_from_slice(&PLIST_MESSAGE.to_le_bytes());
    framed.extend_from_slice(&1u32.to_le_bytes()); // tag
    framed.extend_from_slice(&body);
    stream
        .write_all(&framed)
        .await
        .context("writing to usbmuxd")?;

    let mut header = [0u8; HEADER];
    stream
        .read_exact(&mut header)
        .await
        .context("reading the usbmuxd header")?;
    let length = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let version = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
    let message = u32::from_le_bytes([header[8], header[9], header[10], header[11]]);
    if !(HEADER..=MAX_RESPONSE).contains(&length) {
        bail!(
            "usbmuxd announced {length} bytes (version {version}, message {message}); not a plist"
        );
    }
    if message != PLIST_MESSAGE {
        bail!("usbmuxd sent message type {message}, not a plist");
    }
    let mut body = vec![0u8; length - HEADER];
    stream
        .read_exact(&mut body)
        .await
        .context("reading the usbmuxd answer")?;
    Value::from_reader(Cursor::new(body)).context("parsing usbmuxd's answer")
}

fn int(v: &Value) -> Option<i64> {
    match v {
        Value::Integer(i) => i.as_signed(),
        Value::Boolean(b) => Some(i64::from(*b)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: u32, serial: &str, connection: &str) -> Device {
        Device {
            id,
            serial: serial.into(),
            connection: connection.into(),
            product_id: 0x12ab,
        }
    }

    #[test]
    fn no_selector_takes_the_only_usb_device() {
        let d = [device(1, "ABC", "USB")];
        assert_eq!(pick(&d, None).unwrap().id, 1);
        assert!(pick(&[], None).is_err());
        let two = [device(1, "ABC", "USB"), device(2, "DEF", "USB")];
        let e = pick(&two, None).unwrap_err().to_string();
        assert!(e.contains("--usb-device"), "{e}");
    }

    #[test]
    fn a_selector_matches_a_udid_or_its_tail() {
        let d = [
            device(1, "00008030-00165C3A1E78C02E", "USB"),
            device(2, "00008101-000A1B2C3D4E5F60", "USB"),
        ];
        assert_eq!(pick(&d, Some("1e78c02e")).unwrap().id, 1);
        assert_eq!(pick(&d, Some("00008101-000a1b2c3d4e5f60")).unwrap().id, 2);
        assert!(pick(&d, Some("nope")).is_err());
        assert!(pick(&d, Some("00008")).is_err(), "ambiguous must fail");
    }

    #[test]
    fn the_socket_path_can_be_overridden() {
        // SAFETY: tests run single-threaded per env var here; restore after.
        let old = std::env::var_os("USBMUXD_SOCKET_ADDRESS");
        unsafe { std::env::set_var("USBMUXD_SOCKET_ADDRESS", "unix:/tmp/usbmuxd-test") };
        assert_eq!(socket_path(), PathBuf::from("/tmp/usbmuxd-test"));
        unsafe {
            match old {
                Some(v) => std::env::set_var("USBMUXD_SOCKET_ADDRESS", v),
                None => std::env::remove_var("USBMUXD_SOCKET_ADDRESS"),
            }
        }
    }

    #[test]
    fn requests_round_trip_through_plist() {
        let mut d = base_request();
        d.insert("MessageType".into(), Value::from("Connect"));
        d.insert("PortNumber".into(), Value::from(u64::from(9000u16.to_be())));
        let mut buf = Vec::new();
        Value::Dictionary(d).to_writer_xml(&mut buf).unwrap();
        let back = Value::from_reader(Cursor::new(&buf)).unwrap();
        let back = back.as_dictionary().unwrap();
        assert_eq!(back["MessageType"].as_string(), Some("Connect"));
        // The wire field is big-endian: 9000 (0x2328) byteswapped on x86.
        assert_eq!(int(&back["PortNumber"]), Some(i64::from(9000u16.to_be())));
    }
}
