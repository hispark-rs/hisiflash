//! Experimental WS53 MCU-mode-1 application workflow, separate from WS63.
use anyhow::{Context, Result, bail, ensure};
use clap::{Args, ValueEnum};
use hisi_fwpkg::ws53::{ApplicationPlan, inspect_loader, plan_unsigned_application};
use hisiflash::{
    CancelContext,
    port::{NativePort, Port, SerialConfig},
    target::ws53::{
        ApplicationSector, ApplicationUpdate, SessionFailure, SessionPhase, Ws53Session,
    },
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum Mode {
    Check,
    Flash,
    Recover,
}

/// Explicit caller qualification is required; ROM ACK is not a chip ID.
#[derive(Args)]
pub(crate) struct Options {
    /// Check offline, flash application sectors, or restore the ORIGINAL plan.
    #[arg(value_enum)]
    mode: Mode,
    /// WS53 SDK LoaderBoot envelope (not the raw SDK code).
    #[arg(long)]
    loaderboot: PathBuf,
    /// Independently qualified SHA-256 of the LoaderBoot package.
    #[arg(long)]
    loader_sha256: String,
    /// Original 4 MiB board backup, retained unchanged for recovery.
    #[arg(long)]
    backup: PathBuf,
    /// Independently recorded SHA-256 of the original backup.
    #[arg(long)]
    backup_sha256: String,
    /// Raw Flash-linked application body with entry at 0x45d300; NOT a RAM ELF.
    #[arg(long)]
    application: PathBuf,
    /// SHA-256 of the qualified application body.
    #[arg(long)]
    application_sha256: String,
    /// New directory for durable backups, planned sectors and result logs.
    #[arg(long)]
    output: PathBuf,
    /// Exact USB bridge serial number; required for flash/recover.
    #[arg(long)]
    usb_serial: Option<String>,
    /// Attest an independent read established secure boot disabled on this board.
    /// This flag DOES NOT read or program eFuse/security configuration.
    #[arg(long)]
    secure_boot_disabled: bool,
    /// Send LoaderBoot reset only after full verification, then capture boot UART.
    #[arg(long)]
    reset: bool,
    /// After verification wait up to 30s for stdin BOOT, allowing a logic analyzer to arm.
    #[arg(long, requires = "reset")]
    boot_on_stdin: bool,
    /// Seconds of UART capture after reset (1..60).
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u64).range(1..=60))]
    monitor_seconds: u64,
    /// Require this exact text once in the captured boot log; does not prove cold boot.
    #[arg(long, requires = "reset")]
    expect: Option<String>,
}

const FLASH_LEN: usize = 0x400000;
const CHUNK: u32 = 0x40000;
const NV_START: usize = 0x3fc000;

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn qualified_file(path: &Path, hash: &str, maximum: usize) -> Result<Vec<u8>> {
    ensure!(
        hash.len() == 64
            && hash
                .bytes()
                .all(|b| b.is_ascii_hexdigit()),
        "invalid SHA-256"
    );
    let mut bytes = vec![];
    File::open(path)
        .with_context(|| format!("open {}", path.display()))?
        .take(u64::try_from(maximum)? + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= maximum,
        "input exceeds WS53 bounds: {}",
        path.display()
    );
    ensure!(
        digest(&bytes).eq_ignore_ascii_case(hash),
        "SHA-256 mismatch: {}",
        path.display()
    );
    Ok(bytes)
}

fn save(path: impl AsRef<Path>, bytes: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
#[path = "ws53_tests.rs"]
mod tests;

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn same_port(left: &str, right: &str) -> bool {
    left == right
        || matches!((std::fs::canonicalize(left), std::fs::canonicalize(right)), (Ok(a), Ok(b)) if a == b)
}

fn identify_port(port: &str, expected_serial: &str) -> Result<()> {
    ensure!(!expected_serial.is_empty(), "USB serial must not be empty");
    let matches: Vec<_> = serialport::available_ports()?
        .into_iter()
        .filter(|p| same_port(port, &p.port_name))
        .collect();
    ensure!(
        matches.len() == 1,
        "explicit port must resolve to exactly one enumerated device"
    );
    let serialport::SerialPortType::UsbPort(usb) = &matches[0].port_type else {
        bail!("WS53 profile requires an enumerated USB serial bridge");
    };
    ensure!(
        usb.serial_number
            .as_deref()
            == Some(expected_serial),
        "USB serial does not match selected board connection"
    );
    Ok(())
}

/// Compare all bytes outside mutable NV and, only during recovery, planned sectors.
fn expected_image(
    original: &[u8],
    before: &[u8],
    plan: &ApplicationPlan,
    recovery: bool,
) -> Result<Vec<u8>> {
    ensure!(
        original.len() == FLASH_LEN && before.len() == FLASH_LEN,
        "incomplete full backup"
    );
    // The plan writes the header last and is therefore not address ordered.
    // Sort a copy of its extents for linear verification; do not change the
    // actual transaction order. Only recovery may ignore a planned extent.
    let mut ranges = Vec::with_capacity(
        plan.sectors
            .len(),
    );
    for sector in &plan.sectors {
        let start = sector.offset as usize;
        let end = start
            .checked_add(
                sector
                    .original
                    .len(),
            )
            .context("WS53 sector range overflow")?;
        ensure!(
            start < end
                && end <= NV_START
                && sector
                    .original
                    .len()
                    == sector
                        .replacement
                        .len(),
            "WS53 planned sector outside protected image"
        );
        ranges.push((start, end));
    }
    ranges.sort_unstable();
    let mut cursor = 0;
    for &(start, end) in &ranges {
        ensure!(start >= cursor, "overlapping WS53 planned sectors");
        cursor = end;
    }
    let compare = |start: usize, end: usize| -> Result<()> {
        if let Some(relative) = original[start..end]
            .iter()
            .zip(&before[start..end])
            .position(|(a, b)| a != b)
        {
            let index = start + relative;
            bail!("live backup mismatch at {index:#x}; no erase sent");
        }
        Ok(())
    };
    cursor = 0;
    if recovery {
        for &(start, end) in &ranges {
            compare(cursor, start)?;
            cursor = end;
        }
    }
    compare(cursor, NV_START)?;
    let mut expected = before.to_vec();
    for sector in &plan.sectors {
        let bytes = if recovery {
            &sector.original
        } else {
            &sector.replacement
        };
        let start = sector.offset as usize;
        expected[start..start + bytes.len()].copy_from_slice(bytes);
    }
    Ok(expected)
}

fn wait_for_boot(cancel: &CancelContext) -> Result<()> {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let result = std::io::stdin()
            .read_line(&mut line)
            .map(|_| line);
        let _ = sender.send(result);
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        cancel.check()?;
        ensure!(
            Instant::now() < deadline,
            "BOOT not received within 30s; reset not sent"
        );
        match receiver.recv_timeout(Duration::from_millis(50)) {
            Ok(line) => {
                ensure!(
                    line?.trim() == "BOOT",
                    "expected stdin BOOT; reset not sent"
                );
                return Ok(());
            },
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {},
            Err(error) => return Err(error.into()),
        }
    }
}

fn monitor<P: Port>(port: &mut P, options: &Options, cancel: &CancelContext) -> Result<Vec<u8>> {
    port.set_timeout(Duration::from_millis(50))?;
    let mut log = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(
            options
                .output
                .join("boot-uart.bin"),
        )?;
    let deadline = Instant::now() + Duration::from_secs(options.monitor_seconds);
    let mut data = vec![];
    let mut buffer = [0; 2048];
    let result = (|| -> Result<()> {
        while Instant::now() < deadline {
            cancel.check()?;
            match port.read(&mut buffer) {
                Ok(0) => bail!("serial disconnected during boot observation"),
                Ok(count) => {
                    log.write_all(&buffer[..count])?;
                    ensure!(
                        data.len() + count <= 1024 * 1024,
                        "boot log exceeds 1 MiB bound"
                    );
                    data.extend_from_slice(&buffer[..count]);
                },
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) => {},
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    })();
    log.sync_all()?;
    result?;
    if let Some(marker) = &options.expect {
        ensure!(
            data.windows(marker.len())
                .filter(|w| *w == marker.as_bytes())
                .count()
                == 1,
            "boot marker missing or repeated; inspect boot-uart.bin"
        );
    }
    Ok(data)
}

pub(crate) fn run(
    options: &Options,
    port: Option<&str>,
    baud: Option<u32>,
    chip_selected: bool,
) -> Result<()> {
    run_with_connector(
        options,
        port,
        baud,
        chip_selected,
        &CancelContext::from(hisiflash::is_interrupted_requested as fn() -> bool),
        |name, serial| {
            identify_port(name, serial)?;
            Ok(NativePort::open(&SerialConfig::new(name, 115200))?)
        },
    )
}

// Connector injection stays private, with no CLI option or environment bypass.
// Tests exercise the same validation, persistence and transaction orchestration.
fn run_with_connector<P: Port>(
    options: &Options,
    port: Option<&str>,
    baud: Option<u32>,
    chip_selected: bool,
    cancel: &CancelContext,
    connect: impl FnOnce(&str, &str) -> Result<P>,
) -> Result<()> {
    ensure!(
        !chip_selected,
        "ws53 command explicitly selects WS53; remove global --chip"
    );
    ensure!(
        baud.is_none_or(|b| b == 115200),
        "WS53 qualified transport is 115200 baud"
    );
    ensure!(
        options
            .expect
            .as_ref()
            .is_none_or(|s| !s.is_empty()),
        "boot marker must not be empty"
    );
    if options.mode != Mode::Check {
        ensure!(
            options.secure_boot_disabled,
            "independent secure-boot-disabled qualification is required; no security setting will be modified"
        );
        ensure!(
            port.is_some()
                && options
                    .usb_serial
                    .is_some(),
            "explicit --port and --usb-serial required"
        );
    } else {
        ensure!(!options.reset, "check is offline; --reset is invalid");
    }
    let loader = qualified_file(&options.loaderboot, &options.loader_sha256, 0x10000)?;
    inspect_loader(&loader)?;
    let original = qualified_file(&options.backup, &options.backup_sha256, FLASH_LEN)?;
    let application = qualified_file(&options.application, &options.application_sha256, 0x213000)?;
    ensure!(
        !application.starts_with(b"\x7fELF"),
        "supply a qualified Flash-linked raw body, not ELF/RAM firmware"
    );
    let image_plan = plan_unsigned_application(&original, &application)?;
    let sectors: Vec<_> = image_plan
        .sectors
        .iter()
        .map(|s| ApplicationSector {
            offset: s.offset,
            original: &s.original,
            replacement: &s.replacement,
        })
        .collect();
    let update = ApplicationUpdate::new(&sectors)?;
    let inventory: Vec<_> = sectors
        .iter()
        .map(|s| {
            json!({"offset":s.offset,"length":s.original.len(),
        "original_sha256":digest(s.original),"replacement_sha256":digest(s.replacement)})
        })
        .collect();
    // No output overwrite and no device I/O before complete offline validation.
    let mut directory = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        directory.mode(0o700);
    }
    directory.create(&options.output)?;
    let metadata = json!({"schema":1,"profile":"ws53-mcu-mode1-4mib","mode":format!("{:?}", options.mode),
        "entry":image_plan.entry,"backup_sha256":digest(&original),"loader_sha256":digest(&loader),
        "application_sha256":digest(&application),"sectors":inventory,"usb_serial":options.usb_serial,
        "secure_boot_disabled_caller_attested":options.secure_boot_disabled,"security_read_by_cli":false,
        "elf_layout_validated_by_cli":false,"port":port});
    save(
        options
            .output
            .join("plan.json"),
        &serde_json::to_vec_pretty(&metadata)?,
    )?;
    if options.mode == Mode::Check {
        sync_directory(&options.output)?;
        println!("{}", serde_json::to_string_pretty(&metadata)?);
        return Ok(());
    }
    save(
        options
            .output
            .join("original-full.bin"),
        &original,
    )?;
    save(
        options
            .output
            .join("application.bin"),
        &application,
    )?;
    save(
        options
            .output
            .join("loaderboot.bin"),
        &loader,
    )?;
    for sector in &sectors {
        save(
            options
                .output
                .join(format!("{:06x}-original.bin", sector.offset)),
            sector.original,
        )?;
        save(
            options
                .output
                .join(format!("{:06x}-replacement.bin", sector.offset)),
            sector.replacement,
        )?;
    }
    let out = options
        .output
        .canonicalize()?;
    let recovery = json!({"requires_rom_reentry":true,"automatic_retry":false,"argv":[
        std::env::current_exe()?, "--port", port, "ws53", "recover",
        "--usb-serial",options.usb_serial,"--secure-boot-disabled",
        "--loaderboot",out.join("loaderboot.bin"),"--loader-sha256",digest(&loader),
        "--backup",out.join("original-full.bin"),"--backup-sha256",digest(&original),
        "--application",out.join("application.bin"),"--application-sha256",digest(&application),
        "--output",out.join("recovery"),"--reset"
    ]});
    save(
        out.join("recovery-command.json"),
        &serde_json::to_vec_pretty(&recovery)?,
    )?;
    sync_directory(&options.output)?;
    // Port identity is checked again immediately before opening. USB serial
    // identifies a bridge, NOT the attached CPU. The command selects WS53.
    sync_directory(
        out.parent()
            .context("output directory has no parent")?,
    )?;
    let port_name = port.context("missing selected port")?;
    cancel.check()?;
    let port = connect(
        port_name,
        options
            .usb_serial
            .as_deref()
            .context("missing USB serial")?,
    )?;
    let mut session = Ws53Session::new(port, cancel.clone());
    let mut verified_regions = 0;
    let result = (|| -> Result<serde_json::Value> {
        eprintln!("WS53: enter BootROM now; no automatic reset is attempted");
        session.connect(Duration::from_secs(10))?;
        session.download_loader(&loader)?;
        let mut before = Vec::with_capacity(FLASH_LEN);
        for offset in (0..u32::try_from(FLASH_LEN)?).step_by(CHUNK as usize) {
            before.extend(session.read_flash(offset, CHUNK)?);
            eprintln!("WS53 backup bytes={}", before.len());
        }
        save(
            options
                .output
                .join("before-full.bin"),
            &before,
        )?;
        let expected = expected_image(
            &original,
            &before,
            &image_plan,
            options.mode == Mode::Recover,
        )?;
        save(
            options
                .output
                .join("expected-full.bin"),
            &expected,
        )?;
        sync_directory(&options.output)?;
        if options.mode == Mode::Recover {
            session.recover(&update)?;
        } else {
            session.apply(&update)?;
        }
        verified_regions = sectors.len();
        session.verify_full_image(&expected, |n| eprintln!("WS53 verified bytes={n}"))?;
        let verified = json!({"full_readback_verified":true,"expected_sha256":digest(&expected),
            "before_sha256":digest(&before),"nv_before_and_after_sha256":digest(&before[NV_START..]),
            "reset_requested":false,"cold_boot_verified":false});
        save(
            options
                .output
                .join("verified.json"),
            &serde_json::to_vec_pretty(&verified)?,
        )?;
        sync_directory(&options.output)?;
        println!("FULL_VERIFY_OK");
        std::io::stdout().flush()?;
        Ok(verified)
    })();
    let mut report = match result {
        Ok(report) => report,
        Err(error) => {
            let application_failure = match error.downcast_ref::<SessionFailure>() {
                Some(SessionFailure::Application(failure)) => Some(failure),
                _ => None,
            };
            let failure_report = json!({
                "progress_scope":"current_invocation",
                "error":format!("{error:#}"),"phase":format!("{:?}", session.phase()),
                "reset_requested":false,"recovery_requires_new_session":true,
                "full_readback_verified":session.phase() == SessionPhase::FullVerified,
                "flash_may_have_changed":verified_regions != 0 || application_failure.is_some_and(|f| f.flash_may_have_changed),
                "verified_sectors":application_failure.map_or(verified_regions, |f| f.verified_sectors),
                "failed_sector_index":application_failure.map(|f| f.index),
                "failed_sector_offset":application_failure.map(|f| f.offset),
                "write_stage":application_failure.map(|f| format!("{:?}", f.source.stage)),
            });
            if let Err(log_error) = save(
                options
                    .output
                    .join("failure.json"),
                &serde_json::to_vec_pretty(&failure_report)?,
            ) {
                return Err(error.context(format!(
                    "WS53 failed without reset; could not save failure.json: {log_error:#}; progress={failure_report}"
                )));
            }
            return Err(error.context("WS53 failed; no reset sent; retain all backups and explicitly recover after reconnect"));
        },
    };
    if options.reset {
        let boot_result = (|| -> Result<()> {
            if options.boot_on_stdin {
                wait_for_boot(cancel)?;
            }
            // Failed serial writes can partially deliver reset. Persist intent
            // before sending and distinguish it from successful delivery.
            report["reset_attempted"] = json!(true);
            save(
                options
                    .output
                    .join("reset-intent.json"),
                &serde_json::to_vec_pretty(&report)?,
            )?;
            sync_directory(&options.output)?;
            session.reset_after_verify()?;
            report["reset_requested"] = json!(true);
            let mut port = session.into_monitor_port()?;
            let boot = monitor(&mut port, options, cancel)?;
            report["boot_uart_sha256"] = json!(digest(&boot));
            report["expected_boot_marker_observed_once"] = json!(
                options
                    .expect
                    .is_some()
            );
            Ok(())
        })();
        if let Err(error) = boot_result {
            report["error"] = json!(format!("{error:#}"));
            if let Err(log_error) = save(
                options
                    .output
                    .join("boot-failure.json"),
                &serde_json::to_vec_pretty(&report)?,
            ) {
                return Err(error.context(format!(
                    "WS53 boot observation failed; could not save boot-failure.json: {log_error:#}; progress={report}"
                )));
            }
            return Err(
                error.context("Flash verified; reset/boot observation failed; no automatic retry")
            );
        }
    }
    save(
        options
            .output
            .join("result.json"),
        &serde_json::to_vec_pretty(&report)?,
    )?;
    sync_directory(&options.output)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

#[cfg(test)]
#[path = "ws53_transport_tests.rs"]
mod transport_tests;
