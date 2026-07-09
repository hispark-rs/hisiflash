//! FWPKG firmware package format.
//!
//! `hisiflash` intentionally does not own package/image format semantics. The
//! source of truth is the `hisi-fwpkg` crate; this module is a compatibility
//! re-export so the serial flashing code can keep its existing API shape while
//! header, partition, CRC, and burn-range parsing live in one place.

pub use hisi_fwpkg::{
    Fwpkg, FwpkgBinInfo, FwpkgHeader, FwpkgVersion, PartitionType,
    fwpkg::{
        BIN_INFO_SIZE, BIN_INFO_SIZE_V2, FWPKG_MAGIC_V1, FWPKG_MAGIC_V2_MAX, FWPKG_MAGIC_V2_MIN,
        HEADER_SIZE, HEADER_SIZE_V2, MAX_PARTITIONS, NAME_SIZE, NAME_SIZE_V2,
    },
};
