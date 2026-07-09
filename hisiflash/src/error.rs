//! Error types for hisiflash.

use {std::io, thiserror::Error};

/// Result type for hisiflash operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Error type for hisiflash operations.
#[derive(Debug, Error)]
pub enum Error {
    /// I/O error (serial port, file operations).
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    /// Serial port error.
    #[error("Serial port error: {0}")]
    Serial(#[from] serialport::Error),

    /// Invalid firmware package format.
    #[error("Invalid FWPKG: {0}")]
    InvalidFwpkg(String),

    /// CRC checksum mismatch.
    #[error("CRC mismatch: expected {expected:#06x}, got {actual:#06x}")]
    CrcMismatch {
        /// Expected CRC value.
        expected: u16,
        /// Actual CRC value.
        actual: u16,
    },

    /// Communication timeout.
    #[error("Timeout: {0}")]
    Timeout(String),

    /// Device not responding or not in boot mode.
    #[error("Device not found or not in boot mode")]
    DeviceNotFound,

    /// Handshake failed.
    #[error("Handshake failed: {0}")]
    HandshakeFailed(String),

    /// Protocol error.
    #[error("Protocol error: {0}")]
    Protocol(String),

    /// YMODEM transfer error.
    #[error("YMODEM error: {0}")]
    Ymodem(String),

    /// Unsupported chip or operation.
    #[error("Unsupported: {0}")]
    Unsupported(String),

    /// Configuration error.
    #[error("Configuration error: {0}")]
    Config(String),
}

impl From<hisi_fwpkg::Error> for Error {
    fn from(value: hisi_fwpkg::Error) -> Self {
        match value {
            hisi_fwpkg::Error::Io(err) => Self::Io(err),
            other => Self::InvalidFwpkg(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display_messages() {
        let err = Error::InvalidFwpkg("bad magic".into());
        assert!(
            err.to_string()
                .contains("bad magic")
        );

        let err = Error::CrcMismatch {
            expected: 0x1234,
            actual: 0x5678,
        };
        let msg = err.to_string();
        assert!(msg.contains("1234"));
        assert!(msg.contains("5678"));

        let err = Error::Timeout("read timed out".into());
        assert!(
            err.to_string()
                .contains("read timed out")
        );

        let err = Error::DeviceNotFound;
        assert!(
            !err.to_string()
                .is_empty()
        );

        let err = Error::HandshakeFailed("no ack".into());
        assert!(
            err.to_string()
                .contains("no ack")
        );

        let err = Error::Protocol("invalid frame".into());
        assert!(
            err.to_string()
                .contains("invalid frame")
        );

        let err = Error::Ymodem("transfer aborted".into());
        assert!(
            err.to_string()
                .contains("transfer aborted")
        );

        let err = Error::Unsupported("bs2x".into());
        assert!(
            err.to_string()
                .contains("bs2x")
        );

        let err = Error::Config("missing field".into());
        assert!(
            err.to_string()
                .contains("missing field")
        );
    }

    #[test]
    fn test_error_from_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let err: Error = io_err.into();
        assert!(matches!(err, Error::Io(_)));
        assert!(
            err.to_string()
                .contains("file not found")
        );
    }

    #[test]
    fn test_error_is_send_sync() {
        fn assert_send<T: Send>() {}
        fn assert_sync<T: Sync>() {}
        assert_send::<Error>();
        assert_sync::<Error>();
    }
}
