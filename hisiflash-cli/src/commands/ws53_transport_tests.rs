//! Command-handler tests with a byte-level BootROM/LoaderBoot transport model.
//! No device enumeration, native port, real firmware or CPU execution occurs.
use super::*;
use hisiflash::protocol::crc::crc16_xmodem;
use std::{
    collections::VecDeque,
    io,
    sync::{Arc, Mutex},
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fault {
    None,
    DisconnectSecondErase,
    CorruptOutsidePlanDuringFinalRead,
    RepeatedBoot,
    PartialReset,
}

struct Device {
    flash: Vec<u8>,
    writes: Vec<u32>,
    reads: Vec<(u32, u32)>,
    resets: usize,
    fault: Fault,
}

struct Transfer {
    offset: Option<usize>,
    length: usize,
    received: usize,
    header_received: bool,
    eot: bool,
    next_sequence: u8,
}

struct ModelPort {
    device: Arc<Mutex<Device>>,
    pending: VecDeque<u8>,
    transfer: Option<Transfer>,
    disconnected: bool,
    timeout: Duration,
    baud: u32,
}

fn ack() -> Vec<u8> {
    let mut bytes = vec![0xef, 0xbe, 0xad, 0xde, 12, 0, 0xe1, 0x1e, 0x5a, 0];
    bytes.extend_from_slice(&crc16_xmodem(&bytes).to_le_bytes());
    bytes
}

fn packet(sequence: u8, payload: &[u8]) -> Vec<u8> {
    let mut bytes = vec![
        if payload.len() == 128 { 1 } else { 2 },
        sequence,
        !sequence,
    ];
    bytes.extend_from_slice(payload);
    bytes.extend_from_slice(&crc16_xmodem(payload).to_be_bytes());
    bytes
}

fn upload(data: &[u8]) -> Vec<u8> {
    let mut wire = ack();
    let mut header = [0; 128];
    let name = format!("flash.bin\0{}\0", data.len());
    header[..name.len()].copy_from_slice(name.as_bytes());
    wire.extend(packet(0, &header));
    for (index, chunk) in data
        .chunks(1024)
        .enumerate()
    {
        let mut padded = [0; 1024];
        padded[..chunk.len()].copy_from_slice(chunk);
        wire.extend(packet(u8::try_from((index + 1) % 256).unwrap(), &padded));
    }
    wire.push(4);
    wire.extend(packet(0, &[0; 128]));
    wire.extend(ack());
    wire
}

impl ModelPort {
    fn new(device: Arc<Mutex<Device>>) -> Self {
        Self {
            device,
            pending: VecDeque::new(),
            transfer: None,
            disconnected: false,
            timeout: Duration::from_secs(3),
            baud: 115200,
        }
    }

    fn start_transfer(&mut self, offset: Option<usize>, length: usize) {
        self.transfer = Some(Transfer {
            offset,
            length,
            received: 0,
            header_received: false,
            eot: false,
            next_sequence: 1,
        });
        self.pending
            .extend(ack());
        self.pending
            .push_back(b'C');
    }

    fn receive(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.disconnected {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        if let Some(transfer) = &mut self.transfer {
            if bytes == [4] {
                assert_eq!(transfer.received, transfer.length);
                transfer.eot = true;
                self.pending
                    .extend([6, b'C']);
                return Ok(());
            }
            assert!(matches!(bytes.len(), 133 | 1029));
            assert_eq!(bytes[2], !bytes[1]);
            let payload = &bytes[3..bytes.len() - 2];
            assert_eq!(
                crc16_xmodem(payload),
                u16::from_be_bytes(
                    bytes[bytes.len() - 2..]
                        .try_into()
                        .unwrap()
                )
            );
            if transfer.eot {
                assert_eq!((bytes[1], payload[0]), (0, 0));
                self.pending
                    .push_back(6);
                self.pending
                    .extend(ack());
                self.transfer = None;
            } else if !transfer.header_received {
                assert_eq!(bytes[1], 0);
                let fields: Vec<_> = payload
                    .split(|&b| b == 0)
                    .collect();
                let size: usize = std::str::from_utf8(fields[1])
                    .unwrap()
                    .split_ascii_whitespace()
                    .next()
                    .unwrap()
                    .parse()
                    .unwrap();
                assert_eq!(size, transfer.length);
                transfer.header_received = true;
                self.pending
                    .extend([6, b'C']);
            } else {
                assert_eq!(bytes[1], transfer.next_sequence);
                transfer.next_sequence = transfer
                    .next_sequence
                    .wrapping_add(1);
                let count = payload
                    .len()
                    .min(transfer.length - transfer.received);
                if let Some(offset) = transfer.offset {
                    let start = offset + transfer.received;
                    self.device
                        .lock()
                        .unwrap()
                        .flash[start..start + count]
                        .copy_from_slice(&payload[..count]);
                }
                transfer.received += count;
                self.pending
                    .push_back(6);
            }
            return Ok(());
        }
        // Upload ACK/C controls have no payload writes. A fresh command starts
        // only when the previous upload's completion ACK has been consumed.
        if bytes.len() < 8 {
            return Ok(());
        }
        assert!(
            self.pending
                .is_empty(),
            "command while unread response remains"
        );
        assert_eq!(&bytes[..4], &[0xef, 0xbe, 0xad, 0xde]);
        assert_eq!(
            usize::from(u16::from_le_bytes(
                bytes[4..6]
                    .try_into()
                    .unwrap()
            )),
            bytes.len()
        );
        assert_eq!(bytes[7], !bytes[6]);
        assert_eq!(
            crc16_xmodem(&bytes[..bytes.len() - 2]),
            u16::from_le_bytes(
                bytes[bytes.len() - 2..]
                    .try_into()
                    .unwrap()
            )
        );
        match bytes[6] {
            0xf0 => self.start_transfer(None, 0x7380),
            0xb4 => {
                let length = u32::from_le_bytes(
                    bytes[8..12]
                        .try_into()
                        .unwrap(),
                );
                let offset = u32::from_le_bytes(
                    bytes[12..16]
                        .try_into()
                        .unwrap(),
                );
                let mut device = self
                    .device
                    .lock()
                    .unwrap();
                device
                    .reads
                    .push((offset, length));
                if device.fault == Fault::CorruptOutsidePlanDuringFinalRead
                    && !device
                        .writes
                        .is_empty()
                    && offset == 0
                {
                    device.flash[0x8000] ^= 1;
                }
                self.pending
                    .extend(upload(
                        &device.flash[offset as usize..(offset + length) as usize],
                    ));
            },
            0xd2 => {
                let offset = u32::from_le_bytes(
                    bytes[8..12]
                        .try_into()
                        .unwrap(),
                ) - 0x400000;
                let length = u32::from_le_bytes(
                    bytes[12..16]
                        .try_into()
                        .unwrap(),
                );
                assert_eq!(
                    length,
                    u32::from_le_bytes(
                        bytes[16..20]
                            .try_into()
                            .unwrap()
                    )
                );
                assert_eq!(&bytes[20..22], &[0, 255]);
                let mut device = self
                    .device
                    .lock()
                    .unwrap();
                device
                    .writes
                    .push(offset);
                device.flash[offset as usize..(offset + length) as usize].fill(0xff);
                if device.fault == Fault::DisconnectSecondErase
                    && device
                        .writes
                        .len()
                        == 2
                {
                    self.disconnected = true;
                    return Err(io::ErrorKind::BrokenPipe.into());
                }
                drop(device);
                self.start_transfer(Some(offset as usize), length as usize);
            },
            0x87 => {
                let mut device = self
                    .device
                    .lock()
                    .unwrap();
                device.resets += 1;
                if device.fault == Fault::PartialReset {
                    self.disconnected = true;
                    return Err(io::ErrorKind::BrokenPipe.into());
                }
                self.pending
                    .extend(b"SIMULATED BOOT PASS\r\n");
                if device.fault == Fault::RepeatedBoot {
                    self.pending
                        .extend(b"SIMULATED BOOT PASS\r\n");
                }
            },
            value => panic!("unexpected device command {value:#x}"),
        }
        Ok(())
    }
}

impl Read for ModelPort {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if self.disconnected {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        let count = bytes
            .len()
            .min(
                self.pending
                    .len(),
            );
        if count == 0 {
            std::thread::sleep(Duration::from_millis(1));
            return Err(io::ErrorKind::TimedOut.into());
        }
        for byte in &mut bytes[..count] {
            *byte = self
                .pending
                .pop_front()
                .unwrap();
        }
        Ok(count)
    }
}
impl Write for ModelPort {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.receive(bytes)?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl Port for ModelPort {
    fn set_timeout(&mut self, timeout: Duration) -> hisiflash::Result<()> {
        self.timeout = timeout;
        Ok(())
    }
    fn timeout(&self) -> Duration {
        self.timeout
    }
    fn set_baud_rate(&mut self, baud: u32) -> hisiflash::Result<()> {
        assert_eq!(baud, 115200);
        self.baud = baud;
        Ok(())
    }
    fn baud_rate(&self) -> u32 {
        self.baud
    }
    fn clear_buffers(&mut self) -> hisiflash::Result<()> {
        assert!(
            self.pending
                .is_empty()
        );
        Ok(())
    }
    fn name(&self) -> &'static str {
        "in-memory WS53 protocol model"
    }
    fn set_dtr(&mut self, _: bool) -> hisiflash::Result<()> {
        panic!("no line reset")
    }
    fn set_rts(&mut self, _: bool) -> hisiflash::Result<()> {
        panic!("no line reset")
    }
    fn read_cts(&mut self) -> hisiflash::Result<bool> {
        Ok(false)
    }
    fn read_dsr(&mut self) -> hisiflash::Result<bool> {
        Ok(false)
    }
    fn close(&mut self) -> hisiflash::Result<()> {
        Ok(())
    }
}

fn setup(dir: &Path, fault: Fault) -> (Options, Arc<Mutex<Device>>) {
    let mut options = super::tests::fixture(dir);
    options.mode = Mode::Flash;
    options.secure_boot_disabled = true;
    options.usb_serial = Some("SIMULATED".into());
    options.reset = true;
    options.expect = Some("SIMULATED BOOT PASS".into());
    let mut flash = std::fs::read(&options.backup).unwrap();
    flash[NV_START..].fill(0x71);
    (
        options,
        Arc::new(Mutex::new(Device {
            flash,
            writes: vec![],
            reads: vec![],
            resets: 0,
            fault,
        })),
    )
}

fn execute(options: &Options, device: &Arc<Mutex<Device>>) -> Result<()> {
    run_with_connector(
        options,
        Some("MODEL"),
        None,
        false,
        &CancelContext::none(),
        |name, serial| {
            assert_eq!((name, serial), ("MODEL", "SIMULATED"));
            // All recovery inputs must be on disk before transport can be opened.
            for file in [
                "original-full.bin",
                "loaderboot.bin",
                "application.bin",
                "recovery-command.json",
            ] {
                assert!(
                    options
                        .output
                        .join(file)
                        .is_file()
                );
            }
            Ok(ModelPort::new(Arc::clone(device)))
        },
    )
}

fn report(options: &Options, name: &str) -> serde_json::Value {
    serde_json::from_slice(
        &std::fs::read(
            options
                .output
                .join(name),
        )
        .unwrap(),
    )
    .unwrap()
}

#[test]
fn command_transaction_programs_verifies_and_monitors_without_cold_boot_claim() {
    let dir = tempfile::tempdir().unwrap();
    let (options, device) = setup(dir.path(), Fault::None);
    execute(&options, &device).unwrap();
    assert_eq!(
        device
            .lock()
            .unwrap()
            .writes,
        [0x5d000, 0x5e000, 0x5f000, 0x30000]
    );
    assert_eq!(
        device
            .lock()
            .unwrap()
            .resets,
        1
    );
    assert_eq!(
        std::fs::read(
            options
                .output
                .join("expected-full.bin")
        )
        .unwrap(),
        device
            .lock()
            .unwrap()
            .flash
    );
    assert!(
        device
            .lock()
            .unwrap()
            .flash[NV_START..]
            .iter()
            .all(|&b| b == 0x71)
    );
    let result = report(&options, "result.json");
    assert_eq!(result["full_readback_verified"], true);
    assert_eq!(result["expected_boot_marker_observed_once"], true);
    assert_eq!(result["cold_boot_verified"], false);
    let recovery = report(&options, "recovery-command.json");
    assert_eq!(recovery["automatic_retry"], false);
    assert!(
        recovery["argv"]
            .as_array()
            .unwrap()
            .iter()
            .all(serde_json::Value::is_string)
    );
}

#[test]
fn partial_erase_never_resets_and_fresh_recovery_restores_original_with_current_nv() {
    let dir = tempfile::tempdir().unwrap();
    let (mut options, device) = setup(dir.path(), Fault::DisconnectSecondErase);
    assert!(execute(&options, &device).is_err());
    assert_eq!(
        device
            .lock()
            .unwrap()
            .writes,
        [0x5d000, 0x5e000]
    );
    assert_eq!(
        device
            .lock()
            .unwrap()
            .resets,
        0
    );
    assert!(
        device
            .lock()
            .unwrap()
            .flash[0x5e000..0x5f000]
            .iter()
            .all(|&b| b == 0xff)
    );
    let failure = report(&options, "failure.json");
    assert_eq!(failure["phase"], "Failed");
    assert_eq!(failure["progress_scope"], "current_invocation");
    assert_eq!(failure["flash_may_have_changed"], true);
    assert_eq!(failure["verified_sectors"], 1);
    assert_eq!(failure["failed_sector_index"], 1);
    assert_eq!(failure["failed_sector_offset"], 0x5e000);
    assert_eq!(failure["write_stage"], "Erase");
    assert_eq!(failure["full_readback_verified"], false);
    assert!(
        failure["error"]
            .as_str()
            .unwrap()
            .contains("after 1 verified sectors")
    );
    assert!(
        !options
            .output
            .join("verified.json")
            .exists()
    );
    options.mode = Mode::Recover;
    options.output = dir
        .path()
        .join("recovered");
    device
        .lock()
        .unwrap()
        .fault = Fault::None;
    execute(&options, &device).unwrap();
    let original = std::fs::read(&options.backup).unwrap();
    assert_eq!(
        &device
            .lock()
            .unwrap()
            .flash[..NV_START],
        &original[..NV_START]
    );
    assert!(
        device
            .lock()
            .unwrap()
            .flash[NV_START..]
            .iter()
            .all(|&b| b == 0x71)
    );
    assert_eq!(
        device
            .lock()
            .unwrap()
            .resets,
        1
    );
    assert_eq!(
        report(&options, "result.json")["full_readback_verified"],
        true
    );
}

#[test]
fn post_write_protected_corruption_fails_full_verify_and_forbids_reset() {
    let dir = tempfile::tempdir().unwrap();
    let (options, device) = setup(dir.path(), Fault::CorruptOutsidePlanDuringFinalRead);
    assert!(execute(&options, &device).is_err());
    assert_eq!(
        device
            .lock()
            .unwrap()
            .writes
            .len(),
        4
    );
    assert_eq!(
        device
            .lock()
            .unwrap()
            .resets,
        0
    );
    assert!(
        report(&options, "failure.json")["error"]
            .as_str()
            .unwrap()
            .contains("full verification mismatch")
    );
    assert!(
        !options
            .output
            .join("verified.json")
            .exists()
    );
}

#[test]
fn reset_delivery_and_boot_failures_do_not_report_application_success() {
    for fault in [Fault::PartialReset, Fault::RepeatedBoot] {
        let dir = tempfile::tempdir().unwrap();
        let (options, device) = setup(dir.path(), fault);
        assert!(execute(&options, &device).is_err());
        assert_eq!(
            report(&options, "verified.json")["full_readback_verified"],
            true
        );
        let failure = report(&options, "boot-failure.json");
        assert_eq!(failure["reset_attempted"], true);
        assert_eq!(failure["reset_requested"], fault == Fault::RepeatedBoot);
        assert_eq!(
            device
                .lock()
                .unwrap()
                .resets,
            1
        );
        assert_eq!(failure["cold_boot_verified"], false);
        assert!(
            !options
                .output
                .join("result.json")
                .exists()
        );
    }
}

#[test]
fn durable_record_failures_prevent_erase_or_reset_and_keep_precise_progress() {
    for after_write in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (options, device) = setup(dir.path(), Fault::None);
        let conflict = if after_write {
            "verified.json"
        } else {
            "expected-full.bin"
        };
        let result = run_with_connector(
            &options,
            Some("MODEL"),
            None,
            false,
            &CancelContext::none(),
            |_, _| {
                // A directory at the output file path causes create_new to fail
                // deterministically, independently of host uid or disk capacity.
                std::fs::create_dir(
                    options
                        .output
                        .join(conflict),
                )
                .unwrap();
                Ok(ModelPort::new(Arc::clone(&device)))
            },
        );
        assert!(result.is_err());
        assert_eq!(
            device
                .lock()
                .unwrap()
                .writes
                .len(),
            if after_write { 4 } else { 0 }
        );
        assert_eq!(
            device
                .lock()
                .unwrap()
                .resets,
            0
        );
        let failure = report(&options, "failure.json");
        assert_eq!(failure["flash_may_have_changed"], after_write);
        assert_eq!(failure["full_readback_verified"], after_write);
        assert_eq!(failure["verified_sectors"], if after_write { 4 } else { 0 });
        assert!(failure["failed_sector_offset"].is_null());
        assert_eq!(failure["reset_requested"], false);
        assert!(
            !options
                .output
                .join("result.json")
                .exists()
        );
    }
}

#[test]
fn cancellation_before_connection_never_opens_a_transport() {
    let dir = tempfile::tempdir().unwrap();
    let (options, _) = setup(dir.path(), Fault::None);
    let result = run_with_connector(
        &options,
        Some("MODEL"),
        None,
        false,
        &CancelContext::new(|| true),
        |_, _| -> Result<ModelPort> { panic!("cancelled before opening") },
    );
    assert!(result.is_err());
    assert!(
        options
            .output
            .join("recovery-command.json")
            .is_file()
    );
    assert!(
        !options
            .output
            .join("before-full.bin")
            .exists()
    );
}

#[test]
fn failure_log_write_error_does_not_hide_partial_flash_or_reset_delivery() {
    for fault in [Fault::DisconnectSecondErase, Fault::PartialReset] {
        let dir = tempfile::tempdir().unwrap();
        let (options, device) = setup(dir.path(), fault);
        let name = if fault == Fault::DisconnectSecondErase {
            "failure.json"
        } else {
            "boot-failure.json"
        };
        let error = run_with_connector(
            &options,
            Some("MODEL"),
            None,
            false,
            &CancelContext::none(),
            |_, _| {
                std::fs::create_dir(
                    options
                        .output
                        .join(name),
                )
                .unwrap();
                Ok(ModelPort::new(Arc::clone(&device)))
            },
        )
        .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("could not save"), "{text}");
        if fault == Fault::DisconnectSecondErase {
            assert!(text.contains("\"flash_may_have_changed\":true"), "{text}");
            assert!(text.contains("after 1 verified sectors"), "{text}");
            assert_eq!(
                device
                    .lock()
                    .unwrap()
                    .resets,
                0
            );
        } else {
            assert!(text.contains("\"reset_attempted\":true"), "{text}");
            assert!(text.contains("\"reset_requested\":false"), "{text}");
            assert_eq!(
                device
                    .lock()
                    .unwrap()
                    .resets,
                1
            );
        }
        assert!(
            !options
                .output
                .join("result.json")
                .exists()
        );
    }
}
