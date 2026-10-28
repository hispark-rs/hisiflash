//! WS53 vendor B4 upload: strict, bounded YMODEM receiver for Flash backups.
use super::{Decoder, RomHandshake};
use crate::{CancelContext, Error, Result, port::Port, protocol::crc::crc16_xmodem};
use std::{
    collections::VecDeque,
    io::ErrorKind,
    time::{Duration, Instant},
};

struct Receiver<'a, P> {
    port: &'a mut P,
    cancel: &'a CancelContext,
    pending: VecDeque<u8>,
    deadline: Instant,
}
impl<P: Port> Receiver<'_, P> {
    fn byte(&mut self, deadline: Instant) -> Result<u8> {
        loop {
            self.cancel
                .check()?;
            if Instant::now() >= deadline.min(self.deadline) {
                return Err(Error::Timeout(
                    "WS53 Flash upload deadline; reconnect required".into(),
                ));
            }
            if let Some(byte) = self
                .pending
                .pop_front()
            {
                return Ok(byte);
            }
            let mut bytes = [0; 256];
            match self
                .port
                .read(&mut bytes)
            {
                Ok(0) => return Err(std::io::Error::from(ErrorKind::UnexpectedEof).into()),
                Ok(count) => self
                    .pending
                    .extend(&bytes[..count]),
                Err(error)
                    if matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => {},
                Err(error) => return Err(error.into()),
            }
        }
    }
    fn ack(&mut self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut decoder = Decoder::default();
        loop {
            if decoder
                .feed(&[self.byte(deadline)?])?
                .is_some()
            {
                return Ok(());
            }
        }
    }
    fn packet(&mut self, allow_boot_log: bool) -> Result<Option<(u8, Vec<u8>)>> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let kind = loop {
            let byte = self.byte(deadline)?;
            match byte {
                1 | 2 => break byte,
                4 => return Ok(None),
                0x18 => return Err(Error::Ymodem("WS53 upload cancelled by device".into())),
                _ if allow_boot_log && (byte.is_ascii_graphic() || byte.is_ascii_whitespace()) => {
                },
                _ => {
                    return Err(Error::Ymodem(format!(
                        "unexpected WS53 upload byte {byte:#x}"
                    )));
                },
            }
        };
        let seq = self.byte(deadline)?;
        if self.byte(deadline)? != !seq {
            return Err(Error::Ymodem(
                "WS53 upload sequence complement mismatch".into(),
            ));
        }
        let mut payload = vec![0; if kind == 1 { 128 } else { 1024 }];
        for byte in &mut payload {
            *byte = self.byte(deadline)?;
        }
        let got = u16::from_be_bytes([self.byte(deadline)?, self.byte(deadline)?]);
        let expected = crc16_xmodem(&payload);
        if got != expected {
            return Err(Error::CrcMismatch {
                expected,
                actual: got,
            });
        }
        Ok(Some((seq, payload)))
    }
    fn transfer(&mut self, len: usize) -> Result<Vec<u8>> {
        self.ack()?;
        self.port
            .write_all_bytes(b"C")?;
        let (seq, header) = self
            .packet(true)?
            .ok_or_else(|| Error::Ymodem("early upload EOT".into()))?;
        let fields: Vec<_> = header
            .split(|&b| b == 0)
            .collect();
        let length_field = fields
            .get(1)
            .copied()
            .unwrap_or_default();
        let advertised = std::str::from_utf8(length_field)
            .ok()
            .and_then(|s| {
                s.split_ascii_whitespace()
                    .next()
            })
            .and_then(|s| {
                s.parse::<usize>()
                    .ok()
            });
        if seq != 0 || header.len() != 128 || fields[0].is_empty() || advertised != Some(len) {
            return Err(Error::Ymodem(
                "WS53 upload filename/length header mismatch".into(),
            ));
        }
        self.port
            .write_all_bytes(&[6, b'C'])?;
        let mut data = Vec::with_capacity(len);
        let mut expected = 1u8;
        let mut previous: Option<(u8, Vec<u8>)> = None;
        while let Some((seq, payload)) = self.packet(false)? {
            if previous
                .as_ref()
                .is_some_and(|p| p.0 == seq && p.1 == payload)
            {
                self.port
                    .write_all_bytes(&[6])?;
                continue;
            }
            if seq != expected || data.len() == len {
                return Err(Error::Ymodem("WS53 upload sequence/size mismatch".into()));
            }
            data.extend_from_slice(
                &payload[..payload
                    .len()
                    .min(len - data.len())],
            );
            expected = expected.wrapping_add(1);
            previous = Some((seq, payload));
            self.port
                .write_all_bytes(&[6])?;
        }
        if data.len() != len {
            return Err(Error::Ymodem("truncated WS53 upload".into()));
        }
        // WifiBurnCtrl::HandleUploadInitAck: WS53 EOT response is C, not ACK+C.
        self.port
            .write_all_bytes(b"C")?;
        let end = self.packet(false)?;
        // Empty filename ends YMODEM. This SDK loader clears only the first
        // 128 bytes of the 133-byte packet, leaving three padding bytes from
        // its previous data block; those are covered by CRC but not metadata.
        if !matches!(end, Some((0, ref payload)) if payload.len() == 128 && payload[0] == 0) {
            return Err(Error::Ymodem("invalid WS53 upload finish block".into()));
        }
        self.port
            .write_all_bytes(&[6])?;
        self.ack()?;
        Ok(data)
    }
}

/// Read a bounded Flash window through an already running WS53 LoaderBoot.
///
/// Addresses are physical Flash offsets in 0..0x400000 (not CPU addresses); this
/// development profile only supports 1..256 KiB per transaction. It sends B4
/// and YMODEM controls, never erase/write/protection commands. Every ACK and
/// packet CRC, advertised length, sequence and terminator must pass. Failed
/// transfers return no data; reset/reconnect before retrying. Device identity
/// and the LoaderBoot package must have been independently qualified by caller.
pub fn read_flash<P: Port>(
    port: &mut P,
    address: u32,
    length: u32,
    ready: RomHandshake,
    cancel: &CancelContext,
) -> Result<Vec<u8>> {
    if length == 0
        || length > 0x40000
        || address % 4 != 0
        || address
            .checked_add(length)
            .is_none_or(|end| end > 0x400000)
    {
        return Err(Error::Config(
            "WS53 upload outside qualified 4 MiB Flash window / 256 KiB chunk".into(),
        ));
    }
    cancel.check()?;
    let saved = port.timeout();
    let result = (|| {
        port.set_timeout(Duration::from_millis(20))?;
        let mut command = vec![0xef, 0xbe, 0xad, 0xde, 18, 0, 0xb4, 0x4b];
        command.extend_from_slice(&length.to_le_bytes());
        command.extend_from_slice(&address.to_le_bytes());
        command.extend_from_slice(&crc16_xmodem(&command).to_le_bytes());
        port.write_all_bytes(&command)?;
        let mut receiver = Receiver {
            port,
            cancel,
            pending: ready
                .trailing
                .into(),
            deadline: Instant::now() + Duration::from_secs(15 + u64::from(length) / 4096),
        };
        receiver.transfer(length as usize)
    })();
    let restore = port.set_timeout(saved);
    match (result, restore) {
        (Ok(data), Ok(())) => Ok(data),
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
    }
}
