//! Qualified A-board original-sector rewrite/recovery experiment, not an app packer.
use hisiflash::{
    CancelContext,
    port::{NativePort, Port, SerialConfig},
    target::ws53::{
        RomHandshake, load_loaderboot, probe_bootrom, read_flash, replace_application_sectors,
        request_loader_reset,
    },
};
use sha2::{Digest, Sha256};
use std::{io::Write, path::PathBuf, time::Duration};
fn save(path: PathBuf, bytes: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 6 || !matches!(args[5].as_str(), "rewrite" | "recover") {
        return Err("usage: ws53_rewrite_sector UART PINNED_LOADER PINNED_FULL_BACKUP NEW_OUTPUT_DIRECTORY rewrite|recover".into());
    }
    let loader = std::fs::read(&args[2])?;
    let original = std::fs::read(&args[3])?;
    if format!("{:x}", Sha256::digest(&loader))
        != "b849592b656f5e938bc2184a7d1b8203085ab6c75f5effb85e646e28790ff10c"
        || original.len() != 0x400000
        || format!("{:x}", Sha256::digest(&original))
            != "c144174e9352e89fd9d70da6641fdbbc8df7a9685a8207c614fc388e2b591e16"
    {
        return Err("unqualified loader/backup; no device accessed".into());
    }
    let out = PathBuf::from(&args[4]);
    std::fs::create_dir(&out)?;
    let target = &original[0x60000..0x61000];
    // Durable recovery payload is created before even entering the BootROM.
    save(out.join("restore-sector.bin"), target)?;
    let cancel = CancelContext::none();
    let mut port = NativePort::open(&SerialConfig::new(&args[1], 115200))?;
    port.set_dtr(false)?;
    port.set_rts(false)?;
    let response = probe_bootrom(&mut port, Duration::from_secs(10), &cancel)?;
    println!("SEBOOT_ACK={:02x?}", response.ack);
    let mut ready = load_loaderboot(&mut port, &loader, response, &cancel)?;
    println!("LOADER_ACK={:02x?}", ready.ack);
    let input = RomHandshake {
        ack: ready.ack,
        trailing: std::mem::take(&mut ready.trailing),
    };
    let live = read_flash(&mut port, 0x60000, 4096, input, &cancel)?;
    save(out.join("before-sector.bin"), &live)?;
    let nv_ready = RomHandshake {
        ack: ready.ack,
        trailing: vec![],
    };
    let nv_before = read_flash(&mut port, 0x3fc000, 0x4000, nv_ready, &cancel)?;
    save(out.join("before-nv.bin"), &nv_before)?;
    if args[5] == "rewrite" && live != target {
        return Err("stale original backup; no erase sent".into());
    }
    println!(
        "WRITE_BEGIN offset=0x60000 length=4096 original_sha256={:x} live_sha256={:x}",
        Sha256::digest(target),
        Sha256::digest(&live)
    );
    if let Err(error) =
        replace_application_sectors(&mut port, 0x60000, &live, target, ready, &cancel)
    {
        eprintln!(
            "WRITE_FAILED may_have_changed={} stage={:?}",
            error.flash_may_have_changed, error.stage
        );
        return Err(error.into());
    }
    println!("WRITE_VERIFIED offset=0x60000 length=4096");
    let mut restored = Vec::with_capacity(0x400000);
    for offset in (0..0x400000).step_by(0x40000) {
        let ready = RomHandshake {
            ack: [0; 12],
            trailing: vec![],
        };
        restored.extend(read_flash(&mut port, offset, 0x40000, ready, &cancel)?);
        println!("VERIFY_CHUNK offset={offset:#x}");
    }
    save(out.join("after-full.bin"), &restored)?;
    if restored[..0x3fc000] != original[..0x3fc000] {
        return Err("full immutable Flash comparison failed; do not reset to application".into());
    }
    if restored[0x3fc000..] != nv_before {
        return Err("NV changed during LoaderBoot transaction; no application reset sent".into());
    }
    println!(
        "NV_SESSION_VERIFY_OK sha256={:x}",
        Sha256::digest(&nv_before)
    );
    println!(
        "FULL_VERIFY_OK sha256={:x} nv_equal={}",
        Sha256::digest(&restored),
        restored[0x3fc000..] == original[0x3fc000..]
    );
    request_loader_reset(&mut port, &cancel)?;
    println!("SERIAL_RESET_SENT");
    Ok(())
}
