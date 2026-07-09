//! WS63 flasher implementation.
//!
//! This module provides the main flasher interface for the WS63 chip.
//!
//! ## Generic Port Support
//!
//! The flasher uses a generic `Port` trait, allowing it to work with different
//! serial port implementations:
//!
//! - **Native platforms**: Uses the `serialport` crate via `NativePort`
//! - **WASM/Web**: Can use Web Serial API via `WebSerialPort` (experimental)
//!
//! ## Example
//!
//! ```rust,no_run
//! use hisiflash::{ChipFamily, Fwpkg};
//!
//! fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Create flasher using chip abstraction
//!     let mut flasher = ChipFamily::Ws63.create_flasher("/dev/ttyUSB0", 921600, false, 0)?;
//!
//!     // Connect to device
//!     flasher.connect()?;
//!
//!     // Flash firmware
//!     let fwpkg = Fwpkg::from_file("firmware.fwpkg")?;
//!     flasher.flash_fwpkg(&fwpkg, None, &mut |name, current, total| {
//!         println!("Flashing {}: {}/{}", name, current, total);
//!     })?;
//!
//!     Ok(())
//! }
//! ```

use {
    crate::{
        CancelContext,
        error::{Error, Result},
        image::fwpkg::Fwpkg,
        port::Port,
        protocol::ymodem::{YmodemConfig, YmodemTransfer},
        target::ws63::protocol::{CommandFrame, DEFAULT_BAUD, contains_handshake_ack},
    },
    log::{debug, info, trace, warn},
    std::{
        thread,
        time::{Duration, Instant},
    },
};

/// Timeout for waiting for handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// Delay after changing baud rate.
///
/// Increased to 300ms to give CH340/CH341 adapters enough time to stabilize
/// after a baud rate switch before YMODEM transfers begin.
const BAUD_CHANGE_DELAY: Duration = Duration::from_millis(300);

/// Delay between partition transfers to prevent serial data stale.
const PARTITION_DELAY: Duration = Duration::from_millis(100);

/// Timeout for waiting for SEBOOT magic response.
const MAGIC_TIMEOUT: Duration = Duration::from_secs(10);

/// BurnTool uses a longer common timeout after each YMODEM transfer completes,
/// because the target may spend noticeable time finalizing the flashed image
/// before emitting the next SEBOOT ACK.
const POST_TRANSFER_MAGIC_TIMEOUT: Duration = Duration::from_secs(15);

/// Delay between connection retry attempts.
const CONNECT_RETRY_DELAY: Duration = Duration::from_millis(500);

/// Maximum number of connection attempts.
const MAX_CONNECT_ATTEMPTS: usize = 7;

/// Maximum number of download retry attempts.
const MAX_DOWNLOAD_RETRIES: usize = 3;

const BAUD_FALLBACKS: &[u32] = &[921_600, 460_800, 230_400, 115_200];

fn lower_fallback_baud(current: u32) -> Option<u32> {
    BAUD_FALLBACKS
        .iter()
        .copied()
        .find(|baud| *baud < current)
}

fn is_interrupted_error(e: &Error) -> bool {
    match e {
        Error::Io(io) => {
            io.kind() == std::io::ErrorKind::Interrupted
                || io.raw_os_error() == Some(4)
                || io
                    .to_string()
                    .to_ascii_lowercase()
                    .contains("interrupted")
        },
        Error::Serial(serial) => {
            matches!(
                serial.kind(),
                serialport::ErrorKind::Io(std::io::ErrorKind::Interrupted)
            ) || serial
                .to_string()
                .to_ascii_lowercase()
                .contains("interrupted")
        },
        _ => e
            .to_string()
            .to_ascii_lowercase()
            .contains("interrupted"),
    }
}

fn sleep_interruptible(cancel: &CancelContext, total: Duration) -> Result<()> {
    const CHUNK: Duration = Duration::from_millis(20);

    let start = Instant::now();
    while start.elapsed() < total {
        cancel.check()?;
        let elapsed = start.elapsed();
        let remain = total.saturating_sub(elapsed);
        thread::sleep(remain.min(CHUNK));
    }

    Ok(())
}

/// WS63 flasher.
///
/// Generic over the port type `P`, which must implement the `Port` trait.
/// This allows the flasher to work with different serial port implementations.
pub struct Ws63Flasher<P: Port> {
    port: P,
    target_baud: u32,
    active_baud: u32,
    late_baud: bool,
    finish_without_c: bool,
    prefetched_magic_bytes: Vec<u8>,
    prefetched_ymodem_bytes: Vec<u8>,
    verbose: u8,
    cancel: CancelContext,
}

// Implementation for any Port type
impl<P: Port> Ws63Flasher<P> {
    /// Create a new WS63 flasher with an existing port.
    ///
    /// This flasher will NOT respond to Ctrl-C interrupts.
    /// For interruptible flasher, use [`with_cancel`](Self::with_cancel).
    ///
    /// # Arguments
    ///
    /// * `port` - An opened serial port implementing the `Port` trait
    /// * `target_baud` - Target baud rate for data transfer
    #[allow(dead_code)]
    pub fn new(port: P, target_baud: u32) -> Self {
        Self {
            port,
            target_baud,
            active_baud: DEFAULT_BAUD,
            late_baud: false,
            finish_without_c: true,
            prefetched_magic_bytes: Vec::new(),
            prefetched_ymodem_bytes: Vec::new(),
            verbose: 0,
            cancel: CancelContext::none(),
        }
    }

    /// Create a new WS63 flasher with custom cancel context.
    ///
    /// Use this when you need custom cancellation behavior (e.g., Ctrl-C support).
    ///
    /// # Arguments
    ///
    /// * `port` - An opened serial port implementing the `Port` trait
    /// * `target_baud` - Target baud rate for data transfer
    /// * `cancel` - Cancellation context for interruptible operations
    ///
    /// # Example
    ///
    /// ```ignore
    /// use hisiflash::CancelContext;
    ///
    /// // Create with global interrupt support
    /// let cancel = hisiflash::cancel_context_from_global();
    /// let flasher = Ws63Flasher::with_cancel(port, 921600, cancel);
    /// ```
    pub fn with_cancel(port: P, target_baud: u32, cancel: CancelContext) -> Self {
        Self {
            port,
            target_baud,
            active_baud: DEFAULT_BAUD,
            late_baud: false,
            finish_without_c: true,
            prefetched_magic_bytes: Vec::new(),
            prefetched_ymodem_bytes: Vec::new(),
            verbose: 0,
            cancel,
        }
    }

    /// Set late baud rate change mode.
    ///
    /// In late baud mode, the baud rate is changed after LoaderBoot is loaded,
    /// which may be necessary for some firmware configurations.
    #[must_use]
    pub fn with_late_baud(mut self, late_baud: bool) -> Self {
        self.late_baud = late_baud;
        self
    }

    /// Control whether YMODEM should send the finish block when EOT is ACKed
    /// without a trailing 'C'.
    #[must_use]
    pub fn with_finish_without_c(mut self, finish_without_c: bool) -> Self {
        self.finish_without_c = finish_without_c;
        self
    }

    /// Set verbose output level.
    #[must_use]
    pub fn with_verbose(mut self, verbose: u8) -> Self {
        self.verbose = verbose;
        self
    }

    /// Connect to the device.
    ///
    /// This waits for the device to boot into download mode and performs
    /// the initial handshake with retry mechanism.
    pub fn connect(&mut self) -> Result<()> {
        info!(
            "Waiting for device on {}...",
            self.port
                .name()
        );
        info!("Please reset the device to enter download mode.");

        for attempt in 1..=MAX_CONNECT_ATTEMPTS {
            self.cancel
                .check()?;

            if attempt > 1 {
                info!("Connection attempt {attempt}/{MAX_CONNECT_ATTEMPTS}");
            }

            match self.try_connect() {
                Ok(()) => {
                    return Ok(());
                },
                Err(e) => {
                    if is_interrupted_error(&e) {
                        return Err(e);
                    }

                    if attempt < MAX_CONNECT_ATTEMPTS {
                        warn!("Connection failed (attempt {attempt}/{MAX_CONNECT_ATTEMPTS}): {e}");
                        sleep_interruptible(&self.cancel, CONNECT_RETRY_DELAY)?;
                        self.port
                            .clear_buffers()?;
                    } else {
                        return Err(e);
                    }
                },
            }
        }

        Err(Error::Timeout(format!(
            "Connection failed after {MAX_CONNECT_ATTEMPTS} attempts"
        )))
    }

    /// Single connection attempt.
    fn try_connect(&mut self) -> Result<()> {
        self.cancel
            .check()?;

        self.port
            .clear_buffers()?;

        let start = Instant::now();
        let handshake_frame = CommandFrame::handshake(self.target_baud);
        let handshake_data = handshake_frame.build();

        // Send handshake frames repeatedly until we get a response
        while start.elapsed() < HANDSHAKE_TIMEOUT {
            self.cancel
                .check()?;

            // Send handshake
            if let Err(e) = self
                .port
                .write_all(&handshake_data)
            {
                if e.kind() == std::io::ErrorKind::Interrupted {
                    return Err(Error::Io(e));
                }
                trace!("Write error (ignoring): {e}");
            }
            if let Err(e) = self
                .port
                .flush()
            {
                if e.kind() == std::io::ErrorKind::Interrupted {
                    return Err(Error::Io(e));
                }
            }

            // Small delay
            sleep_interruptible(&self.cancel, Duration::from_millis(10))?;

            // Check for response
            let mut buf = [0u8; 256];
            match self
                .port
                .read(&mut buf)
            {
                Ok(n) if n > 0 => {
                    trace!("Received {n} bytes");
                    if contains_handshake_ack(&buf[..n]) {
                        info!("Handshake successful!");

                        // Change baud rate if not in late mode
                        if !self.late_baud && self.target_baud != DEFAULT_BAUD {
                            self.change_baud_rate(self.target_baud)?;
                        } else {
                            self.active_baud = DEFAULT_BAUD;
                        }

                        return Ok(());
                    }
                },
                Ok(_) => {},
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {},
                Err(e) => {
                    if e.kind() == std::io::ErrorKind::Interrupted {
                        return Err(Error::Io(e));
                    }
                    trace!("Read error (ignoring): {e}");
                },
            }
        }

        Err(Error::Timeout(format!(
            "No response after {} seconds",
            HANDSHAKE_TIMEOUT.as_secs()
        )))
    }

    /// Change the baud rate.
    fn change_baud_rate(&mut self, baud: u32) -> Result<()> {
        self.cancel
            .check()?;

        info!("Changing baud rate to {baud}");

        // Send baud rate change command
        let frame = CommandFrame::set_baud_rate(baud);
        self.send_command(&frame)?;

        // Wait for command to be processed
        sleep_interruptible(&self.cancel, BAUD_CHANGE_DELAY)?;

        // Change local baud rate
        self.port
            .set_baud_rate(baud)?;

        // Clear buffers
        sleep_interruptible(&self.cancel, BAUD_CHANGE_DELAY)?;
        self.port
            .clear_buffers()?;

        self.active_baud = baud;
        debug!("Baud rate changed to {baud}");
        Ok(())
    }

    fn lower_baud_for_retry(&mut self, failed_name: &str, err: &Error) -> Result<bool> {
        let current_baud = self
            .port
            .baud_rate();
        self.active_baud = current_baud;
        let Some(next_baud) = lower_fallback_baud(current_baud) else {
            return Ok(false);
        };

        warn!(
            "Download for {failed_name} failed at {current_baud} baud: {err}; retrying at \
             {next_baud} baud"
        );
        self.change_baud_rate(next_baud)?;
        Ok(true)
    }

    /// Send a command frame.
    fn send_command(&mut self, frame: &CommandFrame) -> Result<()> {
        let data = frame.build();
        trace!(
            "Sending command {:?}: {} bytes",
            frame.command(),
            data.len()
        );

        self.port
            .write_all(&data)?;
        self.port
            .flush()?;

        Ok(())
    }

    /// Wait for SEBOOT magic (0xDEADBEEF) response from device.
    ///
    /// After LoaderBoot YMODEM transfer or after sending a download command,
    /// the device responds with a SEBOOT frame starting with the magic bytes.
    /// This function reads bytes until the magic sequence is found, then
    /// drains the remaining frame data.
    fn wait_for_magic(&mut self, timeout: Duration) -> Result<()> {
        let magic: [u8; 4] = [0xEF, 0xBE, 0xAD, 0xDE]; // Little-endian DEADBEEF
        let start = Instant::now();
        let mut collected = std::mem::take(&mut self.prefetched_magic_bytes);

        debug!("Waiting for SEBOOT magic...");

        while start.elapsed() < timeout {
            self.cancel
                .check()?;

            if let Some(pos) = collected
                .windows(magic.len())
                .position(|window| window == magic)
            {
                if collected.len() >= pos + 6 {
                    let len = u16::from_le_bytes([collected[pos + 4], collected[pos + 5]]) as usize;
                    if collected.len() >= pos + len {
                        let remainder = collected[pos + len..].to_vec();
                        if !remainder.is_empty() {
                            trace!("wait_for_magic remainder: {remainder:02X?}");
                            self.prefetched_ymodem_bytes
                                .extend(remainder);
                        }
                        debug!("Received SEBOOT magic response");
                        return Ok(());
                    }
                }
            }

            let mut buf = [0u8; 64];
            match self
                .port
                .read(&mut buf)
            {
                Ok(n) if n > 0 => {
                    trace!("wait_for_magic chunk: {:02X?}", &buf[..n]);
                    collected.extend_from_slice(&buf[..n]);
                    if collected.len() > 512 {
                        let keep_from = collected
                            .len()
                            .saturating_sub(64);
                        collected.drain(..keep_from);
                    }
                },
                Ok(_) => {},
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {},
                Err(e) => {
                    if e.kind() == std::io::ErrorKind::Interrupted {
                        return Err(Error::Io(e));
                    }
                    return Err(Error::Io(e));
                },
            }
        }

        Err(Error::Timeout("Timeout waiting for SEBOOT magic".into()))
    }

    /// Transfer LoaderBoot via YMODEM without sending a download command.
    ///
    /// After handshake, the device enters YMODEM mode directly for LoaderBoot.
    /// No download command (0xD2) should be sent. This matches the official
    /// fbb_burntool behavior where LOADER type partitions skip the download
    /// command and go straight to YMODEM transfer.
    fn transfer_loaderboot<F>(&mut self, name: &str, data: &[u8], progress: &mut F) -> Result<()>
    where
        F: FnMut(&str, usize, usize),
    {
        self.cancel
            .check()?;

        debug!(
            "Transferring LoaderBoot {} ({} bytes) via YMODEM",
            name,
            data.len()
        );

        let config = YmodemConfig {
            char_timeout: Duration::from_secs(1),
            c_timeout: Duration::from_secs(30),
            max_retries: 10,
            finish_without_c: self.finish_without_c,
            verbose: self.verbose,
        };

        let prefetched_input = std::mem::take(&mut self.prefetched_ymodem_bytes);
        let mut ymodem = YmodemTransfer::with_config(&mut self.port, config, &self.cancel)
            .with_prefetched_input(prefetched_input);
        ymodem.transfer(name, data, |current, total| {
            progress(name, current, total);
        })?;
        self.prefetched_magic_bytes = ymodem.take_trailing_data();

        debug!("LoaderBoot transfer complete");
        Ok(())
    }

    /// Flash a FWPKG firmware package.
    ///
    /// # Arguments
    ///
    /// * `fwpkg` - The firmware package to flash
    /// * `filter` - Optional filter for partition names (None = flash all)
    /// * `progress` - Progress callback (partition_name, current_bytes,
    ///   total_bytes)
    pub fn flash_fwpkg<F>(
        &mut self,
        fwpkg: &Fwpkg,
        filter: Option<&[&str]>,
        mut progress: F,
    ) -> Result<()>
    where
        F: FnMut(&str, usize, usize),
    {
        self.cancel
            .check()?;

        if let Some(loaderboot) = fwpkg.loaderboot() {
            info!("Flashing LoaderBoot: {}", loaderboot.name);

            // LoaderBoot: NO download command. After handshake ACK, the device
            // enters YMODEM mode directly. This matches fbb_burntool and ws63flash.
            let lb_data = fwpkg.bin_data(loaderboot)?;
            self.transfer_loaderboot(&loaderboot.name, lb_data, &mut progress)?;

            // Wait for LoaderBoot to initialize (device sends SEBOOT magic when ready)
            self.wait_for_magic(POST_TRANSFER_MAGIC_TIMEOUT)?;

            // Change baud rate if in late mode
            if self.late_baud && self.target_baud != DEFAULT_BAUD {
                self.change_baud_rate(self.target_baud)?;
            }
        } else {
            info!(
                "FWPKG has no LoaderBoot partition; flashing app-only package through existing \
                 SEBOOT session"
            );

            // App-only packages are intended for development flows where a
            // LoaderBoot/SEBOOT session is already active (for example after a
            // ROM handshake or an external loader step). Probe that state
            // explicitly so a ROM-only connection fails early with a protocol
            // timeout instead of silently treating app bytes as LoaderBoot.
            self.wait_for_magic(MAGIC_TIMEOUT)?;

            if self.late_baud && self.target_baud != DEFAULT_BAUD {
                self.change_baud_rate(self.target_baud)?;
            }
        }

        // Flash remaining partitions
        let mut flashed_any = false;
        for bin in fwpkg.normal_bins() {
            self.cancel
                .check()?;

            // Apply filter if provided
            if let Some(names) = filter {
                if !names
                    .iter()
                    .any(|n| {
                        bin.name
                            .contains(n)
                    })
                {
                    debug!("Skipping partition: {}", bin.name);
                    continue;
                }
            }

            info!(
                "Flashing partition: {} -> 0x{:08X}",
                bin.name, bin.burn_addr
            );

            let bin_data = fwpkg.bin_data(bin)?;
            self.download_binary(&bin.name, bin_data, bin.burn_addr, &mut progress)?;
            flashed_any = true;

            // Inter-partition delay to prevent serial data stale
            // (MCU won't respond if next command follows immediately)
            sleep_interruptible(&self.cancel, PARTITION_DELAY)?;
        }

        if !flashed_any {
            return Err(Error::InvalidFwpkg(
                "No non-LoaderBoot partitions selected for flashing".into(),
            ));
        }

        info!("Flashing complete!");
        Ok(())
    }

    /// Download a single binary to flash with retry mechanism.
    #[allow(clippy::cast_possible_truncation)]
    fn download_binary<F>(
        &mut self,
        name: &str,
        data: &[u8],
        addr: u32,
        progress: &mut F,
    ) -> Result<()>
    where
        F: FnMut(&str, usize, usize),
    {
        self.cancel
            .check()?;

        let mut last_error = None;

        for attempt in 1..=MAX_DOWNLOAD_RETRIES {
            self.cancel
                .check()?;

            match self.try_download_binary(name, data, addr, progress) {
                Ok(()) => {
                    return Ok(());
                },
                Err(e) => {
                    if is_interrupted_error(&e) || crate::is_interrupted_requested() {
                        return Err(e);
                    }

                    if attempt < MAX_DOWNLOAD_RETRIES {
                        let lowered = self.lower_baud_for_retry(name, &e)?;
                        if !lowered {
                            warn!(
                                "Download failed for {name} (attempt \
                                 {attempt}/{MAX_DOWNLOAD_RETRIES}): {e}"
                            );
                            warn!("Retrying...");
                        }
                        last_error = Some(e);

                        // Clear buffers and wait before retry
                        let _ = self
                            .port
                            .clear_buffers();
                        sleep_interruptible(&self.cancel, CONNECT_RETRY_DELAY)?;
                    } else {
                        return Err(e);
                    }
                },
            }
        }

        // Use unwrap_or_else to ensure we never lose error information
        Err(last_error.unwrap_or_else(|| {
            Error::Protocol("Download failed after all retries (no error captured)".into())
        }))
    }

    /// Single attempt to download a binary.
    fn try_download_binary<F>(
        &mut self,
        name: &str,
        data: &[u8],
        addr: u32,
        progress: &mut F,
    ) -> Result<()>
    where
        F: FnMut(&str, usize, usize),
    {
        self.cancel
            .check()?;

        // Check for oversized data that would truncate
        let len = u32::try_from(data.len()).map_err(|_| {
            Error::Protocol(format!("Firmware too large ({} bytes > 4GB)", data.len()))
        })?;

        debug!(
            "Downloading {} ({} bytes) to 0x{:08X}",
            name,
            data.len(),
            addr
        );

        // Calculate aligned erase size (align up to 0x1000 = 4KB boundary)
        // This matches the official fbb_burntool behavior.
        let erase_size = (len + 0xFFF) & !0xFFF;

        // Send download command
        let frame = CommandFrame::download(addr, len, erase_size);
        self.send_command(&frame)?;

        // Wait for ACK frame (SEBOOT magic response) from device
        // The device responds with a SEBOOT frame after processing the download
        // command. ws63flash calls uart_read_until_magic() here.
        self.wait_for_magic(POST_TRANSFER_MAGIC_TIMEOUT)?;

        // Transfer using YMODEM
        // Note: ymodem.transfer() internally calls wait_for_c(), so we don't need
        // to call it here. The device sends 'C' after the ACK frame.
        let config = YmodemConfig {
            char_timeout: Duration::from_secs(1),
            c_timeout: Duration::from_secs(30),
            max_retries: 10,
            finish_without_c: self.finish_without_c,
            verbose: self.verbose,
        };

        let prefetched_input = std::mem::take(&mut self.prefetched_ymodem_bytes);
        let mut ymodem = YmodemTransfer::with_config(&mut self.port, config, &self.cancel)
            .with_prefetched_input(prefetched_input);
        ymodem.transfer(name, data, |current, total| {
            progress(name, current, total);
        })?;
        self.prefetched_magic_bytes = ymodem.take_trailing_data();

        // BurnTool waits for a SEBOOT ACK after each partition transfer before
        // issuing the next download command. BS2X requires the same sequencing.
        self.wait_for_magic(POST_TRANSFER_MAGIC_TIMEOUT)?;

        debug!("{name} transfer complete");
        Ok(())
    }

    /// Write raw binary data to flash.
    ///
    /// # Arguments
    ///
    /// * `loaderboot` - LoaderBoot binary data (required for first-stage boot)
    /// * `bins` - List of (data, address) pairs to flash
    pub fn write_bins(&mut self, loaderboot: &[u8], bins: &[(&[u8], u32)]) -> Result<()> {
        self.cancel
            .check()?;

        info!("Writing LoaderBoot ({} bytes)", loaderboot.len());

        // Transfer LoaderBoot (no download command)
        self.transfer_loaderboot("loaderboot", loaderboot, &mut |_, _, _| {})?;

        // Wait for LoaderBoot to initialize
        self.wait_for_magic(MAGIC_TIMEOUT)?;

        // Change baud rate if in late mode
        if self.late_baud && self.target_baud != DEFAULT_BAUD {
            self.change_baud_rate(self.target_baud)?;
        }

        // Download remaining binaries
        for (i, (data, addr)) in bins
            .iter()
            .enumerate()
        {
            self.cancel
                .check()?;

            let name = format!("binary_{i}");
            info!("Writing {} ({} bytes) to 0x{:08X}", name, data.len(), addr);
            self.download_binary(&name, data, *addr, &mut |_, _, _| {})?;

            // Inter-partition delay
            sleep_interruptible(&self.cancel, PARTITION_DELAY)?;
        }

        Ok(())
    }

    /// Erase entire flash.
    pub fn erase_all(&mut self) -> Result<()> {
        self.cancel
            .check()?;

        info!("Erasing entire flash...");

        let frame = CommandFrame::erase_all();
        self.send_command(&frame)?;

        // Wait for erase to complete
        sleep_interruptible(&self.cancel, Duration::from_secs(5))?;

        info!("Flash erased");
        Ok(())
    }

    /// Reset the device.
    pub fn reset(&mut self) -> Result<()> {
        self.cancel
            .check()?;

        info!("Resetting device...");

        let frame = CommandFrame::reset();
        self.send_command(&frame)?;

        Ok(())
    }
}

// Native-specific convenience functions
#[cfg(feature = "native")]
mod native_impl {
    use {
        super::{
            DEFAULT_BAUD, Duration, Error, Result, Ws63Flasher, debug, sleep_interruptible, warn,
        },
        crate::port::NativePort,
    };

    impl Ws63Flasher<NativePort> {
        /// Create a new WS63 flasher by opening a serial port.
        ///
        /// This is a convenience function for native platforms that opens
        /// the port with default settings.
        ///
        /// # Arguments
        ///
        /// * `port_name` - Serial port name (e.g., "/dev/ttyUSB0" or "COM3")
        /// * `target_baud` - Target baud rate for data transfer
        pub fn open(port_name: &str, target_baud: u32) -> Result<Self> {
            Self::open_with_retry(port_name, target_baud)
        }

        /// Open a serial port with full configuration (P0: 完整配置支持).
        ///
        /// This allows customization of all serial port parameters.
        ///
        /// # Arguments
        ///
        /// * `config` - Serial port configuration
        pub fn open_with_config(config: crate::port::SerialConfig) -> Result<Self> {
            Self::open_with_config_retry(config)
        }

        /// Open serial port with full config and retry mechanism.
        #[allow(clippy::needless_pass_by_value)]
        fn open_with_config_retry(config: crate::port::SerialConfig) -> Result<Self> {
            const MAX_OPEN_PORT_ATTEMPTS: usize = 3;
            const OPEN_RETRY_DELAY: Duration = Duration::from_millis(500);

            let mut last_error = None;

            for attempt in 1..=MAX_OPEN_PORT_ATTEMPTS {
                match NativePort::open(&config) {
                    Ok(port) => {
                        if attempt > 1 {
                            debug!("Port opened on attempt {attempt}");
                        }
                        let mut flasher = Self::with_cancel(
                            port,
                            config.baud_rate,
                            crate::cancel_context_from_global(),
                        );
                        flasher.active_baud = config.baud_rate;
                        return Ok(flasher);
                    },
                    Err(e) => {
                        warn!(
                            "Failed to open port {} (attempt {}/{}): {e}",
                            config.port_name, attempt, MAX_OPEN_PORT_ATTEMPTS
                        );
                        last_error = Some(e);

                        if attempt < MAX_OPEN_PORT_ATTEMPTS {
                            sleep_interruptible(
                                &crate::cancel_context_from_global(),
                                OPEN_RETRY_DELAY,
                            )?;
                        }
                    },
                }
            }

            Err(last_error.unwrap_or_else(|| {
                Error::Config(format!(
                    "Failed to open port after {MAX_OPEN_PORT_ATTEMPTS} attempts"
                ))
            }))
        }

        /// Open serial port with retry mechanism.
        fn open_with_retry(port_name: &str, target_baud: u32) -> Result<Self> {
            const MAX_OPEN_PORT_ATTEMPTS: usize = 3;
            const OPEN_RETRY_DELAY: Duration = Duration::from_millis(500);

            let mut last_error = None;

            for attempt in 1..=MAX_OPEN_PORT_ATTEMPTS {
                let config = crate::port::SerialConfig::new(port_name, DEFAULT_BAUD);
                match NativePort::open(&config) {
                    Ok(port) => {
                        if attempt > 1 {
                            debug!("Port opened on attempt {attempt}");
                        }
                        return Ok(Self::with_cancel(
                            port,
                            target_baud,
                            crate::cancel_context_from_global(),
                        ));
                    },
                    Err(e) => {
                        warn!(
                            "Failed to open port {port_name} (attempt \
                             {attempt}/{MAX_OPEN_PORT_ATTEMPTS}): {e}"
                        );
                        last_error = Some(e);

                        if attempt < MAX_OPEN_PORT_ATTEMPTS {
                            sleep_interruptible(
                                &crate::cancel_context_from_global(),
                                OPEN_RETRY_DELAY,
                            )?;
                        }
                    },
                }
            }

            Err(last_error.unwrap_or_else(|| {
                Error::Config(format!(
                    "Failed to open port {port_name} after {MAX_OPEN_PORT_ATTEMPTS} attempts"
                ))
            }))
        }
    }
}

impl<P: Port> crate::target::Flasher for Ws63Flasher<P> {
    fn connect(&mut self) -> Result<()> {
        self.connect()
    }

    fn flash_fwpkg(
        &mut self,
        fwpkg: &Fwpkg,
        filter: Option<&[&str]>,
        progress: &mut dyn FnMut(&str, usize, usize),
    ) -> Result<()> {
        self.flash_fwpkg(fwpkg, filter, |name, current, total| {
            progress(name, current, total);
        })
    }

    fn write_bins(&mut self, loaderboot: &[u8], bins: &[(&[u8], u32)]) -> Result<()> {
        self.write_bins(loaderboot, bins)
    }

    fn erase_all(&mut self) -> Result<()> {
        self.erase_all()
    }

    fn reset(&mut self) -> Result<()> {
        self.reset()
    }

    fn connection_baud(&self) -> u32 {
        DEFAULT_BAUD
    }

    fn target_baud(&self) -> Option<u32> {
        Some(self.target_baud)
    }

    fn close(&mut self) {
        // Close the underlying port to release resources
        // This is important for proper cleanup after reset
        let _ = self
            .port
            .close();
    }

    fn into_monitor(self: Box<Self>, baud_rate: u32) -> Result<crate::monitor::MonitorSession> {
        let Self { port, .. } = *self;
        port.into_monitor_session(baud_rate)
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::port::Port,
        std::{
            io::{Read, Write},
            sync::{Arc, Mutex},
        },
    };

    /// Mock port implementation for testing without real hardware.
    ///
    /// This implementation uses an internal buffer to simulate serial port
    /// behavior, allowing unit tests to run without actual hardware.
    #[derive(Clone)]
    struct MockPort {
        name: String,
        baud_rate: u32,
        timeout: Duration,
        max_read_size: usize,
        read_buffer: Arc<Mutex<Vec<u8>>>,
        write_buffer: Arc<Mutex<Vec<u8>>>,
        dtr: bool,
        rts: bool,
    }

    impl MockPort {
        fn new(name: &str) -> Self {
            Self {
                name: name.to_string(),
                baud_rate: 115200,
                timeout: Duration::from_secs(1),
                max_read_size: 1,
                read_buffer: Arc::new(Mutex::new(Vec::new())),
                write_buffer: Arc::new(Mutex::new(Vec::new())),
                dtr: false,
                rts: false,
            }
        }

        /// Add data to the read buffer (simulates receiving data from device).
        fn add_read_data(&self, data: &[u8]) {
            let mut buf = self
                .read_buffer
                .lock()
                .unwrap();
            buf.extend_from_slice(data);
        }

        /// Get data written to the port (simulates sending data to device).
        fn get_written_data(&self) -> Vec<u8> {
            let buf = self
                .write_buffer
                .lock()
                .unwrap();
            buf.clone()
        }

        /// Clear all buffers.
        fn clear(&self) {
            let mut read_buf = self
                .read_buffer
                .lock()
                .unwrap();
            let mut write_buf = self
                .write_buffer
                .lock()
                .unwrap();
            read_buf.clear();
            write_buf.clear();
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

        fn set_baud_rate(&mut self, baud_rate: u32) -> Result<()> {
            self.baud_rate = baud_rate;
            Ok(())
        }

        fn baud_rate(&self) -> u32 {
            self.baud_rate
        }

        fn clear_buffers(&mut self) -> Result<()> {
            self.clear();
            Ok(())
        }

        fn name(&self) -> &str {
            &self.name
        }

        fn set_dtr(&mut self, level: bool) -> Result<()> {
            self.dtr = level;
            Ok(())
        }

        fn set_rts(&mut self, level: bool) -> Result<()> {
            self.rts = level;
            Ok(())
        }

        fn read_cts(&mut self) -> Result<bool> {
            Ok(true) // Assume CTS is asserted
        }

        fn read_dsr(&mut self) -> Result<bool> {
            Ok(true) // Assume DSR is asserted
        }

        fn close(&mut self) -> Result<()> {
            // Clear all buffers to simulate port closure
            self.clear();
            Ok(())
        }
    }

    impl Read for MockPort {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let mut read_buf = self
                .read_buffer
                .lock()
                .map_err(|e| std::io::Error::other(format!("mutex poisoned: {e}")))?;

            if read_buf.is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "no data available",
                ));
            }

            let to_read = std::cmp::min(buf.len(), read_buf.len()).min(self.max_read_size);
            buf[..to_read].copy_from_slice(&read_buf[..to_read]);
            read_buf.drain(..to_read);
            Ok(to_read)
        }
    }

    impl Write for MockPort {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let mut write_buf = self
                .write_buffer
                .lock()
                .map_err(|e| std::io::Error::other(format!("mutex poisoned: {e}")))?;
            write_buf.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Test creating a Ws63Flasher with a mock port.
    #[test]
    fn test_flasher_new_with_mock_port() {
        let port = MockPort::new("/dev/ttyUSB0");
        let flasher = Ws63Flasher::with_cancel(port, 921600, CancelContext::none());

        assert_eq!(flasher.target_baud, 921600);
        assert!(!flasher.late_baud);
        assert_eq!(flasher.verbose, 0);
    }

    /// Test builder methods on Ws63Flasher.
    #[test]
    fn test_flasher_builder_methods() {
        let port = MockPort::new("/dev/ttyUSB0");
        let flasher = Ws63Flasher::with_cancel(port, 921600, CancelContext::none())
            .with_late_baud(true)
            .with_verbose(2);

        assert!(flasher.late_baud);
        assert_eq!(flasher.verbose, 2);
    }

    /// Test MockPort read/write operations.
    #[test]
    fn test_mock_port_read_write() {
        let mut port = MockPort::new("/dev/ttyUSB0");

        // Add some data to read buffer
        port.add_read_data(&[0xDE, 0xAD, 0xBE, 0xEF]);

        // Write some data
        port.write_all(b"test")
            .unwrap();
        port.flush()
            .unwrap();

        // Verify written data
        let written = port.get_written_data();
        assert_eq!(written, b"test");

        // Read data - use read_exact to handle partial reads properly
        let mut buf = [0u8; 4];
        std::io::Read::read_exact(&mut port, &mut buf).unwrap();
        assert_eq!(&buf, &[0xDE, 0xAD, 0xBE, 0xEF]);
    }

    /// Test MockPort buffer operations.
    #[test]
    fn test_mock_port_buffers() {
        let mut port = MockPort::new("/dev/ttyUSB0");

        // Clear buffers
        port.clear();
        assert!(
            port.get_written_data()
                .is_empty()
        );

        // Write and add read data
        port.write_all(b"hello")
            .unwrap();
        port.add_read_data(&[1, 2, 3]);

        // Verify
        assert_eq!(port.get_written_data(), b"hello");

        let mut buf = [0u8; 3];
        std::io::Read::read_exact(&mut port, &mut buf).unwrap();
        assert_eq!(&buf, &[1, 2, 3]);

        // Clear and verify
        port.clear();
        assert!(
            port.get_written_data()
                .is_empty()
        );
    }

    /// Test MockPort pin control.
    #[test]
    fn test_mock_port_pin_control() {
        let mut port = MockPort::new("/dev/ttyUSB0");

        assert!(!port.dtr);
        assert!(!port.rts);

        port.set_dtr(true)
            .unwrap();
        port.set_rts(true)
            .unwrap();

        assert!(port.dtr);
        assert!(port.rts);
    }

    /// Test MockPort baud rate and timeout.
    #[test]
    fn test_mock_port_baud_timeout() {
        let mut port = MockPort::new("/dev/ttyUSB0");

        assert_eq!(port.baud_rate(), 115200);
        assert_eq!(port.timeout(), Duration::from_secs(1));

        port.set_baud_rate(921600)
            .unwrap();
        port.set_timeout(Duration::from_millis(500))
            .unwrap();

        assert_eq!(port.baud_rate(), 921600);
        assert_eq!(port.timeout(), Duration::from_millis(500));
    }

    /// Test MockPort name.
    #[test]
    fn test_mock_port_name() {
        let port = MockPort::new("/dev/ttyUSB1");
        assert_eq!(port.name(), "/dev/ttyUSB1");

        let port2 = MockPort::new("COM3");
        assert_eq!(port2.name(), "COM3");
    }

    /// Test creating flasher with mock port through
    /// ChipFamily::create_flasher_with_port.
    #[test]
    fn test_create_flasher_with_mock_port() {
        use crate::target::ChipFamily;

        let port = MockPort::new("/dev/ttyUSB0");
        let flasher = ChipFamily::Ws63.create_flasher_with_port(port, 921600, false, 0);

        assert!(flasher.is_ok());
        let flasher = flasher.unwrap();

        // Flasher should be usable (even though connect will fail without mock response
        // data)
        assert_eq!(flasher.connection_baud(), 115200); // DEFAULT_BAUD for handshake
        assert_eq!(flasher.target_baud(), Some(921600));
    }

    /// Test that Flasher trait object works correctly.
    #[test]
    fn test_flasher_trait_object() {
        use crate::target::Flasher;

        let port = MockPort::new("/dev/ttyUSB0");
        let flasher: Box<dyn Flasher> = Box::new(Ws63Flasher::with_cancel(
            port,
            921600,
            CancelContext::none(),
        ));

        assert_eq!(flasher.connection_baud(), 115200);
        assert_eq!(flasher.target_baud(), Some(921600));
    }

    /// Test multiple flasher instances with same mock port clone.
    #[test]
    fn test_multiple_flashers_same_port() {
        use crate::target::ChipFamily;

        let port = MockPort::new("/dev/ttyUSB0");
        let port_clone = port.clone();

        let flasher1 = ChipFamily::Ws63.create_flasher_with_port(port, 921600, false, 0);
        let flasher2 = ChipFamily::Ws63.create_flasher_with_port(port_clone, 115200, true, 1);

        assert!(flasher1.is_ok());
        assert!(flasher2.is_ok());

        let flasher1 = flasher1.unwrap();
        let flasher2 = flasher2.unwrap();

        assert_eq!(flasher1.target_baud(), Some(921600));
        assert_eq!(flasher2.target_baud(), Some(115200));
    }

    /// Test shared SEBOOT chip families can reuse the generic serial flasher.
    #[test]
    fn test_create_flasher_with_port_shared_seboot_chips() {
        use crate::target::ChipFamily;

        let port = MockPort::new("/dev/ttyUSB0");
        let result = ChipFamily::Bs2x.create_flasher_with_port(port, 115200, false, 0);

        assert!(result.is_ok());

        let port = MockPort::new("/dev/ttyUSB1");
        let result = ChipFamily::Bs25.create_flasher_with_port(port, 115200, false, 0);

        assert!(result.is_ok());
    }

    /// Test unsupported chip family still returns an error for generic ports.
    #[test]
    fn test_create_flasher_with_port_unsupported_chip() {
        use crate::target::ChipFamily;

        let port = MockPort::new("/dev/ttyUSB0");
        let result = ChipFamily::Generic.create_flasher_with_port(port, 115200, false, 0);

        assert!(result.is_err());
        // Verify error is the Unsupported variant
        assert!(matches!(result, Err(crate::error::Error::Unsupported(_))));
    }

    #[test]
    fn test_lower_fallback_baud_sequence() {
        assert_eq!(lower_fallback_baud(2_000_000), Some(921_600));
        assert_eq!(lower_fallback_baud(921_600), Some(460_800));
        assert_eq!(lower_fallback_baud(460_800), Some(230_400));
        assert_eq!(lower_fallback_baud(230_400), Some(115_200));
        assert_eq!(lower_fallback_baud(115_200), None);
    }

    #[test]
    fn test_download_retry_lowers_baud() {
        let mut port = MockPort::new("/dev/ttyUSB0");
        port.set_baud_rate(921_600)
            .unwrap();

        let mut flasher = Ws63Flasher::with_cancel(port, 921_600, CancelContext::none());
        flasher.active_baud = 921_600;

        let lowered = flasher
            .lower_baud_for_retry("app.bin", &Error::Ymodem("nak".into()))
            .unwrap();

        assert!(lowered);
        assert_eq!(flasher.active_baud, 460_800);
        assert_eq!(
            flasher
                .port
                .baud_rate(),
            460_800
        );
    }

    #[test]
    fn test_is_interrupted_error_for_io_interrupted_and_message() {
        let e1 = Error::Io(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "operation interrupted",
        ));
        assert!(is_interrupted_error(&e1));

        let e2 = Error::Io(std::io::Error::other("Interrupted system call"));
        assert!(is_interrupted_error(&e2));
    }

    #[test]
    fn test_download_binary_interrupted_short_circuits_retry() {
        crate::test_set_interrupted(true);

        let port = MockPort::new("/dev/ttyUSB0");
        let cancel = crate::cancel_context_from_global();
        let mut flasher = Ws63Flasher::with_cancel(port, 921600, cancel);
        let mut progress_calls = 0usize;

        let result = flasher.download_binary(
            "app.bin",
            &[0x01, 0x02, 0x03],
            0x0023_0000,
            &mut |_, _, _| {
                progress_calls += 1;
            },
        );

        assert!(matches!(
            result,
            Err(Error::Io(ref io)) if io.kind() == std::io::ErrorKind::Interrupted
        ));
        assert_eq!(progress_calls, 0);
        assert!(
            flasher
                .port
                .get_written_data()
                .is_empty(),
            "Interrupted download should not send frames or enter retry loop"
        );

        crate::test_set_interrupted(false);
    }

    // =====================================================================
    // Regression tests for protocol fixes (CRC fix + flash protocol fix)
    // =====================================================================

    /// Regression: erase_size must be aligned to 0x1000 (4KB) boundary.
    ///
    /// The official fbb_burntool aligns erase_size to 0x1000:
    ///   `if (eraseSize % 0x1000 != 0) eraseSize = 0x1000 * (eraseSize / 0x1000
    /// + 1)`
    ///
    /// Previously hisiflash passed `len` directly as erase_size without
    /// alignment.
    #[test]
    fn test_erase_size_alignment_4k() {
        // Already aligned values should stay the same
        assert_eq!((0x1000u32 + 0xFFF) & !0xFFF, 0x1000);
        assert_eq!((0x2000u32 + 0xFFF) & !0xFFF, 0x2000);
        assert_eq!((0x10000u32 + 0xFFF) & !0xFFF, 0x10000);

        // Non-aligned values should be rounded up to next 4KB boundary
        assert_eq!((1u32 + 0xFFF) & !0xFFF, 0x1000);
        assert_eq!((0x1001u32 + 0xFFF) & !0xFFF, 0x2000);
        assert_eq!((0x2001u32 + 0xFFF) & !0xFFF, 0x3000);
        assert_eq!((0xFFFu32 + 0xFFF) & !0xFFF, 0x1000);

        // Typical firmware sizes from ws63-liteos-app_all.fwpkg
        // root_params_sign.bin: length = 0x8F4 (2292 bytes)
        assert_eq!((0x8F4u32 + 0xFFF) & !0xFFF, 0x1000);
        // root_params_sign_b.bin: similar
        assert_eq!((0x900u32 + 0xFFF) & !0xFFF, 0x1000);
        // A larger typical partition
        assert_eq!((0x12345u32 + 0xFFF) & !0xFFF, 0x13000);
    }

    /// Regression: wait_for_magic correctly detects SEBOOT magic bytes.
    ///
    /// After LoaderBoot transfer and after each download command, the device
    /// sends a SEBOOT frame starting with 0xDEADBEEF (little-endian: EF BE AD
    /// DE). wait_for_magic must find this pattern in the byte stream.
    #[test]
    fn test_wait_for_magic_finds_magic() {
        let port = MockPort::new("/dev/ttyUSB0");

        // Simulate device response: some garbage then magic + frame data
        let mut response = vec![0x00, 0x41, 0x42]; // garbage bytes
        response.extend_from_slice(&[0xEF, 0xBE, 0xAD, 0xDE]); // magic
        response.extend_from_slice(&[0x0C, 0x00, 0xE1, 0x1E, 0x5A, 0x00, 0x00, 0x00]); // frame
        port.add_read_data(&response);

        let mut flasher = Ws63Flasher::with_cancel(port, 921600, CancelContext::none());
        let result = flasher.wait_for_magic(Duration::from_millis(500));
        assert!(
            result.is_ok(),
            "wait_for_magic should succeed when magic is present"
        );
    }

    /// Regression: wait_for_magic times out when no magic present.
    #[test]
    fn test_wait_for_magic_timeout_no_magic() {
        let port = MockPort::new("/dev/ttyUSB0");
        // No data in buffer -> should timeout
        let mut flasher = Ws63Flasher::with_cancel(port, 921600, CancelContext::none());
        let result = flasher.wait_for_magic(Duration::from_millis(100));
        assert!(
            result.is_err(),
            "wait_for_magic should timeout with no data"
        );
    }

    /// Regression: wait_for_magic with magic preceded by partial match.
    ///
    /// Tests the edge case where some bytes of the magic appear before the
    /// full magic sequence (e.g., 0xEF followed by garbage, then the real
    /// magic).
    #[test]
    fn test_wait_for_magic_partial_then_real() {
        let port = MockPort::new("/dev/ttyUSB0");

        // Partial magic (0xEF 0xBE) then non-magic, then real magic
        let mut response = Vec::new();
        response.extend_from_slice(&[0xEF, 0xBE, 0x00]); // partial match then break
        response.extend_from_slice(&[0xEF, 0xBE, 0xAD, 0xDE]); // real magic
        response.extend_from_slice(&[0x0C, 0x00, 0xE1, 0x1E, 0x5A, 0x00, 0x00, 0x00]); // complete frame tail
        port.add_read_data(&response);

        let mut flasher = Ws63Flasher::with_cancel(port, 921600, CancelContext::none());
        let result = flasher.wait_for_magic(Duration::from_millis(500));
        assert!(
            result.is_ok(),
            "wait_for_magic should handle partial matches"
        );
    }

    /// Regression: LoaderBoot must NOT send download command (0xD2).
    ///
    /// In the official fbb_burntool, `SendBurnCmd()` skips the download payload
    /// for LOADER type: `if (GetCurrentCmdType() != BurnCtrl::LOADER)`.
    /// ws63flash also only calls ymodem_xfer() directly after handshake for
    /// LoaderBoot.
    ///
    /// Previously hisiflash called download_binary() for LoaderBoot, which sent
    /// a 0xD2 download command frame. This caused the device to misinterpret
    /// the frame as data corruption.
    #[test]
    fn test_loaderboot_no_download_command() {
        let port = MockPort::new("/dev/ttyUSB0");

        // Simulate: device sends 'C' for YMODEM, then ACKs all blocks, then magic
        let response = vec![
            b'C', // YMODEM 'C' request
            0x06, // ACK for block 0 (file info)
            0x06, // ACK for data block
            0x06, // ACK for EOT
            0x06, // ACK for finish block
        ];
        port.add_read_data(&response);

        let mut flasher = Ws63Flasher::with_cancel(port, 921600, CancelContext::none());
        let result = flasher.transfer_loaderboot("test.bin", &[0xAA], &mut |_, _, _| {});

        // Transfer should succeed (or fail on mock port details, but NOT send 0xD2)
        // The key assertion: check that no download command frame was written
        let written = flasher
            .port
            .get_written_data();

        // Download command frame starts with magic + has cmd byte 0xD2
        // Scan the written data for 0xD2 command byte at the expected position
        // Frame format: [EF BE AD DE] [len_lo len_hi] [CMD] [SCMD] ...
        let has_download_cmd = written
            .windows(8)
            .any(|w| {
                w[0] == 0xEF
                    && w[1] == 0xBE
                    && w[2] == 0xAD
                    && w[3] == 0xDE
                    && w[6] == 0xD2
                    && w[7] == 0x2D
            });

        assert!(
            !has_download_cmd,
            "LoaderBoot transfer must NOT send download command (0xD2). Written data should only \
             contain YMODEM blocks, not SEBOOT command frames."
        );

        // Also verify that the YMODEM transfer actually wrote something
        assert!(
            !written.is_empty(),
            "YMODEM transfer should have written data for LoaderBoot"
        );

        // Verify the result succeeded
        assert!(
            result.is_ok(),
            "LoaderBoot transfer should succeed: {:?}",
            result.err()
        );
    }

    /// Regression: download_binary for normal partitions MUST send download
    /// command (0xD2).
    ///
    /// After LoaderBoot, all subsequent partitions require a download command
    /// with addr, len, and aligned erase_size before the YMODEM transfer.
    #[test]
    fn test_normal_partition_sends_download_command() {
        let port = MockPort::new("/dev/ttyUSB0");

        // Simulate: device sends magic ACK after download command, then 'C' for YMODEM
        let mut response = Vec::new();
        // ACK frame for download command (magic + frame data)
        response.extend_from_slice(&[0xEF, 0xBE, 0xAD, 0xDE]);
        response.extend_from_slice(&[0x0C, 0x00, 0xE1, 0x1E, 0x5A, 0x00, 0x00, 0x00]);
        // Note: wait_for_magic drains remaining bytes after the magic in one read call,
        // so YMODEM responses (C, ACKs) get consumed. This is a mock limitation.
        // We just verify the download command was sent; full flow is tested on
        // hardware.
        port.add_read_data(&response);

        let mut flasher = Ws63Flasher::with_cancel(port, 921600, CancelContext::none());
        let test_data = vec![0xBB; 100];
        // The transfer will fail because 'C' and ACKs were drained by wait_for_magic,
        // but we only care about verifying the download command was sent.
        let _result = flasher.try_download_binary(
            "test_partition.bin",
            &test_data,
            0x00800000,
            &mut |_, _, _| {},
        );

        let written = flasher
            .port
            .get_written_data();

        // Verify download command WAS sent
        let has_download_cmd = written
            .windows(8)
            .any(|w| {
                w[0] == 0xEF
                    && w[1] == 0xBE
                    && w[2] == 0xAD
                    && w[3] == 0xDE
                    && w[6] == 0xD2
                    && w[7] == 0x2D
            });

        assert!(
            has_download_cmd,
            "Normal partition download must send download command (0xD2). Written data should \
             contain a SEBOOT command frame."
        );
    }

    /// Regression: download command frame must contain properly aligned
    /// erase_size.
    ///
    /// Verifies the actual bytes written in the download command frame have
    /// the erase_size field aligned to 0x1000 (4KB).
    #[test]
    fn test_download_frame_erase_size_in_bytes() {
        // Test with a non-aligned length (100 bytes = 0x64)
        // Expected erase_size: (0x64 + 0xFFF) & !0xFFF = 0x1000
        let frame = CommandFrame::download(0x00800000, 100, (100 + 0xFFF) & !0xFFF);
        let data = frame.build();

        // Frame layout: Magic(4) + Len(2) + CMD(1) + SCMD(1) + addr(4) + len(4) +
        // erase_size(4) + const(2) + CRC(2) erase_size starts at offset 16
        let erase_size = u32::from_le_bytes([data[16], data[17], data[18], data[19]]);
        assert_eq!(
            erase_size, 0x1000,
            "erase_size for 100 bytes should be 0x1000 (4KB aligned), got 0x{erase_size:X}"
        );

        // Test with exactly 4KB
        let frame2 = CommandFrame::download(0x00800000, 0x1000, (0x1000u32 + 0xFFF) & !0xFFF);
        let data2 = frame2.build();
        let erase_size2 = u32::from_le_bytes([data2[16], data2[17], data2[18], data2[19]]);
        assert_eq!(
            erase_size2, 0x1000,
            "erase_size for exactly 4KB should remain 0x1000"
        );

        // Test with 4KB + 1
        let frame3 = CommandFrame::download(0x00800000, 0x1001, (0x1001u32 + 0xFFF) & !0xFFF);
        let data3 = frame3.build();
        let erase_size3 = u32::from_le_bytes([data3[16], data3[17], data3[18], data3[19]]);
        assert_eq!(
            erase_size3, 0x2000,
            "erase_size for 0x1001 bytes should be 0x2000 (next 4KB boundary)"
        );
    }
}
