//! Restricted sector replacement for the observed WS53 MCU-mode-1 layout.
//! Format construction belongs to hisi-fwpkg. This is not a general flasher.
use super::{Decoder, RomHandshake, read_flash};
use crate::{
    CancelContext, Error, Result,
    port::Port,
    protocol::{
        crc::crc16_xmodem,
        ymodem::{YmodemConfig, YmodemTransfer},
    },
};
use std::{
    io::ErrorKind,
    time::{Duration, Instant},
};

/// Stage of a failed replacement, retained for an explicit recovery decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteStage {
    /// Argument/preservation validation; no I/O.
    Validate,
    /// Compare live bytes with the caller's durable backup before erasing.
    BackupCheck,
    /// D2 command may already have started erasing.
    Erase,
    /// YMODEM image transfer or its completion acknowledgement.
    Program,
    /// Independent B4 readback must equal all requested bytes.
    Verify,
    /// Restoring the caller's serial timeout failed.
    RestoreTimeout,
}

/// Failure never implies rollback. Reset/reconnect and recover from the backup.
#[derive(Debug, thiserror::Error)]
#[error(
    "WS53 {stage:?} failed (Flash may have changed: {flash_may_have_changed}); reconnect required: {source}"
)]
pub struct WriteFailure {
    /// The transaction stage that failed.
    pub stage: WriteStage,
    /// True from the first attempted D2 write, including partial serial writes.
    pub flash_may_have_changed: bool,
    /// Original protocol or I/O failure.
    #[source]
    pub source: Error,
}

pub(super) fn validate(offset: u32, backup: &[u8], image: &[u8]) -> Result<u32> {
    let length = image.len();
    if length == 0
        || length > 0x40000
        || length % 0x1000 != 0
        || offset % 0x1000 != 0
        || backup.len() != length
    {
        return Err(Error::Config(
            "WS53 replacement requires equal whole-sector backup/image, at most 256 KiB".into(),
        ));
    }
    let length32 =
        u32::try_from(length).map_err(|_| Error::Config("WS53 image too large".into()))?;
    let end = offset
        .checked_add(length32)
        .ok_or_else(|| Error::Config("WS53 range overflow".into()))?;
    // Only the combined header sector or the application subpartition. Never
    // erase the intervening control-core payload, bootloaders, params or NV.
    if !((offset == 0x30000 && length == 0x1000) || (offset >= 0x5d000 && end <= 0x270000)) {
        return Err(Error::Config(
            "WS53 replacement outside qualified application/header sectors".into(),
        ));
    }
    for (address, (&old, &new)) in (offset..end).zip(
        backup
            .iter()
            .zip(image),
    ) {
        let permitted = (0x30124..0x30148).contains(&address) // body length and hash only
            || (0x5d300..0x270000).contains(&address);
        if old != new && !permitted {
            return Err(Error::Config(
                "WS53 replacement changes preserved control bytes or image policy".into(),
            ));
        }
    }
    Ok(length32)
}

fn frame(offset: u32, length: u32) -> Vec<u8> {
    // fbb_burntool Channel.h::SEBOOT_DOWNLOAD_FLASHIMAGE; SDK packet.py
    // supplies CPU-mapped addresses to D2, unlike physical-offset B4.
    let mut bytes = vec![0xef, 0xbe, 0xad, 0xde, 24, 0, 0xd2, 0x2d];
    bytes.extend_from_slice(&(0x400000 + offset).to_le_bytes());
    bytes.extend_from_slice(&length.to_le_bytes());
    bytes.extend_from_slice(&length.to_le_bytes()); // exact erase span, never chip erase
    bytes.extend_from_slice(&[0, 255]); // ordinary Flash, not ROM/eFuse/NV
    bytes.extend_from_slice(&crc16_xmodem(&bytes).to_le_bytes());
    bytes
}

fn ack<P: Port>(
    port: &mut P,
    initial: &[u8],
    timeout: Duration,
    cancel: &CancelContext,
) -> Result<RomHandshake> {
    let mut decoder = Decoder::default();
    if let Some(value) = decoder.feed(initial)? {
        return Ok(value);
    }
    let deadline = Instant::now() + timeout;
    let mut bytes = [0; 256];
    while Instant::now() < deadline {
        cancel.check()?;
        match port.read(&mut bytes) {
            Ok(0) => return Err(std::io::Error::from(ErrorKind::UnexpectedEof).into()),
            Ok(count) => {
                if let Some(value) = decoder.feed(&bytes[..count])? {
                    return Ok(value);
                }
            },
            Err(e) if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => {},
            Err(e) => return Err(e.into()),
        }
    }
    Err(Error::Timeout("WS53 Flash command completion ACK".into()))
}

/// Replace backed-up application sectors and verify every byte by B4 readback.
///
/// Experimental observed layout only: [0x5d000,0x270000), plus the combined
/// header sector at0x30000 where only body length/hash may change. All offsets
/// are physical Flash offsets. Preserves the 0x300-byte application placeholder
/// and control-core bytes. No chip erase, OTP/security or baud commands.
/// Caller must qualify the WS53/LoaderBoot, actual partition table and raw
/// mapping, save the backup durably and validate the complete image/boot chain
/// with hisi-fwpkg before calling. This API does not do image-format parsing.
/// It compares live bytes with that backup before D2; stale backup is refused.
/// D2 is never retried. Any error requires reset/reconnect, possibly recovery;
/// no automatic reset to a partially written application. Success means byte
/// verification, not a valid signature or successful startup/cold boot.
pub fn replace_application_sectors<P: Port>(
    port: &mut P,
    offset: u32,
    backup: &[u8],
    image: &[u8],
    ready: RomHandshake,
    cancel: &CancelContext,
) -> std::result::Result<(), WriteFailure> {
    let length = validate(offset, backup, image).map_err(|source| WriteFailure {
        stage: WriteStage::Validate,
        flash_may_have_changed: false,
        source,
    })?;
    perform_replacement(port, offset, backup, image, length, ready, cancel)
}

// Only called after validating either a normal replacement or the complete
// original/proposed pair in ApplicationUpdate. Recovery compares live bytes,
// but may restore erased preserved bytes from the qualified original backup.
pub(super) fn perform_replacement<P: Port>(
    port: &mut P,
    offset: u32,
    backup: &[u8],
    image: &[u8],
    length: u32,
    ready: RomHandshake,
    cancel: &CancelContext,
) -> std::result::Result<(), WriteFailure> {
    let mut stage = WriteStage::BackupCheck;
    let mut changed = false;
    let saved = port.timeout();
    let result = (|| {
        cancel.check()?;
        port.set_timeout(Duration::from_millis(20))?;
        stage = WriteStage::BackupCheck;
        if read_flash(port, offset, length, ready, cancel)? != backup {
            return Err(Error::Protocol(
                "live Flash differs from saved backup; no erase sent".into(),
            ));
        }
        cancel.check()?;
        stage = WriteStage::Erase;
        changed = true;
        port.write_all_bytes(&frame(offset, length))?;
        let ready = ack(port, &[], Duration::from_secs(30), cancel)?;
        stage = WriteStage::Program;
        let config = YmodemConfig {
            char_timeout: Duration::from_secs(1),
            c_timeout: Duration::from_secs(5),
            max_retries: 3,
            finish_without_c: true,
            verbose: 0,
        };
        let mut transfer =
            YmodemTransfer::with_config(port, config, cancel).with_prefetched_input(ready.trailing);
        transfer.transfer("application", image, |_, _| {})?;
        let trailing = transfer.take_trailing_data();
        let done = ack(port, &trailing, Duration::from_secs(5), cancel)?;
        stage = WriteStage::Verify;
        if read_flash(port, offset, length, done, cancel)? != image {
            return Err(Error::Protocol(
                "WS53 post-program readback mismatch".into(),
            ));
        }
        Ok(())
    })();
    let restore = port.set_timeout(saved);
    match (result, restore) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(source), _) => Err(WriteFailure {
            stage,
            flash_may_have_changed: changed,
            source,
        }),
        (Ok(()), Err(source)) => Err(WriteFailure {
            stage: WriteStage::RestoreTimeout,
            flash_may_have_changed: changed,
            source,
        }),
    }
}
