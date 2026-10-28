use super::*;
use clap::Parser;

fn put(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

// Synthetic MCU-mode-1 envelopes; no vendor code, keys or board backups.
pub(super) fn fixture(dir: &Path) -> Options {
    let mut loader = vec![0; 0x7380];
    loader[..0x80].fill(0xff);
    for (offset, value) in [
        (0x80, 0x4bd2f01e),
        (0x84, 0x10000),
        (0x88, 0x100),
        (0x180, 0x4bd2f02d),
        (0x184, 0x10000),
        (0x188, 0x200),
        (0x1a4, 0x7000),
        (0x204, 0x10000),
    ] {
        put(&mut loader, offset, value);
    }
    let hash = Sha256::digest(&loader[0x380..]);
    loader[0x1a8..0x1c8].copy_from_slice(&hash);
    let mut backup = vec![0xff; FLASH_LEN];
    put(&mut backup, 0x380, 0x4b87a52d);
    put(&mut backup, 0x384, 0x10000);
    backup[0x38c..0x392].copy_from_slice(&[20, 0, 3, 0x20, 0x21, 0x10]);
    for (i, (start, len)) in [(0x30000, 0x240000), (0x5d000, 0x213000), (0x3fc000, 0x4000)]
        .into_iter()
        .enumerate()
    {
        put(&mut backup, 0x394 + 8 * i, start);
        put(&mut backup, 0x398 + 8 * i, len);
    }
    for (offset, value) in [
        (0, 0x4b0f2d1e),
        (4, 0x10000),
        (8, 0x100),
        (0x48, 0),
        (0x100, 0x4b0f2d2d),
        (0x104, 0x10000),
        (0x108, 0x200),
        (0x120, 0),
        (0x124, 0x40000),
        (0x148, 0x3c7896e1),
        (0x17c, 0),
    ] {
        put(&mut backup, 0x30000 + offset, value);
    }
    backup[0x30300..0x5d000].fill(0x42);
    backup[0x5d000..0x5d300].fill(0);
    let hash = Sha256::digest(&backup[0x30300..0x70300]);
    backup[0x30128..0x30148].copy_from_slice(&hash);
    let application = vec![0x33; 8192];
    save(dir.join("loader.bin"), &loader).unwrap();
    save(dir.join("backup.bin"), &backup).unwrap();
    save(dir.join("app.bin"), &application).unwrap();
    Options {
        mode: Mode::Check,
        loaderboot: dir.join("loader.bin"),
        loader_sha256: digest(&loader),
        backup: dir.join("backup.bin"),
        backup_sha256: digest(&backup),
        application: dir.join("app.bin"),
        application_sha256: digest(&application),
        output: dir.join("check"),
        usb_serial: None,
        secure_boot_disabled: false,
        reset: false,
        boot_on_stdin: false,
        monitor_seconds: 1,
        expect: None,
    }
}

#[test]
fn offline_check_uses_ws53_plan_and_never_needs_a_port() {
    let dir = tempfile::tempdir().unwrap();
    let options = fixture(dir.path());
    run(&options, Some("/definitely/not/a/serial/port"), None, false).unwrap();
    let plan: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            options
                .output
                .join("plan.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(plan["entry"], 0x45d300);
    let offsets: Vec<_> = plan["sectors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            s["offset"]
                .as_u64()
                .unwrap()
        })
        .collect();
    assert_eq!(offsets, [0x5d000, 0x5e000, 0x5f000, 0x30000]);
    assert_eq!(plan["security_read_by_cli"], false);
    assert_eq!(plan["elf_layout_validated_by_cli"], false);
    assert_eq!(
        std::fs::read_dir(&options.output)
            .unwrap()
            .count(),
        1
    );
    let bytes = std::fs::read(
        options
            .output
            .join("plan.json"),
    )
    .unwrap();
    assert!(run(&options, None, None, false).is_err());
    assert_eq!(
        std::fs::read(
            options
                .output
                .join("plan.json")
        )
        .unwrap(),
        bytes
    );
}

#[test]
fn malformed_inputs_are_rejected_before_output_or_port_access() {
    for fault in 0..7 {
        let dir = tempfile::tempdir().unwrap();
        let mut options = fixture(dir.path());
        match fault {
            0 => options.backup_sha256 = "00".repeat(32),
            1 => options.loader_sha256 = "g".repeat(64),
            2 => {
                let mut loader = std::fs::read(&options.loaderboot).unwrap();
                loader[0x1fc] = 1; // compressed, despite a matching caller hash
                options.loader_sha256 = digest(&loader);
                std::fs::write(&options.loaderboot, loader).unwrap();
            },
            3 => {
                let mut backup = std::fs::read(&options.backup).unwrap();
                backup[0x30148] = 0; // unsupported encryption policy
                options.backup_sha256 = digest(&backup);
                std::fs::write(&options.backup, backup).unwrap();
            },
            4 => {
                let app = b"\x7fELFnot a Flash body";
                options.application_sha256 = digest(app);
                std::fs::write(&options.application, app).unwrap();
            },
            5 => options.reset = true,
            6 => options.expect = Some(String::new()),
            _ => unreachable!(),
        }
        assert!(
            run(&options, Some("/nonexistent"), None, false).is_err(),
            "fault {fault}"
        );
        assert!(
            !options
                .output
                .exists()
        );
    }
}

#[test]
fn live_commands_require_explicit_identity_policy_and_qualified_baud() {
    let dir = tempfile::tempdir().unwrap();
    let mut options = fixture(dir.path());
    for mode in [Mode::Flash, Mode::Recover] {
        options.mode = mode;
        options.secure_boot_disabled = false;
        assert!(run(&options, None, None, false).is_err());
        options.secure_boot_disabled = true;
        assert!(run(&options, Some("/nonexistent"), None, false).is_err());
        options.usb_serial = Some("explicit".into());
        assert!(run(&options, None, None, false).is_err());
        assert!(run(&options, Some("/nonexistent"), Some(921600), false).is_err());
        assert!(run(&options, Some("/nonexistent"), None, true).is_err());
        assert!(
            !options
                .output
                .exists()
        );
        options.usb_serial = None;
    }
}

#[test]
fn preserved_regions_and_same_session_nv_are_checked_in_apply_and_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let options = fixture(dir.path());
    let original = std::fs::read(options.backup).unwrap();
    let application = std::fs::read(options.application).unwrap();
    let plan = plan_unsigned_application(&original, &application).unwrap();
    let mut before = original.clone();
    before[NV_START..].fill(0x12);
    let after = expected_image(&original, &before, &plan, false).unwrap();
    assert_eq!(&after[NV_START..], &before[NV_START..]);
    assert_eq!(&after[0x5d300..0x5d300 + application.len()], &application);
    for offset in [
        0, 0x2000, 0x8000, 0xc000, 0x10000, 0x20000, 0x32000, 0x270000, 0x3f3000, 0x3fa000,
    ] {
        let mut corrupt = before.clone();
        corrupt[offset] ^= 1;
        for recovery in [false, true] {
            assert!(
                expected_image(&original, &corrupt, &plan, recovery).is_err(),
                "{offset:x}"
            );
        }
    }
    for sector in &plan.sectors {
        let start = sector.offset as usize;
        before[start
            ..start
                + sector
                    .original
                    .len()]
            .fill(0xff);
    }
    assert!(expected_image(&original, &before, &plan, false).is_err());
    let restored = expected_image(&original, &before, &plan, true).unwrap();
    assert_eq!(&restored[..NV_START], &original[..NV_START]);
    assert_eq!(&restored[NV_START..], &before[NV_START..]);
}

#[test]
fn recovery_checks_large_unordered_plan_and_rejects_bad_extents() {
    let dir = tempfile::tempdir().unwrap();
    let options = fixture(dir.path());
    let original = std::fs::read(options.backup).unwrap();
    let application = std::fs::read(options.application).unwrap();
    let mut plan = plan_unsigned_application(&original, &application).unwrap();
    // Synthetic extents exercise a large plan and deliberately remain after the
    // header in transaction order. This does not qualify a larger Flash image.
    for index in 0..500 {
        let start = 0x60000 + index * 0x1000;
        plan.sectors
            .push(hisi_fwpkg::ws53::ApplicationSector {
                offset: u32::try_from(start).unwrap(),
                original: original[start..start + 0x1000].to_vec(),
                replacement: vec![0x33; 0x1000],
            });
    }
    let mut before = original.clone();
    for sector in &plan.sectors {
        let start = sector.offset as usize;
        before[start
            ..start
                + sector
                    .original
                    .len()]
            .fill(0x55);
    }
    before[NV_START..].fill(0x12);
    let restored = expected_image(&original, &before, &plan, true).unwrap();
    assert_eq!(&restored[..NV_START], &original[..NV_START]);
    assert_eq!(&restored[NV_START..], &before[NV_START..]);
    assert!(expected_image(&original, &before, &plan, false).is_err());
    before[0x5cfff] ^= 1; // immediately before the application region
    assert!(expected_image(&original, &before, &plan, true).is_err());
    before[0x5cfff] ^= 1;
    plan.sectors
        .last_mut()
        .unwrap()
        .offset = plan.sectors[0].offset;
    assert!(expected_image(&original, &before, &plan, true).is_err());
    plan.sectors
        .last_mut()
        .unwrap()
        .offset = u32::try_from(NV_START).unwrap();
    assert!(expected_image(&original, &before, &plan, true).is_err());
}

#[test]
fn ws53_cli_has_bounded_monitor_and_explicit_boot_gate() {
    let common = [
        "hisiflash",
        "ws53",
        "check",
        "--loaderboot",
        "l",
        "--loader-sha256",
        "h",
        "--backup",
        "b",
        "--backup-sha256",
        "h",
        "--application",
        "a",
        "--application-sha256",
        "h",
        "--output",
        "o",
    ];
    assert!(crate::Cli::try_parse_from(common).is_ok());
    for extra in [
        vec!["--monitor-seconds", "0"],
        vec!["--monitor-seconds", "61"],
        vec!["--boot-on-stdin"],
        vec!["--expect", "PASS"],
    ] {
        assert!(
            crate::Cli::try_parse_from(
                common
                    .into_iter()
                    .chain(extra)
            )
            .is_err()
        );
    }
    assert!(
        crate::Cli::try_parse_from(
            common
                .into_iter()
                .chain(["--reset", "--boot-on-stdin", "--expect", "PASS"])
        )
        .is_ok()
    );
}
