//! Thin, safe-ish wrappers over the Win32 calls the probes share.
//!
//! Deliberately defines the device structures and IOCTL codes locally with `#[repr(C)]`
//! rather than depending on whichever names the `windows` crate exposes this release.
//! These are stable, documented kernel ABIs; pinning them here keeps probe code
//! immune to crate churn and makes the layout auditable in one place.

pub mod device;
pub mod ioctl;

use std::fmt;

/// A Win32 call that failed, with the OS error attached.
#[derive(Debug, Clone)]
pub struct WinError {
    pub context: &'static str,
    pub code: u32,
}

impl WinError {
    pub fn last(context: &'static str) -> Self {
        Self {
            context,
            code: unsafe { windows::Win32::Foundation::GetLastError().0 },
        }
    }

    pub fn new(context: &'static str, code: u32) -> Self {
        Self { context, code }
    }
}

impl fmt::Display for WinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} failed (os error {})", self.context, self.code)
    }
}

impl std::error::Error for WinError {}

pub type WinResult<T> = Result<T, WinError>;

/// Decode a NUL-terminated UTF-16 buffer, tolerating a missing terminator.
pub fn wide_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Encode a Rust string as a NUL-terminated UTF-16 vector for Win32 calls.
pub fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Are we running elevated?
///
/// Every probe that needs ring-0 access checks this first so it can return
/// `Unavailable::RequiresElevation` instead of a confusing access-denied error.
pub fn is_elevated() -> bool {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut size = std::mem::size_of::<TOKEN_ELEVATION>() as u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut _),
            size,
            &mut size,
        )
        .is_ok();
        let _ = windows::Win32::Foundation::CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_decoding_stops_at_nul() {
        let buf: Vec<u16> = "MS-N014\0garbage".encode_utf16().collect();
        assert_eq!(wide_to_string(&buf), "MS-N014");
    }

    #[test]
    fn wide_decoding_tolerates_missing_terminator() {
        let buf: Vec<u16> = "MSI Corp.".encode_utf16().collect();
        assert_eq!(wide_to_string(&buf), "MSI Corp.");
    }

    #[test]
    fn roundtrip_wide() {
        let w = to_wide("Lithium Ion");
        assert_eq!(*w.last().unwrap(), 0);
        assert_eq!(wide_to_string(&w), "Lithium Ion");
    }
}
