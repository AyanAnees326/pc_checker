//! PawnIO driver detection and install guidance.
//!
//! This module deliberately does **not** silently download and run the PawnIO
//! installer. Installing a kernel driver is a system-level change, and the honest
//! way to handle that is the same principle the rest of this app follows for
//! anything irreversible on someone else's machine: show what's needed and why, and
//! let the person running the scan act on it — not do it invisibly on their behalf.
//! `status()` tells the caller what's true right now; the UI is responsible for the
//! consent screen and for pointing the user at [`INSTALL_URL`].

use serde::{Deserialize, Serialize};

use super::ffi::PawnIoLib;

/// Where to get PawnIO. Kept as a named constant rather than inlined in the UI so
/// there is exactly one place to update if the project ever needs to point at a
/// mirrored or pinned build instead.
pub const INSTALL_URL: &str = "https://pawnio.eu/";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PawnIoStatus {
    /// `PawnIOLib.dll` could not be loaded, or loaded but would not answer a version
    /// query. `detail` carries the real Win32 error rather than collapsing every
    /// cause into a bare "not installed" — "the DLL isn't on the search path" and
    /// "the DLL loaded but the driver service won't respond" are different problems
    /// with different fixes, and the person reading this banner cannot tell them
    /// apart without the actual error text.
    NotInstalled { detail: String },
    /// The library loaded and reports a version.
    Installed { version: String },
}

/// Check whether PawnIO is available on this machine right now.
///
/// Loading `PawnIOLib.dll` and asking its version is enough to answer "is it
/// installed and working" without needing to inspect the Service Control Manager —
/// if the DLL loads and answers, the driver package is present.
pub fn status() -> PawnIoStatus {
    match PawnIoLib::load() {
        Ok(lib) => match lib.version() {
            Ok((major, minor, patch)) => PawnIoStatus::Installed {
                version: format!("{major}.{minor}.{patch}"),
            },
            // The DLL loaded but couldn't answer a version query — still "not
            // usable" from this app's point of view, but a materially different
            // cause from the DLL never loading at all, so it gets its own detail.
            Err(e) => PawnIoStatus::NotInstalled {
                detail: format!("PawnIOLib.dll loaded but pawnio_version failed: {e}"),
            },
        },
        Err(e) => PawnIoStatus::NotInstalled {
            detail: format!("PawnIOLib.dll: {e}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_always_explains_itself_either_way() {
        // Whether PawnIO is installed varies by machine, so this asserts that each
        // answer is *self-explanatory* rather than asserting which answer it is —
        // the point of this type is that the UI can always tell the user something
        // specific, whichever branch it lands on.
        match status() {
            PawnIoStatus::NotInstalled { detail } => {
                assert!(!detail.is_empty(), "detail must explain why PawnIO looks absent");
            }
            PawnIoStatus::Installed { version } => {
                assert!(!version.is_empty(), "an installed PawnIO must report a version");
            }
        }
    }
}
