//! Opting the current process out of Windows' EcoQoS execution-speed throttling for
//! the duration of a stress run.
//!
//! Without this, Windows may classify a background or non-visible-window GUI process
//! as work that "does not contribute to the foreground user experience" and throttle
//! its threads onto slower cores or lower clocks to save power — a stress test that
//! looks like it's running but barely loads anything. Every stress-kernel test in
//! this codebase runs inside `cargo test`, a normal foreground console process, which
//! never exhibits this; the shipped app is a GUI process, which can.
//!
//! Signature and usage pattern copied from Microsoft's own `SetProcessInformation`
//! documentation example, not guessed — an empty `ControlMask` would silently be a
//! no-op rather than an error.

use windows::Win32::System::Threading::{
    GetCurrentProcess, ProcessPowerThrottling, SetProcessInformation,
    PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
    PROCESS_POWER_THROTTLING_STATE,
};

/// While alive, this process is explicitly opted out of execution-speed throttling.
/// Reverts to system-managed behaviour on drop, so the app does not permanently
/// insist on never being throttled once the stress run ends.
pub struct ExecutionSpeedGuard;

impl ExecutionSpeedGuard {
    pub fn engage() -> Self {
        apply(PROCESS_POWER_THROTTLING_EXECUTION_SPEED, 0);
        Self
    }
}

impl Drop for ExecutionSpeedGuard {
    fn drop(&mut self) {
        // ControlMask = 0 means "don't control any mechanism" — hands execution-speed
        // decisions back to the system's own heuristics, per Microsoft's documented
        // "reset to default system managed behavior" pattern.
        apply(0, 0);
    }
}

fn apply(control_mask: u32, state_mask: u32) {
    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: control_mask,
        StateMask: state_mask,
    };
    unsafe {
        // Best-effort: an older Windows without this API (pre-Windows 8) simply
        // keeps running at whatever QoS the system already picked.
        let _ = SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            &state as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engaging_and_dropping_the_guard_does_not_panic_on_this_machine() {
        let guard = ExecutionSpeedGuard::engage();
        drop(guard);
    }
}
