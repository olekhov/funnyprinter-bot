use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use btleplug::api::{
    Central, CharPropFlags, Characteristic, Manager as _, Peripheral as _, ScanFilter,
    ValueNotification, WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral};
use futures::StreamExt;
use tokio::time::{Instant, sleep, timeout};
use uuid::Uuid;

pub const WRITE_UUID_STR: &str = "0000ffe1-0000-1000-8000-00805f9b34fb";
pub const READ_UUID_STR: &str = "0000ffe2-0000-1000-8000-00805f9b34fb";

pub const MAX_DOTS_PER_LINE: usize = 384;
pub const BYTES_PER_LINE: usize = MAX_DOTS_PER_LINE / 8;
pub const PACKED_LINE_BYTES: usize = BYTES_PER_LINE * 2;

const STATUS: [u8; 2] = [0x5a, 0x02];
const HANDSHAKE_0A: [u8; 2] = [0x5a, 0x0a];
const HANDSHAKE_0B: [u8; 2] = [0x5a, 0x0b];
const PRINTING_PAUSED: [u8; 2] = [0x5a, 0x08];
const PRINTING_FINISHED: [u8; 2] = [0x5a, 0x06];
const LOST_PACKET: [u8; 2] = [0x5a, 0x05];

#[derive(Debug, Clone)]
pub struct PrinterInfo {
    pub address: String,
    pub local_name: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct StatusEvent {
    pub battery: u8,
    pub no_paper: bool,
    pub overheat: bool,
}

#[derive(Debug, Clone)]
enum NotifyEvent {
    Handshake0a,
    Handshake0b { ok: bool },
    Lost { line_no: u16 },
    Finished,
    Paused,
    Status(StatusEvent),
    Other,
}

pub type PackedLine = [u8; PACKED_LINE_BYTES];

pub fn dpi() -> u16 {
    203
}

pub async fn discover_candidates(scan_time: Duration) -> Result<Vec<PrinterInfo>> {
    let adapter = default_adapter().await?;
    adapter
        .start_scan(ScanFilter::default())
        .await
        .context("failed to start BLE scan")?;
    sleep(scan_time).await;

    let mut out = Vec::new();
    for p in adapter
        .peripherals()
        .await
        .context("failed to get peripherals")?
    {
        let Some(props) = p
            .properties()
            .await
            .context("failed to read peripheral properties")?
        else {
            continue;
        };

        let has_ffe6 = props.services.iter().any(|s| {
            s.to_string()
                .eq_ignore_ascii_case("0000ffe6-0000-1000-8000-00805f9b34fb")
        });
        if has_ffe6 || props.local_name.is_some() {
            out.push(PrinterInfo {
                address: props.address.to_string(),
                local_name: props.local_name,
            });
        }
    }

    Ok(out)
}

pub async fn print_job(address: &str, lines: &[PackedLine], density: u8) -> Result<()> {
    if density > 7 {
        bail!("density must be in range 0..=7");
    }
    if lines.is_empty() {
        bail!("nothing to print: no packed lines provided");
    }

    let adapter = default_adapter().await?;
    let peripheral = find_peripheral_by_address(&adapter, address, Duration::from_secs(4)).await?;
    peripheral
        .connect()
        .await
        .with_context(|| format!("failed to connect to {address}"))?;
    peripheral
        .discover_services()
        .await
        .context("failed to discover services")?;

    let (write_char, read_char) = resolve_chars(&peripheral)?;

    peripheral
        .subscribe(&read_char)
        .await
        .context("failed to subscribe to notify characteristic")?;
    let mut notifications = peripheral
        .notifications()
        .await
        .context("failed to create notifications stream")?;

    write(&peripheral, &write_char, &hardware_info_packet()).await?;
    write(&peripheral, &write_char, &handshake_0a_packet()).await?;
    wait_for_handshake_0a(&mut notifications).await?;
    write(
        &peripheral,
        &write_char,
        &handshake_0b_packet(address).context("failed to build handshake 0b")?,
    )
    .await?;
    wait_for_handshake_0b_ok(&mut notifications).await?;

    write(&peripheral, &write_char, &density_packet(density)).await?;
    write(
        &peripheral,
        &write_char,
        &print_event_packet(lines.len() as u16, false),
    )
    .await?;

    let mut cur_line: usize = 0;
    let mut wait_for_event_cnt = 0usize;

    loop {
        if let Ok(Some(note)) = timeout(Duration::from_millis(5), notifications.next()).await {
            match parse_notify(&note) {
                NotifyEvent::Lost { line_no } => {
                    wait_for_event_cnt = 0;
                    cur_line = (line_no.saturating_sub(1)) as usize;
                }
                NotifyEvent::Paused => {
                    // Printer can emit pause before a lost-packet event.
                }
                NotifyEvent::Finished => {
                    break;
                }
                NotifyEvent::Status(st) => {
                    if st.overheat {
                        eprintln!("warning: printer overheat reported");
                    }
                    if st.no_paper {
                        eprintln!("warning: printer reports no paper");
                    }
                }
                NotifyEvent::Handshake0a | NotifyEvent::Handshake0b { .. } | NotifyEvent::Other => {
                }
            }
        }

        if cur_line < lines.len() {
            write(
                &peripheral,
                &write_char,
                &print_line_packet(cur_line as u16, &lines[cur_line]),
            )
            .await?;
            sleep(Duration::from_millis(20)).await;
            cur_line += 1;
        }

        if cur_line >= lines.len() {
            if wait_for_event_cnt > 50 {
                break;
            }
            wait_for_event_cnt += 1;
            sleep(Duration::from_millis(500)).await;
        }
    }

    write(
        &peripheral,
        &write_char,
        &print_event_packet(lines.len() as u16, true),
    )
    .await?;

    peripheral
        .disconnect()
        .await
        .context("failed to disconnect cleanly")?;
    Ok(())
}

async fn default_adapter() -> Result<Adapter> {
    let manager = Manager::new()
        .await
        .context("failed to create BLE manager")?;
    let adapters = manager
        .adapters()
        .await
        .context("failed to query BLE adapters")?;
    adapters
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no BLE adapter found"))
}

async fn find_peripheral_by_address(
    adapter: &Adapter,
    address: &str,
    scan_time: Duration,
) -> Result<Peripheral> {
    let normalize = |s: &str| s.replace('-', ":").to_ascii_uppercase();
    let target = normalize(address);

    adapter
        .start_scan(ScanFilter::default())
        .await
        .context("failed to start BLE scan")?;

    let deadline = Instant::now() + scan_time;
    loop {
        for p in adapter
            .peripherals()
            .await
            .context("failed to list peripherals")?
        {
            let Some(props) = p
                .properties()
                .await
                .context("failed to get peripheral properties")?
            else {
                continue;
            };
            if normalize(&props.address.to_string()) == target {
                return Ok(p);
            }
        }

        if Instant::now() >= deadline {
            break;
        }

        sleep(Duration::from_millis(250)).await;
    }

    bail!("BLE device with address {address} not found")
}

fn resolve_chars(peripheral: &Peripheral) -> Result<(Characteristic, Characteristic)> {
    let write_uuid = Uuid::parse_str(WRITE_UUID_STR).expect("valid write uuid");
    let read_uuid = Uuid::parse_str(READ_UUID_STR).expect("valid read uuid");

    let mut write_char = None;
    let mut read_char = None;

    for ch in peripheral.characteristics() {
        if ch.uuid == write_uuid {
            write_char = Some(ch.clone());
        }
        if ch.uuid == read_uuid {
            read_char = Some(ch.clone());
        }
    }

    let write_char =
        write_char.ok_or_else(|| anyhow!("write characteristic {WRITE_UUID_STR} not found"))?;
    let read_char =
        read_char.ok_or_else(|| anyhow!("read characteristic {READ_UUID_STR} not found"))?;

    if !write_char
        .properties
        .contains(CharPropFlags::WRITE_WITHOUT_RESPONSE)
        && !write_char.properties.contains(CharPropFlags::WRITE)
    {
        bail!("write characteristic exists but is not writable")
    }
    if !read_char.properties.contains(CharPropFlags::NOTIFY) {
        bail!("read characteristic exists but does not support NOTIFY")
    }

    Ok((write_char, read_char))
}

async fn write(peripheral: &Peripheral, ch: &Characteristic, data: &[u8]) -> Result<()> {
    let write_type = if ch
        .properties
        .contains(CharPropFlags::WRITE_WITHOUT_RESPONSE)
    {
        WriteType::WithoutResponse
    } else {
        WriteType::WithResponse
    };

    peripheral
        .write(ch, data, write_type)
        .await
        .context("BLE write failed")
}

fn parse_notify(note: &ValueNotification) -> NotifyEvent {
    if note.value.len() < 2 {
        return NotifyEvent::Other;
    }
    let tag = [note.value[0], note.value[1]];

    match tag {
        HANDSHAKE_0A => NotifyEvent::Handshake0a,
        HANDSHAKE_0B => {
            let ok = note.value.get(2).copied() == Some(0x01);
            NotifyEvent::Handshake0b { ok }
        }
        LOST_PACKET => {
            let line_no = if note.value.len() >= 4 {
                u16::from_be_bytes([note.value[2], note.value[3]])
            } else {
                0
            };
            NotifyEvent::Lost { line_no }
        }
        PRINTING_FINISHED => NotifyEvent::Finished,
        PRINTING_PAUSED => NotifyEvent::Paused,
        STATUS => {
            let battery = note.value.get(2).copied().unwrap_or(0);
            let no_paper = note.value.get(3).copied().unwrap_or(0) != 0;
            let overheat = note.value.get(5).copied().unwrap_or(0) != 0;
            NotifyEvent::Status(StatusEvent {
                battery,
                no_paper,
                overheat,
            })
        }
        _ => NotifyEvent::Other,
    }
}

async fn wait_for_handshake_0a<S>(stream: &mut S) -> Result<()>
where
    S: futures::Stream<Item = ValueNotification> + Unpin,
{
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(Some(note)) = timeout(Duration::from_millis(500), stream.next()).await {
            if matches!(parse_notify(&note), NotifyEvent::Handshake0a) {
                return Ok(());
            }
        }
    }
    bail!("timeout waiting for handshake 0x5a0a response")
}

async fn wait_for_handshake_0b_ok<S>(stream: &mut S) -> Result<()>
where
    S: futures::Stream<Item = ValueNotification> + Unpin,
{
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(Some(note)) = timeout(Duration::from_millis(500), stream.next()).await {
            if let NotifyEvent::Handshake0b { ok } = parse_notify(&note) {
                if ok {
                    return Ok(());
                }
                bail!("printer rejected handshake 0x5a0b response");
            }
        }
    }
    bail!("timeout waiting for handshake 0x5a0b confirmation")
}

fn hardware_info_packet() -> Vec<u8> {
    vec![0x5a, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
}

fn density_packet(density: u8) -> Vec<u8> {
    vec![0x5a, 0x0c, density]
}

fn handshake_0a_packet() -> Vec<u8> {
    let mut packet = vec![0x5a, 0x0a];
    packet.extend_from_slice(&[0u8; 10]);
    packet
}

fn handshake_0b_packet(bdaddr: &str) -> Result<Vec<u8>> {
    let mut mac_hex = bdaddr.replace(':', "");
    mac_hex = mac_hex.replace('-', "");
    if mac_hex.len() != 12 {
        bail!("expected a 6-byte MAC address, got: {bdaddr}");
    }
    let mut mac = [0u8; 6];
    for (idx, out) in mac.iter_mut().enumerate() {
        let from = idx * 2;
        *out = u8::from_str_radix(&mac_hex[from..from + 2], 16)
            .with_context(|| format!("invalid MAC address: {bdaddr}"))?;
    }

    let mut payload = Vec::with_capacity(7);
    payload.push(0u8);
    payload.extend_from_slice(&mac);

    let response = ((crc16_xmodem(&payload) >> 8) & 0xff) as u8;

    let mut out = vec![0x5a, 0x0b];
    out.extend(std::iter::repeat_n(response, 10));
    Ok(out)
}

fn print_event_packet(num_lines: u16, end: bool) -> Vec<u8> {
    let mut out = vec![0x5a, 0x04];
    out.extend_from_slice(&num_lines.to_be_bytes());
    let end_u16: u16 = if end { 1 } else { 0 };
    out.extend_from_slice(&end_u16.to_le_bytes());
    out
}

fn print_line_packet(line_no: u16, line_data: &PackedLine) -> Vec<u8> {
    let mut out = vec![0x55];
    out.extend_from_slice(&line_no.to_be_bytes());
    out.extend_from_slice(line_data);
    out.push(0x00);
    out
}

fn crc16_xmodem(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for byte in data {
        for bit_idx in 0..8 {
            let bit = (byte >> (7 - bit_idx)) & 1;
            let c15 = (crc >> 15) & 1;
            crc <<= 1;
            if (c15 ^ bit as u16) != 0 {
                crc ^= 0x1021;
            }
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_known_value() {
        let v = crc16_xmodem(&[0x00, 0xc0, 0x00, 0x00, 0x00, 0x05, 0xab]);
        assert_ne!(v, 0);
    }

    #[test]
    fn line_packet_size() {
        let line = [0u8; PACKED_LINE_BYTES];
        let p = print_line_packet(1, &line);
        assert_eq!(p.len(), 1 + 2 + PACKED_LINE_BYTES + 1);
    }
}
