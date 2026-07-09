# hisiflash-cli Changelog

All notable changes to `hisiflash-cli` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this crate follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
with pre-release tags.

## [Unreleased]

## [1.0.0-beta.1] - 2026-07-09

### Changed
- Firmware package metadata reporting now reuses the `hisi-fwpkg` parser/partition semantics instead of maintaining a separate fwpkg interpretation in the CLI stack.
- `info --json` now aligns its partition metadata with `hisi-fwpkg plan --format json` for the same fwpkg input.

### Compatibility
- Beta release: GitHub release artifacts are produced for validation, but crates.io publishing remains reserved for the stable `1.0.0` release.
- Requires `hisi-fwpkg` library crate `0.3.0` for the shared parser semantics.

## [1.0.0-alpha.12] - 2026-04-28

### Added
- Added `HISIFLASH_LANG` environment variable support to control help text language (e.g., `HISIFLASH_LANG=en hisiflash --help` shows English).
- `monitor` interactive command now supports timestamp display, log file writing, and shortcut keys (`Ctrl+C`, `Ctrl+R`, `Ctrl+T`).
- `flash` supports interactive firmware auto-discovery/selection when firmware path is omitted.
- Project-level testing/validation docs were added and linked in README (`docs/testing/*`).
- `flash --monitor` now hands off the flasher's open serial handle directly to the monitor, eliminating the close→reopen window that previously dropped the chip's early bootlog after reset. A new `monitor.reusing` status message is shown when handoff succeeds.

### Changed
- Target chip selection no longer falls back silently to ws63; the CLI now infers the chip from the firmware path first and prompts only in interactive TTY sessions.
- Help text and localized messages now describe the shared serial path for BS2X and BS25 targets.
- Refactored CLI argument parsing to use `disable_help_flag` and manual parameter definition for localized help text.
- CLI now reports invalid `flash.chip` values in config explicitly instead of silently falling back.
- Interactive serial selection hints are written to `stderr` for better script compatibility.
- Multi-port serial selection behavior was tightened for non-interactive mode and clearer fallback errors.
- Chip option help text was expanded for better discoverability.
- Terminal-width aware truncation was added for firmware/port labels and prompts to reduce line wrapping.
- Error contract tightened to avoid duplicate error lines for `erase` without `--all`.
- Default runtime logging is quieter (`warn` by default, `info` with `-v`).
- `monitor` now supports clean/raw output modes (`--clean-output` default, `--raw` optional).
- `monitor` output handling was hardened with lossy UTF-8 draining and cleaner line-state management.
- `monitor` Ctrl+R reset validation now uses a two-layer model: reset-signal sent + reset-evidence classification (observed/weak/unconfirmed), instead of relying on generic post-output presence.
- `monitor` status rendering now uses synchronized status-line output to improve alignment under streaming output.
- `monitor` stream contract is now split in non-TTY mode (`stdout` for serial data, `stderr` for status), while TTY mode remains merged for alignment.
- `flash --monitor` no longer prompts for the serial port a second time when multiple ports are present; the port resolved during flash is reused automatically.

### Fixed
- Fixed non-interactive flashing flows to fail fast when the chip cannot be inferred instead of proceeding with an incorrect default target.
- Fixed `hisiflash --help` behavior when no subcommand is provided - now shows help instead of error.
- Fixed `HISIFLASH_LANG` environment variable not taking effect before help output - locale is now set before argument parsing.
- Cancellation now uses a dedicated `Cancelled:` style output instead of generic error wording.
- Flow-control wiring hint is no longer printed after confirmed reset evidence; it now appears only for weak/unconfirmed reset evidence or reset-signal failure.
- Fixed `flash --monitor` double serial-port prompt when multiple ports are detected.

### Compatibility
- User-visible CLI behavior changed (default log verbosity, monitor status line, and some error outputs).
- `flash --monitor` handoff path requires `hisiflash` library crate `0.4.0`.

## Historical Notes

- `v1.0.0` stable has **not** been published yet.
- Prior history was tracked in workspace root `CHANGELOG.md` and mixed CLI/lib changes.

## Release Workflow Template

When preparing a CLI release, use this consistent workflow:

1. Move finished entries from `Unreleased` into a new version section:

	```markdown
	## [1.0.0-alpha.10] - YYYY-MM-DD

	### Changed
	- ...
	```

2. Keep `Unreleased` at the top and empty (or only future items).
3. Ensure `hisiflash-cli/Cargo.toml` version matches the new section.
4. Mark behavior-affecting updates in a `Compatibility` subsection.
5. Add/refresh compare links at the bottom when tags are created.
