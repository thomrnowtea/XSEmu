//! BCPack for DirectStorage: the decoder lives in the platform-independent `xcl-bcpack` crate;
//! this module finds its table file.
//!
//! The tables are constants of the format that XCL does not ship. They are read once from
//! `XCL_BCPACK_TABLES`, else `%LOCALAPPDATA%\XCL\bcpack-tables.bin`. Without them BCPack
//! requests keep failing with E_DECOMPRESSION, and one warning says where the file was expected.

use std::path::PathBuf;
use std::sync::OnceLock;

pub use xcl_bcpack::{decode, Error, Tables};

/// The table file, or `None` if it is missing or invalid.
pub fn tables() -> Option<&'static Tables> {
    static TABLES: OnceLock<Option<Tables>> = OnceLock::new();
    TABLES
        .get_or_init(|| {
            let Some(path) = table_path() else {
                crate::xlog_warn!("BCPack: no table file (set XCL_BCPACK_TABLES); BCPack requests fail with E_DECOMPRESSION");
                return None;
            };
            // This is a host file, even if it is on a drive letter the title mounts.
            match crate::vfs_hooks::with_host_io(|| Tables::load(&path)) {
                Ok(t) => {
                    crate::xlog_info!("BCPack: tables loaded from {}", path.display());
                    Some(t)
                }
                Err(e) => {
                    crate::xlog_warn!("BCPack: no usable tables at {} ({e}); BCPack requests fail with E_DECOMPRESSION", path.display());
                    None
                }
            }
        })
        .as_ref()
}

fn table_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("XCL_BCPACK_TABLES").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    // Unit tests do not pick up a file installed in the user's profile.
    if cfg!(test) {
        return None;
    }
    let base = std::env::var_os("LOCALAPPDATA").filter(|p| !p.is_empty())?;
    Some(PathBuf::from(base).join("XCL").join("bcpack-tables.bin"))
}

/// The BcpackModes `decode` handles: BC1 and BC3–BC7.
pub fn supported(mode: u8) -> bool {
    matches!(mode, 1 | 3..=7)
}
