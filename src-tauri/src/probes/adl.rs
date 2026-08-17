//! AMD GPU telemetry via `atiadlxx.dll` — AMD's public Display Library (ADL/ADL2),
//! the flat C API Afterburner and GPU-Z actually use for AMD, as opposed to ADLX
//! (AMD's newer SDK, which is COM-vtable-based and was judged too risky to hand-bind
//! without the SDK headers in hand — see `probes::nvml`'s module doc comment for the
//! parallel reasoning on why NVML was chosen over guessing at an unfamiliar ABI).
//!
//! Every signature, struct layout, and function name here is copied from AMD's real,
//! working sample code in `GPUOpen-LibrariesAndSDKs/display-library`
//! (`Sample/PMLog/PMLog.cpp`, `include/adl_structures.h`, `include/adl_defines.h`;
//! MIT-licensed), not guessed. That sample caught a real trap worth calling out: its
//! own local variable is named `ADL2_Adapter_PMLog_Support_Start`, but the actual
//! `GetProcAddress` string it resolves is `"ADL2_Adapter_PMLog_Start"` — binding to
//! the variable's own name instead of the string AMD's sample actually passes would
//! have silently failed to resolve on every machine.
//!
//! **PMLog** is AMD's modern (Vega/Navi/RDNA-generation) telemetry mechanism, and the
//! one this module uses: after a one-time `Start` call naming the sensors of
//! interest, the driver keeps a shared-memory region continuously updated, so
//! reading a sample is a direct memory read with no further IPC per tick — a good
//! match for this app's ~4 Hz sampler.

use std::ffi::c_void;

use serde::{Deserialize, Serialize};

use crate::win::dynlib::Library;
use crate::win::{WinError, WinResult};

const ADL_OK: i32 = 0;
const ADL_MAX_PATH: usize = 256;
const ADL_PMLOG_MAX_SENSORS: usize = 256;

// Sensor IDs (adl_defines.h). 0 is the array terminator, not a real sensor.
const ADL_SENSOR_MAXTYPES: u16 = 0;
const ADL_PMLOG_CLK_GFXCLK: u16 = 1;
const ADL_PMLOG_TEMPERATURE_EDGE: u16 = 8;
const ADL_PMLOG_FAN_RPM: u16 = 14;
const ADL_PMLOG_FAN_PERCENTAGE: u16 = 15;
const ADL_PMLOG_ASIC_POWER: u16 = 23;
const ADL_PMLOG_TEMPERATURE_HOTSPOT: u16 = 27;
const ADL_PMLOG_GFX_POWER: u16 = 30;
const ADL_PMLOG_THROTTLER_STATUS: u16 = 35;

type AdlContextHandle = *mut c_void;
/// `ADL_D3DKMT_HANDLE` aliases Windows' own `D3DKMT_HANDLE`, a plain `UINT`.
type AdlD3dKmtHandle = u32;

#[repr(C)]
#[derive(Copy, Clone)]
struct AdapterInfo {
    i_size: i32,
    i_adapter_index: i32,
    str_udid: [u8; ADL_MAX_PATH],
    i_bus_number: i32,
    i_device_number: i32,
    i_function_number: i32,
    i_vendor_id: i32,
    str_adapter_name: [u8; ADL_MAX_PATH],
    str_display_name: [u8; ADL_MAX_PATH],
    i_present: i32,
    i_exist: i32,
    str_driver_path: [u8; ADL_MAX_PATH],
    str_driver_path_ext: [u8; ADL_MAX_PATH],
    str_pnp_string: [u8; ADL_MAX_PATH],
    i_os_display_index: i32,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct AdlPmLogSupportInfo {
    sensors: [u16; ADL_PMLOG_MAX_SENSORS],
    reserved: [i32; 16],
}

#[repr(C)]
#[derive(Copy, Clone)]
struct AdlPmLogStartInput {
    sensors: [u16; ADL_PMLOG_MAX_SENSORS],
    sample_rate_ms: u32,
    reserved: [i32; 15],
}

#[repr(C)]
#[derive(Copy, Clone)]
struct AdlPmLogStartOutput {
    logging_address: *mut c_void,
    reserved: [i32; 14],
}

#[repr(C)]
#[derive(Copy, Clone)]
struct AdlPmLogData {
    version: u32,
    active_sample_rate: u32,
    last_updated: u64,
    values: [[u32; 2]; ADL_PMLOG_MAX_SENSORS],
    // `reserved` omitted: nothing after `values` is read, so the struct's tail
    // doesn't need to be represented for this to be safely `read_unaligned` from the
    // driver's shared-memory region.
}

type FnMallocCallback = unsafe extern "system" fn(i32) -> *mut c_void;
/// Legacy (non-ADL2) create/destroy: no context in or out. These set up a single
/// process-global ADL session that both the legacy adapter-enumeration calls and the
/// ADL2 PMLog calls below can share by passing a NULL context — the exact pattern
/// AMD's own `PMLog.cpp` sample uses (its `context` variable is declared but never
/// assigned, and is passed NULL throughout). Mixing `ADL2_Main_Control_Create`'s real
/// context with the legacy enumeration calls was tried first and produced a
/// silently-empty adapter list, since the legacy calls only ever look at the global
/// session, not a caller-supplied context.
type FnMainControlCreate = unsafe extern "system" fn(FnMallocCallback, i32) -> i32;
type FnMainControlDestroy = unsafe extern "system" fn() -> i32;
type FnAdapterNumberOfAdaptersGet = unsafe extern "system" fn(*mut i32) -> i32;
type FnAdapterAdapterInfoGet = unsafe extern "system" fn(*mut AdapterInfo, i32) -> i32;
type FnDevicePmLogDeviceCreate = unsafe extern "system" fn(AdlContextHandle, i32, *mut AdlD3dKmtHandle) -> i32;
type FnDevicePmLogDeviceDestroy = unsafe extern "system" fn(AdlContextHandle, AdlD3dKmtHandle) -> i32;
type FnAdapterPmLogSupportGet = unsafe extern "system" fn(AdlContextHandle, i32, *mut AdlPmLogSupportInfo) -> i32;
type FnAdapterPmLogStart = unsafe extern "system" fn(
    AdlContextHandle,
    i32,
    *mut AdlPmLogStartInput,
    *mut AdlPmLogStartOutput,
    AdlD3dKmtHandle,
) -> i32;
type FnAdapterPmLogStop = unsafe extern "system" fn(AdlContextHandle, i32, AdlD3dKmtHandle) -> i32;

unsafe extern "system" fn adl_malloc(size: i32) -> *mut c_void {
    // ADL calls this to allocate its own output buffers (e.g. the AdapterInfo array);
    // it does not free them itself — matching AMD's own sample, this is intentionally
    // process-lifetime allocated and never explicitly freed, since this app opens ADL
    // once per stress run and the OS reclaims it on process exit.
    libc_malloc(size as usize)
}

// A tiny local stand-in for `malloc` so this module needs no extra crate dependency
// for the one allocation ADL's callback contract requires.
unsafe fn libc_malloc(size: usize) -> *mut c_void {
    let layout = std::alloc::Layout::from_size_align(size.max(1), 8).unwrap();
    std::alloc::alloc(layout) as *mut c_void
}

/// `ADL_Main_Control_Create`/`_Destroy` operate on a single process-global ADL
/// session, not a per-caller handle — that is the entire reason the NULL-context
/// pattern in `open()` works at all. Two `AdlSession`s opened concurrently on
/// different threads would race on that global state: one's `Drop` can call
/// `ADL_Main_Control_Destroy` while another thread is still reading its
/// `pLoggingAddress` shared-memory pointer, which is exactly the access violation
/// this codebase's own parallel test run hit before this lock existed (two `#[test]`
/// functions each independently opening a session). Held for an `AdlSession`'s
/// entire lifetime, so only one can exist process-wide at a time — sufficient for
/// this app's real usage (one active GPU stress telemetry session, ever) and it
/// closes the race at its root rather than papering over one interleaving of it.
static ADL_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// One open ADL2 session with an active PMLog subscription for one adapter.
pub struct AdlSession {
    _lock: std::sync::MutexGuard<'static, ()>,
    _lib: Library,
    context: AdlContextHandle,
    device: AdlD3dKmtHandle,
    adapter_index: i32,
    data: *const AdlPmLogData,
    fn_main_control_destroy: FnMainControlDestroy,
    fn_device_pmlog_destroy: FnDevicePmLogDeviceDestroy,
    fn_adapter_pmlog_stop: FnAdapterPmLogStop,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct AdlThrottleStatus {
    pub any_throttling: bool,
}

impl AdlSession {
    /// Open ADL, find the first present AMD adapter (`atiadlxx.dll` only ever knows
    /// about AMD hardware, so there is nothing else it could return — see
    /// `find_amd_adapter`'s doc comment), and start a PMLog subscription for the
    /// sensors this app reads. Caller is expected to have already confirmed the
    /// target GPU is AMD (e.g. from the DXGI vendor ID) before calling this.
    pub fn open() -> WinResult<Self> {
        // Recover from a poisoned lock (a previous session panicked while holding
        // it) rather than propagating the poison forever — this app's own worker
        // threads already treat a panic as "stop this run", not "corrupt every
        // future one".
        let lock = ADL_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

        let lib = Library::load("atiadlxx.dll").or_else(|_| Library::load("atiadlxy.dll"))?;

        macro_rules! resolve {
            ($name:literal) => {
                unsafe { lib.proc_as($name) }.ok_or_else(|| WinError::new(concat!($name, " missing"), 0))?
            };
        }

        let create: FnMainControlCreate = resolve!("ADL_Main_Control_Create");
        let destroy: FnMainControlDestroy = resolve!("ADL_Main_Control_Destroy");
        let num_adapters: FnAdapterNumberOfAdaptersGet = resolve!("ADL_Adapter_NumberOfAdapters_Get");
        let adapter_info: FnAdapterAdapterInfoGet = resolve!("ADL_Adapter_AdapterInfo_Get");
        let device_create: FnDevicePmLogDeviceCreate = resolve!("ADL2_Device_PMLog_Device_Create");
        let device_destroy: FnDevicePmLogDeviceDestroy = resolve!("ADL2_Device_PMLog_Device_Destroy");
        let pmlog_support: FnAdapterPmLogSupportGet = resolve!("ADL2_Adapter_PMLog_Support_Get");
        // Real exported symbol names — not the sample's own (misleading) local
        // variable names. See the module doc comment.
        let pmlog_start: FnAdapterPmLogStart = resolve!("ADL2_Adapter_PMLog_Start");
        let pmlog_stop: FnAdapterPmLogStop = resolve!("ADL2_Adapter_PMLog_Stop");

        // 0, not 1: "1" restricts enumeration to adapters ADL considers connected to
        // an active display output, which undercounts real GPU-compute-capable
        // adapters in non-standard display sessions. "0" enumerates every adapter
        // physically present and enabled regardless of display-connection state,
        // which is what this app actually wants.
        if unsafe { create(adl_malloc, 0) } != ADL_OK {
            return Err(WinError::new("ADL_Main_Control_Create failed", 0));
        }
        // NULL context: every ADL2_* call below shares the single global session
        // `ADL_Main_Control_Create` just established — see this function's own
        // `FnMainControlCreate` doc comment for why a real ADL2 context here breaks
        // the legacy enumeration calls this session also needs.
        let context: AdlContextHandle = std::ptr::null_mut();

        let adapter_index = match find_amd_adapter(num_adapters, adapter_info) {
            Some(i) => i,
            None => {
                unsafe { destroy() };
                return Err(WinError::new("no AMD adapter found via ADL", 0));
            }
        };

        let mut device: AdlD3dKmtHandle = 0;
        if unsafe { device_create(context, adapter_index, &mut device) } != ADL_OK {
            unsafe { destroy() };
            return Err(WinError::new("ADL2_Device_PMLog_Device_Create failed", 0));
        }

        let mut support = AdlPmLogSupportInfo {
            sensors: [ADL_SENSOR_MAXTYPES; ADL_PMLOG_MAX_SENSORS],
            reserved: [0; 16],
        };
        if unsafe { pmlog_support(context, adapter_index, &mut support) } != ADL_OK {
            unsafe {
                device_destroy(context, device);
                destroy();
            }
            return Err(WinError::new("ADL2_Adapter_PMLog_Support_Get failed", 0));
        }

        // Subscribe only to sensors this app actually reads, and only the ones the
        // adapter reports supporting — starting a PMLog subscription with an
        // unsupported sensor ID is a documented failure mode of `PMLog_Start`.
        let wanted = [
            ADL_PMLOG_CLK_GFXCLK,
            ADL_PMLOG_TEMPERATURE_EDGE,
            ADL_PMLOG_TEMPERATURE_HOTSPOT,
            ADL_PMLOG_FAN_RPM,
            ADL_PMLOG_FAN_PERCENTAGE,
            ADL_PMLOG_GFX_POWER,
            ADL_PMLOG_ASIC_POWER,
            ADL_PMLOG_THROTTLER_STATUS,
        ];
        let supported: std::collections::HashSet<u16> = support
            .sensors
            .iter()
            .copied()
            .take_while(|&s| s != ADL_SENSOR_MAXTYPES)
            .collect();

        let mut start_input = AdlPmLogStartInput {
            sensors: [ADL_SENSOR_MAXTYPES; ADL_PMLOG_MAX_SENSORS],
            sample_rate_ms: 100,
            reserved: [0; 15],
        };
        let mut n = 0;
        for &sensor in wanted.iter().filter(|s| supported.contains(s)) {
            start_input.sensors[n] = sensor;
            n += 1;
        }
        start_input.sensors[n] = ADL_SENSOR_MAXTYPES;

        let mut start_output = AdlPmLogStartOutput {
            logging_address: std::ptr::null_mut(),
            reserved: [0; 14],
        };
        if unsafe { pmlog_start(context, adapter_index, &mut start_input, &mut start_output, device) } != ADL_OK {
            unsafe {
                device_destroy(context, device);
                destroy();
            }
            return Err(WinError::new("ADL2_Adapter_PMLog_Start failed", 0));
        }

        Ok(Self {
            _lock: lock,
            _lib: lib,
            context,
            device,
            adapter_index,
            data: start_output.logging_address as *const AdlPmLogData,
            fn_main_control_destroy: destroy,
            fn_device_pmlog_destroy: device_destroy,
            fn_adapter_pmlog_stop: pmlog_stop,
        })
    }

    /// Read the driver's live-updated sensor snapshot. No IOCTL/API call per read —
    /// this is a direct read of the shared-memory region ADL keeps current.
    fn snapshot(&self) -> Option<AdlPmLogData> {
        if self.data.is_null() {
            return None;
        }
        // Safety: `self.data` points into memory ADL itself mapped and keeps valid
        // for the lifetime of this session (torn down together in `Drop`).
        Some(unsafe { std::ptr::read_unaligned(self.data) })
    }

    fn sensor(&self, id: u16) -> Option<u32> {
        let snap = self.snapshot()?;
        snap.values
            .iter()
            .take_while(|v| v[0] != ADL_SENSOR_MAXTYPES as u32)
            .find(|v| v[0] == id as u32)
            .map(|v| v[1])
    }

    pub fn graphics_clock_mhz(&self) -> Option<u32> {
        self.sensor(ADL_PMLOG_CLK_GFXCLK)
    }

    pub fn edge_temperature_c(&self) -> Option<u32> {
        self.sensor(ADL_PMLOG_TEMPERATURE_EDGE)
    }

    pub fn hotspot_temperature_c(&self) -> Option<u32> {
        self.sensor(ADL_PMLOG_TEMPERATURE_HOTSPOT)
    }

    /// Tachometer reading. Some boards (especially blower-style laptop GPUs) don't
    /// expose this even when they report fan percentage — degrades to `None` rather
    /// than a fabricated value, per this app's `Option<T>` sensor contract.
    pub fn fan_rpm(&self) -> Option<u32> {
        self.sensor(ADL_PMLOG_FAN_RPM)
    }

    pub fn fan_percent(&self) -> Option<u32> {
        self.sensor(ADL_PMLOG_FAN_PERCENTAGE)
    }

    /// Total ASIC (board) power if the adapter reports it, falling back to the GFX rail
    /// alone — not every GPU generation exposes both.
    ///
    /// ASIC first, deliberately: GFX power covers only the graphics core, excluding
    /// VRAM, VRM losses and fans, so on this machine's RX 6700 XT it reads roughly
    /// 40-50 W below the total. Board power is what GPU-Z, HWiNFO and Afterburner label
    /// "GPU Power", and it is the only figure comparable to the card's rated TBP —
    /// which is the comparison a buyer is actually making when asking whether a GPU
    /// pulls what it should under load.
    pub fn power_watts(&self) -> Option<f64> {
        self.sensor(ADL_PMLOG_ASIC_POWER)
            .or_else(|| self.sensor(ADL_PMLOG_GFX_POWER))
            .map(|w| w as f64)
    }

    pub fn throttle_status(&self) -> Option<AdlThrottleStatus> {
        self.sensor(ADL_PMLOG_THROTTLER_STATUS).map(|bits| AdlThrottleStatus {
            any_throttling: bits != 0,
        })
    }
}

impl Drop for AdlSession {
    fn drop(&mut self) {
        unsafe {
            let _ = (self.fn_adapter_pmlog_stop)(self.context, self.adapter_index, self.device);
            let _ = (self.fn_device_pmlog_destroy)(self.context, self.device);
            let _ = (self.fn_main_control_destroy)();
        }
    }
}

/// `atiadlxx.dll` is AMD's own driver library — every adapter it enumerates is
/// already an AMD adapter, so no vendor-ID match is needed (or reliable: verified
/// empirically on this dev machine's real RX 6700 XT, ADL's `iVendorID` field reports
/// the decimal value `1002`, not the PCI hex vendor ID `0x1002` = 4098 — a documented
/// ADL quirk, not a bug in this code). Picks the first `present` entry; ADL can list
/// several logical entries for one physical GPU (7 were observed here for one card),
/// and any of them opens a PMLog session against the same underlying device.
fn find_amd_adapter(
    num_adapters: FnAdapterNumberOfAdaptersGet,
    adapter_info: FnAdapterAdapterInfoGet,
) -> Option<i32> {
    let debug = std::env::var("ADL_DEBUG").is_ok();

    let mut count = 0i32;
    let rc = unsafe { num_adapters(&mut count) };
    if debug {
        eprintln!("ADL_Adapter_NumberOfAdapters_Get -> rc={rc} count={count}");
    }
    if rc != ADL_OK || count <= 0 {
        return None;
    }

    let mut infos = vec![
        AdapterInfo {
            i_size: 0,
            i_adapter_index: 0,
            str_udid: [0; ADL_MAX_PATH],
            i_bus_number: 0,
            i_device_number: 0,
            i_function_number: 0,
            i_vendor_id: 0,
            str_adapter_name: [0; ADL_MAX_PATH],
            str_display_name: [0; ADL_MAX_PATH],
            i_present: 0,
            i_exist: 0,
            str_driver_path: [0; ADL_MAX_PATH],
            str_driver_path_ext: [0; ADL_MAX_PATH],
            str_pnp_string: [0; ADL_MAX_PATH],
            i_os_display_index: 0,
        };
        count as usize
    ];

    let bytes = (count as usize) * std::mem::size_of::<AdapterInfo>();
    let rc2 = unsafe { adapter_info(infos.as_mut_ptr(), bytes as i32) };
    if debug {
        eprintln!("ADL_Adapter_AdapterInfo_Get -> rc={rc2}");
    }
    if rc2 != ADL_OK {
        return None;
    }

    if debug {
        for a in &infos {
            eprintln!(
                "ADL adapter idx={} present={} vendor={} bus={} dev={} func={}",
                a.i_adapter_index, a.i_present, a.i_vendor_id, a.i_bus_number, a.i_device_number, a.i_function_number
            );
        }
    }

    infos.iter().find(|a| a.i_present != 0).map(|a| a.i_adapter_index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn struct_sizes_are_self_consistent() {
        // AdapterInfo must be exactly what ADL_Adapter_AdapterInfo_Get expects per
        // entry, or the byte-size passed to it (count * size_of::<AdapterInfo>())
        // would silently misalign every adapter after the first.
        assert_eq!(std::mem::size_of::<AdapterInfo>(), 4 + 4 + 256 + 4 + 4 + 4 + 4 + 256 + 256 + 4 + 4 + 256 + 256 + 256 + 4);
    }

    /// This dev machine's real AMD GPU is the test case: open a real ADL2 PMLog
    /// session, read a few sensors, and confirm at least the graphics clock comes
    /// back with a plausible value.
    #[test]
    fn opens_and_reads_real_sensors_on_this_machines_amd_gpu() {
        let session = AdlSession::open().expect("failed to open ADL session on a real AMD GPU");

        // Give the driver a moment to populate the shared memory region after Start.
        std::thread::sleep(std::time::Duration::from_millis(200));

        let clock = session.graphics_clock_mhz();
        assert!(clock.is_some(), "expected a graphics clock reading");
        let clock = clock.unwrap();
        assert!(clock > 0 && clock < 5000, "implausible graphics clock: {clock} MHz");
    }
}
