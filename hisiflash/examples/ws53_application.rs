//! A-board candidate02 application experiment. No image packing or generic CLI.
//! The complete plan is generated/validated by hisi-fwpkg, then pinned here.
use hisiflash::{
    CancelContext,
    port::{NativePort, Port, SerialConfig},
    target::ws53::{
        ApplicationSector, ApplicationUpdate, RomHandshake, load_loaderboot, probe_bootrom,
        read_flash, request_loader_reset,
    },
};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const BACKUP_SHA: &str = "c144174e9352e89fd9d70da6641fdbbc8df7a9685a8207c614fc388e2b591e16";
const LOADER_SHA: &str = "b849592b656f5e938bc2184a7d1b8203085ab6c75f5effb85e646e28790ff10c";
const PLAN_SHA: &str = "dce2de510eada5f7938bfc8986be8c45922c7626e6c5add09a4d7bdf7c57faaa";
type AnyResult<T> = Result<T, Box<dyn std::error::Error>>;
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn save(path: impl AsRef<Path>, data: &[u8]) -> AnyResult<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(data)?;
    file.sync_all()?;
    Ok(())
}
fn empty_ready() -> RomHandshake {
    RomHandshake {
        ack: [0; 12],
        trailing: vec![],
    }
}
struct OwnedSector {
    offset: u32,
    original: Vec<u8>,
    replacement: Vec<u8>,
}
fn load_plan(path: &Path, original: &[u8]) -> AnyResult<Vec<OwnedSector>> {
    let manifest = std::fs::read(path.join("sectors.tsv"))?;
    if digest(&manifest) != PLAN_SHA {
        return Err("unqualified candidate plan".into());
    }
    let mut sectors = vec![];
    for (row, expected) in std::str::from_utf8(&manifest)?
        .lines()
        .skip(1)
        .zip([0x5d000, 0x5e000, 0x30000])
    {
        let fields: Vec<_> = row
            .split('\t')
            .collect();
        if fields.len() != 4 || fields[1] != "4096" {
            return Err("bad sector inventory".into());
        }
        let offset = u32::from_str_radix(
            fields[0]
                .strip_prefix("0x")
                .ok_or("offset format")?,
            16,
        )?;
        if offset != expected {
            return Err("unqualified sector ordering".into());
        }
        let before = std::fs::read(path.join(format!("{offset:06x}-original.bin")))?;
        let after = std::fs::read(path.join(format!("{offset:06x}-replacement.bin")))?;
        if before.len() != 4096
            || after.len() != 4096
            || digest(&before) != fields[2]
            || digest(&after) != fields[3]
            || before != original[offset as usize..offset as usize + 4096]
        {
            return Err("plan payload hash/original mismatch".into());
        }
        sectors.push(OwnedSector {
            offset,
            original: before,
            replacement: after,
        });
    }
    if sectors.len() != 3 {
        return Err("incomplete plan".into());
    }
    Ok(sectors)
}
fn read_full<P: Port>(
    port: &mut P,
    mut ready: RomHandshake,
    cancel: &CancelContext,
    label: &str,
) -> AnyResult<Vec<u8>> {
    let mut bytes = Vec::with_capacity(0x400000);
    for offset in (0..0x400000).step_by(0x40000) {
        bytes.extend(read_flash(
            port,
            offset,
            0x40000,
            std::mem::replace(&mut ready, empty_ready()),
            cancel,
        )?);
        println!("READ_CHUNK phase={label} offset={offset:#x}");
        std::io::stdout().flush()?;
    }
    Ok(bytes)
}
fn main() -> AnyResult<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 7 || !matches!(args[6].as_str(), "apply" | "recover" | "check") {
        return Err("usage: ws53_application UART PINNED_LOADER PINNED_FULL_BACKUP PLAN_DIRECTORY NEW_OUTPUT_DIRECTORY apply|recover|check".into());
    }
    let recovery = args[6] == "recover";
    let loader = std::fs::read(&args[2])?;
    let original = std::fs::read(&args[3])?;
    if digest(&loader) != LOADER_SHA
        || original.len() != 0x400000
        || digest(&original) != BACKUP_SHA
    {
        return Err("unqualified loader/backup; no device accessed".into());
    }
    let owned = load_plan(Path::new(&args[4]), &original)?;
    let sectors: Vec<_> = owned
        .iter()
        .map(|s| ApplicationSector {
            offset: s.offset,
            original: &s.original,
            replacement: &s.replacement,
        })
        .collect();
    let plan = ApplicationUpdate::new(&sectors)?;
    // Offline qualification mode is intentionally incapable of opening UART.
    if args[6] == "check" {
        println!("PLAN_VALIDATED sectors=3 header_last=true");
        return Ok(());
    }
    let out = PathBuf::from(&args[5]);
    std::fs::create_dir(&out)?;
    for sector in &sectors {
        save(
            out.join(format!("{:06x}-original.bin", sector.offset)),
            sector.original,
        )?;
        save(
            out.join(format!("{:06x}-replacement.bin", sector.offset)),
            sector.replacement,
        )?;
    }
    // This experimental fixture runs on the qualified Linux hardware host.
    #[cfg(unix)]
    std::fs::File::open(&out)?.sync_all()?;
    let cancel = CancelContext::none();
    let mut port = NativePort::open(&SerialConfig::new(&args[1], 115200))?;
    port.set_dtr(false)?;
    port.set_rts(false)?;
    let rom = probe_bootrom(&mut port, Duration::from_secs(10), &cancel)?;
    println!("SEBOOT_ACK={:02x?}", rom.ack);
    let ready = load_loaderboot(&mut port, &loader, rom, &cancel)?;
    println!("LOADER_ACK={:02x?}", ready.ack);
    let before = read_full(&mut port, ready, &cancel, "before")?;
    save(out.join("before-full.bin"), &before)?;
    // NV can evolve while original firmware runs. Compare every other byte to
    // the qualified original, excluding only planned damaged sectors in recovery.
    for index in 0..0x3fc000 {
        let affected = sectors
            .iter()
            .any(|s| {
                (s.offset as usize
                    ..s.offset as usize
                        + s.original
                            .len())
                    .contains(&index)
            });
        if before[index] != original[index] && !(recovery && affected) {
            return Err(format!("unexpected live byte at {index:#x}; no erase sent").into());
        }
    }
    let mut expected = before.clone();
    for sector in &sectors {
        let bytes = if recovery {
            sector.original
        } else {
            sector.replacement
        };
        expected[sector.offset as usize..sector.offset as usize + bytes.len()]
            .copy_from_slice(bytes);
    }
    save(out.join("expected-full.bin"), &expected)?;
    #[cfg(unix)]
    std::fs::File::open(&out)?.sync_all()?;
    println!(
        "BACKUP_VALIDATED immutable=true mode={} before_sha256={}",
        args[6],
        digest(&before)
    );
    std::io::stdout().flush()?;
    let result = if recovery {
        plan.recover(&mut port, empty_ready(), &cancel)
    } else {
        plan.apply(&mut port, empty_ready(), &cancel)
    };
    if let Err(error) = result {
        eprintln!(
            "UPDATE_FAILED index={} offset={:#x} verified={} may_have_changed={} stage={:?}",
            error.index,
            error.offset,
            error.verified_sectors,
            error.flash_may_have_changed,
            error
                .source
                .stage
        );
        return Err(error.into());
    }
    println!("UPDATE_VERIFIED sectors=3 header_last=true");
    let after = read_full(&mut port, empty_ready(), &cancel, "after")?;
    save(out.join("after-full.bin"), &after)?;
    if after != expected {
        return Err("full Flash comparison failed; no application reset sent".into());
    }
    println!(
        "FULL_VERIFY_OK sha256={} nv_sha256={}",
        digest(&after),
        digest(&after[0x3fc000..])
    );
    // Give the HIL runner time to arm DSLogic before sending serial reset.
    // EOF/anything else refuses reset and leaves a completely verified image.
    println!("VERIFIED_WAIT_BOOT");
    std::io::stdout().flush()?;
    let mut go = String::new();
    std::io::stdin().read_line(&mut go)?;
    if go.trim() != "BOOT" {
        return Err("no BOOT trigger; image verified, reset not sent".into());
    }
    port.set_timeout(Duration::from_millis(50))?;
    request_loader_reset(&mut port, &cancel)?;
    println!("SERIAL_RESET_SENT");
    std::io::stdout().flush()?;
    let start = Instant::now();
    let mut boot = Vec::new();
    let mut buf = [0; 4096];
    let mut at_sent = false;
    while start.elapsed() < Duration::from_secs(10) {
        if recovery && !at_sent && start.elapsed() > Duration::from_secs(3) {
            port.write_all_bytes(b"AT\r\n")?;
            at_sent = true;
        }
        match port.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => boot.extend_from_slice(&buf[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) => {},
            Err(e) => {
                save(out.join("boot-uart.bin"), &boot)?;
                return Err(e.into());
            },
        }
        if boot.len() > 512 * 1024 {
            save(out.join("boot-uart.bin"), &boot)?;
            return Err("boot output limit".into());
        }
    }
    save(out.join("boot-uart.bin"), &boot)?;
    let expected: &[&[u8]] = if recovery {
        &[b"\r\nOK\r\n"]
    } else {
        &[
            b"WS53 FLASH data/bss OK\r\n",
            b"WS53 FLASH watchdog stopped\r\n",
            b"WS53 FLASH PASS irq=53 count=32 gpio17_edges=32 interval_us=10000\r\n",
        ]
    };
    if !expected
        .iter()
        .all(|text| {
            boot.windows(text.len())
                .filter(|window| window == text)
                .count()
                == 1
        })
    {
        return Err(
            "boot transcript did not pass; image readback was verified; inspect before recovery"
                .into(),
        );
    }
    println!(
        "BOOT_VERIFIED mode={} uart_sha256={} cold_boot=false",
        args[6],
        digest(&boot)
    );
    Ok(())
}
