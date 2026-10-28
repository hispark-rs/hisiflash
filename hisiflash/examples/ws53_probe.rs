//! Development-only WS53 probe; optional pinned SDK LoaderBoot RAM transfer.
use hisiflash::{
    CancelContext,
    port::{NativePort, Port, SerialConfig},
    target::ws53::{load_loaderboot, probe_bootrom, read_flash, request_loader_reset},
};
use sha2::{Digest, Sha256};
use std::time::Duration;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args: Vec<String> = std::env::args().collect();
    let reset_requested = args
        .last()
        .is_some_and(|s| s == "--reset");
    if reset_requested {
        args.pop();
    }
    if reset_requested && args.len() < 3 {
        return Err("--reset requires LoaderBoot".into());
    }
    let readback_len = args
        .get(4)
        .map(|s| s.parse::<u32>())
        .transpose()?
        .unwrap_or(4096);
    if readback_len == 0 || readback_len > 0x400000 || readback_len % 4096 != 0 {
        return Err("readback size must be 4 KiB aligned and within 4 MiB".into());
    }
    let loader = args
        .get(2)
        .map(std::fs::read)
        .transpose()?;
    if let Some(data) = &loader {
        // Development fixture only. General WS53 parsing belongs in hisi-fwpkg.
        // SDK 163d9150: FF root fallback + SignSuite=4 + interim 28 KiB body.
        if format!("{:x}", Sha256::digest(data))
            != "b849592b656f5e938bc2184a7d1b8203085ab6c75f5effb85e646e28790ff10c"
        {
            return Err("unqualified WS53 LoaderBoot fixture; rejected before serial I/O".into());
        }
    }
    let path = args
        .get(1)
        .ok_or("usage: ws53_probe /dev/serial/by-id/... (explicit WS53 selection)")?;
    let mut port = NativePort::open(&SerialConfig::new(path, 115_200))?;
    port.set_dtr(false)?;
    port.set_rts(false)?;
    let response = probe_bootrom(&mut port, Duration::from_secs(10), &CancelContext::none())?;
    println!(
        "SEBOOT_ACK={:02x?} trailing={:02x?}; chip identity unverified; flash_written=false",
        response.ack, response.trailing
    );
    if let Some(data) = loader {
        let mut ready = load_loaderboot(&mut port, &data, response, &CancelContext::none())?;
        println!(
            "LOADER_ACK={:02x?} trailing={:02x?}; flash_written=false",
            ready.ack, ready.trailing
        );
        if let Some(path) = args.get(3) {
            use std::io::Write;
            let mut out = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)?;
            let mut hash = Sha256::new();
            let mut offset = 0;
            while offset < readback_len {
                let length = (readback_len - offset).min(0x40000);
                let input = hisiflash::target::ws53::RomHandshake {
                    ack: ready.ack,
                    trailing: std::mem::take(&mut ready.trailing),
                };
                let bytes = read_flash(&mut port, offset, length, input, &CancelContext::none())?;
                out.write_all(&bytes)?;
                out.sync_all()?;
                hash.update(&bytes);
                println!(
                    "READBACK_CHUNK offset={offset:#x} size={length} sha256={:x}",
                    Sha256::digest(&bytes)
                );
                offset += length;
            }
            println!(
                "READBACK_OK physical_offset=0 size={} sha256={:x}; flash_written=false",
                readback_len,
                hash.finalize()
            );
        }
        if reset_requested {
            request_loader_reset(&mut port, &CancelContext::none())?;
            println!("SERIAL_RESET_SENT; startup_not_verified=true");
        }
    }
    Ok(())
}
