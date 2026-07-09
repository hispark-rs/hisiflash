# hisiflash (library) Changelog

All notable changes to the `hisiflash` library crate are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this crate follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed
- Automatically lower the active transfer baud rate and retry non-interrupted
  partition downloads when a YMODEM/SEBOOT transfer fails, improving reliability
  on unstable USB-serial links.

## [0.4.0] - 2026-04-28

### Added
- Experimental shared-serial flashing support for BS2X and BS25 targets via the common SEBOOT transport.
- `MonitorSession::from_serialport(port, baud_rate)` — builds a monitor session from an already-open serial handle; sets baud rate, timeout, and flushes buffers.
- `Port::into_monitor_session(self, baud_rate)` — surrenders the underlying transport to a `MonitorSession` without closing and reopening the file descriptor. `NativePort` provides a real implementation; other ports return `Err(Unsupported)` by default.
- `Flasher::into_monitor(self: Box<Self>, baud_rate)` — consumes the flasher and yields a `MonitorSession`, enabling zero-gap `flash → monitor` transitions. `Ws63Flasher` provides a real implementation; other flashers return `Err(Unsupported)` by default.

### Changed
- SEBOOT stage transitions now preserve prefetched serial bytes across LoaderBoot and partition downloads.
- Post-transfer device readiness handling now waits for the next SEBOOT ACK before continuing on BS2X-style flows.

### Fixed
- Fixed BS21E flashing failures caused by mixed ACK and C responses during YMODEM session shutdown.
- Fixed loss of trailing SEBOOT response bytes when the finish-block ACK and next frame arrived in the same serial read.

### Compatibility
- Additive: new trait methods have default implementations; existing code compiles unchanged.

## [0.3.0] - 2026-03-06

### Added
- Host discovery APIs:
  - `discover_ports`
  - `discover_hisilicon_ports`
  - `auto_detect_port`
- Native monitor primitives:
  - `MonitorSession`
  - `split_utf8`
  - `drain_utf8_lossy`
  - `clean_monitor_text`
  - `format_monitor_output`
- Explicit cancellation context with interrupt propagation mechanism:
  - `set_interrupt_flag` / `clear_interrupt_flag` / `was_interrupted`
  - Support for Ctrl+C interruption during long-running operations

### Changed
- `format_monitor_output` now normalizes standalone `\r`/`\n` line endings more consistently.
- Non-timestamp formatting path now keeps `at_line_start` state aligned with emitted output.
- Monitor text-processing flow was hardened for mixed UTF-8/invalid-byte streams in long-running sessions.
- `Port` trait's `read_cts` and `read_dsr` methods now accept mutable reference to satisfy `serialport` crate requirements.

### Compatibility
- Changes are additive with behavior refinements in monitor text formatting/output helpers.

## Historical Notes

- Prior history was tracked in workspace root `CHANGELOG.md` and mixed CLI/lib changes.

## Release Workflow Template

When preparing a library release, use this consistent workflow:

1. Move finished entries from `Unreleased` into a new version section:

  ```markdown
  ## [0.2.1] - YYYY-MM-DD

  ### Added
  - ...
  ```

2. Keep `Unreleased` at the top and empty (or only future items).
3. Ensure `hisiflash/Cargo.toml` version matches the new section.
4. Call out API-compatibility impact explicitly (`Compatibility` section).
5. Add/refresh compare links at the bottom when tags are created.
