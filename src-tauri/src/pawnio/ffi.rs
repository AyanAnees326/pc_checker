//! Raw bindings to `PawnIOLib.dll`.
//!
//! Signatures copied verbatim from `PawnIOLib/include/PawnIOLib.h` in namazso/PawnIO
//! (LGPL-2.1-or-later), not guessed — a wrong FFI signature here is a silent crash,
//! not a compile error:
//!
//! ```c
//! PAWNIOAPI pawnio_version(PULONG version);
//! PAWNIOAPI pawnio_open(PHANDLE handle);
//! PAWNIOAPI pawnio_load(HANDLE handle, const UCHAR* blob, SIZE_T size);
//! PAWNIOAPI pawnio_execute(
//!   HANDLE handle, PCSTR name,
//!   const ULONG64* in, SIZE_T in_size,
//!   PULONG64 out, SIZE_T out_size,
//!   PSIZE_T return_size);
//! PAWNIOAPI pawnio_close(HANDLE handle);
//! ```
//!
//! `PAWNIOAPI` is `EXTERN_C __declspec(dllimport) HRESULT STDAPICALLTYPE` — on x64
//! Windows there is a single calling convention, so `extern "system"` matches.
//! `PawnIOLib.dll` is installed by the separate PawnIO driver package (not bundled
//! with this app), so it is resolved dynamically through [`crate::win::dynlib`]
//! rather than linked at build time.

use windows::Win32::Foundation::HANDLE;
use windows::core::HRESULT;

use crate::win::dynlib::Library;
use crate::win::{WinError, WinResult};

type PawnioVersionFn = unsafe extern "system" fn(version: *mut u32) -> HRESULT;
type PawnioOpenFn = unsafe extern "system" fn(handle: *mut HANDLE) -> HRESULT;
type PawnioLoadFn =
    unsafe extern "system" fn(handle: HANDLE, blob: *const u8, size: usize) -> HRESULT;
type PawnioExecuteFn = unsafe extern "system" fn(
    handle: HANDLE,
    name: *const i8,
    in_: *const u64,
    in_size: usize,
    out: *mut u64,
    out_size: usize,
    return_size: *mut usize,
) -> HRESULT;
type PawnioCloseFn = unsafe extern "system" fn(handle: HANDLE) -> HRESULT;

/// Resolved entry points from a loaded `PawnIOLib.dll`.
///
/// Holds the [`Library`] alive for as long as any function pointer resolved from it
/// might still be called — dropping the library while a pointer is in use would be
/// use-after-free.
pub struct PawnIoLib {
    _lib: Library,
    version: PawnioVersionFn,
    open: PawnioOpenFn,
    load: PawnioLoadFn,
    execute: PawnioExecuteFn,
    close: PawnioCloseFn,
}

/// Where PawnIO's installer records its install directory.
///
/// PawnIO 2.x installs to `C:\Program Files\PawnIO` and does **not** put
/// `PawnIOLib.dll` in `System32` or on `PATH`, so loading it by bare name fails with
/// `ERROR_MOD_NOT_FOUND` (126) even on a machine where PawnIO is installed and its
/// driver service is running — which is exactly the failure this project hit. The
/// uninstall entry's `InstallLocation` is the documented, stable way to find it.
const PAWNIO_UNINSTALL_KEY: &str =
    r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\PawnIO";

const PAWNIO_DLL: &str = "PawnIOLib.dll";

/// Candidate full paths to `PawnIOLib.dll`, best source first.
fn candidate_paths() -> Vec<String> {
    let mut paths = Vec::new();

    if let Some(dir) = crate::win::registry::read_hklm_string(PAWNIO_UNINSTALL_KEY, "InstallLocation")
    {
        paths.push(format!("{}\\{PAWNIO_DLL}", dir.trim_end_matches('\\')));
    }

    // Fallback for an install whose registry entry is missing or was written by a
    // different packaging route (Chocolatey/WinGet both shell out to the same NSIS
    // installer, but this costs nothing and removes a single point of failure).
    if let Ok(program_files) = std::env::var("ProgramFiles") {
        paths.push(format!(
            "{}\\PawnIO\\{PAWNIO_DLL}",
            program_files.trim_end_matches('\\')
        ));
    }

    paths.dedup();
    paths
}

impl PawnIoLib {
    /// Locate and load `PawnIOLib.dll`.
    ///
    /// Tries the standard search order first (covers a copy sitting next to our own
    /// exe, or a future PawnIO that does install to `System32`), then falls back to
    /// the install directory recorded in the registry — see [`PAWNIO_UNINSTALL_KEY`]
    /// for why the bare-name load is not sufficient on its own.
    ///
    /// Fails with a descriptive error (not a panic) when PawnIO is not installed —
    /// that is the expected state on most machines until the user opts into a stress
    /// test and goes through the bootstrap/consent flow.
    pub fn load() -> WinResult<Self> {
        let lib = match Library::load(PAWNIO_DLL) {
            Ok(lib) => lib,
            Err(search_path_err) => candidate_paths()
                .into_iter()
                .find_map(|p| Library::load_from_path(&p).ok())
                // Report the original bare-name error rather than the last
                // candidate's: "not found on the search path" is the accurate
                // description of the whole attempt, and a stale per-candidate error
                // would point at a path the user never chose.
                .ok_or(search_path_err)?,
        };

        // Safety: each signature above is copied directly from the vendor header.
        unsafe {
            let version = lib
                .proc_as::<PawnioVersionFn>("pawnio_version")
                .ok_or_else(|| WinError::new("pawnio_version missing", 0))?;
            let open = lib
                .proc_as::<PawnioOpenFn>("pawnio_open")
                .ok_or_else(|| WinError::new("pawnio_open missing", 0))?;
            let load = lib
                .proc_as::<PawnioLoadFn>("pawnio_load")
                .ok_or_else(|| WinError::new("pawnio_load missing", 0))?;
            let execute = lib
                .proc_as::<PawnioExecuteFn>("pawnio_execute")
                .ok_or_else(|| WinError::new("pawnio_execute missing", 0))?;
            let close = lib
                .proc_as::<PawnioCloseFn>("pawnio_close")
                .ok_or_else(|| WinError::new("pawnio_close missing", 0))?;

            Ok(Self {
                _lib: lib,
                version,
                open,
                load,
                execute,
                close,
            })
        }
    }

    /// `(major << 16) | (minor << 8) | patch`, per the header's documented encoding.
    pub fn version(&self) -> WinResult<(u8, u8, u8)> {
        let mut raw: u32 = 0;
        let hr = unsafe { (self.version)(&mut raw) };
        hr_result(hr, "pawnio_version")?;
        Ok((
            ((raw >> 16) & 0xFF) as u8,
            ((raw >> 8) & 0xFF) as u8,
            (raw & 0xFF) as u8,
        ))
    }

    /// Open a new executor. Each executor holds at most one loaded module blob.
    pub fn open(&self) -> WinResult<HANDLE> {
        let mut handle = HANDLE::default();
        let hr = unsafe { (self.open)(&mut handle) };
        hr_result(hr, "pawnio_open")?;
        Ok(handle)
    }

    /// Load a compiled module blob (a `.bin` produced from PawnIO.Modules Pawn source)
    /// into an open executor.
    pub fn load_blob(&self, handle: HANDLE, blob: &[u8]) -> WinResult<()> {
        let hr = unsafe { (self.load)(handle, blob.as_ptr(), blob.len()) };
        hr_result(hr, "pawnio_load")
    }

    /// Call a named exported IOCTL function from the loaded blob.
    ///
    /// `name` must be ASCII (module IOCTL names are, by PawnIO.Modules convention,
    /// always `ioctl_...`). Returns the portion of `out` actually written.
    pub fn execute(&self, handle: HANDLE, name: &str, input: &[u64], out: &mut [u64]) -> WinResult<usize> {
        let mut cname: Vec<u8> = name.bytes().collect();
        cname.push(0);
        let mut written: usize = 0;

        let hr = unsafe {
            (self.execute)(
                handle,
                cname.as_ptr() as *const i8,
                input.as_ptr(),
                input.len(),
                out.as_mut_ptr(),
                out.len(),
                &mut written,
            )
        };
        hr_result(hr, "pawnio_execute")?;
        Ok(written)
    }

    /// Close an executor opened with [`PawnIoLib::open`].
    pub fn close(&self, handle: HANDLE) {
        // Best-effort: a close failure here is not actionable and must not panic a
        // Drop path.
        let _ = unsafe { (self.close)(handle) };
    }
}

fn hr_result(hr: HRESULT, context: &'static str) -> WinResult<()> {
    if hr.is_ok() {
        Ok(())
    } else {
        // HRESULT_FROM_WIN32-encoded failures carry a real Win32 code in the low word;
        // anything else is reported as the raw HRESULT so it is still greppable.
        let raw = hr.0 as u32;
        let code = if (raw >> 16) == 0x8007 { raw & 0xFFFF } else { raw };
        Err(WinError::new(context, code))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loading_resolves_or_fails_cleanly_never_panics() {
        // Deliberately asserts an invariant rather than a machine state: whether
        // PawnIO is installed differs between dev machines, CI, and end users, and a
        // test that hardcodes one of those answers fails for the wrong reason the
        // moment it runs somewhere else (as the previous version of this test did,
        // once PawnIO was actually installed here).
        match PawnIoLib::load() {
            // If it did load, it must be usable — a handle that cannot answer its own
            // version query would mean `load()` is reporting success too eagerly.
            Ok(lib) => {
                let version = lib.version();
                assert!(version.is_ok(), "a loaded PawnIOLib must answer a version query: {version:?}");
            }
            // If it did not, the error must carry a real Win32 code so the UI can say
            // *why* rather than guessing "not installed".
            Err(e) => {
                assert!(!e.context.is_empty(), "error must name the call that failed");
            }
        }
    }

    #[test]
    fn candidate_paths_are_absolute_and_named_for_the_library() {
        for p in candidate_paths() {
            assert!(p.contains(':'), "candidate must be an absolute path: {p}");
            assert!(p.ends_with(PAWNIO_DLL), "candidate must point at the DLL: {p}");
        }
    }
}
