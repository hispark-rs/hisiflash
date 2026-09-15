//! Chip/target abstraction for supporting multiple HiSilicon chips.
//!
//! This module provides a trait-based abstraction for different chip families,
//! allowing the same codebase to support WS63, BS2X, and other HiSilicon chips.

use {
    crate::{
        error::{Error, Result},
        image::fwpkg::Fwpkg,
        port::{Port, SerialConfig},
    },
    std::fmt,
};

/// Supported chip families.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ChipFamily {
    /// WS63 series (WiFi + BLE).
    #[default]
    Ws63,
    /// BS2X series (BS21, BS25, etc. - BLE only).
    Bs2x,
    /// BS25 specific.
    Bs25,
    /// WS53 series.
    Ws53,
    /// SW39 series.
    Sw39,
    /// Generic HiSilicon (unknown specific type).
    Generic,
}

impl ChipFamily {
    /// Get default baud rate for this chip family.
    #[must_use]
    pub fn default_baud(&self) -> u32 {
        // All chips currently use 115200 as default
        115200
    }

    /// Get high-speed baud rate for this chip family.
    #[must_use]
    pub fn high_speed_baud(&self) -> u32 {
        match self {
            Self::Bs2x | Self::Bs25 => 2_000_000,
            _ => 921_600,
        }
    }

    /// Get recommended flash baud rate for this chip family.
    ///
    /// BS2X/BS25 chips use 460800 as the recommended rate because CH340/CH341
    /// USB-serial adapters (commonly used with these chips) are unreliable at
    /// 921600 baud, causing YMODEM transfer failures around the 2KB mark.
    /// WS63 and other chips use 921600 for maximum throughput.
    #[must_use]
    pub fn recommended_flash_baud(&self) -> u32 {
        match self {
            Self::Bs2x | Self::Bs25 => 460_800,
            _ => 921_600,
        }
    }

    /// Get supported baud rates for this chip family.
    #[must_use]
    pub fn supported_bauds(&self) -> &'static [u32] {
        match self {
            Self::Bs2x | Self::Bs25 => &[115_200, 230_400, 460_800, 921_600, 2_000_000],
            _ => &[115_200, 230_400, 460_800, 921_600],
        }
    }

    /// Check if this chip family supports USB DFU mode.
    pub fn supports_usb_dfu(&self) -> bool {
        matches!(self, Self::Bs2x | Self::Bs25)
    }

    /// Check if this chip family supports eFuse operations.
    pub fn supports_efuse(&self) -> bool {
        true // All HiSilicon chips support eFuse
    }

    fn uses_at_reset_before_connect(self) -> bool {
        matches!(self, Self::Ws63)
    }

    /// Check if this chip family requires signed firmware.
    pub fn requires_signed_firmware(&self) -> bool {
        // Some chips require signed firmware for security
        matches!(self, Self::Ws63 | Self::Bs2x | Self::Bs25)
    }

    /// Get the chip family from a string name.
    pub fn from_name(name: &str) -> Option<Self> {
        match name
            .to_lowercase()
            .as_str()
        {
            "ws63" => Some(Self::Ws63),
            "bs2x" | "bs21" => Some(Self::Bs2x),
            "bs25" => Some(Self::Bs25),
            "ws53" => Some(Self::Ws53),
            "sw39" => Some(Self::Sw39),
            "generic" | "auto" => Some(Self::Generic),
            _ => None,
        }
    }
}

impl fmt::Display for ChipFamily {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ws63 => write!(f, "WS63"),
            Self::Bs2x => write!(f, "BS2X"),
            Self::Bs25 => write!(f, "BS25"),
            Self::Ws53 => write!(f, "WS53"),
            Self::Sw39 => write!(f, "SW39"),
            Self::Generic => write!(f, "Generic"),
        }
    }
}

/// Chip configuration parameters.
#[derive(Debug, Clone)]
pub struct ChipConfig {
    /// Chip family.
    pub family: ChipFamily,
    /// Initial baud rate for handshake.
    pub init_baud: u32,
    /// Target baud rate for data transfer.
    pub target_baud: u32,
    /// Use late baud rate switch (after loaderboot).
    pub late_baud_switch: bool,
    /// Handshake timeout in seconds.
    pub handshake_timeout_secs: u32,
    /// Data transfer timeout in seconds.
    pub transfer_timeout_secs: u32,
}

impl ChipConfig {
    /// Create a new chip configuration for the given family.
    pub fn new(family: ChipFamily) -> Self {
        Self {
            family,
            init_baud: family.default_baud(),
            target_baud: family.high_speed_baud(),
            late_baud_switch: false,
            handshake_timeout_secs: 30,
            transfer_timeout_secs: 60,
        }
    }

    /// Set the target baud rate.
    #[must_use]
    pub fn with_baud(mut self, baud: u32) -> Self {
        self.target_baud = baud;
        self
    }

    /// Enable late baud rate switching.
    #[must_use]
    pub fn with_late_baud(mut self, late: bool) -> Self {
        self.late_baud_switch = late;
        self
    }

    /// Set handshake timeout.
    #[must_use]
    pub fn with_handshake_timeout(mut self, secs: u32) -> Self {
        self.handshake_timeout_secs = secs;
        self
    }
}

impl Default for ChipConfig {
    fn default() -> Self {
        Self::new(ChipFamily::default())
    }
}

/// Trait for flashing operations across all chip families.
///
/// This trait provides a unified interface for flashing firmware,
/// allowing the CLI to work with any chip family through a common API.
pub trait Flasher {
    /// Connect to the device and perform handshake.
    fn connect(&mut self) -> Result<()>;

    /// Flash a complete FWPKG firmware package.
    ///
    /// # Arguments
    ///
    /// * `fwpkg` - The firmware package to flash
    /// * `filter` - Optional filter for partition names (None = flash all)
    /// * `progress` - Progress callback (partition_name, current_bytes,
    ///   total_bytes)
    fn flash_fwpkg(
        &mut self,
        fwpkg: &Fwpkg,
        filter: Option<&[&str]>,
        progress: &mut dyn FnMut(&str, usize, usize),
    ) -> Result<()>;

    /// Flash raw binary files.
    fn write_bins(&mut self, loaderboot: &[u8], bins: &[(&[u8], u32)]) -> Result<()>;

    /// Erase entire flash.
    fn erase_all(&mut self) -> Result<()>;

    /// Reset the device.
    fn reset(&mut self) -> Result<()>;

    /// Get the connection baud rate.
    fn connection_baud(&self) -> u32;

    /// Get the target transfer baud rate (if different from connection).
    fn target_baud(&self) -> Option<u32>;

    /// Close the flasher and release resources.
    ///
    /// This method ensures the serial port is properly closed.
    /// It is safe to call even if the connection is not active.
    /// After calling this method, the flasher cannot be used.
    fn close(&mut self);

    /// Hand off the underlying serial port to a [`crate::monitor::MonitorSession`].
    ///
    /// Consumes the flasher and re-purposes its open serial handle for the
    /// monitor without going through close/reopen, which would otherwise
    /// drop the early bootlog the chip emits right after [`Self::reset`].
    ///
    /// `baud_rate` is the operating-mode baud rate (typically 115200) the
    /// device will speak after reboot, and it will be applied to the handle
    /// before returning.
    ///
    /// The default implementation returns [`Error::Unsupported`]. Concrete
    /// flashers backed by a real serial port should override.
    ///
    /// Only available with the `native` feature.
    #[cfg(feature = "native")]
    fn into_monitor(self: Box<Self>, _baud_rate: u32) -> Result<crate::monitor::MonitorSession> {
        Err(crate::error::Error::Unsupported(
            "Flasher does not support monitor handoff".into(),
        ))
    }
}

impl ChipFamily {
    /// Create a flasher instance for this chip family (native platforms).
    ///
    /// This is the main entry point for creating chip-specific flashers.
    ///
    /// # Arguments
    ///
    /// * `port_name` - Serial port name (e.g., "/dev/ttyUSB0")
    /// * `target_baud` - Target baud rate for data transfer
    /// * `late_baud` - Use late baud rate switch (after LoaderBoot)
    /// * `verbose` - Verbose output level
    ///
    /// # Returns
    ///
    /// A boxed flasher instance implementing the `Flasher` trait
    #[cfg(feature = "native")]
    pub fn create_flasher(
        &self,
        port_name: &str,
        target_baud: u32,
        late_baud: bool,
        verbose: u8,
    ) -> Result<Box<dyn Flasher>> {
        match self {
            Self::Ws63 | Self::Bs2x | Self::Bs25 => {
                // WS63/BS2X/BS25 currently share the same serial SEBOOT/YMODEM
                // transport implementation. Chip-specific quirks are handled in
                // the shared protocol layer.
                let flasher = super::ws63::flasher::Ws63Flasher::open(port_name, target_baud)?
                    .with_late_baud(late_baud)
                    .with_at_reset(self.uses_at_reset_before_connect())
                    .with_finish_without_c(!matches!(self, Self::Bs2x | Self::Bs25))
                    .with_verbose(verbose);
                Ok(Box::new(flasher))
            },
            Self::Ws53 | Self::Sw39 => Err(Error::Unsupported(format!(
                "{self} series support coming soon"
            ))),
            Self::Generic => Err(Error::Unsupported(
                "Cannot create flasher for generic chip family".into(),
            )),
        }
    }

    /// Create a flasher with an existing port (generic, works for any Port
    /// type).
    ///
    /// This is useful for testing or custom port implementations.
    #[cfg(feature = "native")]
    pub fn create_flasher_with_port<P: Port + 'static>(
        &self,
        port: P,
        target_baud: u32,
        late_baud: bool,
        verbose: u8,
    ) -> Result<Box<dyn Flasher>> {
        self.create_flasher_with_port_and_cancel(
            port,
            target_baud,
            late_baud,
            verbose,
            crate::CancelContext::none(),
        )
    }

    /// Create a flasher with an existing port and explicit cancel context.
    ///
    /// This is the recommended way to create a flasher when you want to
    /// support cancellation (Ctrl-C) from the embedding application.
    #[cfg(feature = "native")]
    pub fn create_flasher_with_port_and_cancel<P: Port + 'static>(
        &self,
        port: P,
        target_baud: u32,
        late_baud: bool,
        verbose: u8,
        cancel: crate::CancelContext,
    ) -> Result<Box<dyn Flasher>> {
        match self {
            Self::Ws63 | Self::Bs2x | Self::Bs25 => {
                let flasher =
                    super::ws63::flasher::Ws63Flasher::with_cancel(port, target_baud, cancel)
                        .with_late_baud(late_baud)
                        .with_at_reset(self.uses_at_reset_before_connect())
                        .with_finish_without_c(!matches!(self, Self::Bs2x | Self::Bs25))
                        .with_verbose(verbose);
                Ok(Box::new(flasher))
            },
            _ => Err(Error::Unsupported(format!(
                "Unsupported chip family for generic port: {self}"
            ))),
        }
    }

    /// Create a flasher with full serial configuration (P0: 完整配置支持).
    ///
    /// This allows customization of all serial port parameters including
    /// baud rate, data bits, parity, stop bits, and flow control.
    ///
    /// # Arguments
    ///
    /// * `config` - Serial port configuration
    /// * `late_baud` - Use late baud rate switch (after LoaderBoot)
    /// * `verbose` - Verbose output level
    ///
    /// # Returns
    ///
    /// A boxed flasher instance implementing the `Flasher` trait
    #[cfg(feature = "native")]
    pub fn create_flasher_with_config(
        &self,
        config: SerialConfig,
        late_baud: bool,
        verbose: u8,
    ) -> Result<Box<dyn Flasher>> {
        match self {
            Self::Ws63 | Self::Bs2x | Self::Bs25 => {
                let flasher = super::ws63::flasher::Ws63Flasher::open_with_config(config)?
                    .with_late_baud(late_baud)
                    .with_at_reset(self.uses_at_reset_before_connect())
                    .with_finish_without_c(!matches!(self, Self::Bs2x | Self::Bs25))
                    .with_verbose(verbose);
                Ok(Box::new(flasher))
            },
            Self::Ws53 | Self::Sw39 => Err(Error::Unsupported(format!(
                "{self} series support coming soon"
            ))),
            Self::Generic => Err(Error::Unsupported(
                "Cannot create flasher for generic chip family".into(),
            )),
        }
    }
}

/// Trait for chip-specific implementations.
///
/// This trait allows different chip families to have custom behavior
/// while sharing common flashing logic.
pub trait ChipOps {
    /// Get the chip family.
    fn family(&self) -> ChipFamily;

    /// Get the chip configuration.
    fn config(&self) -> &ChipConfig;

    /// Prepare a binary for flashing (e.g., add signing header).
    fn prepare_binary(&self, data: &[u8], _addr: u32) -> Result<Vec<u8>> {
        // Default: return data unchanged
        Ok(data.to_vec())
    }

    /// Check if a binary needs signing.
    fn needs_signing(&self, _addr: u32) -> bool {
        false
    }

    /// Get the flash base address for this chip.
    fn flash_base(&self) -> u32 {
        0x00000000
    }

    /// Get the maximum flash size for this chip.
    fn flash_size(&self) -> u32 {
        0x00800000 // 8MB default
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chip_family_from_name() {
        assert_eq!(ChipFamily::from_name("ws63"), Some(ChipFamily::Ws63));
        assert_eq!(ChipFamily::from_name("BS2X"), Some(ChipFamily::Bs2x));
        assert_eq!(ChipFamily::from_name("bs21"), Some(ChipFamily::Bs2x));
        assert_eq!(ChipFamily::from_name("bs25"), Some(ChipFamily::Bs25));
        assert_eq!(ChipFamily::from_name("ws53"), Some(ChipFamily::Ws53));
        assert_eq!(ChipFamily::from_name("sw39"), Some(ChipFamily::Sw39));
        assert_eq!(ChipFamily::from_name("generic"), Some(ChipFamily::Generic));
        assert_eq!(ChipFamily::from_name("auto"), Some(ChipFamily::Generic));
        assert_eq!(ChipFamily::from_name("unknown"), None);
        assert_eq!(ChipFamily::from_name(""), None);
    }

    #[test]
    fn test_chip_family_from_name_case_insensitive() {
        assert_eq!(ChipFamily::from_name("WS63"), Some(ChipFamily::Ws63));
        assert_eq!(ChipFamily::from_name("Ws63"), Some(ChipFamily::Ws63));
        assert_eq!(ChipFamily::from_name("BS25"), Some(ChipFamily::Bs25));
    }

    #[test]
    fn test_chip_config_defaults() {
        let config = ChipConfig::new(ChipFamily::Ws63);
        assert_eq!(config.init_baud, 115200);
        assert_eq!(config.target_baud, 921600);
        assert!(!config.late_baud_switch);
        assert_eq!(config.handshake_timeout_secs, 30);
        assert_eq!(config.transfer_timeout_secs, 60);
    }

    #[test]
    fn test_chip_config_bs2x_defaults() {
        let config = ChipConfig::new(ChipFamily::Bs2x);
        assert_eq!(config.init_baud, 115200);
        assert_eq!(config.target_baud, 2_000_000);
    }

    #[test]
    fn test_chip_config_builder() {
        let config = ChipConfig::new(ChipFamily::Ws63)
            .with_baud(460800)
            .with_late_baud(true)
            .with_handshake_timeout(10);
        assert_eq!(config.target_baud, 460800);
        assert!(config.late_baud_switch);
        assert_eq!(config.handshake_timeout_secs, 10);
    }

    #[test]
    fn test_chip_config_default_trait() {
        let config = ChipConfig::default();
        assert_eq!(config.family, ChipFamily::Ws63); // Default is Ws63
    }

    #[test]
    fn test_chip_family_default() {
        let family = ChipFamily::default();
        assert_eq!(family, ChipFamily::Ws63);
    }

    #[test]
    fn test_at_reset_is_ws63_only() {
        assert!(ChipFamily::Ws63.uses_at_reset_before_connect());
        assert!(!ChipFamily::Bs2x.uses_at_reset_before_connect());
        assert!(!ChipFamily::Bs25.uses_at_reset_before_connect());
    }

    #[test]
    fn test_chip_family_display() {
        assert_eq!(ChipFamily::Ws63.to_string(), "WS63");
        assert_eq!(ChipFamily::Bs2x.to_string(), "BS2X");
        assert_eq!(ChipFamily::Bs25.to_string(), "BS25");
        assert_eq!(ChipFamily::Ws53.to_string(), "WS53");
        assert_eq!(ChipFamily::Sw39.to_string(), "SW39");
        assert_eq!(ChipFamily::Generic.to_string(), "Generic");
    }

    #[test]
    fn test_chip_family_default_baud() {
        // All chips use 115200 as default
        for family in [
            ChipFamily::Ws63,
            ChipFamily::Bs2x,
            ChipFamily::Bs25,
            ChipFamily::Generic,
        ] {
            assert_eq!(family.default_baud(), 115200, "Failed for {family}");
        }
    }

    #[test]
    fn test_chip_family_high_speed_baud() {
        assert_eq!(ChipFamily::Ws63.high_speed_baud(), 921_600);
        assert_eq!(ChipFamily::Bs2x.high_speed_baud(), 2_000_000);
        assert_eq!(ChipFamily::Bs25.high_speed_baud(), 2_000_000);
        assert_eq!(ChipFamily::Generic.high_speed_baud(), 921_600);
    }

    #[test]
    fn test_chip_family_supported_bauds() {
        let ws63_bauds = ChipFamily::Ws63.supported_bauds();
        assert!(ws63_bauds.contains(&115_200));
        assert!(ws63_bauds.contains(&921_600));
        assert!(!ws63_bauds.contains(&2_000_000));

        let bs2x_bauds = ChipFamily::Bs2x.supported_bauds();
        assert!(bs2x_bauds.contains(&2_000_000));
    }

    #[test]
    fn test_chip_family_usb_dfu() {
        assert!(!ChipFamily::Ws63.supports_usb_dfu());
        assert!(ChipFamily::Bs2x.supports_usb_dfu());
        assert!(ChipFamily::Bs25.supports_usb_dfu());
        assert!(!ChipFamily::Generic.supports_usb_dfu());
    }

    #[test]
    fn test_chip_family_efuse() {
        // All chips support eFuse
        for family in [
            ChipFamily::Ws63,
            ChipFamily::Bs2x,
            ChipFamily::Bs25,
            ChipFamily::Generic,
        ] {
            assert!(family.supports_efuse());
        }
    }

    #[test]
    fn test_chip_family_signed_firmware() {
        assert!(ChipFamily::Ws63.requires_signed_firmware());
        assert!(ChipFamily::Bs2x.requires_signed_firmware());
        assert!(ChipFamily::Bs25.requires_signed_firmware());
        assert!(!ChipFamily::Generic.requires_signed_firmware());
    }

    #[test]
    fn test_chip_family_clone_eq() {
        let a = ChipFamily::Ws63;
        let b = a;
        assert_eq!(a, b);

        let c = ChipFamily::Bs2x;
        assert_ne!(a, c);
    }

    #[test]
    fn test_chip_family_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(ChipFamily::Ws63);
        set.insert(ChipFamily::Bs2x);
        set.insert(ChipFamily::Ws63); // duplicate
        assert_eq!(set.len(), 2);
    }

    #[cfg(feature = "native")]
    #[test]
    fn test_create_flasher_supported_shared_seboot_chips() {
        let result = ChipFamily::Bs2x.create_flasher("/dev/null", 115200, false, 0);
        assert!(!matches!(result, Err(Error::Unsupported(_))));

        let result = ChipFamily::Bs25.create_flasher("/dev/null", 115200, false, 0);
        assert!(!matches!(result, Err(Error::Unsupported(_))));
    }

    #[cfg(feature = "native")]
    #[test]
    fn test_create_flasher_unsupported_chip() {
        let result = ChipFamily::Generic.create_flasher("/dev/null", 115200, false, 0);
        assert!(result.is_err());
    }
}
