//! WS63 boot protocol implementation.
//!
//! This module implements the HiSilicon boot protocol used by the WS63 chip.
//!
//! ## Frame Format
//!
//! ```text
//! +------------+--------+-----+------+---------------+--------+
//! |   Magic    | Length | CMD | SCMD |     Data      | CRC16  |
//! +------------+--------+-----+------+---------------+--------+
//! |   4 bytes  | 2 bytes| 1   | 1    |   variable    | 2 bytes|
//! +------------+--------+-----+------+---------------+--------+
//! | 0xDEADBEEF |  total | cmd | ~cmd |   payload     | CRC    |
//! +------------+--------+-----+------+---------------+--------+
//! ```

use {
    crate::protocol::crc::crc16_xmodem,
    byteorder::{LittleEndian, WriteBytesExt},
};

/// Frame magic number.
pub const FRAME_MAGIC: u32 = 0xDEADBEEF;

/// Default initial baud rate for handshake.
pub const DEFAULT_BAUD: u32 = 115200;

/// High-speed baud rate after handshake.
pub const HIGH_BAUD: u32 = 921600;

/// Canonical first 10 bytes of a successful handshake response.
///
/// Some ROM revisions return `0xA5` instead of `0x00` in the final status byte;
/// matching therefore uses only the first nine bytes and validates the full CRC.
pub const HANDSHAKE_ACK: [u8; 10] = [
    0xEF, 0xBE, 0xAD, 0xDE, // Magic (little-endian)
    0x0C, 0x00, // Length = 12
    0xE1, 0x1E, // CMD = 0xE1, SCMD = 0x1E (swapped 0x0F)
    0x5A, 0x00, // ACK = 0x5A (success)
];

/// Total length of a WS63 handshake ACK frame, including its CRC16.
pub const HANDSHAKE_ACK_FRAME_LEN: usize = 12;

const HANDSHAKE_ACK_FIXED_PREFIX_LEN: usize = 9;

/// WS63 command types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Command {
    /// Handshake command (establish connection).
    Handshake = 0xF0,
    /// Set baud rate command.
    SetBaudRate = 0x5A,
    /// Download/erase command.
    Download = 0xD2,
    /// Reset command.
    Reset = 0x87,
}

impl Command {
    /// Get the swapped command byte (SCMD).
    /// SCMD = (CMD << 4) | (CMD >> 4)
    pub fn swapped(self) -> u8 {
        let cmd = self as u8;
        cmd.rotate_right(4)
    }
}

/// Command frame builder.
#[derive(Debug)]
pub struct CommandFrame {
    cmd: Command,
    data: Vec<u8>,
}

impl CommandFrame {
    /// Create a new command frame.
    pub fn new(cmd: Command) -> Self {
        Self {
            cmd,
            data: Vec::new(),
        }
    }

    /// Create a handshake command frame.
    ///
    /// # Arguments
    ///
    /// * `baud` - The baud rate to use for communication.
    #[allow(clippy::unwrap_used)] // Writing to Vec<u8> cannot fail
    pub fn handshake(baud: u32) -> Self {
        let mut frame = Self::new(Command::Handshake);
        frame
            .data
            .write_u32::<LittleEndian>(baud)
            .unwrap();
        frame
            .data
            .write_u32::<LittleEndian>(0x0108)
            .unwrap(); // Magic constant
        frame
    }

    /// Create a set baud rate command frame.
    #[allow(clippy::unwrap_used)] // Writing to Vec<u8> cannot fail
    pub fn set_baud_rate(baud: u32) -> Self {
        let mut frame = Self::new(Command::SetBaudRate);
        frame
            .data
            .write_u32::<LittleEndian>(baud)
            .unwrap();
        frame
            .data
            .write_u32::<LittleEndian>(0x0108)
            .unwrap();
        frame
    }

    /// Create a download command frame.
    ///
    /// # Arguments
    ///
    /// * `addr` - Flash address to write to.
    /// * `len` - Data length.
    /// * `erase_size` - Size to erase (0xFFFFFFFF for full erase).
    #[allow(clippy::unwrap_used)] // Writing to Vec<u8> cannot fail
    pub fn download(addr: u32, len: u32, erase_size: u32) -> Self {
        let mut frame = Self::new(Command::Download);
        frame
            .data
            .write_u32::<LittleEndian>(addr)
            .unwrap();
        frame
            .data
            .write_u32::<LittleEndian>(len)
            .unwrap();
        frame
            .data
            .write_u32::<LittleEndian>(erase_size)
            .unwrap();
        frame
            .data
            .extend_from_slice(&[0x00, 0xFF]); // Constant bytes
        frame
    }

    /// Create an erase-all command frame.
    pub fn erase_all() -> Self {
        Self::download(0, 0, 0xFFFFFFFF)
    }

    /// Create a reset command frame.
    pub fn reset() -> Self {
        let mut frame = Self::new(Command::Reset);
        frame
            .data
            .extend_from_slice(&[0x00, 0x00]);
        frame
    }

    /// Build the complete frame data.
    #[allow(clippy::cast_possible_truncation)]
    #[allow(clippy::unwrap_used)] // Writing to Vec<u8> cannot fail
    pub fn build(&self) -> Vec<u8> {
        // Total length = Magic(4) + Len(2) + CMD(1) + SCMD(1) + Data + CRC(2)
        let total_len = 10
            + self
                .data
                .len();
        let mut buf = Vec::with_capacity(total_len);

        // Magic (little-endian)
        buf.write_u32::<LittleEndian>(FRAME_MAGIC)
            .unwrap();

        // Length - safe cast, frame size < 64KB
        buf.write_u16::<LittleEndian>(total_len as u16)
            .unwrap();

        // CMD + SCMD
        buf.push(self.cmd as u8);
        buf.push(
            self.cmd
                .swapped(),
        );

        // Data
        buf.extend_from_slice(&self.data);

        // CRC16 (calculated over everything before CRC)
        let crc = crc16_xmodem(&buf);
        buf.write_u16::<LittleEndian>(crc)
            .unwrap();

        buf
    }

    /// Get the command type.
    pub fn command(&self) -> Command {
        self.cmd
    }
}

/// Response frame parser.
#[derive(Debug)]
pub struct ResponseFrame {
    /// Command byte from response.
    pub cmd: u8,
    /// Sub-command byte from response.
    pub scmd: u8,
    /// Response data.
    pub data: Vec<u8>,
}

impl ResponseFrame {
    /// Parse a response frame from raw data.
    ///
    /// Returns `None` if the data is not a valid frame.
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < 10 {
            return None;
        }

        // Find magic
        let magic_pos = data
            .windows(4)
            .position(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]) == FRAME_MAGIC)?;

        let frame = &data[magic_pos..];
        if frame.len() < 10 {
            return None;
        }

        let len = u16::from_le_bytes([frame[4], frame[5]]) as usize;
        if frame.len() < len {
            return None;
        }

        let cmd = frame[6];
        let scmd = frame[7];
        let data = frame[8..len - 2].to_vec();

        Some(Self { cmd, scmd, data })
    }

    /// Check if this is a successful handshake ACK.
    pub fn is_handshake_ack(&self) -> bool {
        // CMD = 0xE1 (response to 0x0F), first data byte = 0x5A (ACK)
        self.cmd == 0xE1
            && !self
                .data
                .is_empty()
            && self.data[0] == 0x5A
    }

    /// Check if this is a successful ACK response.
    pub fn is_ack(&self) -> bool {
        !self
            .data
            .is_empty()
            && self.data[0] == 0x5A
    }
}

/// Find a complete, CRC-valid handshake success ACK in a noisy byte stream.
///
/// Byte 9 is a device status/error-code field. Captured WS63 ROMs return both
/// `0x00` and `0xA5` there even when byte 8 reports success (`0x5A`), so it must
/// not be part of the fixed prefix match.
pub(super) fn find_handshake_ack(data: &[u8]) -> Option<(usize, usize)> {
    data.windows(HANDSHAKE_ACK_FRAME_LEN)
        .enumerate()
        .find_map(|(start, frame)| {
            if frame[..HANDSHAKE_ACK_FIXED_PREFIX_LEN]
                != HANDSHAKE_ACK[..HANDSHAKE_ACK_FIXED_PREFIX_LEN]
            {
                return None;
            }

            let expected_crc = crc16_xmodem(&frame[..HANDSHAKE_ACK_FRAME_LEN - 2]);
            let received_crc = u16::from_le_bytes([
                frame[HANDSHAKE_ACK_FRAME_LEN - 2],
                frame[HANDSHAKE_ACK_FRAME_LEN - 1],
            ]);
            (expected_crc == received_crc).then_some((start, start + HANDSHAKE_ACK_FRAME_LEN))
        })
}

/// Check if data contains a complete, CRC-valid handshake success ACK.
pub fn contains_handshake_ack(data: &[u8]) -> bool {
    find_handshake_ack(data).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_command_swapped() {
        assert_eq!(Command::Handshake.swapped(), 0x0F);
        assert_eq!(Command::SetBaudRate.swapped(), 0xA5);
        assert_eq!(Command::Download.swapped(), 0x2D);
        assert_eq!(Command::Reset.swapped(), 0x78);
    }

    #[test]
    fn test_handshake_frame() {
        let frame = CommandFrame::handshake(115200);
        let data = frame.build();

        // Check magic
        assert_eq!(&data[0..4], &[0xEF, 0xBE, 0xAD, 0xDE]);

        // Check CMD and SCMD
        assert_eq!(data[6], 0xF0);
        assert_eq!(data[7], 0x0F);
    }

    #[test]
    fn test_download_frame() {
        let frame = CommandFrame::download(0x00800000, 0x1000, 0x1000);
        let data = frame.build();

        assert_eq!(&data[0..4], &[0xEF, 0xBE, 0xAD, 0xDE]);
        assert_eq!(data[6], 0xD2);
        assert_eq!(data[7], 0x2D);
    }

    #[test]
    fn test_erase_all_frame() {
        let frame = CommandFrame::erase_all();
        let data = frame.build();

        // Check that erase_size is 0xFFFFFFFF
        // Data layout: addr(4) + len(4) + erase_size(4) + const(2)
        // erase_size is at offset 8 + 8 = 16
        assert_eq!(&data[16..20], &[0xFF, 0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn test_contains_handshake_ack() {
        let mut ack = HANDSHAKE_ACK.to_vec();
        let crc = crate::protocol::crc::crc16_xmodem(&ack);
        ack.extend_from_slice(&crc.to_le_bytes());

        // Should find ACK in exact match
        assert!(contains_handshake_ack(&ack));

        // Should find ACK with garbage before/after
        let mut data = vec![0x00, 0x00];
        data.extend_from_slice(&ack);
        data.extend_from_slice(&[0x00, 0x00]);
        assert!(contains_handshake_ack(&data));

        // Captured ROM response: success result 0x5A with status byte 0xA5.
        assert!(contains_handshake_ack(&[
            0xEF, 0xBE, 0xAD, 0xDE, 0x0C, 0x00, 0xE1, 0x1E, 0x5A, 0xA5, 0xDA, 0xC7,
        ]));

        // A matching prefix with a corrupt CRC is not a valid ACK.
        let mut corrupt = ack;
        corrupt[HANDSHAKE_ACK_FRAME_LEN - 1] ^= 0xFF;
        assert!(!contains_handshake_ack(&corrupt));

        // Should not find ACK in random data
        assert!(!contains_handshake_ack(&[0x00; 20]));
    }

    #[test]
    fn test_response_frame_parse_handshake_ack() {
        // Build a valid response frame: magic + len(12) + cmd(0xE1) + scmd(0x1E) +
        // data(0x5A, 0x00) + crc
        let mut buf = Vec::new();
        buf.extend_from_slice(&FRAME_MAGIC.to_le_bytes());
        buf.extend_from_slice(&12u16.to_le_bytes()); // len
        buf.push(0xE1); // cmd
        buf.push(0x1E); // scmd
        buf.push(0x5A); // ACK success
        buf.push(0x00); // error code
        let crc = crate::protocol::crc::crc16_xmodem(&buf);
        buf.extend_from_slice(&crc.to_le_bytes());

        let resp = ResponseFrame::parse(&buf);
        assert!(resp.is_some());
        let resp = resp.unwrap();
        assert!(resp.is_handshake_ack());
        assert!(resp.is_ack());
        assert_eq!(resp.cmd, 0xE1);
        assert_eq!(resp.scmd, 0x1E);
    }

    #[test]
    fn test_response_frame_parse_failure() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&FRAME_MAGIC.to_le_bytes());
        buf.extend_from_slice(&12u16.to_le_bytes());
        buf.push(0xE1);
        buf.push(0x1E);
        buf.push(0x00); // Not ACK
        buf.push(0x01); // error code
        let crc = crate::protocol::crc::crc16_xmodem(&buf);
        buf.extend_from_slice(&crc.to_le_bytes());

        let resp = ResponseFrame::parse(&buf).unwrap();
        assert!(!resp.is_ack());
        assert!(!resp.is_handshake_ack());
    }

    #[test]
    fn test_response_frame_parse_too_short() {
        assert!(ResponseFrame::parse(&[0; 5]).is_none());
    }

    #[test]
    fn test_response_frame_parse_no_magic() {
        let data = vec![0x00; 20];
        assert!(ResponseFrame::parse(&data).is_none());
    }

    #[test]
    fn test_response_frame_parse_with_prefix() {
        let mut buf = vec![0xFF; 3];
        buf.extend_from_slice(&FRAME_MAGIC.to_le_bytes());
        buf.extend_from_slice(&12u16.to_le_bytes());
        buf.push(0xE1);
        buf.push(0x1E);
        buf.push(0x5A);
        buf.push(0x00);
        let crc = crate::protocol::crc::crc16_xmodem(&buf[3..]);
        buf.extend_from_slice(&crc.to_le_bytes());

        let resp = ResponseFrame::parse(&buf);
        assert!(resp.is_some());
    }

    #[test]
    fn test_command_frame_command_getter() {
        let frame = CommandFrame::handshake(115200);
        assert_eq!(frame.command(), Command::Handshake);

        let frame = CommandFrame::reset();
        assert_eq!(frame.command(), Command::Reset);

        let frame = CommandFrame::set_baud_rate(921600);
        assert_eq!(frame.command(), Command::SetBaudRate);

        let frame = CommandFrame::download(0, 0, 0);
        assert_eq!(frame.command(), Command::Download);
    }

    #[test]
    fn test_reset_frame_structure() {
        let frame = CommandFrame::reset();
        let data = frame.build();
        assert_eq!(data[6], Command::Reset as u8);
        assert_eq!(data[7], Command::Reset.swapped());
        // Total: magic(4) + len(2) + cmd(1) + scmd(1) + data(2) + crc(2) = 12
        assert_eq!(data.len(), 12);
    }

    #[test]
    fn test_frame_magic_bytes() {
        let frame = CommandFrame::handshake(115200);
        let data = frame.build();
        // Little-endian 0xDEADBEEF = EF BE AD DE
        assert_eq!(&data[0..4], &[0xEF, 0xBE, 0xAD, 0xDE]);
    }

    #[test]
    fn test_frame_length_field_matches_actual() {
        let frame = CommandFrame::handshake(115200);
        let data = frame.build();
        let len_field = u16::from_le_bytes([data[4], data[5]]) as usize;
        assert_eq!(len_field, data.len());
    }

    #[test]
    fn test_constants() {
        assert_eq!(FRAME_MAGIC, 0xDEADBEEF);
        assert_eq!(DEFAULT_BAUD, 115200);
        assert_eq!(HIGH_BAUD, 921600);
        assert_eq!(HANDSHAKE_ACK.len(), 10);
        assert_eq!(HANDSHAKE_ACK_FRAME_LEN, 12);
    }
}
