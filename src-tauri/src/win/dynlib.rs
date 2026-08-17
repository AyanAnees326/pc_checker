//! Dynamic library loading — `LoadLibraryW` / `GetProcAddress` / `FreeLibrary`.
//!
//! Shared by every FFI surface this project talks to that we do not link against at
//! build time: `PawnIOLib.dll` (installed by the separate PawnIO driver package, not
//! bundled with this app) and `nvml.dll` (shipped with the NVIDIA driver, not present
//! at all on non-NVIDIA machines). Both are optional at runtime — a missing DLL is a
//! `Reading::missing`, not a build failure or a crash.

use super::{to_wide, WinError, WinResult};
use windows::Win32::Foundation::{FreeLibrary, HMODULE};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LoadLibraryW, LOAD_WITH_ALTERED_SEARCH_PATH,
};
use windows::core::{PCSTR, PCWSTR};

/// An owned module handle that frees itself.
///
/// Wrong to `FreeLibrary` a module that outlives its last caller — every FFI wrapper
/// built on this keeps the `Library` alive for as long as it holds function pointers
/// resolved from it.
pub struct Library(HMODULE);

impl Library {
    /// Load a DLL by name (or full path), following the standard search order.
    pub fn load(name: &str) -> WinResult<Self> {
        let wide = to_wide(name);
        let handle = unsafe { LoadLibraryW(PCWSTR(wide.as_ptr())) }
            .map_err(|_| WinError::last("LoadLibraryW"))?;
        Ok(Self(handle))
    }

    /// Load a DLL from an absolute path outside the standard search order.
    ///
    /// Uses `LOAD_WITH_ALTERED_SEARCH_PATH` so the DLL's *own* directory is searched
    /// for its dependencies. Without that flag a DLL living somewhere like
    /// `C:\Program Files\Vendor\` loads but any sibling DLL it imports would still be
    /// looked up relative to *our* executable, which is the classic way this kind of
    /// out-of-path load fails confusingly at first call rather than at load time.
    pub fn load_from_path(path: &str) -> WinResult<Self> {
        let wide = to_wide(path);
        let handle =
            unsafe { LoadLibraryExW(PCWSTR(wide.as_ptr()), None, LOAD_WITH_ALTERED_SEARCH_PATH) }
                .map_err(|_| WinError::last("LoadLibraryExW"))?;
        Ok(Self(handle))
    }

    /// Resolve an exported function by its (ASCII) name.
    ///
    /// Returns `None` rather than an error on a missing symbol — callers use this to
    /// probe for optional entry points across DLL versions.
    pub fn proc(&self, name: &str) -> Option<unsafe extern "system" fn() -> isize> {
        // GetProcAddress takes a NUL-terminated ANSI string; symbol names are ASCII by
        // convention for every C export we call through this.
        let mut ascii: Vec<u8> = name.bytes().collect();
        ascii.push(0);
        unsafe { GetProcAddress(self.0, PCSTR(ascii.as_ptr())) }
    }

    /// Resolve a symbol and reinterpret it as `F`.
    ///
    /// # Safety
    /// `F` must exactly match the calling convention and signature of the exported
    /// function, or calling the result is undefined behaviour. This is the one place
    /// in the codebase where a mismatched FFI signature is a silent miscompile rather
    /// than a linker error, which is why every caller documents the signature against
    /// the vendor header it was copied from.
    pub unsafe fn proc_as<F: Copy>(&self, name: &str) -> Option<F> {
        let ptr = self.proc(name)?;
        // Function pointers and `fn() -> isize` are both single machine words; this
        // transmute only changes the type the compiler treats that word as.
        Some(std::mem::transmute_copy::<_, F>(&ptr))
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        unsafe {
            let _ = FreeLibrary(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_a_dll_that_is_always_present() {
        let lib = Library::load("kernel32.dll");
        assert!(lib.is_ok(), "kernel32.dll must always be loadable: {:?}", lib.err());
    }

    #[test]
    fn resolves_a_known_export() {
        let lib = Library::load("kernel32.dll").unwrap();
        assert!(lib.proc("GetCurrentProcessId").is_some());
    }

    #[test]
    fn missing_symbol_is_none_not_a_panic() {
        let lib = Library::load("kernel32.dll").unwrap();
        assert!(lib.proc("ThisFunctionDoesNotExist_Really").is_none());
    }

    #[test]
    fn missing_dll_is_an_error_not_a_panic() {
        let lib = Library::load("this_dll_does_not_exist_at_all.dll");
        assert!(lib.is_err());
    }

    #[test]
    fn typed_proc_calls_through_correctly() {
        type GetCurrentProcessId = unsafe extern "system" fn() -> u32;
        let lib = Library::load("kernel32.dll").unwrap();
        let f: GetCurrentProcessId = unsafe { lib.proc_as("GetCurrentProcessId") }.unwrap();
        let pid = unsafe { f() };
        assert!(pid > 0);
    }
}
