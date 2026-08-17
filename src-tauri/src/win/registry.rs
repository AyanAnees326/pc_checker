//! Reading single string values out of `HKEY_LOCAL_MACHINE`.
//!
//! Exists because "where did this optional third-party component install itself?" is
//! not answerable from a DLL search path — an installer that writes to
//! `C:\Program Files\...` rather than `System32` is invisible to `LoadLibraryW` by
//! bare name, so the registry entry it *does* leave behind is the only reliable way
//! to find it. See [`crate::pawnio::ffi`] for the case that motivated this.

use windows::core::PCWSTR;
use windows::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ,
    REG_VALUE_TYPE,
};

use super::{to_wide, wide_to_string};

/// Read a `REG_SZ`/`REG_EXPAND_SZ` value from `HKLM\<subkey>`.
///
/// Returns `None` for a missing key, a missing value, or an empty string — callers
/// treat all three the same way ("not recorded here"), so distinguishing them would
/// add a branch nothing acts on differently.
pub fn read_hklm_string(subkey: &str, value: &str) -> Option<String> {
    unsafe {
        let path = to_wide(subkey);
        let mut key = HKEY::default();
        if RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(path.as_ptr()),
            0,
            KEY_READ,
            &mut key,
        )
        .is_err()
        {
            return None;
        }

        let name = to_wide(value);
        // MAX_PATH-sized paths in UTF-16 plus headroom; a longer value is not a path
        // and is not something this helper is for.
        let mut buf = [0u8; 1024];
        let mut len = buf.len() as u32;
        let mut kind = REG_VALUE_TYPE::default();
        let ok = RegQueryValueExW(
            key,
            PCWSTR(name.as_ptr()),
            None,
            Some(&mut kind),
            Some(buf.as_mut_ptr()),
            Some(&mut len),
        )
        .is_ok();
        let _ = RegCloseKey(key);

        if !ok || len < 2 {
            return None;
        }

        let chars: Vec<u16> = buf[..len as usize]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let s = wide_to_string(&chars).trim().to_string();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_value_windows_always_has() {
        // ProgramFilesDir is present on every Windows install since NT.
        let v = read_hklm_string(
            r"SOFTWARE\Microsoft\Windows\CurrentVersion",
            "ProgramFilesDir",
        );
        assert!(v.is_some(), "expected ProgramFilesDir to be readable");
        assert!(v.unwrap().contains(':'), "expected an absolute path");
    }

    #[test]
    fn missing_key_is_none_not_a_panic() {
        assert!(read_hklm_string(r"SOFTWARE\NoSuchVendor\NoSuchProduct", "Nope").is_none());
    }

    #[test]
    fn missing_value_under_a_real_key_is_none() {
        assert!(read_hklm_string(
            r"SOFTWARE\Microsoft\Windows\CurrentVersion",
            "NoSuchValueName_Really"
        )
        .is_none());
    }
}
