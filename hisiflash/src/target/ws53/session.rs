//! State-checked WS53 transport for a caller-qualified device and image plan.
use super::{
    ApplicationFailure, ApplicationUpdate, RomHandshake, load_loaderboot, probe_bootrom,
    read_flash, request_loader_reset,
};
use crate::{CancelContext, Error, port::Port};
use std::time::Duration;

/// Observable protocol phase. None of these states establishes chip identity,
/// signature validity, application execution, or physical cold boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionPhase {
    /// No successful ROM handshake yet; caller arranges entry/reset.
    Fresh,
    /// CRC-valid ROM handshake; LoaderBoot may be transferred next.
    Rom,
    /// Qualified LoaderBoot transferred and its completion ACK received.
    Loader,
    /// All planned regions were written/read back; full verification pending.
    RegionsVerified,
    /// Entire 4 MiB matches caller's expected image including protected bytes/NV.
    FullVerified,
    /// Reset command written; startup must be observed independently.
    ResetRequested,
    /// Transaction failed; discard session and explicitly reset/reconnect.
    Failed,
}

/// Session failures preserve underlying application-write progress when present.
#[derive(Debug, thiserror::Error)]
pub enum SessionFailure {
    /// Operation was not legal in the current phase; no I/O was attempted.
    #[error("WS53 operation {operation} is not allowed in phase {phase:?}")]
    Phase {
        /// Requested operation.
        operation: &'static str,
        /// Current phase.
        phase: SessionPhase,
    },
    /// Configuration, protocol, cancellation or I/O failure.
    #[error(transparent)]
    Protocol(#[from] Error),
    /// Application-region write/recovery failure with partial progress.
    #[error(transparent)]
    Application(#[from] ApplicationFailure),
}

/// Experimental WS53 session; keeps one serial handle through reset/monitor.
///
/// Caller must select/identify the board, qualify LoaderBoot and image format
/// with hisi-fwpkg, establish security policy without programming it, and save
/// durable backups before `apply` or `recover`. Shared SEBOOT ACK is not a chip
/// ID. There is no chip erase, eFuse programming, automatic reset or baud switch.
/// A failed transaction makes further I/O impossible through this session.
pub struct Ws53Session<P: Port> {
    port: P,
    phase: SessionPhase,
    ready: Option<RomHandshake>,
    cancel: CancelContext,
}

impl<P: Port> Ws53Session<P> {
    /// Adopt a caller-configured 8N1 port. Construction performs no I/O/reset.
    pub fn new(port: P, cancel: CancelContext) -> Self {
        Self {
            port,
            phase: SessionPhase::Fresh,
            ready: None,
            cancel,
        }
    }

    /// Current protocol phase, never a claim of application boot success.
    #[must_use]
    pub fn phase(&self) -> SessionPhase {
        self.phase
    }

    fn require(
        &self,
        operation: &'static str,
        phases: &[SessionPhase],
    ) -> Result<(), SessionFailure> {
        if phases.contains(&self.phase) {
            Ok(())
        } else {
            Err(SessionFailure::Phase {
                operation,
                phase: self.phase,
            })
        }
    }

    fn begin(&mut self) -> RomHandshake {
        self.phase = SessionPhase::Failed;
        self.ready
            .take()
            .unwrap_or(RomHandshake {
                ack: [0; 12],
                trailing: vec![],
            })
    }

    /// Handshake at 115200 after the caller has arranged physical ROM entry.
    pub fn connect(&mut self, timeout: Duration) -> Result<(), SessionFailure> {
        self.require("connect", &[SessionPhase::Fresh])?;
        if timeout.is_zero() || timeout > Duration::from_secs(30) {
            return Err(Error::Config("WS53 handshake timeout must be in (0,30s]".into()).into());
        }
        self.phase = SessionPhase::Failed;
        self.ready = Some(probe_bootrom(&mut self.port, timeout, &self.cancel)?);
        self.phase = SessionPhase::Rom;
        Ok(())
    }

    /// Execute a separately validated/provenanced WS53 LoaderBoot RAM package.
    /// This layer only bounds transport length, not image-format authenticity.
    pub fn download_loader(&mut self, package: &[u8]) -> Result<(), SessionFailure> {
        self.require("download_loader", &[SessionPhase::Rom])?;
        if package.is_empty() || package.len() > 0x10000 {
            return Err(Error::Config("WS53 LoaderBoot package must fit 64 KiB".into()).into());
        }
        let ready = self.begin();
        self.ready = Some(load_loaderboot(
            &mut self.port,
            package,
            ready,
            &self.cancel,
        )?);
        self.phase = SessionPhase::Loader;
        Ok(())
    }

    /// Read an aligned physical Flash range (<=256KiB) without changing phase
    /// on success. Protocol failures require reconnect, including during audit.
    pub fn read_flash(&mut self, offset: u32, length: u32) -> Result<Vec<u8>, SessionFailure> {
        self.require(
            "read_flash",
            &[
                SessionPhase::Loader,
                SessionPhase::RegionsVerified,
                SessionPhase::FullVerified,
            ],
        )?;
        if length == 0
            || length > 0x40000
            || offset % 4 != 0
            || offset
                .checked_add(length)
                .is_none_or(|end| end > 0x400000)
        {
            return Err(Error::Config("WS53 Flash read range invalid".into()).into());
        }
        let previous = self.phase;
        let ready = self.begin();
        let bytes = read_flash(&mut self.port, offset, length, ready, &self.cancel)?;
        self.phase = previous;
        Ok(bytes)
    }

    /// Apply a prevalidated transport plan after durable backup/protection checks.
    /// Reset remains forbidden until `verify_full_image` succeeds.
    pub fn apply(&mut self, plan: &ApplicationUpdate<'_>) -> Result<(), SessionFailure> {
        self.require("apply", &[SessionPhase::Loader])?;
        let ready = self.begin();
        plan.apply(&mut self.port, ready, &self.cancel)?;
        self.phase = SessionPhase::RegionsVerified;
        Ok(())
    }

    /// Explicit recovery in a NEW LoaderBoot session from the retained original
    /// plan. Outside-plan protection and backup authenticity remain caller checks.
    pub fn recover(&mut self, plan: &ApplicationUpdate<'_>) -> Result<(), SessionFailure> {
        self.require("recover", &[SessionPhase::Loader])?;
        let ready = self.begin();
        plan.recover(&mut self.port, ready, &self.cancel)?;
        self.phase = SessionPhase::RegionsVerified;
        Ok(())
    }

    /// Verify all 4 MiB against the planned image and the same-session NV snapshot.
    /// Caller constructs this expected image from the qualified backup/plan.
    /// Reports total verified bytes after each successful chunk; stores no full
    /// image internally. Any mismatch/read failure/cancel forbids reset.
    pub fn verify_full_image(
        &mut self,
        expected: &[u8],
        mut progress: impl FnMut(usize),
    ) -> Result<(), SessionFailure> {
        self.require(
            "verify_full_image",
            &[SessionPhase::RegionsVerified, SessionPhase::FullVerified],
        )?;
        if expected.len() != 0x400000 {
            return Err(
                Error::Config("WS53 full verification requires exactly 4 MiB".into()).into(),
            );
        }
        let mut ready = self.begin();
        for offset in (0..0x400000u32).step_by(0x40000) {
            let bytes = read_flash(&mut self.port, offset, 0x40000, ready, &self.cancel)?;
            ready = RomHandshake {
                ack: [0; 12],
                trailing: vec![],
            };
            let start = offset as usize;
            if bytes != expected[start..start + 0x40000] {
                return Err(Error::Protocol(format!(
                    "WS53 full verification mismatch at {offset:#x}; reset forbidden"
                ))
                .into());
            }
            progress(start + 0x40000);
        }
        self.phase = SessionPhase::FullVerified;
        Ok(())
    }

    /// Send serial reset only after complete verification, never on errors.
    /// Success means command delivery; caller separately observes application.
    pub fn reset_after_verify(&mut self) -> Result<(), SessionFailure> {
        self.require("reset_after_verify", &[SessionPhase::FullVerified])?;
        self.phase = SessionPhase::Failed;
        request_loader_reset(&mut self.port, &self.cancel)?;
        self.phase = SessionPhase::ResetRequested;
        Ok(())
    }

    /// Transfer the SAME live handle to a monitor after requesting reset, so
    /// early boot bytes are not discarded by a close/reopen or buffer clear.
    pub fn into_monitor_port(self) -> Result<P, SessionFailure> {
        self.require("into_monitor_port", &[SessionPhase::ResetRequested])?;
        Ok(self.port)
    }
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
