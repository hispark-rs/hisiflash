//! hisiflash CLI - Command-line tool for flashing HiSilicon chips.
//!
//! ## Features
//!
//! - Flash FWPKG firmware packages
//! - Write raw binary files to flash
//! - Erase flash memory
//! - Interactive serial port selection
//! - Shell completion generation
//! - Environment variable support
//! - Internationalization (i18n) support

use {
    anyhow::Result,
    clap::{
        CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum, error::ErrorKind,
        parser::ValueSource,
    },
    clap_complete::Shell,
    console::style,
    dialoguer::{Error as DialoguerError, Select, theme::ColorfulTheme},
    env_logger::Env,
    hisiflash::{ChipFamily, Error as LibError, clear_interrupt_flag},
    log::debug,
    rust_i18n::t,
    std::{
        env,
        io::IsTerminal,
        path::{Path, PathBuf},
        sync::OnceLock,
    },
    thiserror::Error,
};

/// Whether stderr is a terminal (set once at startup).
static STDERR_IS_TTY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
/// Whether process received SIGINT/Ctrl-C.
static INTERRUPTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Ensures Ctrl-C handler is installed only once.
static SIGNAL_HANDLER_INSTALLED: OnceLock<()> = OnceLock::new();

/// Check if emoji/animations should be used (TTY and colors enabled).
pub(crate) fn use_fancy_output() -> bool {
    STDERR_IS_TTY.load(std::sync::atomic::Ordering::Relaxed) && console::colors_enabled_stderr()
}

pub(crate) fn was_interrupted() -> bool {
    INTERRUPTED.load(std::sync::atomic::Ordering::Relaxed)
}

pub(crate) fn clear_interrupted_flag() {
    INTERRUPTED.store(false, std::sync::atomic::Ordering::Relaxed);
}

fn install_signal_handler() -> Result<()> {
    if SIGNAL_HANDLER_INSTALLED
        .get()
        .is_some()
    {
        return Ok(());
    }

    ctrlc::set_handler(|| {
        INTERRUPTED.store(true, std::sync::atomic::Ordering::Relaxed);
        // Also set the global interrupt flag in the library
        hisiflash::set_interrupt_flag();
    })
    .map_err(|e| anyhow::anyhow!("failed to install Ctrl-C handler: {e}"))?;

    let _ = SIGNAL_HANDLER_INSTALLED.set(());
    Ok(())
}

mod commands;
mod config;
mod help;
mod serial;

use {
    commands::{
        completions::{cmd_completions, cmd_completions_install},
        firmware::resolve_firmware,
        flash::{cmd_erase, cmd_flash, cmd_write, cmd_write_program},
        info::{cmd_info, cmd_list_ports},
        monitor::{cmd_monitor, cmd_monitor_with_session},
    },
    config::Config,
    help::{build_localized_command, detect_locale},
    serial::{SerialOptions, ask_remember_port, select_serial_port},
};

// Initialize i18n with locale files from the locales directory
rust_i18n::i18n!("locales", fallback = "en");

/// hisiflash - A cross-platform tool for flashing HiSilicon chips.
///
/// Environment variables:
///   HISIFLASH_PORT              - Default serial port
///   HISIFLASH_BAUD              - Default baud rate (default: 921600)
///   HISIFLASH_CHIP              - Default chip type (ws63, bs2x, bs25)
///   HISIFLASH_LANG              - Language/locale (en, zh-CN)
///   HISIFLASH_NON_INTERACTIVE   - Non-interactive mode (disable prompts)
#[derive(Parser)]
#[command(name = "hisiflash")]
#[command(author, version, about, long_about = None)]
#[command(propagate_version = true)]
#[command(after_help = "For more information, visit: https://github.com/hispark-rs/hisiflash")]
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct Cli {
    /// Serial port to use (auto-detected if not specified).
    #[arg(short, long, global = true, env = "HISIFLASH_PORT")]
    pub(crate) port: Option<String>,

    /// Baud rate for data transfer.
    #[arg(short, long, global = true, env = "HISIFLASH_BAUD")]
    pub(crate) baud: Option<u32>,

    /// Target chip type.
    #[arg(short, long, global = true, env = "HISIFLASH_CHIP")]
    pub(crate) chip: Option<Chip>,

    /// Language/locale for messages (e.g., en, zh-CN).
    #[arg(long, global = true, env = "HISIFLASH_LANG")]
    pub(crate) lang: Option<String>,

    /// Verbose output level (-v, -vv, -vvv for increasing detail).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub(crate) verbose: u8,

    /// Quiet mode (suppress non-essential output).
    #[arg(short, long, global = true)]
    pub(crate) quiet: bool,

    /// Non-interactive mode (fail instead of prompting).
    #[arg(long, global = true, env = "HISIFLASH_NON_INTERACTIVE")]
    pub(crate) non_interactive: bool,

    /// Confirm port selection even for auto-detected ports.
    #[arg(long, global = true)]
    pub(crate) confirm_port: bool,

    /// List all available ports (including unknown types).
    #[arg(long, global = true)]
    pub(crate) list_all_ports: bool,

    /// Path to a configuration file.
    #[arg(long = "config", global = true, value_name = "PATH")]
    pub(crate) config_path: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

/// Supported chip types.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum Chip {
    /// WS63 chip (WiFi + BLE).
    Ws63,
    /// BS2X series (BS21, BLE only) via shared SEBOOT serial path.
    Bs2x,
    /// BS25 (BLE with enhanced features) via shared SEBOOT serial path.
    Bs25,
}

impl From<Chip> for ChipFamily {
    fn from(chip: Chip) -> Self {
        match chip {
            Chip::Ws63 => ChipFamily::Ws63,
            Chip::Bs2x => ChipFamily::Bs2x,
            Chip::Bs25 => ChipFamily::Bs25,
        }
    }
}

impl Chip {
    fn as_cli_name(self) -> &'static str {
        match self {
            Self::Ws63 => "ws63",
            Self::Bs2x => "bs2x",
            Self::Bs25 => "bs25",
        }
    }

    fn from_config_name(name: &str) -> Option<Self> {
        match ChipFamily::from_name(name)? {
            ChipFamily::Ws63 => Some(Self::Ws63),
            ChipFamily::Bs2x => Some(Self::Bs2x),
            ChipFamily::Bs25 => Some(Self::Bs25),
            _ => None,
        }
    }
}

fn guess_chip_from_firmware_path(path: &Path) -> Option<Chip> {
    let name = path
        .file_name()
        .and_then(|file_name| file_name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let full_path = path
        .to_string_lossy()
        .to_ascii_lowercase();

    let haystacks = [name.as_str(), full_path.as_str()];

    if haystacks
        .iter()
        .any(|text| text.contains("bs25"))
    {
        return Some(Chip::Bs25);
    }

    if haystacks
        .iter()
        .any(|text| {
            ["bs2x", "bs21", "bs21e", "bs20", "bs22"]
                .iter()
                .any(|needle| text.contains(needle))
        })
    {
        return Some(Chip::Bs2x);
    }

    if haystacks
        .iter()
        .any(|text| text.contains("ws63"))
    {
        return Some(Chip::Ws63);
    }

    None
}

fn ensure_chip_prompt_tty() -> Result<()> {
    if std::io::stdin().is_terminal() && std::io::stderr().is_terminal() {
        Ok(())
    } else {
        Err(CliError::Usage(t!("chip.interactive_requires_tty").to_string()).into())
    }
}

fn map_chip_prompt_error(err: DialoguerError) -> anyhow::Error {
    match err {
        DialoguerError::IO(io_err) => {
            if io_err.kind() == std::io::ErrorKind::Interrupted {
                CliError::Cancelled(t!("chip.selection_cancelled").to_string()).into()
            } else {
                CliError::Usage(t!("chip.prompt_failed").to_string()).into()
            }
        },
    }
}

fn prompt_for_chip() -> Result<Chip> {
    const CHIPS: [Chip; 3] = [Chip::Ws63, Chip::Bs2x, Chip::Bs25];

    let labels: Vec<&str> = CHIPS
        .iter()
        .map(|chip| chip.as_cli_name())
        .collect();
    let selection = Select::with_theme(&ColorfulTheme::default())
        .with_prompt(t!("chip.select_prompt").to_string())
        .items(&labels)
        .default(0)
        .interact_opt()
        .map_err(map_chip_prompt_error)?;

    selection
        .map(|index| CHIPS[index])
        .ok_or_else(|| CliError::Cancelled(t!("chip.selection_cancelled").to_string()).into())
}

fn resolve_effective_chip(cli: &Cli, firmware: Option<&Path>) -> Result<Chip> {
    if let Some(chip) = cli.chip {
        return Ok(chip);
    }

    if let Some(path) = firmware {
        if let Some(chip) = guess_chip_from_firmware_path(path) {
            if !cli.quiet {
                eprintln!(
                    "{} {}",
                    style("🔎").blue(),
                    t!("chip.inferred_from_firmware", chip = chip.as_cli_name())
                );
            }
            return Ok(chip);
        }
    }

    if cli.non_interactive {
        let message = if firmware.is_some() {
            t!("chip.inference_failed_non_interactive").to_string()
        } else {
            t!("chip.required_non_interactive").to_string()
        };
        return Err(CliError::Usage(message).into());
    }

    ensure_chip_prompt_tty()?;

    if firmware.is_some() && !cli.quiet {
        eprintln!("{} {}", style("ℹ").blue(), t!("chip.could_not_infer"));
    }

    prompt_for_chip()
}

/// Resolve the effective baud rate to use for flashing.
///
/// If the user explicitly specified `--baud`, that value is used unconditionally.
/// Otherwise, the chip-specific recommended rate is used:
/// - BS2X/BS25 → 460800 (CH340/CH341 adapters are unreliable at 921600)
/// - WS63 and others → 921600 (maximum throughput)
pub(crate) fn resolve_effective_baud(cli_baud: Option<u32>, chip: ChipFamily) -> u32 {
    cli_baud.unwrap_or_else(|| chip.recommended_flash_baud())
}

#[derive(Debug, Error)]
pub(crate) enum CliError {
    // Usage: command-line syntax/arguments/environment usage problems.
    // Mapped to exit code 2 to match CLI usage-error conventions.
    #[error("{0}")]
    Usage(String),
    // Config: invalid or conflicting persisted configuration values.
    // Mapped to exit code 3 for explicit configuration failures.
    #[error("{0}")]
    Config(String),
    // Cancelled: user/signal initiated interruption.
    // Mapped to 130 (128 + SIGINT) for script-friendly interrupt semantics.
    #[error("{0}")]
    Cancelled(String),
}

#[derive(Debug, Error)]
#[error("json error response emitted")]
struct JsonErrorResponseEmitted {
    exit_code: i32,
}

impl CliError {
    fn exit_code(&self) -> i32 {
        match self {
            Self::Usage(_) => 2,
            Self::Config(_) => 3,
            Self::Cancelled(_) => 130,
        }
    }
}

/// Available commands.
#[derive(Subcommand)]
enum Commands {
    /// Flash a FWPKG firmware package.
    Flash {
        /// Path to the FWPKG firmware file (auto-detected if omitted).
        firmware: Option<PathBuf>,

        /// Only flash specified partitions (comma-separated).
        #[arg(long)]
        filter: Option<String>,

        /// Use late baud rate change (after LoaderBoot).
        #[arg(long)]
        late_baud: bool,

        /// Skip CRC verification.
        #[arg(long)]
        skip_verify: bool,

        /// Open serial monitor after flashing.
        #[arg(long)]
        monitor: bool,

        /// Baud rate for serial monitor (used with --monitor).
        #[arg(long, default_value = "115200")]
        monitor_baud: u32,

        /// Serial port for monitor session (can differ from flashing port).
        #[arg(long)]
        monitor_port: Option<String>,

        /// Clean monitor output by filtering non-printable control characters.
        #[arg(long = "monitor-clean-output", action = clap::ArgAction::Set, default_value_t = true)]
        monitor_clean_output: bool,

        /// Show raw monitor output without control-character filtering.
        #[arg(long = "monitor-raw", conflicts_with = "monitor_clean_output")]
        monitor_raw: bool,
    },

    /// Write raw binary files to flash.
    Write {
        /// LoaderBoot binary file.
        #[arg(long, required = true)]
        loaderboot: PathBuf,

        /// Binary file to flash (format: file:address, can be repeated).
        #[arg(long = "bin", value_parser = parse_bin_arg)]
        bins: Vec<(PathBuf, u32)>,

        /// Use late baud rate change.
        #[arg(long)]
        late_baud: bool,
    },

    /// Write a single binary with program data.
    WriteProgram {
        /// LoaderBoot binary file.
        #[arg(long, required = true)]
        loaderboot: PathBuf,

        /// Program binary file.
        program: PathBuf,

        /// Flash address for program.
        #[arg(short, long, value_parser = parse_hex_u32)]
        address: u32,

        /// Use late baud rate change.
        #[arg(long)]
        late_baud: bool,
    },

    /// Erase flash memory.
    Erase {
        /// Erase entire flash (required confirmation).
        #[arg(long)]
        all: bool,
    },

    /// Show information about a firmware file.
    Info {
        /// Path to the FWPKG firmware file.
        firmware: PathBuf,

        /// Output information as JSON to stdout.
        #[arg(long)]
        json: bool,
    },

    /// List available serial ports.
    ListPorts {
        /// Output port list as JSON to stdout.
        #[arg(long)]
        json: bool,
    },

    /// Open serial monitor.
    Monitor {
        /// Serial port for monitor session (overrides global --port for this
        /// command).
        #[arg(long)]
        monitor_port: Option<String>,

        /// Baud rate for monitoring (default: 115200).
        #[arg(long, default_value = "115200")]
        monitor_baud: u32,

        /// Show timestamps on each line.
        #[arg(long)]
        timestamp: bool,

        /// Save output to a log file.
        #[arg(long, value_name = "FILE")]
        log: Option<PathBuf>,

        /// Clean output by filtering non-printable control characters.
        #[arg(long = "clean-output", action = clap::ArgAction::Set, default_value_t = true)]
        clean_output: bool,

        /// Show raw serial output without control-character filtering.
        #[arg(long, conflicts_with = "clean_output")]
        raw: bool,
    },

    /// Generate shell completion scripts.
    Completions {
        /// Shell type for completions (auto-detected if not specified with
        /// --install).
        #[arg(value_enum)]
        shell: Option<Shell>,

        /// Automatically install completions to your shell configuration.
        #[arg(long)]
        install: bool,
    },
}

/// Parse binary argument in format "file:address".
fn parse_bin_arg(s: &str) -> Result<(PathBuf, u32), String> {
    let Some((path_str, addr_str)) = s.rsplit_once(':') else {
        return Err(format!(
            "Invalid format: '{s}'. Expected 'file:address' (e.g., 'firmware.bin:0x00800000')"
        ));
    };

    if path_str.is_empty() || addr_str.is_empty() {
        return Err(format!(
            "Invalid format: '{s}'. Expected 'file:address' (e.g., 'firmware.bin:0x00800000')"
        ));
    }

    let path = PathBuf::from(path_str);
    let addr = parse_hex_u32(addr_str)?;

    Ok((path, addr))
}

/// Parse hexadecimal address (supports 0x prefix and underscores).
fn parse_hex_u32(s: &str) -> Result<u32, String> {
    let s = s.trim();
    let s = s
        .trim_start_matches("0x")
        .trim_start_matches("0X");
    // Support underscore separators like 0x00_80_00_00
    let s: String = s
        .chars()
        .filter(|c| *c != '_')
        .collect();
    u32::from_str_radix(&s, 16).map_err(|e| format!("Invalid hex address: {e}"))
}

fn main() {
    match run() {
        Ok(()) => {},
        Err(err) => {
            let code = map_exit_code(&err);
            if err
                .downcast_ref::<JsonErrorResponseEmitted>()
                .is_some()
            {
                std::process::exit(code);
            }
            if code == 130 {
                eprintln!(
                    "{} {err}",
                    style("Cancelled:")
                        .yellow()
                        .bold()
                );
            } else {
                eprintln!(
                    "{} {err}",
                    style("Error:")
                        .red()
                        .bold()
                );
            }
            std::process::exit(code);
        },
    }
}

fn run() -> Result<()> {
    install_signal_handler()?;
    clear_interrupted_flag();
    clear_interrupt_flag();

    let raw_args: Vec<String> = env::args().collect();
    let result = run_with_args(&raw_args);

    if was_interrupted() {
        match result {
            Err(err)
                if err
                    .downcast_ref::<CliError>()
                    .is_some() =>
            {
                Err(err)
            },
            _ => Err(CliError::Cancelled(t!("error.interrupted").to_string()).into()),
        }
    } else {
        result
    }
}

/// Parse arguments and handle help/version manually.
///
/// This approach allows clap's `#[arg(env = "...")]` to work normally,
/// while still supporting localized help text.
fn run_with_args(raw_args: &[String]) -> Result<()> {
    // First, set locale from env var (for help output before parsing)
    let locale_from_env = std::env::var("HISIFLASH_LANG").ok();
    let locale = locale_from_env
        .clone()
        .unwrap_or_else(detect_locale);
    rust_i18n::set_locale(&locale);

    // Try to parse arguments (clap will read HISIFLASH_LANG from env automatically)
    let matches = match Cli::command().try_get_matches_from(raw_args.to_owned()) {
        Ok(m) => m,
        Err(err) => {
            match err.kind() {
                // Handle version explicitly for i18n
                ErrorKind::DisplayVersion => {
                    println!("hisiflash {}", env!("CARGO_PKG_VERSION"));
                    return Ok(());
                },
                // Handle help or no subcommand - show localized help
                ErrorKind::DisplayHelp
                | ErrorKind::MissingSubcommand
                | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
                    let mut app = build_localized_command();
                    let _ = app.print_help();
                    return Ok(());
                },
                _ => return Err(CliError::Usage(err.to_string()).into()),
            }
        },
    };

    // Parse CLI struct from matches
    let mut cli = Cli::from_arg_matches(&matches).map_err(|e| CliError::Usage(e.to_string()))?;

    // Note: help/version are handled automatically by clap's ArgAction::Help/ArgAction::Version
    // which trigger localized help via build_localized_command() in error handling

    // --- NO_COLOR and TTY detection (clig.dev best practice) ---
    let stderr_is_tty = console::Term::stderr().is_term();
    STDERR_IS_TTY.store(stderr_is_tty, std::sync::atomic::Ordering::Relaxed);

    if env::var("NO_COLOR").is_ok() || !stderr_is_tty {
        console::set_colors_enabled(false);
        console::set_colors_enabled_stderr(false);
    }

    debug!(
        "Using locale: {}",
        cli.lang
            .as_deref()
            .unwrap_or("en")
    );

    // Setup logging based on verbosity
    let log_level = if cli.quiet {
        "error"
    } else {
        match cli.verbose {
            0 => "warn",
            1 => "info",
            2 => "debug",
            _ => "trace",
        }
    };
    env_logger::Builder::from_env(Env::default().default_filter_or(log_level))
        .format_target(cli.verbose >= 2)
        .format_timestamp(if cli.verbose >= 2 {
            Some(env_logger::TimestampPrecision::Millis)
        } else {
            None
        })
        .init();

    debug!(
        "hisiflash v{} (verbose level: {})",
        env!("CARGO_PKG_VERSION"),
        cli.verbose
    );

    // Load configuration
    let mut config = if let Some(ref path) = cli.config_path {
        Config::load_from_path(path)
    } else {
        Config::load()
    };

    apply_config_defaults(&mut cli, &matches, &config)?;

    match &cli.command {
        Commands::Flash {
            firmware,
            filter,
            late_baud,
            skip_verify,
            monitor,
            monitor_baud,
            monitor_port,
            monitor_clean_output,
            monitor_raw,
        } => {
            let firmware = resolve_firmware(firmware.as_ref(), cli.non_interactive, cli.quiet)?;
            let chip = resolve_effective_chip(&cli, Some(&firmware))?;
            // When `--monitor` is requested without an explicit
            // `--monitor-port`, hand the flasher's still-open serial handle
            // straight over to the monitor. This both avoids a second
            // interactive port picker and preserves the early bootlog the
            // chip emits right after reset (a close/reopen cycle would
            // otherwise drop those bytes). When `--monitor-port` pins a
            // different port, fall back to the regular open path.
            let want_handoff = *monitor && monitor_port.is_none();
            let outcome = cmd_flash(
                &cli,
                &mut config,
                &firmware,
                filter.as_ref(),
                *late_baud,
                *skip_verify,
                chip.into(),
                want_handoff,
            )?;
            if *monitor {
                eprintln!();
                let clean_output = *monitor_clean_output && !*monitor_raw;
                if want_handoff {
                    let flasher = outcome
                        .flasher
                        .expect("cmd_flash must return a live flasher when keep_open is set");
                    match flasher.into_monitor(*monitor_baud) {
                        Ok(session) => {
                            cmd_monitor_with_session(
                                session,
                                &outcome.port,
                                *monitor_baud,
                                false,
                                clean_output,
                                None,
                                true,
                            )?;
                        },
                        Err(err) => {
                            // Handoff failed — close any lingering state and
                            // fall back to opening the port fresh, still
                            // reusing the same port name to avoid a second
                            // picker prompt.
                            log::warn!("Monitor handoff failed, falling back to reopen: {err}");
                            cmd_monitor(
                                &cli,
                                &mut config,
                                Some(
                                    outcome
                                        .port
                                        .as_str(),
                                ),
                                *monitor_baud,
                                false,
                                clean_output,
                                None,
                            )?;
                        },
                    }
                } else {
                    cmd_monitor(
                        &cli,
                        &mut config,
                        monitor_port.as_deref(),
                        *monitor_baud,
                        false,
                        clean_output,
                        None,
                    )?;
                }
            }
        },
        Commands::Write {
            loaderboot,
            bins,
            late_baud,
        } => {
            let chip = resolve_effective_chip(&cli, None)?;
            cmd_write(&cli, &mut config, loaderboot, bins, *late_baud, chip.into())?;
        },
        Commands::WriteProgram {
            loaderboot,
            program,
            address,
            late_baud,
        } => {
            let chip = resolve_effective_chip(&cli, None)?;
            cmd_write_program(
                &cli,
                &mut config,
                loaderboot,
                program.clone(),
                *address,
                *late_baud,
                chip.into(),
            )?;
        },
        Commands::Erase { all } => {
            let chip = resolve_effective_chip(&cli, None)?;
            cmd_erase(&cli, &mut config, *all, chip.into())?;
        },
        Commands::Info { firmware, json } => {
            if *json {
                if let Err(err) = cmd_info(firmware, true) {
                    let code = map_exit_code(&err);
                    emit_structured_json_error("info", code, &err)?;
                    return Err(JsonErrorResponseEmitted { exit_code: code }.into());
                }
            } else {
                cmd_info(firmware, false)?;
            }
        },
        Commands::ListPorts { json } => {
            if *json {
                if let Err(err) = cmd_list_ports(true) {
                    let code = map_exit_code(&err);
                    emit_structured_json_error("list-ports", code, &err)?;
                    return Err(JsonErrorResponseEmitted { exit_code: code }.into());
                }
            } else {
                cmd_list_ports(false)?;
            }
        },
        Commands::Monitor {
            monitor_port,
            monitor_baud,
            timestamp,
            log,
            clean_output,
            raw,
        } => {
            cmd_monitor(
                &cli,
                &mut config,
                monitor_port.as_deref(),
                *monitor_baud,
                *timestamp,
                *clean_output && !*raw,
                log.as_ref(),
            )?;
        },
        Commands::Completions { shell, install } => {
            if *install {
                cmd_completions_install(*shell)?;
            } else {
                let shell = (*shell).ok_or_else(|| {
                    CliError::Usage(
                        "specify a shell type, e.g.: hisiflash completions bash\n  Or use \
                         hisiflash completions --install to auto-install completions."
                            .to_string(),
                    )
                })?;
                cmd_completions(shell);
            }
        },
    }

    Ok(())
}

fn apply_config_defaults(cli: &mut Cli, matches: &clap::ArgMatches, config: &Config) -> Result<()> {
    // Apply default baud from config if not set on command line
    if cli
        .baud
        .is_none()
    {
        if let Some(config_baud) = config
            .port
            .connection
            .baud
        {
            cli.baud = Some(config_baud);
        }
    }

    // Apply default chip from config if not set on command line
    if matches
        .value_source("chip")
        .is_none()
    {
        if let Some(config_chip_name) = config
            .flash
            .chip
            .as_deref()
        {
            let config_chip = Chip::from_config_name(config_chip_name).ok_or_else(|| {
                CliError::Config(
                    t!(
                        "error.invalid_config_chip",
                        chip = config_chip_name,
                        supported = "ws63, bs2x, bs25"
                    )
                    .to_string(),
                )
            })?;
            cli.chip = Some(config_chip);
        }
    }

    match &mut cli.command {
        Commands::Flash {
            late_baud,
            skip_verify,
            ..
        } => {
            if !matches!(
                matches
                    .subcommand()
                    .and_then(|(_, m)| m.value_source("late_baud")),
                Some(ValueSource::CommandLine)
            ) {
                *late_baud = config
                    .flash
                    .late_baud;
            }
            if !matches!(
                matches
                    .subcommand()
                    .and_then(|(_, m)| m.value_source("skip_verify")),
                Some(ValueSource::CommandLine)
            ) {
                *skip_verify = config
                    .flash
                    .skip_verify;
            }
        },
        Commands::Write { late_baud, .. } | Commands::WriteProgram { late_baud, .. }
            if !matches!(
                matches
                    .subcommand()
                    .and_then(|(_, m)| m.value_source("late_baud")),
                Some(ValueSource::CommandLine)
            ) =>
        {
            *late_baud = config
                .flash
                .late_baud;
        },
        _ => {},
    }

    Ok(())
}

fn map_exit_code(err: &anyhow::Error) -> i32 {
    if let Some(json_err) = err.downcast_ref::<JsonErrorResponseEmitted>() {
        return json_err.exit_code;
    }

    // Priority 1: explicit CLI domain errors (stable contract for scripts).
    if let Some(cli_err) = err.downcast_ref::<CliError>() {
        return cli_err.exit_code();
    }

    // Priority 2: library errors mapped to coarse CLI classes.
    // Keep this mapping conservative to avoid breaking automation expectations.
    if let Some(lib_err) = err.downcast_ref::<LibError>() {
        return match lib_err {
            LibError::DeviceNotFound => 4,
            LibError::Config(_) => 3,
            LibError::Unsupported(_) => 5,
            _ => 1,
        };
    }

    // Fallback: unexpected/unclassified failure.
    1
}

fn emit_structured_json_error(command: &str, exit_code: i32, err: &anyhow::Error) -> Result<()> {
    let body = serde_json::json!({
        "ok": false,
        "error": {
            "command": command,
            "exit_code": exit_code,
            "message": err.to_string(),
        }
    });
    println!("{}", serde_json::to_string_pretty(&body)?);
    Ok(())
}

/// Get serial port from CLI args or interactive selection.
pub(crate) fn get_port(cli: &Cli, config: &mut Config) -> Result<String> {
    let options = SerialOptions {
        port: cli
            .port
            .clone(),
        list_all_ports: cli.list_all_ports,
        non_interactive: cli.non_interactive,
        confirm_port: cli.confirm_port,
    };

    let selected = select_serial_port(&options, config)?;

    // Ask to remember if not a known device and interactive mode
    if !selected.is_known && !cli.non_interactive {
        ask_remember_port(&selected.port, config)?;
    }

    Ok(selected
        .port
        .name)
}

#[cfg(test)]
mod locale_tests {
    use crate::help::SUPPORTED_LOCALES;

    /// Helper to test locale matching logic without sys_locale
    fn match_locale(locale: &str) -> String {
        // Normalize the locale string
        let locale = locale
            .split('.')
            .next()
            .unwrap_or(locale);
        let locale = locale.replace('_', "-");

        if SUPPORTED_LOCALES.contains(&locale.as_str()) {
            return locale;
        }

        let lang_code = locale
            .split('-')
            .next()
            .unwrap_or(&locale);
        match lang_code
            .to_lowercase()
            .as_str()
        {
            "zh" => "zh-CN".to_string(),
            _ => "en".to_string(),
        }
    }

    #[test]
    fn test_locale_chinese_variants() {
        assert_eq!(match_locale("zh_CN.UTF-8"), "zh-CN");
        assert_eq!(match_locale("zh_CN"), "zh-CN");
        assert_eq!(match_locale("zh-CN"), "zh-CN");
        assert_eq!(match_locale("zh_TW.UTF-8"), "zh-CN"); // Taiwan -> Simplified (fallback)
        assert_eq!(match_locale("zh"), "zh-CN");
    }

    #[test]
    fn test_locale_english_variants() {
        assert_eq!(match_locale("en_US.UTF-8"), "en");
        assert_eq!(match_locale("en_GB.UTF-8"), "en");
        assert_eq!(match_locale("en_US"), "en");
        assert_eq!(match_locale("en"), "en");
    }

    #[test]
    fn test_locale_posix_defaults() {
        assert_eq!(match_locale("C"), "en");
        assert_eq!(match_locale("POSIX"), "en");
        assert_eq!(match_locale("C.UTF-8"), "en");
    }

    #[test]
    fn test_locale_unsupported_fallback() {
        assert_eq!(match_locale("de_DE.UTF-8"), "en"); // German -> English
        assert_eq!(match_locale("ja_JP.UTF-8"), "en"); // Japanese -> English
        assert_eq!(match_locale("fr_FR"), "en"); // French -> English
    }
}

#[cfg(test)]
mod cli_tests {
    use {
        super::*,
        crate::{
            commands::info::partition_type_str,
            help::{build_localized_command, localize_arg},
        },
        clap::CommandFactory,
        std::sync::Mutex,
    };

    // ---- use_fancy_output ----

    #[test]
    fn test_use_fancy_output_follows_tty_flag() {
        // When STDERR_IS_TTY is false, use_fancy_output should return false.
        STDERR_IS_TTY.store(false, std::sync::atomic::Ordering::Relaxed);
        assert!(!use_fancy_output());
        // Restore to true for other tests.
        STDERR_IS_TTY.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    // ---- parse_hex_u32 additional edge cases ----

    #[test]
    fn test_cli_parse_flash_without_firmware() {
        // firmware is now optional: `flash` alone should parse successfully
        let cli = Cli::try_parse_from(["hisiflash", "flash"]);
        assert!(cli.is_ok());
        if let Commands::Flash { firmware, .. } = &cli
            .unwrap()
            .command
        {
            assert!(firmware.is_none());
        } else {
            panic!("Expected Flash command");
        }
    }

    /// Global lock for `rust_i18n::set_locale` which mutates global state.
    /// Only held during set_locale + command construction; assertions run
    /// lock-free so tests can maximally overlap.
    static LOCALE_LOCK: Mutex<()> = Mutex::new(());

    /// Build a localized command for the given locale.
    /// Holds the lock only during `set_locale` + `build_localized_command`;
    /// the returned `Command` has all i18n strings baked in and is safe
    /// to inspect without the lock.
    fn localized_cmd(locale: &str) -> clap::Command {
        let _lock = LOCALE_LOCK
            .lock()
            .unwrap();
        rust_i18n::set_locale(locale);
        build_localized_command()
    }

    /// Call `localize_arg` under the locale lock.
    fn localized_arg(locale: &str, arg: clap::Arg) -> clap::Arg {
        let _lock = LOCALE_LOCK
            .lock()
            .unwrap();
        rust_i18n::set_locale(locale);
        localize_arg(arg)
    }

    // ---- clap validation ----

    #[test]
    fn test_cli_command_is_valid() {
        // Verifies that all derive macros produce a valid clap Command
        Cli::command().debug_assert();
    }

    #[test]
    fn test_cli_parse_flash() {
        let cli = Cli::try_parse_from([
            "hisiflash",
            "--port",
            "/dev/ttyUSB0",
            "--baud",
            "460800",
            "flash",
            "firmware.fwpkg",
        ]);
        assert!(cli.is_ok());
        let cli = cli.unwrap();
        assert_eq!(
            cli.port
                .as_deref(),
            Some("/dev/ttyUSB0")
        );
        assert_eq!(cli.baud, Some(460800));
    }

    #[test]
    fn test_cli_parse_flash_with_all_options() {
        let cli = Cli::try_parse_from([
            "hisiflash",
            "flash",
            "fw.fwpkg",
            "--filter",
            "app,flashboot",
            "--late-baud",
            "--skip-verify",
            "--monitor",
        ])
        .unwrap();
        if let Commands::Flash {
            firmware,
            filter,
            late_baud,
            skip_verify,
            monitor,
            monitor_baud,
            monitor_port,
            monitor_clean_output,
            monitor_raw,
        } = cli.command
        {
            assert_eq!(
                firmware
                    .unwrap()
                    .to_str()
                    .unwrap(),
                "fw.fwpkg"
            );
            assert_eq!(filter.as_deref(), Some("app,flashboot"));
            assert!(late_baud);
            assert!(skip_verify);
            assert!(monitor);
            assert_eq!(monitor_baud, 115200);
            assert_eq!(monitor_port, None);
            assert!(monitor_clean_output);
            assert!(!monitor_raw);
        } else {
            panic!("Expected Flash command");
        }
    }

    #[test]
    fn test_cli_parse_write() {
        let cli = Cli::try_parse_from([
            "hisiflash",
            "write",
            "--loaderboot",
            "lb.bin",
            "--bin",
            "app.bin:0x00800000",
        ])
        .unwrap();
        if let Commands::Write {
            loaderboot,
            bins,
            late_baud,
        } = cli.command
        {
            assert_eq!(
                loaderboot
                    .to_str()
                    .unwrap(),
                "lb.bin"
            );
            assert_eq!(bins.len(), 1);
            assert_eq!(
                bins[0]
                    .0
                    .to_str()
                    .unwrap(),
                "app.bin"
            );
            assert_eq!(bins[0].1, 0x00800000);
            assert!(!late_baud);
        } else {
            panic!("Expected Write command");
        }
    }

    #[test]
    fn test_cli_parse_write_program() {
        let cli = Cli::try_parse_from([
            "hisiflash",
            "write-program",
            "--loaderboot",
            "lb.bin",
            "program.bin",
            "--address",
            "0x00800000",
        ])
        .unwrap();
        assert!(matches!(cli.command, Commands::WriteProgram { .. }));
    }

    #[test]
    fn test_cli_parse_erase() {
        let cli = Cli::try_parse_from(["hisiflash", "erase", "--all"]).unwrap();
        if let Commands::Erase { all } = cli.command {
            assert!(all);
        } else {
            panic!("Expected Erase command");
        }
    }

    #[test]
    fn test_cli_parse_info() {
        let cli = Cli::try_parse_from(["hisiflash", "info", "firmware.fwpkg"]).unwrap();
        assert!(matches!(cli.command, Commands::Info { json: false, .. }));
    }

    #[test]
    fn test_cli_parse_info_json() {
        let cli = Cli::try_parse_from(["hisiflash", "info", "--json", "firmware.fwpkg"]).unwrap();
        if let Commands::Info { json, .. } = cli.command {
            assert!(json);
        } else {
            panic!("Expected Info command");
        }
    }

    #[test]
    fn test_cli_parse_list_ports() {
        let cli = Cli::try_parse_from(["hisiflash", "list-ports"]).unwrap();
        assert!(matches!(cli.command, Commands::ListPorts { json: false }));
    }

    #[test]
    fn test_cli_parse_list_ports_json() {
        let cli = Cli::try_parse_from(["hisiflash", "list-ports", "--json"]).unwrap();
        if let Commands::ListPorts { json } = cli.command {
            assert!(json);
        } else {
            panic!("Expected ListPorts command");
        }
    }

    #[test]
    fn test_cli_parse_monitor() {
        let cli = Cli::try_parse_from(["hisiflash", "monitor", "--monitor-baud", "9600"]).unwrap();
        if let Commands::Monitor { monitor_baud, .. } = cli.command {
            assert_eq!(monitor_baud, 9600);
        } else {
            panic!("Expected Monitor command");
        }
    }

    #[test]
    fn test_cli_parse_flash_monitor_port() {
        let cli = Cli::try_parse_from([
            "hisiflash",
            "flash",
            "fw.fwpkg",
            "--monitor",
            "--monitor-port",
            "/dev/ttyUSB1",
        ])
        .unwrap();

        if let Commands::Flash { monitor_port, .. } = cli.command {
            assert_eq!(monitor_port.as_deref(), Some("/dev/ttyUSB1"));
        } else {
            panic!("Expected Flash command");
        }
    }

    #[test]
    fn test_cli_parse_monitor_with_monitor_port() {
        let cli = Cli::try_parse_from(["hisiflash", "monitor", "--monitor-port", "/dev/ttyUSB2"])
            .unwrap();

        if let Commands::Monitor { monitor_port, .. } = cli.command {
            assert_eq!(monitor_port.as_deref(), Some("/dev/ttyUSB2"));
        } else {
            panic!("Expected Monitor command");
        }
    }

    #[test]
    fn test_cli_parse_monitor_default_baud() {
        let cli = Cli::try_parse_from(["hisiflash", "monitor"]).unwrap();
        if let Commands::Monitor {
            monitor_baud,
            clean_output,
            raw,
            ..
        } = cli.command
        {
            assert_eq!(monitor_baud, 115200);
            assert!(clean_output);
            assert!(!raw);
        } else {
            panic!("Expected Monitor command");
        }
    }

    #[test]
    fn test_cli_parse_monitor_raw() {
        let cli = Cli::try_parse_from(["hisiflash", "monitor", "--raw"]).unwrap();
        if let Commands::Monitor {
            clean_output, raw, ..
        } = cli.command
        {
            assert!(raw);
            assert!(clean_output);
        } else {
            panic!("Expected Monitor command");
        }
    }

    #[test]
    fn test_cli_parse_completions() {
        let cli = Cli::try_parse_from(["hisiflash", "completions", "bash"]).unwrap();
        assert!(matches!(cli.command, Commands::Completions { .. }));
    }

    #[test]
    fn test_cli_default_values() {
        let cli = Cli::try_parse_from(["hisiflash", "list-ports"]).unwrap();
        assert_eq!(cli.baud, None);
        assert!(
            cli.chip
                .is_none()
        );
        assert!(!cli.quiet);
        assert!(!cli.non_interactive);
        assert!(!cli.confirm_port);
        assert!(!cli.list_all_ports);
        assert!(
            cli.port
                .is_none()
        );
        assert!(
            cli.lang
                .is_none()
        );
        assert!(
            cli.config_path
                .is_none()
        );
        assert_eq!(cli.verbose, 0);
    }

    #[test]
    fn test_cli_global_options() {
        let cli = Cli::try_parse_from([
            "hisiflash",
            "--port",
            "COM3",
            "--baud",
            "115200",
            "--chip",
            "bs2x",
            "--lang",
            "zh-CN",
            "-vv",
            "--quiet",
            "--non-interactive",
            "--confirm-port",
            "--list-all-ports",
            "--config",
            "/tmp/config.toml",
            "list-ports",
        ])
        .unwrap();
        assert_eq!(
            cli.port
                .as_deref(),
            Some("COM3")
        );
        assert_eq!(cli.baud, Some(115200));
        assert_eq!(
            cli.lang
                .as_deref(),
            Some("zh-CN")
        );
        assert_eq!(cli.verbose, 2);
        assert!(cli.quiet);
        assert!(cli.non_interactive);
        assert!(cli.confirm_port);
        assert!(cli.list_all_ports);
    }

    #[test]
    fn test_apply_config_defaults_for_flash() {
        let mut config = Config::default();
        config
            .port
            .connection
            .baud = Some(460800);
        config
            .flash
            .chip = Some("bs2x".to_string());
        config
            .flash
            .late_baud = true;
        config
            .flash
            .skip_verify = true;

        let cmd = Cli::command();
        let matches = cmd
            .try_get_matches_from(["hisiflash", "flash", "firmware.fwpkg"])
            .unwrap();
        let mut cli = Cli::from_arg_matches(&matches).unwrap();

        apply_config_defaults(&mut cli, &matches, &config).unwrap();

        assert_eq!(cli.baud, Some(460800));

        if let Commands::Flash {
            late_baud,
            skip_verify,
            ..
        } = cli.command
        {
            assert!(late_baud);
            assert!(skip_verify);
        } else {
            panic!("Expected Flash command");
        }
    }

    #[test]
    fn test_apply_config_does_not_override_explicit_cli_values() {
        let mut config = Config::default();
        config
            .port
            .connection
            .baud = Some(460800);
        config
            .flash
            .chip = Some("bs2x".to_string());
        config
            .flash
            .late_baud = false;
        config
            .flash
            .skip_verify = true;

        let cmd = Cli::command();
        let matches = cmd
            .try_get_matches_from([
                "hisiflash",
                "--baud",
                "115200",
                "--chip",
                "ws63",
                "flash",
                "firmware.fwpkg",
                "--late-baud",
            ])
            .unwrap();
        let mut cli = Cli::from_arg_matches(&matches).unwrap();

        apply_config_defaults(&mut cli, &matches, &config).unwrap();

        assert_eq!(cli.baud, Some(115200));
        assert_eq!(cli.chip, Some(Chip::Ws63));

        if let Commands::Flash {
            late_baud,
            skip_verify,
            ..
        } = cli.command
        {
            assert!(late_baud);
            assert!(skip_verify);
        } else {
            panic!("Expected Flash command");
        }
    }

    #[test]
    fn test_run_no_args_returns_ok() {
        let args = vec!["hisiflash".to_string()];
        let result = run_with_args(&args);
        // Help output is printed but function should return Ok
        assert!(result.is_ok(), "Expected Ok but got: {result:?}");
    }

    #[test]
    fn test_run_version_returns_ok() {
        let args = vec!["hisiflash".to_string(), "--version".to_string()];
        let result = run_with_args(&args);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_short_version_returns_ok() {
        let args = vec!["hisiflash".to_string(), "-V".to_string()];
        let result = run_with_args(&args);
        assert!(result.is_ok());
    }

    #[test]
    fn test_apply_config_defaults_invalid_chip() {
        let mut config = Config::default();
        config
            .flash
            .chip = Some("unknown-chip".to_string());

        let cmd = Cli::command();
        let matches = cmd
            .try_get_matches_from(["hisiflash", "list-ports"])
            .unwrap();
        let mut cli = Cli::from_arg_matches(&matches).unwrap();

        let result = apply_config_defaults(&mut cli, &matches, &config);
        assert!(result.is_err());
    }

    #[test]
    fn test_guess_chip_from_firmware_path_bs21e() {
        let chip = guess_chip_from_firmware_path(Path::new(
            "output/bs21e/fwpkg/standard-bs21e-1100e/bs21e_all_in_one.fwpkg",
        ));
        assert_eq!(chip, Some(Chip::Bs2x));
    }

    #[test]
    fn test_guess_chip_from_firmware_path_bs25() {
        let chip = guess_chip_from_firmware_path(Path::new("firmware/bs25_release.fwpkg"));
        assert_eq!(chip, Some(Chip::Bs25));
    }

    #[test]
    fn test_guess_chip_from_firmware_path_unknown() {
        let chip = guess_chip_from_firmware_path(Path::new("firmware/all_in_one.fwpkg"));
        assert_eq!(chip, None);
    }

    #[test]
    fn test_resolve_effective_chip_from_firmware_name() {
        let cli = Cli::try_parse_from(["hisiflash", "flash", "bs21e_all_in_one.fwpkg"]).unwrap();
        let chip = resolve_effective_chip(&cli, Some(Path::new("bs21e_all_in_one.fwpkg"))).unwrap();
        assert_eq!(chip, Chip::Bs2x);
    }

    #[test]
    fn test_resolve_effective_chip_non_interactive_unknown_fails() {
        let cli = Cli::try_parse_from(["hisiflash", "--non-interactive", "flash", "unknown.fwpkg"])
            .unwrap();
        let result = resolve_effective_chip(&cli, Some(Path::new("unknown.fwpkg")));
        assert!(result.is_err());
    }

    #[test]
    fn test_map_exit_code_cancelled_is_130() {
        let err = anyhow::Error::new(CliError::Cancelled("cancelled".to_string()));
        assert_eq!(map_exit_code(&err), 130);
    }

    #[test]
    fn test_cli_invalid_chip() {
        let result = Cli::try_parse_from(["hisiflash", "--chip", "invalid_chip", "list-ports"]);
        assert!(result.is_err());
    }

    // ---- parse_bin_arg ----

    #[test]
    fn test_parse_bin_arg_valid() {
        let (path, addr) = parse_bin_arg("app.bin:0x00800000").unwrap();
        assert_eq!(
            path.to_str()
                .unwrap(),
            "app.bin"
        );
        assert_eq!(addr, 0x00800000);
    }

    #[test]
    fn test_parse_bin_arg_no_prefix() {
        let (path, addr) = parse_bin_arg("data.bin:800000").unwrap();
        assert_eq!(
            path.to_str()
                .unwrap(),
            "data.bin"
        );
        assert_eq!(addr, 0x00800000);
    }

    #[test]
    fn test_parse_bin_arg_invalid_no_colon() {
        assert!(parse_bin_arg("app.bin").is_err());
    }

    #[test]
    fn test_parse_bin_arg_windows_path() {
        let (path, addr) = parse_bin_arg("C:\\fw\\app.bin:0x00800000").unwrap();
        assert_eq!(path.to_string_lossy(), "C:\\fw\\app.bin");
        assert_eq!(addr, 0x00800000);
    }

    #[test]
    fn test_parse_bin_arg_invalid_missing_address() {
        assert!(parse_bin_arg("C:\\fw\\app.bin:").is_err());
    }

    #[test]
    fn test_parse_bin_arg_invalid_address() {
        assert!(parse_bin_arg("app.bin:ZZZZ").is_err());
    }

    // ---- parse_hex_u32 ----

    #[test]
    fn test_parse_hex_u32_with_prefix() {
        assert_eq!(parse_hex_u32("0x00800000").unwrap(), 0x00800000);
        assert_eq!(parse_hex_u32("0X00800000").unwrap(), 0x00800000);
    }

    #[test]
    fn test_parse_hex_u32_without_prefix() {
        assert_eq!(parse_hex_u32("DEADBEEF").unwrap(), 0xDEADBEEF);
        assert_eq!(parse_hex_u32("ff").unwrap(), 0xFF);
    }

    #[test]
    fn test_parse_hex_u32_with_underscores() {
        assert_eq!(parse_hex_u32("0x00_80_00_00").unwrap(), 0x00800000);
    }

    #[test]
    fn test_parse_hex_u32_with_whitespace() {
        assert_eq!(parse_hex_u32("  0xFF  ").unwrap(), 0xFF);
    }

    #[test]
    fn test_parse_hex_u32_invalid() {
        assert!(parse_hex_u32("not_hex").is_err());
        assert!(parse_hex_u32("0xGG").is_err());
    }

    #[test]
    fn test_parse_hex_u32_overflow() {
        assert!(parse_hex_u32("0x1FFFFFFFF").is_err());
    }

    #[test]
    fn test_parse_hex_u32_zero() {
        assert_eq!(parse_hex_u32("0x0").unwrap(), 0);
        assert_eq!(parse_hex_u32("0").unwrap(), 0);
    }

    // ---- Chip conversion ----

    #[test]
    fn test_chip_to_chip_family() {
        use hisiflash::ChipFamily;
        assert_eq!(ChipFamily::from(Chip::Ws63), ChipFamily::Ws63);
        assert_eq!(ChipFamily::from(Chip::Bs2x), ChipFamily::Bs2x);
        assert_eq!(ChipFamily::from(Chip::Bs25), ChipFamily::Bs25);
    }

    // ---- partition_type_str ----

    #[test]
    fn test_partition_type_str_values() {
        use hisiflash::PartitionType;
        assert_eq!(partition_type_str(PartitionType::Loader), "Loader");
        assert_eq!(partition_type_str(PartitionType::Normal), "Normal");
        assert_eq!(partition_type_str(PartitionType::KvNv), "KV-NV");
        assert_eq!(partition_type_str(PartitionType::Flashboot), "FlashBoot");
        assert_eq!(partition_type_str(PartitionType::Factory), "Factory");
        assert_eq!(partition_type_str(PartitionType::Unknown(99)), "Unknown");
    }

    // ---- build_localized_command ----

    /// Render a command's short help to a String for assertion.
    fn render_help(cmd: &mut clap::Command) -> String {
        cmd.build();
        cmd.render_help()
            .to_string()
    }

    /// Render a command's long help to a String for assertion.
    fn render_long_help(cmd: &mut clap::Command) -> String {
        cmd.build();
        cmd.render_long_help()
            .to_string()
    }

    /// Get a built subcommand by name from the localized command.
    fn get_subcmd(app: &mut clap::Command, name: &str) -> clap::Command {
        app.build();
        app.get_subcommands()
            .find(|s| s.get_name() == name)
            .unwrap()
            .clone()
    }

    #[test]
    fn test_build_localized_command_creates_valid_command() {
        let cmd = localized_cmd("en");
        // Should be valid
        cmd.clone()
            .debug_assert();
        assert_eq!(cmd.get_name(), "hisiflash");
    }

    #[test]
    fn test_build_localized_command_has_subcommands() {
        let cmd = localized_cmd("en");
        let subcmd_names: Vec<_> = cmd
            .get_subcommands()
            .map(|s| {
                s.get_name()
                    .to_string()
            })
            .collect();
        assert!(subcmd_names.contains(&"flash".to_string()));
        assert!(subcmd_names.contains(&"write".to_string()));
        assert!(subcmd_names.contains(&"erase".to_string()));
        assert!(subcmd_names.contains(&"info".to_string()));
        assert!(subcmd_names.contains(&"list-ports".to_string()));
        assert!(subcmd_names.contains(&"monitor".to_string()));
        assert!(subcmd_names.contains(&"completions".to_string()));
    }

    #[test]
    fn test_build_localized_command_zh_cn() {
        let cmd = localized_cmd("zh-CN");
        cmd.clone()
            .debug_assert();
        // About should be Chinese
        let about = cmd
            .get_about()
            .map(std::string::ToString::to_string)
            .unwrap_or_default();
        assert!(
            about.contains("海思"),
            "About should be in Chinese: {about}"
        );
    }

    // ---- localize_arg ----

    #[test]
    fn test_localize_arg_known_key() {
        let localized = localized_arg("zh-CN", clap::Arg::new("port").help("original help"));
        let help = localized
            .get_help()
            .map(std::string::ToString::to_string)
            .unwrap_or_default();
        // Should be Chinese translation
        assert!(!help.is_empty());
        assert_ne!(help, "original help");
    }

    #[test]
    fn test_localize_arg_unknown_key() {
        let localized = localized_arg(
            "en",
            clap::Arg::new("nonexistent_arg_xyz").help("keep this"),
        );
        let help = localized
            .get_help()
            .map(std::string::ToString::to_string)
            .unwrap_or_default();
        // Should keep original since no translation exists
        assert_eq!(help, "keep this");
    }

    // ---- Regression tests for help localization (all paths) ----

    #[test]
    fn test_main_help_zh_cn_has_localized_headings() {
        let mut app = localized_cmd("zh-CN");
        let help = render_help(&mut app);
        assert!(help.contains("用法:"), "Missing '用法:' heading:\n{help}");
        assert!(help.contains("命令:"), "Missing '命令:' heading:\n{help}");
        assert!(help.contains("选项:"), "Missing '选项:' heading:\n{help}");
    }

    #[test]
    fn test_main_help_en_has_english_headings() {
        let mut app = localized_cmd("en");
        let help = render_help(&mut app);
        assert!(help.contains("USAGE:"), "Missing 'USAGE:' heading:\n{help}");
        assert!(
            help.contains("COMMANDS:"),
            "Missing 'COMMANDS:' heading:\n{help}"
        );
        assert!(
            help.contains("OPTIONS:"),
            "Missing 'OPTIONS:' heading:\n{help}"
        );
    }

    #[test]
    fn test_main_help_zh_cn_command_descriptions() {
        let mut app = localized_cmd("zh-CN");
        let help = render_help(&mut app);
        assert!(
            help.contains("烧录 FWPKG 固件包"),
            "flash description not localized:\n{help}"
        );
        assert!(
            help.contains("列出可用串口"),
            "list-ports description not localized:\n{help}"
        );
        assert!(
            help.contains("打印帮助信息或指定子命令的帮助"),
            "help description not localized:\n{help}"
        );
    }

    #[test]
    fn test_main_help_zh_cn_option_descriptions() {
        let mut app = localized_cmd("zh-CN");
        let help = render_help(&mut app);
        assert!(
            help.contains("使用的串口"),
            "--port help not localized:\n{help}"
        );
        assert!(
            help.contains("数据传输波特率"),
            "--baud help not localized:\n{help}"
        );
        assert!(
            help.contains("打印帮助信息"),
            "--help help not localized:\n{help}"
        );
        assert!(
            help.contains("打印版本信息"),
            "--version help not localized:\n{help}"
        );
    }

    #[test]
    fn test_main_help_no_english_leaks_zh_cn() {
        let mut app = localized_cmd("zh-CN");
        let help = render_help(&mut app);
        // These English strings should NOT appear in zh-CN output
        assert!(
            !help.contains("Print help"),
            "English 'Print help' leaked into zh-CN:\n{help}"
        );
        assert!(
            !help.contains("Print version"),
            "English 'Print version' leaked into zh-CN:\n{help}"
        );
        assert!(
            !help.contains("USAGE:"),
            "English 'USAGE:' leaked into zh-CN:\n{help}"
        );
        assert!(
            !help.contains("Commands:"),
            "English 'Commands:' leaked into zh-CN:\n{help}"
        );
        assert!(
            !help.contains("Options:"),
            "English 'Options:' heading leaked into zh-CN:\n{help}"
        );
    }

    #[test]
    fn test_subcmd_help_flash_zh_cn_has_localized_content() {
        let mut app = localized_cmd("zh-CN");
        let mut sub = get_subcmd(&mut app, "flash");
        let help = render_help(&mut sub);
        assert!(
            help.contains("烧录 FWPKG 固件包"),
            "flash about not localized:\n{help}"
        );
        assert!(
            help.contains("FWPKG 固件文件路径"),
            "firmware arg not localized:\n{help}"
        );
        assert!(
            help.contains("仅烧录指定分区"),
            "filter arg not localized:\n{help}"
        );
    }

    #[test]
    fn test_subcmd_help_flash_zh_cn_has_localized_headings() {
        let mut app = localized_cmd("zh-CN");
        let mut sub = get_subcmd(&mut app, "flash");
        let help = render_help(&mut sub);
        assert!(
            help.contains("用法:"),
            "Missing '用法:' in flash help:\n{help}"
        );
        assert!(
            help.contains("参数:"),
            "Missing '参数:' in flash help:\n{help}"
        );
        assert!(
            help.contains("选项:"),
            "Missing '选项:' in flash help:\n{help}"
        );
    }

    #[test]
    fn test_subcmd_help_flash_en_has_english_headings() {
        let mut app = localized_cmd("en");
        let mut sub = get_subcmd(&mut app, "flash");
        let help = render_long_help(&mut sub);
        assert!(
            help.contains("USAGE:"),
            "Missing 'USAGE:' in flash help:\n{help}"
        );
        assert!(
            help.contains("ARGUMENTS:"),
            "Missing 'ARGUMENTS:' in flash help:\n{help}"
        );
        assert!(
            help.contains("OPTIONS:"),
            "Missing 'OPTIONS:' in flash help:\n{help}"
        );
    }

    #[test]
    fn test_subcmd_help_flash_global_args_propagated() {
        let mut app = localized_cmd("zh-CN");
        let mut sub = get_subcmd(&mut app, "flash");
        let help = render_help(&mut sub);
        // Global options should appear in subcommand help
        assert!(
            help.contains("--port"),
            "Global --port missing from flash help:\n{help}"
        );
        assert!(
            help.contains("--baud"),
            "Global --baud missing from flash help:\n{help}"
        );
        assert!(
            help.contains("--chip"),
            "Global --chip missing from flash help:\n{help}"
        );
        assert!(
            help.contains("-h, --help"),
            "Global -h/--help missing from flash help:\n{help}"
        );
        assert!(
            help.contains("-V, --version"),
            "Global -V/--version missing from flash help:\n{help}"
        );
    }

    #[test]
    fn test_subcmd_help_flash_no_english_leaks_zh_cn() {
        let mut app = localized_cmd("zh-CN");
        let mut sub = get_subcmd(&mut app, "flash");
        let help = render_long_help(&mut sub);
        assert!(
            !help.contains("Print help"),
            "English 'Print help' leaked into flash zh-CN:\n{help}"
        );
        assert!(
            !help.contains("Print version"),
            "English 'Print version' leaked into flash zh-CN:\n{help}"
        );
        assert!(
            !help.contains("Options:"),
            "English 'Options:' heading leaked into flash zh-CN:\n{help}"
        );
        assert!(
            !help.contains("Arguments:"),
            "English 'Arguments:' heading leaked into flash zh-CN:\n{help}"
        );
    }

    #[test]
    fn test_subcmd_erase_no_arguments_heading() {
        // Erase has no positional args — should not have an "参数:" section
        let mut app = localized_cmd("zh-CN");
        let mut sub = get_subcmd(&mut app, "erase");
        let help = render_help(&mut sub);
        assert!(
            !help.contains("参数:\n"),
            "Erase should not have '参数:' heading with no positionals:\n{help}"
        );
    }

    #[test]
    fn test_subcmd_write_zh_cn_localized() {
        let mut app = localized_cmd("zh-CN");
        let mut sub = get_subcmd(&mut app, "write");
        let help = render_help(&mut sub);
        assert!(
            help.contains("将原始二进制文件写入 Flash"),
            "write about not localized:\n{help}"
        );
    }

    #[test]
    fn test_subcmd_completions_zh_cn_localized() {
        let mut app = localized_cmd("zh-CN");
        let mut sub = get_subcmd(&mut app, "completions");
        let help = render_help(&mut sub);
        assert!(
            help.contains("生成 Shell 补全脚本"),
            "completions about not localized:\n{help}"
        );
        assert!(
            help.contains("自动安装补全脚本"),
            "install arg not localized:\n{help}"
        );
    }

    #[test]
    fn test_long_help_has_more_detail_than_short() {
        let mut app = localized_cmd("zh-CN");
        let short = render_help(&mut app);
        let mut app2 = localized_cmd("zh-CN");
        let long = render_long_help(&mut app2);
        assert!(
            long.len() > short.len(),
            "Long help should be longer than short help"
        );
    }

    #[test]
    fn test_subcmd_long_help_has_more_detail_than_short() {
        let mut app = localized_cmd("zh-CN");
        let mut sub_short = get_subcmd(&mut app, "flash");
        let short = render_help(&mut sub_short);
        let mut app2 = localized_cmd("zh-CN");
        let mut sub_long = get_subcmd(&mut app2, "flash");
        let long = render_long_help(&mut sub_long);
        assert!(
            long.len() > short.len(),
            "Flash long help should be longer than short help"
        );
    }

    #[test]
    fn test_all_subcommands_have_localized_about_zh_cn() {
        let mut app = localized_cmd("zh-CN");
        app.build();
        let expected = [
            ("flash", "烧录"),
            ("write", "写入"),
            ("write-program", "写入"),
            ("erase", "擦除"),
            ("info", "显示"),
            ("list-ports", "列出"),
            ("monitor", "监视器"),
            ("completions", "补全"),
            ("help", "帮助"),
        ];
        for (name, keyword) in expected {
            let sub = app
                .get_subcommands()
                .find(|s| s.get_name() == name);
            assert!(sub.is_some(), "Subcommand '{name}' not found");
            let about = sub
                .unwrap()
                .get_about()
                .map(std::string::ToString::to_string)
                .unwrap_or_default();
            assert!(
                about.contains(keyword),
                "Subcommand '{name}' about should contain '{keyword}', got: '{about}'"
            );
        }
    }
}
