//! Transport-only update ordering/recovery; image format validation is external.
use super::{RomHandshake, WriteFailure, WriteStage, read_flash, write};
use crate::{CancelContext, Error, Result, port::Port};

/// One durable original/replacement pair supplied by a qualified image planner.
/// Offsets are physical Flash offsets, not CPU-mapped addresses.
#[derive(Debug)]
pub struct ApplicationSector<'a> {
    /// Whole-sector start within the qualified application/header windows.
    pub offset: u32,
    /// Original bytes saved before the first modification (not a partial image).
    pub original: &'a [u8],
    /// Validated replacement bytes produced by the external image planner.
    pub replacement: &'a [u8],
}

/// Validated transport plan: non-overlapping ascending application writes,
/// followed by exactly one combined-header sector. Never modifies the plan.
///
/// This does NOT inspect signatures, combined hashes, partitions or chip ID.
/// The caller must qualify those with hisi-fwpkg, check security policy, and
/// durably save every original sector before connecting to the device.
#[derive(Debug)]
pub struct ApplicationUpdate<'a> {
    sectors: &'a [ApplicationSector<'a>],
    lengths: Vec<u32>,
}

/// Failure leaves the boot state unknown; no automatic retry/reset/rollback.
#[derive(Debug, thiserror::Error)]
#[error(
    "WS53 application sector {index} at {offset:#x} failed after {verified_sectors} verified sectors (Flash may have changed: {flash_may_have_changed}): {source}"
)]
pub struct ApplicationFailure {
    /// Index in the validated plan.
    pub index: usize,
    /// Physical offset of the failed transaction.
    pub offset: u32,
    /// Number verified in this invocation before the failure.
    pub verified_sectors: usize,
    /// Includes earlier completed writes, even if this sector failed precheck.
    pub flash_may_have_changed: bool,
    /// Underlying transaction stage and cause.
    #[source]
    pub source: WriteFailure,
}

fn ready() -> RomHandshake {
    RomHandshake {
        ack: [0; 12],
        trailing: vec![],
    }
}

impl<'a> ApplicationUpdate<'a> {
    /// Validate every range and preserved byte before any device I/O.
    /// Requires at least one application write and a final 4KiB header write.
    /// Original/replacement bytes must come from an independently qualified
    /// image plan; this constructor validates transport preservation only.
    pub fn new(sectors: &'a [ApplicationSector<'a>]) -> Result<Self> {
        if sectors.len() < 2
            || sectors
                .last()
                .is_none_or(|s| s.offset != 0x30000)
        {
            return Err(Error::Config(
                "WS53 application plan needs body then final header".into(),
            ));
        }
        let mut end = 0x5d000;
        let mut lengths = Vec::with_capacity(sectors.len());
        for (index, sector) in sectors
            .iter()
            .enumerate()
        {
            let length = write::validate(sector.offset, sector.original, sector.replacement)?;
            lengths.push(length);
            if index + 1 != sectors.len() {
                if sector.offset < end {
                    return Err(Error::Config(
                        "WS53 application sectors overlap or are unordered".into(),
                    ));
                }
                end = sector.offset + length;
            }
        }
        Ok(Self { sectors, lengths })
    }

    /// Compare ALL planned original bytes before the first erase; then write
    /// and independently verify each region, with the combined header last.
    /// Rechecks each region immediately before D2. Does not reset the board.
    /// Caller must also verify protected bytes outside the plan and same-session
    /// NV after completion before allowing application boot.
    pub fn apply<P: Port>(
        &self,
        port: &mut P,
        initial: RomHandshake,
        cancel: &CancelContext,
    ) -> std::result::Result<(), ApplicationFailure> {
        self.execute(port, initial, cancel, false)
    }

    /// Explicit recovery using the ORIGINAL qualified plan, after reconnecting
    /// to BootROM/LoaderBoot. Restores all planned regions and the header last,
    /// including original preserved bytes lost by an interrupted sector erase.
    ///
    /// Never regenerate the original backup from damaged live contents. Caller
    /// must authenticate the retained original/proposed pair and verify that
    /// bytes outside the planned regions still match the expected board image.
    /// The current contents are read and rechecked before each recovery erase.
    /// No automatic application reset; success is byte recovery, not boot proof.
    pub fn recover<P: Port>(
        &self,
        port: &mut P,
        initial: RomHandshake,
        cancel: &CancelContext,
    ) -> std::result::Result<(), ApplicationFailure> {
        self.execute(port, initial, cancel, true)
    }

    fn execute<P: Port>(
        &self,
        port: &mut P,
        mut initial: RomHandshake,
        cancel: &CancelContext,
        recovery: bool,
    ) -> std::result::Result<(), ApplicationFailure> {
        let failure =
            |index: usize, verified_sectors: usize, source: WriteFailure| ApplicationFailure {
                index,
                offset: self.sectors[index].offset,
                verified_sectors,
                flash_may_have_changed: verified_sectors != 0 || source.flash_may_have_changed,
                source,
            };
        let mut live = Vec::with_capacity(
            self.sectors
                .len(),
        );
        for (index, sector) in self
            .sectors
            .iter()
            .enumerate()
        {
            let bytes = read_flash(
                port,
                sector.offset,
                self.lengths[index],
                std::mem::replace(&mut initial, ready()),
                cancel,
            )
            .map_err(|source| {
                failure(
                    index,
                    0,
                    WriteFailure {
                        stage: WriteStage::BackupCheck,
                        flash_may_have_changed: false,
                        source,
                    },
                )
            })?;
            if !recovery && bytes != sector.original {
                return Err(failure(
                    index,
                    0,
                    WriteFailure {
                        stage: WriteStage::BackupCheck,
                        flash_may_have_changed: false,
                        source: Error::Protocol(
                            "WS53 plan original is stale; no erase sent".into(),
                        ),
                    },
                ));
            }
            live.push(bytes);
        }
        for (index, (sector, before)) in self
            .sectors
            .iter()
            .zip(&live)
            .enumerate()
        {
            let target = if recovery {
                sector.original
            } else {
                sector.replacement
            };
            write::perform_replacement(
                port,
                sector.offset,
                before,
                target,
                self.lengths[index],
                ready(),
                cancel,
            )
            .map_err(|source| failure(index, index, source))?;
        }
        Ok(())
    }
}
