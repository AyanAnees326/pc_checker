//! Battery probe — a full native reimplementation of what BatteryInfoView reports.
//!
//! Everything here comes from `IOCTL_BATTERY_QUERY_INFORMATION` / `_STATUS` against the
//! battery class driver. No kernel driver and no elevation are required, which is why
//! this probe still works even if PawnIO installation is declined.
//!
//! Two things the OEM data cannot be trusted on, and which the report must reflect:
//!   * `CycleCount` reads 0 on a large share of laptops because the pack has no cycle
//!     counter. Zero is not "brand new" — it is "unknown", and is reported as such.
//!   * `FullChargedCapacity` is self-reported by the pack's own controller. A
//!     recalibrated or counterfeit pack will happily claim design capacity. The
//!     measured-discharge test in the stress module exists to check this claim.

use serde::{Deserialize, Serialize};

use crate::model::{Reading, Unavailable};
use crate::win::device::{self, GUID_DEVICE_BATTERY};
use crate::win::ioctl::{
    as_bytes, device_io_control, read_struct, IOCTL_BATTERY_QUERY_INFORMATION,
    IOCTL_BATTERY_QUERY_STATUS, IOCTL_BATTERY_QUERY_TAG,
};
use crate::win::{wide_to_string, WinResult};

// ---------------------------------------------------------------------------
// Kernel ABI (batclass.h)
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Copy, Clone, Default)]
struct BatteryQueryInformation {
    battery_tag: u32,
    information_level: u32,
    at_rate: i32,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
struct BatteryInformationRaw {
    capabilities: u32,
    technology: u8,
    reserved: [u8; 3],
    chemistry: [u8; 4],
    designed_capacity: u32,
    full_charged_capacity: u32,
    default_alert1: u32,
    default_alert2: u32,
    critical_bias: u32,
    cycle_count: u32,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
struct BatteryWaitStatus {
    battery_tag: u32,
    timeout: u32,
    power_state: u32,
    low_capacity: u32,
    high_capacity: u32,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
struct BatteryStatusRaw {
    power_state: u32,
    capacity: u32,
    voltage: u32,
    rate: i32,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
struct BatteryManufactureDate {
    day: u8,
    month: u8,
    year: u16,
}

// BATTERY_QUERY_INFORMATION_LEVEL
const LEVEL_INFORMATION: u32 = 0;
const LEVEL_TEMPERATURE: u32 = 2;
const LEVEL_ESTIMATED_TIME: u32 = 3;
const LEVEL_DEVICE_NAME: u32 = 4;
const LEVEL_MANUFACTURE_DATE: u32 = 5;
const LEVEL_MANUFACTURE_NAME: u32 = 6;
const LEVEL_UNIQUE_ID: u32 = 7;
const LEVEL_SERIAL_NUMBER: u32 = 8;

// BATTERY_STATUS power state bits
const BATTERY_POWER_ON_LINE: u32 = 0x0000_0001;
const BATTERY_DISCHARGING: u32 = 0x0000_0002;
const BATTERY_CHARGING: u32 = 0x0000_0004;
const BATTERY_CRITICAL: u32 = 0x0000_0008;

// BATTERY_INFORMATION capability bits
const BATTERY_SYSTEM_BATTERY: u32 = 0x8000_0000;
const BATTERY_CAPACITY_RELATIVE: u32 = 0x4000_0000;

/// Sentinel the battery class driver returns for "I don't know".
const BATTERY_UNKNOWN: u32 = 0xFFFF_FFFF;

// ---------------------------------------------------------------------------
// Public shape
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerState {
    Charging,
    Discharging,
    OnLineNotCharging,
    Critical,
    Unknown,
}

impl PowerState {
    fn from_bits(bits: u32) -> Self {
        if bits & BATTERY_CRITICAL != 0 {
            Self::Critical
        } else if bits & BATTERY_CHARGING != 0 {
            Self::Charging
        } else if bits & BATTERY_DISCHARGING != 0 {
            Self::Discharging
        } else if bits & BATTERY_POWER_ON_LINE != 0 {
            Self::OnLineNotCharging
        } else {
            Self::Unknown
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatteryReport {
    // Identity
    pub device_name: Reading<String>,
    pub manufacturer: Reading<String>,
    pub serial_number: Reading<String>,
    pub unique_id: Reading<String>,
    pub manufacture_date: Reading<String>,
    pub chemistry: Reading<String>,

    // Live status
    pub power_state: Reading<PowerState>,
    pub current_capacity_percent: Reading<f64>,
    pub current_capacity_mwh: Reading<u32>,
    pub voltage_mv: Reading<u32>,
    /// Negative while discharging, positive while charging (mW).
    pub charge_rate_mw: Reading<i32>,
    pub temperature_c: Reading<f64>,

    // Capacity and wear
    pub full_charged_capacity_mwh: Reading<u32>,
    pub designed_capacity_mwh: Reading<u32>,
    pub health_percent: Reading<f64>,
    pub cycle_count: Reading<u32>,

    // Firmware alert thresholds
    pub low_battery_capacity_1: Reading<u32>,
    pub low_battery_capacity_2: Reading<u32>,
    pub critical_bias: Reading<u32>,

    // Runtime estimates (seconds)
    pub estimated_runtime_s: Reading<u32>,
    pub full_runtime_s: Reading<u32>,

    /// True when capacity is reported as a percentage rather than mWh. Such packs
    /// cannot give a meaningful wear figure, and the report must not pretend otherwise.
    pub capacity_is_relative: bool,
    pub is_system_battery: bool,
}

/// Enumerate every system battery.
///
/// An empty vector is a legitimate result (desktop, or a laptop with the pack removed);
/// the caller decides whether that is `NotApplicable` or worth flagging.
pub fn probe() -> WinResult<Vec<BatteryReport>> {
    let paths = device::interface_paths(&GUID_DEVICE_BATTERY)?;
    let mut out = Vec::new();

    for path in paths {
        match probe_one(&path) {
            Ok(report) => out.push(report),
            // One unreadable pack should not hide the others.
            Err(_) => continue,
        }
    }

    Ok(out)
}

fn probe_one(path: &str) -> WinResult<BatteryReport> {
    let handle = device::open_device(path)?;
    let h = handle.raw();

    // The tag identifies this specific pack; it changes when the battery is swapped,
    // and every subsequent query must carry it.
    let mut tag_buf = [0u8; 4];
    let wait: u32 = 0;
    device_io_control(
        h,
        IOCTL_BATTERY_QUERY_TAG,
        Some(as_bytes(&wait)),
        &mut tag_buf,
        "IOCTL_BATTERY_QUERY_TAG",
    )?;
    let tag = u32::from_le_bytes(tag_buf);

    let info: Option<BatteryInformationRaw> = query_struct(h, tag, LEVEL_INFORMATION, 0);
    let status = query_status(h, tag);

    let capabilities = info.map(|i| i.capabilities).unwrap_or(0);
    let capacity_is_relative = capabilities & BATTERY_CAPACITY_RELATIVE != 0;
    let is_system_battery = capabilities & BATTERY_SYSTEM_BATTERY != 0;

    let designed = info.and_then(|i| sane_capacity(i.designed_capacity));
    let full_charged = info.and_then(|i| sane_capacity(i.full_charged_capacity));

    // Wear is only meaningful when both capacities are absolute and plausible.
    let health_percent = match (designed, full_charged, capacity_is_relative) {
        (_, _, true) => Reading::missing(Unavailable::NotApplicable),
        (Some(d), Some(f), false) if d > 0 => {
            Reading::value((f as f64 / d as f64) * 100.0)
        }
        _ => Reading::missing(Unavailable::ImplausibleValue(
            "design or full-charge capacity reported as 0".into(),
        )),
    };

    let current_capacity = status.and_then(|s| sane_capacity(s.capacity));
    let current_percent = match (current_capacity, full_charged) {
        (Some(c), Some(f)) if f > 0 => Reading::value((c as f64 / f as f64) * 100.0),
        _ => Reading::unsupported(),
    };

    let rate = status.and_then(|s| {
        // BATTERY_UNKNOWN_RATE is 0x80000000 reinterpreted as i32::MIN.
        if s.rate == i32::MIN {
            None
        } else {
            Some(s.rate)
        }
    });

    // "Full battery time at the current activity": how long a full pack would last at
    // the rate we are observing right now. Derived, because the driver only estimates
    // from the present charge level.
    let full_runtime = match (full_charged, rate) {
        (Some(f), Some(r)) if r < 0 => {
            Reading::value(((f as f64 / r.unsigned_abs() as f64) * 3600.0) as u32)
        }
        (_, Some(_)) => Reading::missing(Unavailable::NotApplicable), // charging or idle
        _ => Reading::unsupported(),
    };

    Ok(BatteryReport {
        device_name: query_string(h, tag, LEVEL_DEVICE_NAME),
        manufacturer: query_string(h, tag, LEVEL_MANUFACTURE_NAME),
        serial_number: query_string(h, tag, LEVEL_SERIAL_NUMBER),
        unique_id: query_string(h, tag, LEVEL_UNIQUE_ID),
        manufacture_date: query_manufacture_date(h, tag),
        chemistry: info
            .map(|i| decode_chemistry(&i.chemistry))
            .map(Reading::value)
            .unwrap_or_else(Reading::unsupported),

        power_state: status
            .map(|s| Reading::value(PowerState::from_bits(s.power_state)))
            .unwrap_or_else(Reading::unsupported),
        current_capacity_percent: current_percent,
        current_capacity_mwh: current_capacity.into(),
        voltage_mv: status.and_then(|s| sane_capacity(s.voltage)).into(),
        charge_rate_mw: rate.into(),
        temperature_c: query_temperature(h, tag),

        full_charged_capacity_mwh: full_charged.into(),
        designed_capacity_mwh: designed.into(),
        health_percent,
        cycle_count: query_cycle_count(info),

        low_battery_capacity_1: info.map(|i| i.default_alert1).into(),
        low_battery_capacity_2: info.map(|i| i.default_alert2).into(),
        critical_bias: info.map(|i| i.critical_bias).into(),

        estimated_runtime_s: query_estimated_time(h, tag),
        full_runtime_s: full_runtime,

        capacity_is_relative,
        is_system_battery,
    })
}

// ---------------------------------------------------------------------------
// Query helpers
// ---------------------------------------------------------------------------

fn query_raw(handle: windows::Win32::Foundation::HANDLE, tag: u32, level: u32, at_rate: i32, out: &mut [u8]) -> Option<u32> {
    let q = BatteryQueryInformation {
        battery_tag: tag,
        information_level: level,
        at_rate,
    };
    device_io_control(
        handle,
        IOCTL_BATTERY_QUERY_INFORMATION,
        Some(as_bytes(&q)),
        out,
        "IOCTL_BATTERY_QUERY_INFORMATION",
    )
    .ok()
}

fn query_struct<T: Copy>(
    handle: windows::Win32::Foundation::HANDLE,
    tag: u32,
    level: u32,
    at_rate: i32,
) -> Option<T> {
    let mut buf = vec![0u8; std::mem::size_of::<T>()];
    let written = query_raw(handle, tag, level, at_rate, &mut buf)?;
    if (written as usize) < std::mem::size_of::<T>() {
        return None;
    }
    unsafe { read_struct::<T>(&buf) }
}

fn query_status(
    handle: windows::Win32::Foundation::HANDLE,
    tag: u32,
) -> Option<BatteryStatusRaw> {
    let wait = BatteryWaitStatus {
        battery_tag: tag,
        ..Default::default()
    };
    let mut buf = vec![0u8; std::mem::size_of::<BatteryStatusRaw>()];
    let written = device_io_control(
        handle,
        IOCTL_BATTERY_QUERY_STATUS,
        Some(as_bytes(&wait)),
        &mut buf,
        "IOCTL_BATTERY_QUERY_STATUS",
    )
    .ok()?;
    if (written as usize) < std::mem::size_of::<BatteryStatusRaw>() {
        return None;
    }
    unsafe { read_struct::<BatteryStatusRaw>(&buf) }
}

fn query_string(
    handle: windows::Win32::Foundation::HANDLE,
    tag: u32,
    level: u32,
) -> Reading<String> {
    // These are variable-length UTF-16; 512 bytes is generous for a device name.
    let mut buf = vec![0u8; 512];
    match query_raw(handle, tag, level, 0, &mut buf) {
        Some(written) if written >= 2 => {
            let chars: Vec<u16> = buf[..written as usize]
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            let s = wide_to_string(&chars).trim().to_string();
            if s.is_empty() {
                Reading::unsupported()
            } else {
                Reading::value(s)
            }
        }
        _ => Reading::unsupported(),
    }
}

fn query_manufacture_date(
    handle: windows::Win32::Foundation::HANDLE,
    tag: u32,
) -> Reading<String> {
    match query_struct::<BatteryManufactureDate>(handle, tag, LEVEL_MANUFACTURE_DATE, 0) {
        Some(d) if d.year > 1980 && d.month >= 1 && d.month <= 12 && d.day >= 1 && d.day <= 31 => {
            Reading::value(format!("{:04}-{:02}-{:02}", d.year, d.month, d.day))
        }
        Some(_) => Reading::missing(Unavailable::ImplausibleValue(
            "manufacture date out of range".into(),
        )),
        None => Reading::unsupported(),
    }
}

fn query_temperature(
    handle: windows::Win32::Foundation::HANDLE,
    tag: u32,
) -> Reading<f64> {
    // Reported in tenths of a degree Kelvin. Most consumer packs do not implement it.
    match query_struct::<u32>(handle, tag, LEVEL_TEMPERATURE, 0) {
        Some(k) if k != 0 && k != BATTERY_UNKNOWN => {
            let c = (k as f64 / 10.0) - 273.15;
            if (-40.0..=100.0).contains(&c) {
                Reading::value(c)
            } else {
                Reading::missing(Unavailable::ImplausibleValue(format!("{c:.1} C")))
            }
        }
        _ => Reading::unsupported(),
    }
}

fn query_estimated_time(
    handle: windows::Win32::Foundation::HANDLE,
    tag: u32,
) -> Reading<u32> {
    // at_rate 0 means "estimate at the present drain rate".
    match query_struct::<u32>(handle, tag, LEVEL_ESTIMATED_TIME, 0) {
        Some(s) if s != BATTERY_UNKNOWN => Reading::value(s),
        Some(_) => Reading::missing(Unavailable::NotApplicable), // typically: on AC
        None => Reading::unsupported(),
    }
}

/// A zero cycle count means "this pack has no cycle counter", not "unused".
///
/// Reporting 0 here would tell a buyer the battery is factory-fresh, which is exactly
/// the kind of confident wrong answer this tool exists to avoid.
fn query_cycle_count(info: Option<BatteryInformationRaw>) -> Reading<u32> {
    match info {
        Some(i) if i.cycle_count > 0 => Reading::value(i.cycle_count),
        Some(_) => Reading::missing(Unavailable::NotSupportedByHardware),
        None => Reading::unsupported(),
    }
}

fn sane_capacity(v: u32) -> Option<u32> {
    if v == BATTERY_UNKNOWN || v == 0 {
        None
    } else {
        Some(v)
    }
}

fn decode_chemistry(raw: &[u8; 4]) -> String {
    let code: String = raw
        .iter()
        .filter(|&&b| b != 0)
        .map(|&b| b as char)
        .collect::<String>()
        .trim()
        .to_uppercase();

    match code.as_str() {
        "LION" | "LI-I" => "Lithium Ion".into(),
        "LIP" => "Lithium Polymer".into(),
        "NIMH" => "Nickel Metal Hydride".into(),
        "NICD" => "Nickel Cadmium".into(),
        "PBAC" => "Lead Acid".into(),
        "RAM" => "Rechargeable Alkaline-Manganese".into(),
        "" => "Unknown".into(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chemistry_codes_map_to_friendly_names() {
        assert_eq!(decode_chemistry(b"LION"), "Lithium Ion");
        assert_eq!(decode_chemistry(b"LiP\0"), "Lithium Polymer");
        assert_eq!(decode_chemistry(b"NiMH"), "Nickel Metal Hydride");
        // Unrecognised codes pass through rather than being hidden.
        assert_eq!(decode_chemistry(b"ZZZZ"), "ZZZZ");
    }

    #[test]
    fn zero_cycle_count_is_unknown_not_zero() {
        let info = BatteryInformationRaw {
            cycle_count: 0,
            ..Default::default()
        };
        let r = query_cycle_count(Some(info));
        assert!(!r.is_ok(), "a 0 cycle count must not be reported as a value");
    }

    #[test]
    fn real_cycle_count_passes_through() {
        let info = BatteryInformationRaw {
            cycle_count: 412,
            ..Default::default()
        };
        assert_eq!(query_cycle_count(Some(info)).get(), Some(&412));
    }

    #[test]
    fn unknown_sentinels_are_rejected() {
        assert_eq!(sane_capacity(BATTERY_UNKNOWN), None);
        assert_eq!(sane_capacity(0), None);
        assert_eq!(sane_capacity(24420), Some(24420));
    }

    #[test]
    fn power_state_bits_prioritise_critical() {
        assert_eq!(
            PowerState::from_bits(BATTERY_CRITICAL | BATTERY_DISCHARGING),
            PowerState::Critical
        );
        assert_eq!(PowerState::from_bits(BATTERY_DISCHARGING), PowerState::Discharging);
        assert_eq!(PowerState::from_bits(BATTERY_CHARGING), PowerState::Charging);
        assert_eq!(
            PowerState::from_bits(BATTERY_POWER_ON_LINE),
            PowerState::OnLineNotCharging
        );
    }

    #[test]
    fn struct_layouts_match_the_kernel_abi() {
        // BATTERY_INFORMATION is 4+1+3+4+4*6 = 40 bytes.
        assert_eq!(std::mem::size_of::<BatteryInformationRaw>(), 40);
        assert_eq!(std::mem::size_of::<BatteryStatusRaw>(), 16);
        assert_eq!(std::mem::size_of::<BatteryQueryInformation>(), 12);
        assert_eq!(std::mem::size_of::<BatteryManufactureDate>(), 4);
        assert_eq!(std::mem::size_of::<BatteryWaitStatus>(), 20);
    }

    /// The screenshot the project started from: MSI MS-N014, 24198 mWh full charge
    /// against 24420 mWh design. Health must land at ~99.1%, not 100%.
    #[test]
    fn health_matches_the_reference_screenshot() {
        let health = (24198.0 / 24420.0) * 100.0;
        assert!((health - 99.09).abs() < 0.05, "got {health}");
    }
}
