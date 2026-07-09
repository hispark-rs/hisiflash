//! Flash, write, and erase command implementations.

use {
    crate::{Cli, CliError, config::Config, get_port, use_fancy_output, was_interrupted},
    anyhow::{Context, Result},
    console::style,
    hisiflash::{ChipFamily, Flasher, Fwpkg},
    indicatif::{ProgressBar, ProgressStyle},
    rust_i18n::t,
    std::path::PathBuf,
};

fn ensure_not_interrupted() -> Result<()> {
    if was_interrupted() {
        Err(CliError::Cancelled(t!("error.interrupted").to_string()).into())
    } else {
        Ok(())
    }
}

/// Outcome of a flash operation.
///
/// `port` is always the serial port name that was actually used. When
/// [`cmd_flash`] is invoked with `keep_open = true`, `flasher` carries the
/// live, post-reset flasher so the caller can hand its serial handle off to
/// the monitor without going through close/reopen (which would otherwise
/// drop the early bootlog).
pub(crate) struct FlashOutcome {
    pub port: String,
    pub flasher: Option<Box<dyn Flasher>>,
}

/// Flash command implementation.
///
/// When `keep_open` is true, the flasher is reset and returned alive (its
/// underlying serial port stays open) so a subsequent `--monitor` step can
/// inherit the handle. Otherwise the flasher is reset and closed before
/// returning, matching the previous behaviour.
#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_flash(
    cli: &Cli,
    config: &mut Config,
    firmware: &PathBuf,
    filter: Option<&String>,
    late_baud: bool,
    skip_verify: bool,
    loaderboot: Option<&PathBuf>,
    chip: ChipFamily,
    keep_open: bool,
) -> Result<FlashOutcome> {
    if !cli.quiet {
        eprintln!(
            "{} {}",
            style("📦").cyan(),
            t!("flash.loading_firmware", path = firmware.display())
        );
    }

    // Load FWPKG
    let fwpkg = Fwpkg::from_file(firmware).with_context(|| {
        t!(
            "error.load_firmware",
            path = firmware
                .display()
                .to_string()
        )
    })?;

    // Verify CRC
    if !skip_verify {
        fwpkg
            .verify_crc()
            .context(t!("error.crc_failed").to_string())?;
        if !cli.quiet {
            eprintln!("{} {}", style("✓").green(), t!("flash.crc_passed"));
        }
    }

    // Show partition info
    if !cli.quiet {
        eprintln!(
            "{} {}",
            style("ℹ").blue(),
            t!("flash.found_partitions", count = fwpkg.partition_count())
        );
        for bin in &fwpkg.bins {
            let type_str = if bin.is_loaderboot() {
                "(LoaderBoot)"
            } else {
                ""
            };
            eprintln!(
                "    {} {} @ 0x{:08X} ({} bytes) {}",
                style("•").dim(),
                bin.name,
                bin.burn_addr,
                bin.length,
                style(type_str).yellow()
            );
        }
    }

    // Get port
    let port = get_port(cli, config)?;
    let effective_baud = crate::resolve_effective_baud(cli.baud, chip);
    if !cli.quiet {
        eprintln!(
            "{} {}",
            style("🔌").cyan(),
            t!("common.using_port", port = port, baud = effective_baud)
        );
    }

    let mut flasher = chip.create_flasher(&port, effective_baud, late_baud, cli.verbose)?;
    if let Err(err) = ensure_not_interrupted() {
        flasher.close();
        return Err(err);
    }

    // Connect
    if !cli.quiet {
        eprintln!("{} {}", style("⏳").yellow(), t!("common.waiting_device"));
    }
    if let Err(err) = flasher.connect() {
        flasher.close();
        return Err(err.into());
    }
    if let Err(err) = ensure_not_interrupted() {
        flasher.close();
        return Err(err);
    }
    if !cli.quiet {
        eprintln!("{} {}", style("✓").green(), t!("common.connected"));
    }

    // Create progress bar
    let pb = if cli.quiet || !use_fancy_output() {
        ProgressBar::hidden()
    } else {
        let pb = ProgressBar::new(100);
        #[allow(clippy::unwrap_used)] // Static template string
        pb.set_style(
            ProgressStyle::default_bar()
                .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}% {msg}")
                .unwrap()
                .progress_chars("#>-"),
        );
        pb.set_draw_target(indicatif::ProgressDrawTarget::stderr());
        pb
    };

    let external_loaderboot = if fwpkg
        .loaderboot()
        .is_none()
    {
        let loaderboot = loaderboot.ok_or_else(|| {
            CliError::Usage(
                "app-only FWPKG has no LoaderBoot partition; pass --loaderboot <loaderboot.bin>"
                    .into(),
            )
        })?;

        if !cli.quiet {
            eprintln!(
                "{} {}",
                style("📦").cyan(),
                t!("write.loading_loaderboot", path = loaderboot.display())
            );
        }

        Some(std::fs::read(loaderboot).with_context(|| {
            t!(
                "error.read_loaderboot",
                path = loaderboot
                    .display()
                    .to_string()
            )
        })?)
    } else {
        None
    };

    // Flash
    let filter_names: Option<Vec<&str>> = filter
        .as_ref()
        .map(|f| {
            f.split(',')
                .collect()
        });
    let filter_slice = filter_names.as_deref();

    let mut current_partition = String::new();

    let flash_result = if let Some(loaderboot) = external_loaderboot.as_deref() {
        let mut bin_data = Vec::new();
        for bin in fwpkg.normal_bins() {
            if let Some(names) = filter_slice {
                if !names
                    .iter()
                    .any(|n| {
                        bin.name
                            .contains(n)
                    })
                {
                    continue;
                }
            }

            bin_data.push((
                fwpkg
                    .bin_data(bin)?
                    .to_vec(),
                bin.burn_addr,
            ));
        }

        if bin_data.is_empty() {
            Err(hisiflash::Error::InvalidFwpkg(
                "No non-LoaderBoot partitions selected for flashing".into(),
            ))
        } else {
            let bins: Vec<(&[u8], u32)> = bin_data
                .iter()
                .map(|(data, addr)| (data.as_slice(), *addr))
                .collect();
            flasher.write_bins(loaderboot, &bins)
        }
    } else {
        flasher.flash_fwpkg(
            &fwpkg,
            filter_slice,
            &mut |name: &str, current: usize, total: usize| {
                if name != current_partition {
                    current_partition = name.to_string();
                    pb.set_message(t!("flash.flashing", name = name).to_string());
                }
                if let Some(pct) = (current * 100).checked_div(total) {
                    pb.set_position(pct as u64);
                }
            },
        )
    };

    if let Err(err) = flash_result {
        flasher.close();
        return Err(err.into());
    }

    if let Err(err) = ensure_not_interrupted() {
        flasher.close();
        return Err(err);
    }

    if cli.quiet || !use_fancy_output() {
        pb.finish_with_message(t!("common.complete").to_string());
    } else {
        pb.finish_and_clear();
        eprintln!("{} {}", style("✓").green(), t!("common.complete"));
    }

    // Reset device
    if !cli.quiet {
        eprintln!("{} {}", style("🔄").cyan(), t!("common.resetting"));
    }
    if let Err(err) = flasher.reset() {
        flasher.close();
        return Err(err.into());
    }

    if !cli.quiet {
        eprintln!(
            "\n{} {}",
            style("🎉")
                .green()
                .bold(),
            t!("flash.completed")
        );
    }

    if keep_open {
        // Hand over the live flasher (port still open, reset already issued)
        // to the caller. The caller is responsible for either invoking
        // `Flasher::into_monitor` or `Flasher::close` to release the handle.
        Ok(FlashOutcome {
            port,
            flasher: Some(flasher),
        })
    } else {
        // Close the underlying serial port to release resources.
        flasher.close();
        Ok(FlashOutcome {
            port,
            flasher: None,
        })
    }
}

/// Write command implementation.
pub(crate) fn cmd_write(
    cli: &Cli,
    config: &mut Config,
    loaderboot: &PathBuf,
    bins: &[(PathBuf, u32)],
    late_baud: bool,
    chip: ChipFamily,
) -> Result<()> {
    if !cli.quiet {
        eprintln!(
            "{} {}",
            style("📦").cyan(),
            t!("write.loading_loaderboot", path = loaderboot.display())
        );
    }

    let lb_data = std::fs::read(loaderboot).with_context(|| {
        t!(
            "error.read_loaderboot",
            path = loaderboot
                .display()
                .to_string()
        )
    })?;

    let mut bin_data: Vec<(Vec<u8>, u32)> = Vec::new();
    for (path, addr) in bins {
        if !cli.quiet {
            eprintln!(
                "{} {}",
                style("📦").cyan(),
                t!(
                    "write.loading_binary",
                    path = path.display(),
                    addr = format!("{:08X}", addr)
                )
            );
        }
        let data = std::fs::read(path).with_context(|| {
            t!(
                "error.read_binary",
                path = path
                    .display()
                    .to_string()
            )
        })?;
        bin_data.push((data, *addr));
    }

    let port = get_port(cli, config)?;
    let effective_baud = crate::resolve_effective_baud(cli.baud, chip);
    if !cli.quiet {
        eprintln!(
            "{} {}",
            style("🔌").cyan(),
            t!("common.using_port", port = port, baud = effective_baud)
        );
    }

    let mut flasher = chip.create_flasher(&port, effective_baud, late_baud, cli.verbose)?;
    if let Err(err) = ensure_not_interrupted() {
        flasher.close();
        return Err(err);
    }

    if !cli.quiet {
        eprintln!("{} {}", style("⏳").yellow(), t!("common.waiting_device"));
    }
    if let Err(err) = flasher.connect() {
        flasher.close();
        return Err(err.into());
    }
    if let Err(err) = ensure_not_interrupted() {
        flasher.close();
        return Err(err);
    }
    if !cli.quiet {
        eprintln!("{} {}", style("✓").green(), t!("common.connected"));
    }

    let bins_ref: Vec<(&[u8], u32)> = bin_data
        .iter()
        .map(|(d, a)| (d.as_slice(), *a))
        .collect();
    if let Err(err) = flasher.write_bins(&lb_data, &bins_ref) {
        flasher.close();
        return Err(err.into());
    }

    if let Err(err) = ensure_not_interrupted() {
        flasher.close();
        return Err(err);
    }

    if let Err(err) = flasher.reset() {
        flasher.close();
        return Err(err.into());
    }
    flasher.close();

    if !cli.quiet {
        eprintln!(
            "\n{} {}",
            style("🎉")
                .green()
                .bold(),
            t!("write.completed")
        );
    }

    Ok(())
}

/// Write program command implementation.
pub(crate) fn cmd_write_program(
    cli: &Cli,
    config: &mut Config,
    loaderboot: &PathBuf,
    program: PathBuf,
    address: u32,
    late_baud: bool,
    chip: ChipFamily,
) -> Result<()> {
    cmd_write(
        cli,
        config,
        loaderboot,
        &[(program, address)],
        late_baud,
        chip,
    )
}

/// Erase command implementation.
pub(crate) fn cmd_erase(cli: &Cli, config: &mut Config, all: bool, chip: ChipFamily) -> Result<()> {
    if !all {
        if !cli.quiet {
            eprintln!("{} {}", style("⚠").yellow(), t!("erase.use_all_flag"));
        }
        return Err(CliError::Usage(t!("erase.need_all_flag").to_string()).into());
    }

    let port = get_port(cli, config)?;
    let effective_baud = crate::resolve_effective_baud(cli.baud, chip);
    if !cli.quiet {
        eprintln!(
            "{} {}",
            style("🔌").cyan(),
            t!("common.using_port", port = port, baud = effective_baud)
        );
    }

    let mut flasher = chip.create_flasher(&port, effective_baud, false, cli.verbose)?;
    if let Err(err) = ensure_not_interrupted() {
        flasher.close();
        return Err(err);
    }

    if !cli.quiet {
        eprintln!("{} {}", style("⏳").yellow(), t!("common.waiting_device"));
    }
    if let Err(err) = flasher.connect() {
        flasher.close();
        return Err(err.into());
    }
    if let Err(err) = ensure_not_interrupted() {
        flasher.close();
        return Err(err);
    }
    if !cli.quiet {
        eprintln!("{} {}", style("✓").green(), t!("common.connected"));
    }

    if !cli.quiet {
        eprintln!("{} {}", style("🗑").red(), t!("erase.erasing"));
    }
    if let Err(err) = flasher.erase_all() {
        flasher.close();
        return Err(err.into());
    }

    if let Err(err) = ensure_not_interrupted() {
        flasher.close();
        return Err(err);
    }
    flasher.close();

    if !cli.quiet {
        eprintln!(
            "\n{} {}",
            style("✓")
                .green()
                .bold(),
            t!("erase.completed")
        );
    }

    Ok(())
}
