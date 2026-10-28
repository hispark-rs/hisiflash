//! WS53 bring-up: BootROM handshake, LoaderBoot RAM transfer and Flash readback.
//!
//! The vendor burntool explicitly selects the shared SEBOOT/YMODEM path for
//! WS53 (Global.cpp / CmdCtrl.cpp). This does not qualify WS63 image addresses,
//! loader binaries, erase commands or baud switching for WS53. The common ACK
//! contains no chip identifier. The caller must explicitly select the device.
mod readback;
mod session;
mod update;
mod write;
use crate::{
    CancelContext, Error, Result,
    port::Port,
    protocol::{
        crc::crc16_xmodem,
        ymodem::{YmodemConfig, YmodemTransfer},
    },
};
pub use readback::read_flash;
pub use session::{SessionFailure, SessionPhase, Ws53Session};
use std::{
    io::ErrorKind,
    time::{Duration, Instant},
};
pub use update::{ApplicationFailure, ApplicationSector, ApplicationUpdate};
pub use write::{WriteFailure, WriteStage, replace_application_sectors};

/// Vendor WS53 initial handshake: 115200 baud, 8 data bits, 1 stop bit.
/// Source: fbb_burntool Global::ReadCfg WS53 branch, including its CRC.
pub const ROM_HANDSHAKE: [u8; 18] = [
    0xef, 0xbe, 0xad, 0xde, 0x12, 0, 0xf0, 0x0f, 0, 0xc2, 1, 0, 8, 1, 0, 0, 0xe0, 0x64,
];

/// Send the vendor LoaderBoot 0x87 reset request (parameter zero).
///
/// Source: fbb_burntool `WifiBurnCtrl::ResetByCmd` / `SEBOOT_RESET`.
/// Returns only that the request was written, NOT proof of reset or startup.
/// Caller must observe a fresh boot transcript/application response separately.
/// Do not call after a partial program until recovery/verification is complete.
/// No ROM-entry reset or cold-power-cycle behavior is implied.
pub fn request_loader_reset<P: Port>(port: &mut P, cancel: &CancelContext) -> Result<()> {
    cancel.check()?;
    let mut bytes = vec![0xef, 0xbe, 0xad, 0xde, 12, 0, 0x87, 0x78, 0, 0];
    bytes.extend_from_slice(&crc16_xmodem(&bytes).to_le_bytes());
    port.write_all_bytes(&bytes)
}

/// A validated SEBOOT response; not proof of chip identity or flash support.
#[derive(Debug, PartialEq, Eq)]
pub struct RomHandshake {
    /// Unmodified 12-byte ACK, including status and checksum.
    pub ack: [u8; 12],
    /// Bytes received after ACK (for example YMODEM's initial C).
    pub trailing: Vec<u8>,
}

#[derive(Default)]
struct Decoder {
    tail: Vec<u8>,
}
impl Decoder {
    fn feed(&mut self, bytes: &[u8]) -> Result<Option<RomHandshake>> {
        self.tail
            .extend_from_slice(bytes);
        for (index, frame) in self
            .tail
            .windows(12)
            .enumerate()
        {
            if frame[..8] != [0xef, 0xbe, 0xad, 0xde, 12, 0, 0xe1, 0x1e] {
                continue;
            }
            if crc16_xmodem(&frame[..10]) != u16::from_le_bytes([frame[10], frame[11]]) {
                continue;
            }
            if frame[8] != 0x5a {
                return Err(Error::HandshakeFailed(format!(
                    "WS53 SEBOOT refused: result={:#04x}, error={:#04x}",
                    frame[8], frame[9]
                )));
            }
            let mut ack = [0; 12];
            ack.copy_from_slice(frame);
            return Ok(Some(RomHandshake {
                ack,
                trailing: self.tail[index + 12..].to_vec(),
            }));
        }
        // Retain only a possible partial frame, not an unbounded boot log.
        let discard = self
            .tail
            .len()
            .saturating_sub(11);
        self.tail
            .drain(..discard);
        Ok(None)
    }
}

/// Probe a caller-selected WS53 at115200, without loader/erase/write/reset.
///
/// Caller supplies8N1, no flow control, and arranges board reset/download entry.
/// Accepts a deadline in(0,30s]; checks cancellation between bounded I/O calls.
/// A successful ACK is shared by several chips and cannot identify WS53.
/// Restores the original port timeout on every exit; leaves baud at115200.
/// Port failures propagate immediately, permitting explicit reconnect/retry.
pub fn probe_bootrom<P: Port>(
    port: &mut P,
    timeout: Duration,
    cancel: &CancelContext,
) -> Result<RomHandshake> {
    if timeout.is_zero() || timeout > Duration::from_secs(30) {
        return Err(Error::Config("WS53 probe timeout must be in(0,30s]".into()));
    }
    cancel.check()?;
    let saved_timeout = port.timeout();
    let result = (|| {
        port.set_baud_rate(115_200)?;
        port.set_timeout(timeout.min(Duration::from_millis(20)))?;
        port.clear_buffers()?;
        let start = Instant::now();
        let mut decoder = Decoder::default();
        let mut bytes = [0; 256];
        while start.elapsed() < timeout {
            cancel.check()?;
            port.write_all_bytes(&ROM_HANDSHAKE)?;
            match port.read(&mut bytes) {
                Ok(0) => return Err(Error::Io(std::io::Error::from(ErrorKind::UnexpectedEof))),
                Ok(count) => {
                    if let Some(ack) = decoder.feed(&bytes[..count])? {
                        return Ok(ack);
                    }
                },
                Err(error)
                    if error.kind() == ErrorKind::TimedOut
                        || error.kind() == ErrorKind::WouldBlock => {},
                Err(error) => return Err(error.into()),
            }
        }
        Err(Error::Timeout(
            "WS53 BootROM handshake deadline; no flash command sent".into(),
        ))
    })();
    let restore = port.set_timeout(saved_timeout);
    match (result, restore) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
    }
}

/// Download a caller-validated WS53 LoaderBoot package into RAM after handshake.
///
/// This sends YMODEM only, never a Flash download/erase command. The caller must
/// validate the WS53 package (root/header/body), provenance and checksum before
/// opening the session. The 64 KiB bound is the SDK LoaderBoot RAM limit, not a
/// general image-format parser. A successful return requires a CRC-valid SEBOOT
/// ACK after YMODEM, not just its four-byte magic. It does not identify the chip
/// or qualify Flash operations. On failure reset and reconnect; do not continue
/// a partially transferred session. Restores the port timeout on every exit.
pub fn load_loaderboot<P: Port>(
    port: &mut P,
    package: &[u8],
    handshake: RomHandshake,
    cancel: &CancelContext,
) -> Result<RomHandshake> {
    if package.is_empty() || package.len() > 0x10000 {
        return Err(Error::Config(
            "WS53 LoaderBoot package must fit 64 KiB".into(),
        ));
    }
    cancel.check()?;
    let saved_timeout = port.timeout();
    let result = (|| {
        port.set_timeout(Duration::from_millis(20))?;
        let config = YmodemConfig {
            char_timeout: Duration::from_secs(1),
            c_timeout: Duration::from_secs(5),
            max_retries: 3,
            finish_without_c: true,
            verbose: 0,
        };
        let mut transfer = YmodemTransfer::with_config(port, config, cancel)
            .with_prefetched_input(handshake.trailing);
        transfer.transfer("loaderboot", package, |_, _| {})?;
        let mut decoder = Decoder::default();
        if let Some(ack) = decoder.feed(&transfer.take_trailing_data())? {
            return Ok(ack);
        }
        let start = Instant::now();
        let mut bytes = [0; 256];
        while start.elapsed() < Duration::from_secs(3) {
            cancel.check()?;
            match port.read(&mut bytes) {
                Ok(0) => return Err(Error::Io(std::io::Error::from(ErrorKind::UnexpectedEof))),
                Ok(count) => {
                    if let Some(ack) = decoder.feed(&bytes[..count])? {
                        return Ok(ack);
                    }
                },
                Err(error)
                    if matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => {},
                Err(error) => return Err(error.into()),
            }
        }
        Err(Error::Timeout(
            "WS53 LoaderBoot did not send a valid completion ACK; reset required".into(),
        ))
    })();
    let restore = port.set_timeout(saved_timeout);
    match (result, restore) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::VecDeque,
        io::{self, Read, Write},
    };

    pub(super) struct MockPort {
        pub(super) chunks: VecDeque<Vec<u8>>,
        failure: ErrorKind,
        pub(super) writes: Vec<u8>,
        pub(super) timeout: Duration,
        pub(super) baud: u32,
    }
    impl MockPort {
        pub(super) fn new(chunks: Vec<Vec<u8>>, failure: ErrorKind) -> Self {
            Self {
                chunks: chunks.into(),
                failure,
                writes: vec![],
                timeout: Duration::from_secs(3),
                baud: 9600,
            }
        }
    }
    impl Read for MockPort {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            if let Some(mut chunk) = self
                .chunks
                .pop_front()
            {
                let count = bytes
                    .len()
                    .min(chunk.len());
                bytes[..count].copy_from_slice(&chunk[..count]);
                if count < chunk.len() {
                    chunk.drain(..count);
                    self.chunks
                        .push_front(chunk);
                }
                Ok(count)
            } else {
                Err(io::Error::from(self.failure))
            }
        }
    }
    impl Write for MockPort {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.writes
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl Port for MockPort {
        fn set_timeout(&mut self, timeout: Duration) -> Result<()> {
            self.timeout = timeout;
            Ok(())
        }
        fn timeout(&self) -> Duration {
            self.timeout
        }
        fn set_baud_rate(&mut self, baud: u32) -> Result<()> {
            self.baud = baud;
            Ok(())
        }
        fn baud_rate(&self) -> u32 {
            self.baud
        }
        fn clear_buffers(&mut self) -> Result<()> {
            Ok(())
        }
        fn name(&self) -> &'static str {
            "WS53 test"
        }
        fn set_dtr(&mut self, _: bool) -> Result<()> {
            panic!("probe must not reset")
        }
        fn set_rts(&mut self, _: bool) -> Result<()> {
            panic!("probe must not reset")
        }
        fn read_cts(&mut self) -> Result<bool> {
            Ok(false)
        }
        fn read_dsr(&mut self) -> Result<bool> {
            Ok(false)
        }
        fn close(&mut self) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn loader_rejects_bad_size_before_io_and_restores_timeout_on_disconnect() {
        for size in [0, 0x10001] {
            let mut port = MockPort::new(vec![], ErrorKind::BrokenPipe);
            let handshake = RomHandshake {
                ack: response(0x5a, 0),
                trailing: vec![],
            };
            assert!(matches!(
                load_loaderboot(&mut port, &vec![0; size], handshake, &CancelContext::none()),
                Err(Error::Config(_))
            ));
            assert!(
                port.writes
                    .is_empty()
            );
            assert_eq!(port.timeout, Duration::from_secs(3));
        }
        let mut port = MockPort::new(vec![], ErrorKind::BrokenPipe);
        let handshake = RomHandshake {
            ack: response(0x5a, 0),
            trailing: vec![],
        };
        assert!(load_loaderboot(&mut port, &[0], handshake, &CancelContext::none()).is_err());
        assert_eq!(port.timeout, Duration::from_secs(3));
    }

    fn upload_packet(seq: u8, payload: &[u8]) -> Vec<u8> {
        let mut packet = vec![if payload.len() == 128 { 1 } else { 2 }, seq, !seq];
        packet.extend_from_slice(payload);
        packet.extend_from_slice(&crc16_xmodem(payload).to_be_bytes());
        packet
    }

    pub(super) fn upload_wire(body: &[u8]) -> Vec<u8> {
        let mut header = [0; 128];
        let name = format!("flash.bin\0{}\0", body.len());
        header[..name.len()].copy_from_slice(name.as_bytes());
        let mut wire = response(0x5a, 0xa5).to_vec();
        wire.extend(upload_packet(0, &header));
        for (index, chunk) in body
            .chunks(1024)
            .enumerate()
        {
            let mut payload = [0; 1024];
            payload[..chunk.len()].copy_from_slice(chunk);
            wire.extend(upload_packet(
                u8::try_from((index + 1) % 256).unwrap(),
                &payload,
            ));
        }
        wire.push(4);
        wire.extend(upload_packet(0, &[0; 128]));
        wire.extend(response(0x5a, 0));
        wire
    }

    #[test]
    fn loader_reset_request_has_no_program_or_security_payload() {
        let mut port = MockPort::new(vec![], ErrorKind::BrokenPipe);
        request_loader_reset(&mut port, &CancelContext::none()).unwrap();
        assert_eq!(
            &port.writes[..10],
            &[0xef, 0xbe, 0xad, 0xde, 12, 0, 0x87, 0x78, 0, 0]
        );
        assert_eq!(
            port.writes
                .len(),
            12
        );
        assert_eq!(
            crc16_xmodem(&port.writes[..10]),
            u16::from_le_bytes([port.writes[10], port.writes[11]])
        );
        assert_eq!(port.timeout, Duration::from_secs(3));
    }

    #[test]
    fn replacement_checks_backup_program_completion_and_independent_readback() {
        let backup = [0x11; 4096];
        let image = [0x22; 4096];
        let ready = || RomHandshake {
            ack: response(0x5a, 0),
            trailing: vec![],
        };
        let success = || {
            let mut begin = response(0x5a, 0).to_vec();
            begin.push(b'C');
            vec![
                upload_wire(&backup),
                begin,
                vec![6, b'C'],
                vec![6],
                vec![6],
                vec![6],
                vec![6],
                vec![6, b'C'],
                [vec![6], response(0x5a, 0).to_vec()].concat(),
                upload_wire(&image),
            ]
        };
        let mut port = MockPort::new(success(), ErrorKind::BrokenPipe);
        replace_application_sectors(
            &mut port,
            0x60000,
            &backup,
            &image,
            ready(),
            &CancelContext::none(),
        )
        .unwrap();
        assert_eq!(port.timeout, Duration::from_secs(3));
        let d2 = port
            .writes
            .windows(8)
            .position(|b| b == [0xef, 0xbe, 0xad, 0xde, 24, 0, 0xd2, 0x2d])
            .unwrap();
        let cmd = &port.writes[d2..d2 + 24];
        assert_eq!(&cmd[8..12], &0x460000u32.to_le_bytes());
        assert_eq!(&cmd[12..20], &[0, 16, 0, 0, 0, 16, 0, 0]);
        assert_eq!(&cmd[20..22], &[0, 255]);
        assert_eq!(
            crc16_xmodem(&cmd[..22]),
            u16::from_le_bytes([cmd[22], cmd[23]])
        );
        assert_eq!(
            port.writes
                .windows(8)
                .filter(|b| *b == &cmd[..8])
                .count(),
            1
        );
        // No reset command is sent, even after successful verification.
        assert!(
            !port
                .writes
                .windows(8)
                .any(|b| b == [0xef, 0xbe, 0xad, 0xde, 12, 0, 0x87, 0x78])
        );

        for (kind, stage, changed) in [
            (0, WriteStage::BackupCheck, false),
            (1, WriteStage::Erase, true),
            (2, WriteStage::Program, true),
            (3, WriteStage::Verify, true),
            (4, WriteStage::Program, true),
        ] {
            let mut chunks = success();
            match kind {
                0 => chunks[0] = upload_wire(&image), // stale backup
                1 => chunks[1] = response(0xa5, 0x5a).to_vec(),
                2 => chunks.truncate(4), // link lost after first data block
                3 => {
                    *chunks
                        .last_mut()
                        .unwrap() = upload_wire(&backup);
                },
                4 => chunks[8] = [vec![6], response(0xa5, 0x5a).to_vec()].concat(),
                _ => unreachable!(),
            }
            let mut port = MockPort::new(chunks, ErrorKind::BrokenPipe);
            let error = replace_application_sectors(
                &mut port,
                0x60000,
                &backup,
                &image,
                ready(),
                &CancelContext::none(),
            )
            .unwrap_err();
            assert_eq!(
                (error.stage, error.flash_may_have_changed),
                (stage, changed)
            );
            assert_eq!(port.timeout, Duration::from_secs(3));
            if !changed {
                assert!(
                    !port
                        .writes
                        .windows(8)
                        .any(|b| b == &cmd[..8])
                );
            }
        }
    }

    #[test]
    fn replacement_refuses_protected_ranges_and_control_or_header_changes_without_io() {
        let ready = || RomHandshake {
            ack: response(0x5a, 0),
            trailing: vec![],
        };
        for (offset, length) in [
            (0, 4096),
            (0x20000, 4096),
            (0x31000, 4096),
            (0x5c000, 4096),
            (0x270000, 4096),
            (0x3fc000, 4096),
            (u32::MAX, 4096),
            (0x60001, 4096),
            (0x60000, 0),
            (0x60000, 4095),
            (0x60000, 0x41000),
            (0x30000, 8192),
        ] {
            let data = vec![0; length];
            let mut port = MockPort::new(vec![], ErrorKind::BrokenPipe);
            let error = replace_application_sectors(
                &mut port,
                offset,
                &data,
                &data,
                ready(),
                &CancelContext::none(),
            )
            .unwrap_err();
            assert_eq!(error.stage, WriteStage::Validate);
            assert!(
                !error.flash_may_have_changed
                    && port
                        .writes
                        .is_empty()
            );
        }
        for (offset, index) in [
            (0x30000, 0),
            (0x30000, 0x148),
            (0x30000, 0x300),
            (0x5d000, 0x200),
        ] {
            let backup = [0; 4096];
            let mut image = backup;
            image[index] = 1;
            let mut port = MockPort::new(vec![], ErrorKind::BrokenPipe);
            assert!(
                replace_application_sectors(
                    &mut port,
                    offset,
                    &backup,
                    &image,
                    ready(),
                    &CancelContext::none()
                )
                .is_err()
            );
            assert!(
                port.writes
                    .is_empty()
            );
        }
    }

    #[test]
    fn flash_upload_validates_packets_duplicates_sequence_wrap_and_bounds() {
        let body: Vec<_> = (0..33000)
            .map(|n| u8::try_from(n % 251).unwrap())
            .collect();
        let mut header = [0; 128];
        header[..16].copy_from_slice(b"flash.bin\x0033000\0");
        let mut wire = response(0x5a, 0xa5).to_vec();
        wire.extend(upload_packet(0, &header));
        for (index, chunk) in body
            .chunks(128)
            .enumerate()
        {
            let mut payload = [0; 128];
            payload[..chunk.len()].copy_from_slice(chunk);
            let block = upload_packet(u8::try_from((index + 1) % 256).unwrap(), &payload);
            wire.extend(&block);
            if index == 17 {
                wire.extend(&block);
            }
        }
        wire.push(4);
        let mut finish = [0; 128];
        finish[125..].copy_from_slice(&[0x12, 0x34, 0x56]);
        wire.extend(upload_packet(0, &finish));
        wire.extend(response(0x5a, 0));
        let ready = || RomHandshake {
            ack: response(0x5a, 0xa5),
            trailing: b"loaderboot ready\r\n".to_vec(),
        };
        let mut port = MockPort::new(vec![wire.clone()], ErrorKind::BrokenPipe);
        assert_eq!(
            read_flash(&mut port, 0, 33000, ready(), &CancelContext::none()).unwrap(),
            body
        );
        assert_eq!(port.writes[6], 0xb4);
        assert_eq!(&port.writes[8..12], &33000u32.to_le_bytes());
        assert_eq!(&port.writes[12..16], &0u32.to_le_bytes());
        assert_eq!(port.timeout, Duration::from_secs(3));
        // Corrupt frame body, sequence complement, and truncate transfer.
        for bytes in [
            {
                let mut b = wire.clone();
                b[12 + 133 + 4] ^= 1;
                b
            },
            {
                let mut b = wire.clone();
                b[12 + 133 + 2] ^= 1;
                b
            },
            wire[..200].to_vec(),
        ] {
            let mut port = MockPort::new(vec![bytes], ErrorKind::BrokenPipe);
            assert!(read_flash(&mut port, 0, 33000, ready(), &CancelContext::none()).is_err());
            assert_eq!(port.timeout, Duration::from_secs(3));
        }
        for (addr, len) in [
            (1, 1),
            (0x800000, 1),
            (0x400000, 0),
            (0x400000, 0x40001),
            (u32::MAX, 2),
        ] {
            let mut port = MockPort::new(vec![], ErrorKind::BrokenPipe);
            assert!(read_flash(&mut port, addr, len, ready(), &CancelContext::none()).is_err());
            assert!(
                port.writes
                    .is_empty()
            );
        }
    }

    fn update_ready() -> RomHandshake {
        RomHandshake {
            ack: response(0x5a, 0),
            trailing: vec![],
        }
    }

    pub(super) fn replacement_wire(before: &[u8], after: &[u8]) -> Vec<Vec<u8>> {
        let mut begin = response(0x5a, 0).to_vec();
        begin.push(b'C');
        vec![
            upload_wire(before),
            begin,
            vec![6, b'C'],
            vec![6],
            vec![6],
            vec![6],
            vec![6],
            vec![6, b'C'],
            [vec![6], response(0x5a, 0).to_vec()].concat(),
            upload_wire(after),
        ]
    }

    pub(super) fn d2_offsets(writes: &[u8]) -> Vec<u32> {
        writes
            .windows(24)
            .filter(|b| b[..8] == [0xef, 0xbe, 0xad, 0xde, 24, 0, 0xd2, 0x2d])
            .map(|b| {
                u32::from_le_bytes(
                    b[8..12]
                        .try_into()
                        .unwrap(),
                ) - 0x400000
            })
            .collect()
    }

    #[test]
    fn application_plan_prechecks_all_and_writes_header_last_with_explicit_recovery() {
        let body = [0x11; 4096];
        let mut new_body = body;
        new_body[0x300..].fill(0x33);
        let header = [0x22; 4096];
        let mut new_header = header;
        new_header[0x128..0x148].fill(0x44);
        let sectors = [
            ApplicationSector {
                offset: 0x5d000,
                original: &body,
                replacement: &new_body,
            },
            ApplicationSector {
                offset: 0x30000,
                original: &header,
                replacement: &new_header,
            },
        ];
        let plan = ApplicationUpdate::new(&sectors).unwrap();
        for recovery in [false, true] {
            // Entire erased sectors include preserved application placeholder
            // and control bytes: recovery must restore these, not refuse them.
            let erased = [0xff; 4096];
            let (before_body, before_header, after_body, after_header) = if recovery {
                (&erased, &erased, &body, &header)
            } else {
                (&body, &header, &new_body, &new_header)
            };
            let mut chunks = vec![upload_wire(before_body), upload_wire(before_header)];
            chunks.extend(replacement_wire(before_body, after_body));
            chunks.extend(replacement_wire(before_header, after_header));
            let mut port = MockPort::new(chunks, ErrorKind::BrokenPipe);
            if recovery {
                plan.recover(&mut port, update_ready(), &CancelContext::none())
            } else {
                plan.apply(&mut port, update_ready(), &CancelContext::none())
            }
            .unwrap();
            assert_eq!(d2_offsets(&port.writes), [0x5d000, 0x30000]);
            assert!(
                !port
                    .writes
                    .windows(8)
                    .any(|b| b == [0xef, 0xbe, 0xad, 0xde, 12, 0, 0x87, 0x78])
            );
            assert_eq!(port.timeout, Duration::from_secs(3));
        }
        // A stale LAST original must reject before writing the FIRST sector.
        let mut port = MockPort::new(
            vec![upload_wire(&body), upload_wire(&new_header)],
            ErrorKind::BrokenPipe,
        );
        let error = plan
            .apply(&mut port, update_ready(), &CancelContext::none())
            .unwrap_err();
        assert_eq!(
            (
                error.index,
                error.verified_sectors,
                error.flash_may_have_changed
            ),
            (1, 0, false)
        );
        assert!(d2_offsets(&port.writes).is_empty());
        // A later read failure must not forget an already verified first write.
        for recovery in [false, true] {
            let mut chunks = vec![upload_wire(&body), upload_wire(&header)];
            chunks.extend(replacement_wire(
                &body,
                if recovery { &body } else { &new_body },
            ));
            let mut port = MockPort::new(chunks, ErrorKind::BrokenPipe);
            let result = if recovery {
                plan.recover(&mut port, update_ready(), &CancelContext::none())
            } else {
                plan.apply(&mut port, update_ready(), &CancelContext::none())
            };
            let error = result.unwrap_err();
            assert_eq!(
                (
                    error.index,
                    error.verified_sectors,
                    error.flash_may_have_changed
                ),
                (1, 1, true)
            );
            assert_eq!(
                error
                    .source
                    .stage,
                WriteStage::BackupCheck
            );
            assert_eq!(d2_offsets(&port.writes), [0x5d000]);
        }
    }

    #[test]
    fn application_plan_rejects_late_invalid_ranges_overlap_and_protected_changes() {
        let data = [0; 4096];
        for offsets in [
            vec![],
            vec![0x30000],
            vec![0x30000, 0x5d000],
            vec![0x5d000, 0x5d000, 0x30000],
            vec![0x60000, 0x5d000, 0x30000],
            vec![0x5d000, 0x3fc000, 0x30000],
        ] {
            let sectors: Vec<_> = offsets
                .into_iter()
                .map(|offset| ApplicationSector {
                    offset,
                    original: &data,
                    replacement: &data,
                })
                .collect();
            assert!(ApplicationUpdate::new(&sectors).is_err());
        }
        let mut policy_change = data;
        policy_change[0x148] = 1;
        let sectors = [
            ApplicationSector {
                offset: 0x5d000,
                original: &data,
                replacement: &data,
            },
            ApplicationSector {
                offset: 0x30000,
                original: &data,
                replacement: &policy_change,
            },
        ];
        assert!(ApplicationUpdate::new(&sectors).is_err());
    }

    #[test]
    fn upload_rejects_bad_metadata_eot_finish_ack_and_cancel() {
        let mut header = [0; 128];
        header[..13].copy_from_slice(b"upload.bin\x001\0");
        let normal = [
            response(0x5a, 0xa5).to_vec(),
            upload_packet(0, &header),
            upload_packet(1, &[0x42; 128]),
            vec![4],
            upload_packet(0, &[0; 128]),
            response(0x5a, 0xa5).to_vec(),
        ];
        for case in 0..8 {
            let mut parts = normal.clone();
            match case {
                0 => {
                    let mut h = header;
                    h[11] = b'2';
                    parts[1] = upload_packet(0, &h);
                },
                1 => parts[2] = upload_packet(2, &[0x42; 128]),
                2 => parts[2] = vec![4],
                3 => parts[4] = upload_packet(0, &[b'x'; 128]),
                4 => parts[5] = response(0xa5, 0x5a).to_vec(),
                5 => parts[5][10] ^= 1,
                6 => parts[0] = response(0xa5, 0x5a).to_vec(),
                7 => parts[2] = vec![0x18],
                _ => unreachable!(),
            }
            let mut port = MockPort::new(vec![parts.concat()], ErrorKind::BrokenPipe);
            let ready = RomHandshake {
                ack: response(0x5a, 0xa5),
                trailing: vec![],
            };
            assert!(
                read_flash(&mut port, 0, 1, ready, &CancelContext::none()).is_err(),
                "case {case}"
            );
            assert_eq!(port.timeout, Duration::from_secs(3));
        }
    }

    #[test]
    fn loader_requires_crc_valid_completion_and_retains_trailing() {
        for result in [0x5a, 0xa5] {
            let mut completion = response(result, 0).to_vec();
            completion.extend_from_slice(b"ready");
            let mut port = MockPort::new(
                vec![vec![6], vec![6], vec![6, b'C'], vec![6], completion],
                ErrorKind::BrokenPipe,
            );
            let handshake = RomHandshake {
                ack: response(0x5a, 0),
                trailing: vec![b'C'],
            };
            let outcome =
                load_loaderboot(&mut port, &[0x42; 100], handshake, &CancelContext::none());
            if result == 0x5a {
                assert_eq!(
                    outcome
                        .unwrap()
                        .trailing,
                    b"ready"
                );
            } else {
                assert!(matches!(outcome, Err(Error::HandshakeFailed(_))));
            }
            assert_eq!(port.timeout, Duration::from_secs(3));
            // file-info block, one data block, EOT, empty finish block: no D2.
            assert_eq!(
                port.writes
                    .len(),
                133 + 1029 + 1 + 133
            );
            assert_eq!(&port.writes[..3], &[1, 0, 255]);
            assert_eq!(&port.writes[133..136], &[2, 1, 254]);
        }
    }

    #[test]
    fn transaction_restores_timeout_and_never_sends_a_flash_command() {
        let frame = response(0x5a, 0);
        let mut port = MockPort::new(
            vec![frame[..5].to_vec(), frame[5..].to_vec()],
            ErrorKind::BrokenPipe,
        );
        assert_eq!(
            probe_bootrom(&mut port, Duration::from_secs(1), &CancelContext::none())
                .unwrap()
                .ack,
            frame
        );
        assert_eq!(port.timeout, Duration::from_secs(3));
        assert_eq!(port.baud, 115_200);
        assert_eq!(port.writes, ROM_HANDSHAKE.repeat(2));
    }
    #[test]
    fn deadline_disconnect_failure_and_cancel_leave_no_erase_or_loader_transaction() {
        for kind in [ErrorKind::TimedOut, ErrorKind::BrokenPipe] {
            let mut port = MockPort::new(vec![], kind);
            let result = probe_bootrom(&mut port, Duration::from_millis(1), &CancelContext::none());
            if kind == ErrorKind::TimedOut {
                assert!(matches!(result, Err(Error::Timeout(_))));
            } else {
                assert!(matches!(result, Err(Error::Io(_))));
            }
            assert_eq!(port.timeout, Duration::from_secs(3));
            assert!(
                port.writes
                    .chunks_exact(18)
                    .all(|x| x == ROM_HANDSHAKE)
            );
        }
        let mut port = MockPort::new(vec![response(0xa5, 4).to_vec()], ErrorKind::BrokenPipe);
        assert!(matches!(
            probe_bootrom(&mut port, Duration::from_secs(1), &CancelContext::none()),
            Err(Error::HandshakeFailed(_))
        ));
        assert_eq!(port.timeout, Duration::from_secs(3));
        port.writes
            .clear();
        assert!(
            probe_bootrom(
                &mut port,
                Duration::from_secs(1),
                &CancelContext::new(|| true)
            )
            .is_err()
        );
        assert!(
            port.writes
                .is_empty()
        );
        assert!(probe_bootrom(&mut port, Duration::ZERO, &CancelContext::none()).is_err());
        assert!(
            port.writes
                .is_empty()
        );
    }
    pub(super) fn response(result: u8, status: u8) -> [u8; 12] {
        let mut frame = [
            0xef, 0xbe, 0xad, 0xde, 12, 0, 0xe1, 0x1e, result, status, 0, 0,
        ];
        let crc = crc16_xmodem(&frame[..10]).to_le_bytes();
        frame[10..].copy_from_slice(&crc);
        frame
    }
    #[test]
    fn vendor_ws53_handshake_matches_common_seboot_encoder() {
        assert_eq!(
            crate::protocol::seboot::SebootFrame::handshake(115_200).build(),
            ROM_HANDSHAKE
        );
    }
    #[test]
    fn every_split_preserves_partial_ack_and_following_loader_request() {
        for status in [0, 0xa5] {
            let frame = response(0x5a, status);
            for split in 0..12 {
                let mut decoder = Decoder::default();
                assert!(
                    decoder
                        .feed(b"boot log\r\n")
                        .unwrap()
                        .is_none()
                );
                assert!(
                    decoder
                        .feed(&frame[..split])
                        .unwrap()
                        .is_none()
                );
                let mut rest = frame[split..].to_vec();
                rest.extend_from_slice(b"CC");
                assert_eq!(
                    decoder
                        .feed(&rest)
                        .unwrap(),
                    Some(RomHandshake {
                        ack: frame,
                        trailing: b"CC".to_vec()
                    })
                );
            }
        }
    }
    #[test]
    fn corruption_is_not_success_and_does_not_hide_a_following_valid_frame() {
        let frame = response(0x5a, 0);
        for index in 0..12 {
            let mut broken = frame;
            broken[index] ^= 1;
            let mut decoder = Decoder::default();
            assert!(
                decoder
                    .feed(&broken)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                decoder
                    .feed(&frame)
                    .unwrap()
                    .unwrap()
                    .ack,
                frame
            );
        }
    }
    #[test]
    fn explicit_failure_is_returned_and_noise_memory_is_bounded() {
        let mut decoder = Decoder::default();
        for _ in 0..1000 {
            assert!(
                decoder
                    .feed(&[0xff; 256])
                    .unwrap()
                    .is_none()
            );
            assert!(
                decoder
                    .tail
                    .len()
                    <= 11
            );
        }
        assert!(matches!(
            decoder.feed(&response(0xa5, 3)),
            Err(Error::HandshakeFailed(_))
        ));
    }
}
