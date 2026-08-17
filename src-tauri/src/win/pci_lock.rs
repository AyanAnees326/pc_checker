//! The cross-process `Global\Access_PCI` mutex.
//!
//! AMD's SMN (System Management Network) registers are not reachable directly — a
//! read is a two-step dance through the host bridge's PCI config-space index/data
//! register pair. That pair is *global mutable state shared by every process on the
//! machine*: if this app writes the index while HWiNFO, Ryzen Master, or
//! LibreHardwareMonitor is mid-transaction, one of the two reads comes back holding
//! the other's address, and neither side can tell.
//!
//! `Global\Access_PCI` is the de-facto standard mutex the hardware-monitoring
//! ecosystem uses to serialise that pair — LibreHardwareMonitor and the WinRing0
//! lineage before it use this exact name, and PawnIO's own `AMDFamily17.p` source
//! says to hold `\BaseNamedObjects\Access_PCI` (the NT-namespace spelling of the same
//! object) before calling `ioctl_read_smn`. Honouring it is what makes this app a
//! good citizen rather than a source of other tools' bad readings.
//!
//! This is the same class of hazard as the `ADL_LOCK` added earlier in this project,
//! except the contended resource is shared across processes rather than merely across
//! threads — so a plain `Mutex` cannot help.

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};

use super::{to_wide, WinError, WinResult};

/// `\BaseNamedObjects\Access_PCI` in NT naming.
const PCI_MUTEX_NAME: &str = r"Global\Access_PCI";

/// Long enough to ride out another tool's in-flight transaction, short enough that a
/// stuck peer degrades one sample to "unavailable" instead of stalling the sampler.
/// The stress orchestrator samples at 4 Hz (250 ms), so this stays a minority of one
/// interval even in the worst case.
pub const DEFAULT_TIMEOUT_MS: u32 = 50;

/// Holds `Global\Access_PCI` for as long as it is alive.
pub struct PciBusLock {
    handle: HANDLE,
}

impl PciBusLock {
    /// Acquire the lock, waiting up to `timeout_ms`.
    ///
    /// A timeout is a real error rather than a silent "go ahead anyway": proceeding
    /// unlocked is exactly the case that produces a corrupted reading, and a metric
    /// this app cannot take safely must report that rather than guess.
    pub fn acquire(timeout_ms: u32) -> WinResult<Self> {
        let name = to_wide(PCI_MUTEX_NAME);
        // Opens the existing mutex when another tool already created it; creates it
        // (unowned) otherwise.
        let handle = unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) }
            .map_err(|_| WinError::last("CreateMutexW(Access_PCI)"))?;

        let wait = unsafe { WaitForSingleObject(handle, timeout_ms) };
        match wait {
            // WAIT_ABANDONED means the previous owner died holding it. We now own it
            // and are still obliged to release it, so it is a successful acquisition —
            // dropping it here would leak the mutex permanently.
            WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(Self { handle }),
            _ => {
                unsafe {
                    let _ = CloseHandle(handle);
                }
                Err(WinError::new(
                    "PCI bus busy — another hardware-monitoring tool is holding it",
                    wait.0,
                ))
            }
        }
    }
}

impl Drop for PciBusLock {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.handle);
            let _ = CloseHandle(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquires_and_releases_so_a_second_take_succeeds() {
        {
            let first = PciBusLock::acquire(DEFAULT_TIMEOUT_MS);
            assert!(first.is_ok(), "should acquire an uncontended lock: {:?}", first.err());
        }
        // If Drop failed to release, this second acquisition would time out.
        let second = PciBusLock::acquire(DEFAULT_TIMEOUT_MS);
        assert!(second.is_ok(), "lock must be released on drop: {:?}", second.err());
    }

    #[test]
    fn is_reentrant_for_the_owning_thread() {
        // Win32 mutexes are owned per-thread and recursive, so nesting must not
        // deadlock the sampler if a future caller wraps an already-locked region.
        let _outer = PciBusLock::acquire(DEFAULT_TIMEOUT_MS).expect("outer");
        let inner = PciBusLock::acquire(DEFAULT_TIMEOUT_MS);
        assert!(inner.is_ok(), "same-thread reacquisition must not deadlock");
    }
}
