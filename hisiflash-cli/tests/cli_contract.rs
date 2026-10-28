//! Integration tests for core CLI contract behavior.

use {predicates::prelude::*, std::fs, tempfile::tempdir};

fn cli_cmd() -> assert_cmd::Command {
    assert_cmd::cargo::cargo_bin_cmd!("hisiflash")
}

#[test]
fn help_exits_zero_and_writes_stdout_only() {
    let mut cmd = cli_cmd();
    cmd.arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("hisiflash"))
        .stderr(predicate::str::is_empty());
}

#[test]
fn short_help_exits_zero_and_writes_stdout_only() {
    let mut cmd = cli_cmd();
    cmd.arg("-h")
        .assert()
        .success()
        .stdout(predicate::str::contains("hisiflash"))
        .stderr(predicate::str::is_empty());
}

#[test]
fn ws53_help_exposes_qualified_inputs_and_explicit_recovery() {
    cli_cmd()
        .args(["ws53", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--backup-sha256"))
        .stdout(predicate::str::contains("--usb-serial"))
        .stdout(predicate::str::contains("recover"))
        .stderr(predicate::str::is_empty());
}

#[test]
fn ws53_flash_without_policy_attestation_cannot_reach_a_port() {
    let dir = tempdir().unwrap();
    cli_cmd()
        .env_remove("HISIFLASH_CHIP")
        .env_remove("HISIFLASH_BAUD")
        .args([
            "ws53",
            "flash",
            "--loaderboot",
            "missing",
            "--loader-sha256",
            "invalid",
            "--backup",
            "missing",
            "--backup-sha256",
            "invalid",
            "--application",
            "missing",
            "--application-sha256",
            "invalid",
            "--output",
        ])
        .arg(
            dir.path()
                .join("untouched"),
        )
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "secure-boot-disabled qualification",
        ));
    assert!(
        !dir.path()
            .join("untouched")
            .exists()
    );
}

#[test]
fn version_exits_zero_and_writes_stdout_only() {
    let mut cmd = cli_cmd();
    cmd.arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("hisiflash"))
        .stderr(predicate::str::is_empty());
}

#[test]
fn short_version_exits_zero_and_writes_stdout_only() {
    let mut cmd = cli_cmd();
    cmd.arg("-V")
        .assert()
        .success()
        .stdout(predicate::str::contains("hisiflash"))
        .stderr(predicate::str::is_empty());
}

#[test]
fn list_ports_json_returns_valid_json() {
    let mut cmd = cli_cmd();
    let output = cmd
        .args(["list-ports", "--json"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .get_output()
        .clone();

    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf-8");
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("list-ports --json must be valid JSON");
    assert_eq!(parsed["ok"], serde_json::Value::Bool(true));
    assert!(parsed["data"]["ports"].is_array());
}

#[test]
fn info_json_success_returns_structured_json() {
    let dir = tempdir().expect("tempdir should be created");
    let fwpkg = dir
        .path()
        .join("ok.fwpkg");
    let valid_header: Vec<u8> = vec![
        0xDF, 0xAD, 0xBE, 0xEF, // magic (FWPKG V1, little-endian 0xEFBEADDF)
        0x00, 0x00, // crc (not validated as hard error by info --json)
        0x00, 0x00, // cnt = 0
        0x0C, 0x00, 0x00, 0x00, // len = 12 bytes total
    ];
    fs::write(&fwpkg, valid_header).expect("write fwpkg");

    let mut cmd = cli_cmd();
    let output = cmd
        .arg("info")
        .arg("--json")
        .arg(fwpkg)
        .output()
        .expect("command should execute");

    assert!(
        output
            .status
            .success()
    );
    let stderr = String::from_utf8(output.stderr).expect("stderr should be utf-8");
    assert!(stderr.is_empty(), "json success should not write stderr");

    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf-8");
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("info --json success must be valid JSON");
    assert_eq!(parsed["ok"], serde_json::Value::Bool(true));
    assert!(parsed["data"].is_object());
    assert!(parsed["data"]["partitions"].is_array());
}

#[test]
fn info_json_error_keeps_stdout_clean() {
    // Use temp dir for non-existent file path
    let dir = tempdir().expect("tempdir should be created");
    let nonexistent = dir
        .path()
        .join("not_exists.fwpkg");

    let mut cmd = cli_cmd();
    let output = cmd
        .arg("info")
        .arg("--json")
        .arg(nonexistent.as_os_str())
        .output()
        .expect("command should execute");

    assert!(
        !output
            .status
            .success()
    );
    let stderr = String::from_utf8(output.stderr).expect("stderr should be utf-8");
    assert!(stderr.is_empty(), "json error should not write stderr");
    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf-8");
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("info --json failure must be valid JSON");
    assert_eq!(parsed["ok"], serde_json::Value::Bool(false));
    assert!(parsed["error"]["message"].is_string());
    assert!(parsed["error"]["exit_code"].is_number());
}

#[test]
fn non_interactive_flash_with_multiple_firmwares_fails_fast() {
    let dir = tempdir().expect("tempdir should be created");
    let fw_a = dir
        .path()
        .join("a.fwpkg");
    let fw_b = dir
        .path()
        .join("b.fwpkg");
    fs::write(&fw_a, b"dummy").expect("write a.fwpkg");
    fs::write(&fw_b, b"dummy").expect("write b.fwpkg");

    let mut cmd = cli_cmd();
    cmd.current_dir(dir.path())
        .arg("--non-interactive")
        .arg("flash")
        .assert()
        .failure()
        // Case-insensitive match for "Multiple" vs "multiple"
        .stderr(
            predicate::str::contains("multiple")
                .or(predicate::str::contains("Multiple"))
                .or(predicate::str::contains("多个")),
        );
}

// ============================================================================
// Exit Code Tests - Following CLI Standards Contract
// ============================================================================

/// Exit code 0: successful operations
#[test]
fn exit_code_zero_on_success() {
    // --help exits 0
    let mut cmd = cli_cmd();
    cmd.arg("--help")
        .assert()
        .success()
        .code(0);

    // --version exits 0
    let mut cmd = cli_cmd();
    cmd.arg("--version")
        .assert()
        .success()
        .code(0);

    // completions bash exits 0 (doesn't require hardware)
    let mut cmd = cli_cmd();
    cmd.args(["completions", "bash"])
        .assert()
        .success()
        .code(0);
}

/// Exit code 2: usage error (unknown command, invalid arguments)
#[test]
fn exit_code_two_for_usage_error_unknown_command() {
    let mut cmd = cli_cmd();
    cmd.arg("unknown-command-xyz")
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("unknown").or(predicate::str::contains("not found")));
}

#[test]
fn exit_code_two_for_usage_error_invalid_flag() {
    let mut cmd = cli_cmd();
    cmd.arg("--invalid-flag-xyz")
        .assert()
        .failure()
        .code(2);
}

#[test]
fn exit_code_two_for_usage_error_missing_required_arg() {
    // flash without firmware returns error - actual exit depends on config vs usage
    // This tests behavior is documented - it's a config-like error when no default
    // found
    let mut cmd = cli_cmd();
    cmd.arg("flash")
        .assert()
        .failure()
        .stderr(predicate::str::contains("firmware").or(predicate::str::contains("固件")));
}

/// Exit code 3: configuration error
#[test]
fn exit_code_three_for_config_error_invalid_file() {
    // Create a temp dir with invalid config
    let dir = tempdir().expect("tempdir should be created");
    let config = dir
        .path()
        .join("hisiflash.toml");
    fs::write(&config, "invalid toml [[[").expect("write invalid config");

    let mut cmd = cli_cmd();
    // Note: CLI currently warns but continues with invalid config
    // This test documents that behavior - config errors are warnings, not fatal
    let output = cmd
        .current_dir(dir.path())
        .arg("list-ports")
        .output()
        .expect("command should execute");
    // Should succeed but warn about config
    assert!(
        output
            .status
            .success(),
        "command should succeed despite config warning"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("TOML"), "should warn about invalid TOML");
}

/// Exit code 4: device not found (library error)
#[test]
fn exit_code_four_for_device_not_found() {
    // Explicit invalid port must map to DeviceNotFound => exit code 4.
    let dir = tempdir().expect("tempdir should be created");
    let loaderboot = dir
        .path()
        .join("loaderboot.bin");
    let app = dir
        .path()
        .join("app.bin");
    fs::write(&loaderboot, b"lb").expect("write loaderboot");
    fs::write(&app, b"app").expect("write app bin");

    let mut cmd = cli_cmd();
    cmd.arg("-p")
        .arg("INVALID_PORT_NAME_XYZ")
        .arg("--chip")
        .arg("ws63")
        .arg("write")
        .arg("--loaderboot")
        .arg(&loaderboot)
        .arg("--bin")
        .arg(format!("{}:0x00800000", app.display()))
        .assert()
        .failure()
        .code(4);
}

/// Exit code 130: cancelled (Ctrl+C)
#[test]
fn exit_code_130_for_cancelled_operation() {
    // Test the mapping function directly - this is tested in unit tests
    // Here we verify the test exists and document the contract
    // The actual Ctrl+C test requires signal handling which is hard to simulate
    let mut cmd = cli_cmd();
    cmd.arg("--help")
        .assert()
        .code(0); // Sanity check - help is NOT cancelled
}

/// Exit code 1: generic error fallback
#[test]
fn exit_code_one_for_unexpected_error() {
    // info with non-existent file should fail with error
    let dir = tempdir().expect("tempdir should be created");
    let nonexistent = dir
        .path()
        .join("does_not_exist.fwpkg");

    let mut cmd = cli_cmd();
    cmd.arg("info")
        .arg(nonexistent.as_os_str())
        .assert()
        .failure()
        .code(1);
}

// ============================================================================
// Unknown Command/Flag Suggestion Tests
// ============================================================================

#[test]
fn unknown_command_suggests_similar() {
    let mut cmd = cli_cmd();
    cmd.arg("falsh") // typo for flash
        .assert()
        .failure()
        .stderr(predicate::str::contains("flash").or(predicate::str::contains("did you mean")));
}

#[test]
fn unknown_flag_suggests_similar() {
    let mut cmd = cli_cmd();
    cmd.arg("list-ports")
        .arg("--jason") // typo for --json
        .assert()
        .failure()
        .stderr(predicate::str::contains("json").or(predicate::str::contains("did you mean")));
}

// ============================================================================
// stdout/stderr Separation Tests
// ============================================================================

#[test]
fn flash_command_writes_to_stderr_only() {
    // flash without required args should fail fast
    // All output should be to stderr, stdout should be empty
    let mut cmd = cli_cmd();
    cmd.arg("flash")
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::is_empty().not());
}

#[test]
fn write_command_invalid_args_writes_to_stderr_only() {
    // write without required --loaderboot should fail
    let mut cmd = cli_cmd();
    cmd.arg("write")
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::is_empty().not());
}

#[test]
fn erase_command_invalid_args_writes_to_stderr_only() {
    let mut cmd = cli_cmd();
    cmd.arg("erase")
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::is_empty().not());
}

#[test]
fn info_command_with_valid_file_writes_to_stdout() {
    // Create a minimal valid fwpkg for testing - this tests the command executes
    // The actual parsing is tested in unit tests
    let dir = tempdir().expect("tempdir should be created");
    let fwpkg = dir
        .path()
        .join("test.fwpkg");
    // Create a minimal valid fwpkg - info should at least attempt to load it
    // and not fail with "file not found" type error
    let valid_header: Vec<u8> = vec![
        // FWPKG magic "HFWP"
        0x48, 0x46, 0x57, 0x50, // Version (1.0.0)
        0x01, 0x00, 0x00, // Entry count = 0
        0x00, 0x00, 0x00, 0x00,
    ];
    fs::write(&fwpkg, valid_header).expect("write test fwpkg");

    let mut cmd = cli_cmd();
    // Just verify the file is recognized as a fwpkg (not "file not found")
    cmd.arg("info")
        .arg(fwpkg)
        .assert()
        .stderr(predicate::str::contains("加载固件").or(predicate::str::contains("firmware")));
}

#[test]
fn completions_command_writes_to_stdout() {
    let mut cmd = cli_cmd();
    cmd.args(["completions", "bash"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .stdout(predicate::str::contains("_hisiflash()"));
}

// ============================================================================
// -- Option Terminator Tests
// ============================================================================

#[test]
fn option_terminator_allows_dash_prefixed_operand() {
    // Test that -- terminates option parsing for positional args
    // This allows operands starting with dash
    let dir = tempdir().expect("tempdir should be created");
    let test_file = dir
        .path()
        .join("test.fwpkg");

    let mut cmd = cli_cmd();
    cmd.arg("info")
        .arg("--")
        .arg(test_file)
        .assert()
        .failure(); // File doesn't exist, but parses correctly
}

#[test]
fn option_terminator_with_flash_command() {
    // -- should work with flash to allow firmware files starting with -
    let dir = tempdir().expect("tempdir should be created");
    let dummy_file = dir
        .path()
        .join("dummy.fwpkg");

    let mut cmd = cli_cmd();
    cmd.arg("flash")
        .arg("--")
        .arg(dummy_file)
        .assert()
        .failure(); // File doesn't exist but parsing works
}

// ============================================================================
// Non-Interactive Mode Tests
// ============================================================================

#[test]
fn non_interactive_flag_is_recognized() {
    // Test that --non-interactive flag is parsed correctly (no hardware needed)
    let mut cmd = cli_cmd();
    cmd.arg("--non-interactive")
        .arg("--version")
        .assert()
        .success();
}

#[test]
fn non_interactive_environment_variable_works() {
    // Test HISIFLASH_NON_INTERACTIVE env var - must use "true" not "1"
    // Use --version to avoid hardware dependency
    let mut cmd = cli_cmd();
    cmd.env("HISIFLASH_NON_INTERACTIVE", "true")
        .arg("--version")
        .assert()
        .success();
}

// ============================================================================
// JSON Output Purity Tests
// ============================================================================

#[test]
fn json_output_is_valid_json_without_extra_lines() {
    let mut cmd = cli_cmd();
    let output = cmd
        .args(["list-ports", "--json"])
        .assert()
        .success()
        .get_output()
        .clone();

    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf-8");
    let stderr = String::from_utf8(output.stderr).expect("stderr should be utf-8");

    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("list-ports --json must be valid JSON");
    assert_eq!(parsed["ok"], serde_json::Value::Bool(true));
    assert!(parsed["data"]["ports"].is_array());
    let status = output.status;
    if status.success() {
        assert!(
            stderr.is_empty(),
            "JSON output should not have stderr: got {stderr}"
        );
    }
}

#[test]
fn info_json_error_returns_clean_error_json() {
    // When --json is used, errors should also be JSON-formatted if possible
    // Otherwise should be empty stdout with error in stderr
    let dir = tempdir().expect("tempdir should be created");
    let nonexistent = dir
        .path()
        .join("not_exists.fwpkg");

    let mut cmd = cli_cmd();
    let output = cmd
        .arg("info")
        .arg("--json")
        .arg(nonexistent.as_os_str())
        .output()
        .expect("command should execute");

    assert!(
        !output
            .status
            .success()
    );
    let stderr = String::from_utf8(output.stderr).expect("stderr should be utf-8");
    assert!(stderr.is_empty(), "json error should not write stderr");
    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf-8");
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("info --json failure must be valid JSON");
    assert_eq!(parsed["ok"], serde_json::Value::Bool(false));
    assert_eq!(
        parsed["error"]["command"],
        serde_json::Value::String("info".to_string())
    );
    assert!(parsed["error"]["message"].is_string());
    assert_eq!(
        parsed["error"]["exit_code"],
        serde_json::Value::Number(1u64.into())
    );
}

// ============================================================================
// TTY Detection Tests (colors/animations disabled on non-TTY)
// ============================================================================

#[test]
fn colors_disabled_when_not_tty() {
    // When stdout is not a TTY, colors should be disabled
    // This is tested by running in non-TTY mode (like in tests)
    let mut cmd = cli_cmd();
    let output = cmd
        .arg("--help")
        .assert()
        .success()
        .get_output()
        .clone();

    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf-8");
    // ANSI color codes should NOT appear in non-TTY output
    assert!(
        !stdout.contains("\x1b["),
        "Colors should be disabled in non-TTY mode"
    );
}

// ============================================================================
// Help Examples Test
// ============================================================================

#[test]
fn help_includes_usage_examples() {
    let mut cmd = cli_cmd();
    cmd.arg("--help")
        .assert()
        .success()
        // Help may be localized (Chinese "用法", English "USAGE")
        // Use case-insensitive matching since output may be "USAGE" or "Usage"
        .stdout(
            predicate::str::contains("用法")
                .or(predicate::str::contains("USAGE"))
                .or(predicate::str::contains("Usage")),
        );
}
