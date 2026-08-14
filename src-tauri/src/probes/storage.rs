//! Storage probe — NVMe SMART/health via `IOCTL_STORAGE_QUERY_PROPERTY`.
//!
//! Power-on hours is the closest thing a PC has to an odometer, and unlike the chassis
//! condition or a freshly reinstalled Windows, a seller cannot easily reset it. Reading
//! it against the claimed age, the BIOS date and the battery manufacture date is how
//! "barely used" gets tested.
//!
//! Everything here opens the drive with zero access rights (`dwDesiredAccess = 0`),
//! which is sufficient for the query IOCTLs and makes it impossible for this probe to
//! modify a stranger's disk.

use serde::{Deserialize, Serialize};

use crate::model::{Reading, Unavailable};
use crate::probes::ata_smart::{self, AtaHealth};
use crate::win::device::{self, SafeHandle};
use crate::win::ioctl::{as_bytes, device_io_control, read_struct, IOCTL_STORAGE_QUERY_PROPERTY};

// ---------------------------------------------------------------------------
// Kernel ABI (winioctl.h / ntddstor.h)
// ---------------------------------------------------------------------------

const STORAGE_PROPERTY_DEVICE: u32 = 0;
const STORAGE_PROPERTY_DEVICE_PROTOCOL_SPECIFIC: u32 = 50;
const PROPERTY_STANDARD_QUERY: u32 = 0;

const PROTOCOL_TYPE_NVME: u32 = 3;
const NVME_DATA_TYPE_LOG_PAGE: u32 = 2;
const NVME_LOG_PAGE_HEALTH_INFO: u32 = 0x02;

/// NVMe requires the caller to request at least a full 512-byte log page.
const NVME_LOG_PAGE_SIZE: u32 = 512;

#[repr(C)]
#[derive(Copy, Clone, Default)]
struct StorageProtocolSpecificData {
    protocol_type: u32,
    data_type: u32,
    request_value: u32,
    request_sub_value: u32,
    data_offset: u32,
    data_length: u32,
    fixed_protocol_return_data: u32,
    request_sub_value_2: u32,
    request_sub_value_3: u32,
    request_sub_value_4: u32,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
struct StorageProtocolQuery {
    property_id: u32,
    query_type: u32,
    protocol_specific: StorageProtocolSpecificData,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
struct StoragePropertyQuery {
    property_id: u32,
    query_type: u32,
    additional: [u8; 8],
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
struct StorageDeviceDescriptorHeader {
    version: u32,
    size: u32,
    device_type: u8,
    device_type_modifier: u8,
    removable_media: u8,
    command_queueing: u8,
    vendor_id_offset: u32,
    product_id_offset: u32,
    product_revision_offset: u32,
    serial_number_offset: u32,
    bus_type: u32,
    raw_properties_length: u32,
}

/// `STORAGE_BUS_TYPE`
fn bus_type_name(v: u32) -> &'static str {
    match v {
        0x01 => "SCSI",
        0x02 => "ATAPI",
        0x03 => "ATA",
        0x04 => "IEEE 1394",
        0x05 => "SSA",
        0x06 => "Fibre Channel",
        0x07 => "USB",
        0x08 => "RAID",
        0x09 => "iSCSI",
        0x0A => "SAS",
        0x0B => "SATA",
        0x0C => "SD",
        0x0D => "MMC",
        0x11 => "NVMe",
        0x12 => "SCM",
        0x13 => "UFS",
        _ => "Unknown",
    }
}

// ---------------------------------------------------------------------------
// Public shape
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NvmeHealth {
    /// Non-zero means the controller is asserting a fault right now.
    pub critical_warning: u8,
    pub composite_temp_c: Reading<f64>,
    pub available_spare_percent: Reading<u8>,
    pub available_spare_threshold_percent: Reading<u8>,
    /// Vendor wear estimate. Can exceed 100 on a worn drive.
    pub percentage_used: Reading<u8>,
    pub power_on_hours: Reading<u64>,
    pub power_cycles: Reading<u64>,
    pub unsafe_shutdowns: Reading<u64>,
    pub media_errors: Reading<u64>,
    pub error_log_entries: Reading<u64>,
    pub data_units_read: Reading<u64>,
    pub data_units_written: Reading<u64>,
    /// Derived from data units written (1 unit = 512 000 bytes).
    pub terabytes_written: Reading<f64>,
}

/// Drive health, tagged by the command set it came from.
///
/// NVMe and ATA expose genuinely different data — NVMe has a controller-computed
/// `percentage_used`, ATA has per-attribute sector counts — so they are kept distinct
/// rather than flattened into a lowest-common-denominator shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "protocol", rename_all = "snake_case")]
pub enum DriveHealth {
    Nvme(NvmeHealth),
    Ata(AtaHealth),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriveReport {
    pub index: u32,
    pub model: Reading<String>,
    pub serial: Reading<String>,
    pub firmware: Reading<String>,
    pub bus_type: String,
    pub removable: bool,
    pub health: Reading<DriveHealth>,
}

/// Probe every physical drive present.
///
/// Enumeration stops at the first gap of consecutive misses rather than assuming a
/// contiguous range, because drive indices are not guaranteed to be dense.
pub fn probe() -> Vec<DriveReport> {
    let mut out = Vec::new();
    let mut consecutive_misses = 0;

    for index in 0..64u32 {
        match device::open_physical_drive(index) {
            Ok(handle) => {
                consecutive_misses = 0;
                out.push(probe_drive(index, &handle));
            }
            Err(_) => {
                consecutive_misses += 1;
                // Give up once we are clearly past the end of the drive list.
                if consecutive_misses >= 8 && !out.is_empty() {
                    break;
                }
                if consecutive_misses >= 16 {
                    break;
                }
            }
        }
    }

    out
}

fn probe_drive(index: u32, handle: &SafeHandle) -> DriveReport {
    let descriptor = query_device_descriptor(handle);

    let (model, serial, firmware, bus_type, removable) = match &descriptor {
        Some(d) => (
            d.product.clone().into(),
            d.serial.clone().into(),
            d.revision.clone().into(),
            bus_type_name(d.bus_type).to_string(),
            d.removable,
        ),
        None => (
            Reading::unsupported(),
            Reading::unsupported(),
            Reading::unsupported(),
            "Unknown".to_string(),
            false,
        ),
    };

    let bus = descriptor.as_ref().map(|d| d.bus_type).unwrap_or(0);

    let health = match bus {
        0x11 => match query_nvme_health(handle) {
            Ok(h) => Reading::value(DriveHealth::Nvme(h)),
            Err(e) => Reading::failed(e),
        },
        // SATA, ATA and ATAPI all speak the ATA SMART command set.
        0x02 | 0x03 | 0x0B => ata_smart::probe_drive(index).map(DriveHealth::Ata),
        // USB enclosures and RAID volumes usually do not pass either command set through.
        _ => Reading::missing(Unavailable::NotSupportedByHardware),
    };

    DriveReport {
        index,
        model,
        serial,
        firmware,
        bus_type,
        removable,
        health,
    }
}

struct DeviceDescriptor {
    product: Option<String>,
    serial: Option<String>,
    revision: Option<String>,
    bus_type: u32,
    removable: bool,
}

fn query_device_descriptor(handle: &SafeHandle) -> Option<DeviceDescriptor> {
    let query = StoragePropertyQuery {
        property_id: STORAGE_PROPERTY_DEVICE,
        query_type: PROPERTY_STANDARD_QUERY,
        additional: [0; 8],
    };

    let mut buf = vec![0u8; 4096];
    let written = device_io_control(
        handle.raw(),
        IOCTL_STORAGE_QUERY_PROPERTY,
        Some(as_bytes(&query)),
        &mut buf,
        "IOCTL_STORAGE_QUERY_PROPERTY(device)",
    )
    .ok()?;

    let header: StorageDeviceDescriptorHeader =
        unsafe { read_struct(&buf[..written as usize]) }?;

    let at = |offset: u32| -> Option<String> {
        if offset == 0 || offset as usize >= written as usize {
            return None;
        }
        let start = offset as usize;
        let end = buf[start..written as usize]
            .iter()
            .position(|&b| b == 0)
            .map(|p| start + p)
            .unwrap_or(written as usize);
        let s = String::from_utf8_lossy(&buf[start..end]).trim().to_string();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    };

    Some(DeviceDescriptor {
        product: at(header.product_id_offset),
        serial: at(header.serial_number_offset),
        revision: at(header.product_revision_offset),
        bus_type: header.bus_type,
        removable: header.removable_media != 0,
    })
}

fn query_nvme_health(handle: &SafeHandle) -> Result<NvmeHealth, crate::win::WinError> {
    let header_len = std::mem::size_of::<StorageProtocolQuery>() as u32;
    // ProtocolDataOffset is measured from the start of STORAGE_PROTOCOL_SPECIFIC_DATA,
    // which sits 8 bytes into the descriptor.
    let specific_len = std::mem::size_of::<StorageProtocolSpecificData>() as u32;

    let query = StorageProtocolQuery {
        property_id: STORAGE_PROPERTY_DEVICE_PROTOCOL_SPECIFIC,
        query_type: PROPERTY_STANDARD_QUERY,
        protocol_specific: StorageProtocolSpecificData {
            protocol_type: PROTOCOL_TYPE_NVME,
            data_type: NVME_DATA_TYPE_LOG_PAGE,
            request_value: NVME_LOG_PAGE_HEALTH_INFO,
            data_offset: specific_len,
            data_length: NVME_LOG_PAGE_SIZE,
            ..Default::default()
        },
    };

    let mut buf = vec![0u8; (header_len + NVME_LOG_PAGE_SIZE) as usize + 64];
    let written = device_io_control(
        handle.raw(),
        IOCTL_STORAGE_QUERY_PROPERTY,
        Some(as_bytes(&query)),
        &mut buf,
        "IOCTL_STORAGE_QUERY_PROPERTY(nvme health)",
    )?;

    // Descriptor header is Version+Size (8 bytes), then the specific data, then the log.
    let log_start = 8 + specific_len as usize;
    let log = buf
        .get(log_start..(log_start + NVME_LOG_PAGE_SIZE as usize).min(written as usize))
        .filter(|s| s.len() >= 192)
        .ok_or_else(|| {
            crate::win::WinError::new("nvme health log truncated", written)
        })?;

    Ok(parse_nvme_health(log))
}

/// Decode the NVMe SMART / Health Information log page (NVMe spec, log page 02h).
pub fn parse_nvme_health(log: &[u8]) -> NvmeHealth {
    // Counters are 128-bit little-endian. Real drives never approach 2^64 of anything
    // here, so the low 64 bits are the useful part.
    let u128_lo = |off: usize| -> Reading<u64> {
        match log.get(off..off + 8) {
            Some(b) => Reading::value(u64::from_le_bytes([
                b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
            ])),
            None => Reading::unsupported(),
        }
    };

    let composite_temp_c = match log.get(1..3) {
        Some(b) => {
            let kelvin = u16::from_le_bytes([b[0], b[1]]);
            // 0 means "not reported"; the field is absolute Kelvin.
            if kelvin == 0 {
                Reading::unsupported()
            } else {
                let c = kelvin as f64 - 273.15;
                if (-40.0..=125.0).contains(&c) {
                    Reading::value(c)
                } else {
                    Reading::missing(Unavailable::ImplausibleValue(format!("{c:.0} C")))
                }
            }
        }
        None => Reading::unsupported(),
    };

    let data_units_written = u128_lo(48);
    let terabytes_written = match data_units_written.get() {
        // One data unit is 1000 * 512 bytes.
        Some(&units) => Reading::value((units as f64 * 512_000.0) / 1e12),
        None => Reading::unsupported(),
    };

    NvmeHealth {
        critical_warning: log.first().copied().unwrap_or(0),
        composite_temp_c,
        available_spare_percent: log.get(3).copied().into(),
        available_spare_threshold_percent: log.get(4).copied().into(),
        percentage_used: log.get(5).copied().into(),
        data_units_read: u128_lo(32),
        data_units_written,
        power_cycles: u128_lo(112),
        power_on_hours: u128_lo(128),
        unsafe_shutdowns: u128_lo(144),
        media_errors: u128_lo(160),
        error_log_entries: u128_lo(176),
        terabytes_written,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log_with(fields: &[(usize, u64)]) -> Vec<u8> {
        let mut log = vec![0u8; 512];
        for &(off, val) in fields {
            log[off..off + 8].copy_from_slice(&val.to_le_bytes());
        }
        log
    }

    #[test]
    fn decodes_the_odometer_fields() {
        let mut log = log_with(&[(128, 14_236), (112, 1_842), (144, 37)]);
        log[5] = 8; // percentage used
        log[3] = 100; // available spare

        let h = parse_nvme_health(&log);
        assert_eq!(h.power_on_hours.get(), Some(&14_236));
        assert_eq!(h.power_cycles.get(), Some(&1_842));
        assert_eq!(h.unsafe_shutdowns.get(), Some(&37));
        assert_eq!(h.percentage_used.get(), Some(&8));
        assert_eq!(h.available_spare_percent.get(), Some(&100));
    }

    #[test]
    fn terabytes_written_uses_the_512000_byte_data_unit() {
        // 1 TB written == 1e12 / 512000 == 1_953_125 data units.
        let log = log_with(&[(48, 1_953_125)]);
        let h = parse_nvme_health(&log);
        let tbw = *h.terabytes_written.get().unwrap();
        assert!((tbw - 1.0).abs() < 0.001, "expected ~1.0 TB, got {tbw}");
    }

    #[test]
    fn temperature_converts_from_absolute_kelvin() {
        let mut log = vec![0u8; 512];
        log[1..3].copy_from_slice(&313u16.to_le_bytes()); // 313 K -> ~39.85 C
        let h = parse_nvme_health(&log);
        let t = *h.composite_temp_c.get().unwrap();
        assert!((t - 39.85).abs() < 0.1, "got {t}");
    }

    #[test]
    fn zero_temperature_means_not_reported_not_freezing() {
        let log = vec![0u8; 512];
        let h = parse_nvme_health(&log);
        assert!(
            !h.composite_temp_c.is_ok(),
            "0 K must not be reported as -273 C"
        );
    }

    #[test]
    fn short_log_does_not_panic() {
        let h = parse_nvme_health(&[0u8; 16]);
        assert!(!h.power_on_hours.is_ok());
    }

    #[test]
    fn struct_layouts_match_the_kernel_abi() {
        assert_eq!(std::mem::size_of::<StorageProtocolSpecificData>(), 40);
        assert_eq!(std::mem::size_of::<StoragePropertyQuery>(), 16);
        // 8-byte header + 40-byte specific data
        assert_eq!(std::mem::size_of::<StorageProtocolQuery>(), 48);
    }

    #[test]
    fn nvme_bus_type_is_recognised() {
        assert_eq!(bus_type_name(0x11), "NVMe");
        assert_eq!(bus_type_name(0x0B), "SATA");
    }

    #[test]
    fn probing_this_machine_finds_at_least_one_drive() {
        let drives = probe();
        assert!(!drives.is_empty(), "no physical drives enumerated");
    }
}
