//! SATA/ATA SMART, read via `IOCTL_ATA_PASS_THROUGH`.
//!
//! Modern laptops are mostly NVMe, but plenty of used machines — and most cheap ones —
//! still ship a SATA SSD or a spinning disk, and those are exactly the drives most
//! likely to be worn out. Without this path the report would say "health unavailable"
//! on the drives that need checking most.
//!
//! Only `SMART READ DATA` (0xB0/0xD0) is issued: a data-in command that cannot alter
//! the drive.
//!
//! One caveat is handled explicitly rather than hidden. SMART "raw" values are
//! vendor-defined. Power-on hours (attribute 9) and power cycles (12) are consistent
//! enough across vendors to report directly; total-bytes-written (241) is *usually*
//! counted in 512-byte LBAs but some vendors use GB or 32 MiB units, so a value that
//! implies an absurd write volume is rejected rather than printed.

use serde::{Deserialize, Serialize};

use crate::model::{Reading, Unavailable};
use crate::win::device::{open_physical_drive_rw, SafeHandle};
use crate::win::ioctl::{
    as_bytes, device_io_control, IOCTL_ATA_PASS_THROUGH, SMART_GET_VERSION, SMART_RCV_DRIVE_DATA,
};
use crate::win::WinError;

// ---------------------------------------------------------------------------
// Kernel ABI (ntddscsi.h)
// ---------------------------------------------------------------------------

const ATA_FLAGS_DRDY_REQUIRED: u16 = 0x01;
const ATA_FLAGS_DATA_IN: u16 = 0x02;

const SMART_CMD: u8 = 0xB0;
const SMART_READ_DATA: u8 = 0xD0;
/// The magic LBA mid/high pair that identifies a SMART command.
const SMART_LBA_MID: u8 = 0x4F;
const SMART_LBA_HIGH: u8 = 0xC2;

const SECTOR_SIZE: usize = 512;

/// `IDEREGS` — the ATA task-file registers.
#[repr(C)]
#[derive(Copy, Clone, Default)]
struct IdeRegs {
    features: u8,
    sector_count: u8,
    sector_number: u8,
    cyl_low: u8,
    cyl_high: u8,
    drive_head: u8,
    command: u8,
    reserved: u8,
}

/// `SENDCMDINPARAMS` (minus the trailing flexible buffer).
#[repr(C)]
#[derive(Copy, Clone, Default)]
struct SendCmdInParams {
    buffer_size: u32,
    drive_regs: IdeRegs,
    drive_number: u8,
    reserved: [u8; 3],
    dw_reserved: [u32; 4],
}

/// `SENDCMDOUTPARAMS` header: `cBufferSize` + `DRIVERSTATUS`, before `bBuffer`.
#[repr(C)]
#[derive(Copy, Clone, Default)]
struct SendCmdOutHeader {
    buffer_size: u32,
    driver_error: u8,
    ide_error: u8,
    reserved: [u8; 2],
    dw_reserved: [u32; 2],
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
struct AtaPassThroughEx {
    length: u16,
    ata_flags: u16,
    path_id: u8,
    target_id: u8,
    lun: u8,
    reserved_as_uchar: u8,
    data_transfer_length: u32,
    timeout_value: usize,
    reserved_as_ulong: usize,
    data_buffer_offset: usize,
    previous_task_file: [u8; 8],
    current_task_file: [u8; 8],
}

// ---------------------------------------------------------------------------
// Public shape
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmartAttribute {
    pub id: u8,
    pub name: String,
    /// Normalised value, 0-253. Higher is healthier for most attributes.
    pub current: u8,
    pub worst: u8,
    pub raw: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AtaHealth {
    pub power_on_hours: Reading<u64>,
    pub power_cycles: Reading<u64>,
    /// Non-zero means the drive has already remapped failing sectors.
    pub reallocated_sectors: Reading<u64>,
    /// Sectors the drive cannot read but has not yet remapped. The most urgent signal.
    pub pending_sectors: Reading<u64>,
    pub uncorrectable_sectors: Reading<u64>,
    pub temperature_c: Reading<f64>,
    /// SSD endurance remaining, from the normalised wear attribute.
    pub life_remaining_percent: Reading<u8>,
    pub terabytes_written: Reading<f64>,
    pub attributes: Vec<SmartAttribute>,
}

/// Well-known attribute IDs. Names follow common industry usage.
fn attribute_name(id: u8) -> &'static str {
    match id {
        1 => "Raw Read Error Rate",
        3 => "Spin-Up Time",
        4 => "Start/Stop Count",
        5 => "Reallocated Sectors Count",
        7 => "Seek Error Rate",
        9 => "Power-On Hours",
        10 => "Spin Retry Count",
        12 => "Power Cycle Count",
        170 => "Available Reserved Space",
        171 => "Program Fail Count",
        172 => "Erase Fail Count",
        173 => "Wear Leveling Count",
        174 => "Unexpected Power Loss Count",
        175 => "Program Fail Count (Chip)",
        177 => "Wear Leveling Count",
        179 => "Used Reserved Block Count",
        181 => "Program Fail Count (Total)",
        182 => "Erase Fail Count (Total)",
        183 => "Runtime Bad Block",
        184 => "End-to-End Error",
        187 => "Reported Uncorrectable Errors",
        188 => "Command Timeout",
        190 => "Airflow Temperature",
        194 => "Temperature",
        195 => "Hardware ECC Recovered",
        196 => "Reallocation Event Count",
        197 => "Current Pending Sector Count",
        198 => "Offline Uncorrectable Sector Count",
        199 => "UltraDMA CRC Error Count",
        201 => "Soft Read Error Rate",
        202 => "Data Address Mark Errors",
        231 => "SSD Life Left",
        232 => "Available Reserved Space",
        233 => "Media Wearout Indicator",
        241 => "Total LBAs Written",
        242 => "Total LBAs Read",
        _ => "Vendor Specific",
    }
}

/// Issue SMART READ DATA against a physical drive.
///
/// Two transports are attempted. Drivers disagree about which they accept: the AHCI
/// controller this was developed against rejects `IOCTL_ATA_PASS_THROUGH` outright with
/// ERROR_REVISION_MISMATCH while answering the legacy IOCTL, and USB bridges frequently
/// do the reverse. Trying both is the difference between reporting a drive's real
/// history and reporting nothing.
pub fn probe_drive(index: u32) -> Reading<AtaHealth> {
    let handle = match open_physical_drive_rw(index) {
        Ok(h) => h,
        Err(e) if e.code == 5 => return Reading::missing(Unavailable::RequiresElevation),
        Err(e) => return Reading::failed(e),
    };

    let legacy = match read_smart_legacy(&handle) {
        Ok(sector) => return Reading::value(parse_smart(&sector)),
        Err(e) => e,
    };

    match read_smart_passthrough(&handle) {
        Ok(sector) => Reading::value(parse_smart(&sector)),
        Err(passthrough) => classify_failure(legacy, passthrough),
    }
}

/// Decide what a pair of transport failures means for the report.
fn classify_failure(legacy: WinError, passthrough: WinError) -> Reading<AtaHealth> {
    const ACCESS_DENIED: u32 = 5;
    // ERROR_INVALID_FUNCTION, ERROR_NOT_SUPPORTED, ERROR_INVALID_PARAMETER,
    // ERROR_REVISION_MISMATCH: all mean this controller will not carry SMART.
    const UNSUPPORTED: [u32; 4] = [1, 50, 87, 1306];

    if legacy.code == ACCESS_DENIED || passthrough.code == ACCESS_DENIED {
        return Reading::missing(Unavailable::RequiresElevation);
    }
    if UNSUPPORTED.contains(&legacy.code) && UNSUPPORTED.contains(&passthrough.code) {
        return Reading::missing(Unavailable::NotSupportedByHardware);
    }
    Reading::failed(format!("legacy: {legacy}; pass-through: {passthrough}"))
}

/// Legacy `SMART_RCV_DRIVE_DATA` transport.
fn read_smart_legacy(handle: &SafeHandle) -> Result<Vec<u8>, WinError> {
    // Some drivers will not service SMART reads until the version query has been made.
    // A failure here is not fatal, so the result is deliberately ignored.
    let mut version = [0u8; 24];
    let _ = device_io_control(
        handle.raw(),
        SMART_GET_VERSION,
        None,
        &mut version,
        "SMART_GET_VERSION",
    );

    let input = SendCmdInParams {
        buffer_size: SECTOR_SIZE as u32,
        drive_regs: IdeRegs {
            features: SMART_READ_DATA,
            sector_count: 1,
            sector_number: 1,
            cyl_low: SMART_LBA_MID,
            cyl_high: SMART_LBA_HIGH,
            // 0xA0 is the legacy master-device selector this IOCTL expects.
            drive_head: 0xA0,
            command: SMART_CMD,
            reserved: 0,
        },
        // Relative to the handle, which already names one drive.
        drive_number: 0,
        ..Default::default()
    };

    let header_len = std::mem::size_of::<SendCmdOutHeader>();
    let mut output = vec![0u8; header_len + SECTOR_SIZE];

    let written = device_io_control(
        handle.raw(),
        SMART_RCV_DRIVE_DATA,
        Some(as_bytes(&input)),
        &mut output,
        "SMART_RCV_DRIVE_DATA",
    )?;

    if (written as usize) < header_len + SECTOR_SIZE {
        return Err(WinError::new("SMART_RCV_DRIVE_DATA short buffer", written));
    }

    Ok(output[header_len..header_len + SECTOR_SIZE].to_vec())
}

/// Modern `IOCTL_ATA_PASS_THROUGH` transport.
fn read_smart_passthrough(handle: &SafeHandle) -> Result<Vec<u8>, WinError> {
    let header_len = std::mem::size_of::<AtaPassThroughEx>();
    let total = header_len + SECTOR_SIZE;

    let mut task_file = [0u8; 8];
    task_file[0] = SMART_READ_DATA; // Features
    task_file[1] = 1; // SectorCount
    task_file[2] = 0; // LBA Low
    task_file[3] = SMART_LBA_MID;
    task_file[4] = SMART_LBA_HIGH;
    task_file[5] = 0; // Device/Head
    task_file[6] = SMART_CMD; // Command

    let request = AtaPassThroughEx {
        length: header_len as u16,
        ata_flags: ATA_FLAGS_DRDY_REQUIRED | ATA_FLAGS_DATA_IN,
        data_transfer_length: SECTOR_SIZE as u32,
        timeout_value: 10,
        data_buffer_offset: header_len,
        current_task_file: task_file,
        ..Default::default()
    };

    // The request header and the inbound data share one buffer, so it must be passed
    // as both input and output.
    let mut buffer = vec![0u8; total];
    buffer[..header_len].copy_from_slice(unsafe {
        std::slice::from_raw_parts(&request as *const _ as *const u8, header_len)
    });

    let input = buffer.clone();
    let written = device_io_control(
        handle.raw(),
        IOCTL_ATA_PASS_THROUGH,
        Some(&input),
        &mut buffer,
        "IOCTL_ATA_PASS_THROUGH(SMART READ DATA)",
    )?;

    if (written as usize) < total {
        return Err(WinError::new("ata pass-through returned short buffer", written));
    }

    Ok(buffer[header_len..total].to_vec())
}

/// Decode the 512-byte SMART data sector into attributes and derived health figures.
pub fn parse_smart(sector: &[u8]) -> AtaHealth {
    let mut attributes = Vec::new();

    // Attribute table: 30 entries of 12 bytes, starting at offset 2.
    for i in 0..30 {
        let off = 2 + i * 12;
        let entry = match sector.get(off..off + 12) {
            Some(e) => e,
            None => break,
        };
        let id = entry[0];
        // ID 0 marks an unused slot.
        if id == 0 {
            continue;
        }
        // Raw is 6 bytes little-endian at offset 5.
        let raw = (entry[5] as u64)
            | ((entry[6] as u64) << 8)
            | ((entry[7] as u64) << 16)
            | ((entry[8] as u64) << 24)
            | ((entry[9] as u64) << 32)
            | ((entry[10] as u64) << 40);

        attributes.push(SmartAttribute {
            id,
            name: attribute_name(id).to_string(),
            current: entry[3],
            worst: entry[4],
            raw,
        });
    }

    let raw_of = |id: u8| attributes.iter().find(|a| a.id == id).map(|a| a.raw);
    let norm_of = |id: u8| attributes.iter().find(|a| a.id == id).map(|a| a.current);

    // Power-on hours occupies the low 32 bits; some drives pack other data above.
    let power_on_hours = raw_of(9).map(|r| r & 0xFFFF_FFFF);

    // Temperature: the low byte is the current reading on essentially all vendors.
    // Attribute 194 is preferred, 190 (airflow) is the fallback.
    let temperature_c = match raw_of(194).or_else(|| raw_of(190)) {
        Some(r) => {
            let t = (r & 0xFF) as f64;
            if (1.0..=110.0).contains(&t) {
                Reading::value(t)
            } else {
                Reading::missing(Unavailable::ImplausibleValue(format!("{t:.0} C")))
            }
        }
        None => Reading::unsupported(),
    };

    // SSD endurance: the normalised value of the wear attribute counts down from 100.
    let life_remaining_percent = match norm_of(231)
        .or_else(|| norm_of(233))
        .or_else(|| norm_of(177))
        .or_else(|| norm_of(173))
    {
        Some(v) if v <= 100 => Reading::value(v),
        // A spinning disk has no endurance figure, and 253 is the "not populated" value.
        _ => Reading::missing(Unavailable::NotSupportedByHardware),
    };

    // Total LBAs Written is usually 512-byte sectors, but the unit is vendor-defined.
    // Rather than print a number that could be off by orders of magnitude, reject
    // values implying a write volume no consumer drive reaches.
    let terabytes_written = match raw_of(241) {
        Some(lbas) if lbas > 0 => {
            let tb = (lbas as f64 * 512.0) / 1e12;
            if tb < 20_000.0 {
                Reading::value(tb)
            } else {
                Reading::missing(Unavailable::ImplausibleValue(
                    "attribute 241 uses a vendor-specific unit".into(),
                ))
            }
        }
        _ => Reading::unsupported(),
    };

    AtaHealth {
        power_on_hours: power_on_hours.into(),
        power_cycles: raw_of(12).map(|r| r & 0xFFFF_FFFF).into(),
        reallocated_sectors: raw_of(5).into(),
        pending_sectors: raw_of(197).into(),
        uncorrectable_sectors: raw_of(198).into(),
        temperature_c,
        life_remaining_percent,
        terabytes_written,
        attributes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a SMART sector with the given (id, current, worst, raw) attributes.
    fn sector_with(attrs: &[(u8, u8, u8, u64)]) -> Vec<u8> {
        let mut s = vec![0u8; SECTOR_SIZE];
        s[0] = 0x10; // revision
        for (i, &(id, current, worst, raw)) in attrs.iter().enumerate() {
            let off = 2 + i * 12;
            s[off] = id;
            s[off + 3] = current;
            s[off + 4] = worst;
            for b in 0..6 {
                s[off + 5 + b] = ((raw >> (8 * b)) & 0xFF) as u8;
            }
        }
        s
    }

    #[test]
    fn struct_layout_matches_ntddscsi_h() {
        // x64: 8 + 4 (+4 pad) + 8 + 8 + 8 + 8 + 8 = 56
        assert_eq!(std::mem::size_of::<AtaPassThroughEx>(), 56);
        assert_eq!(std::mem::size_of::<IdeRegs>(), 8);
        // SENDCMDINPARAMS without its trailing bBuffer[1] element.
        assert_eq!(std::mem::size_of::<SendCmdInParams>(), 32);
        // SENDCMDOUTPARAMS header: cBufferSize + DRIVERSTATUS.
        assert_eq!(std::mem::size_of::<SendCmdOutHeader>(), 16);
    }

    #[test]
    fn smart_ioctl_codes_are_correct() {
        assert_eq!(IOCTL_ATA_PASS_THROUGH, 0x0004_D02C);
        assert_eq!(SMART_RCV_DRIVE_DATA, 0x0007_C088);
        assert_eq!(SMART_GET_VERSION, 0x0007_4080);
    }

    #[test]
    fn both_transports_failing_as_unsupported_reads_as_hardware_limitation() {
        let r = classify_failure(
            WinError::new("legacy", 1),
            WinError::new("passthrough", 1306),
        );
        assert!(!r.is_ok());
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("not_supported_by_hardware"), "got {json}");
    }

    #[test]
    fn access_denied_on_either_transport_asks_for_elevation() {
        let r = classify_failure(WinError::new("legacy", 5), WinError::new("pt", 1306));
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("requires_elevation"), "got {json}");
    }

    #[test]
    fn decodes_the_odometer_attributes() {
        let s = sector_with(&[(9, 95, 95, 21_544), (12, 99, 99, 3_812)]);
        let h = parse_smart(&s);
        assert_eq!(h.power_on_hours.get(), Some(&21_544));
        assert_eq!(h.power_cycles.get(), Some(&3_812));
    }

    #[test]
    fn surfaces_failing_sector_counts() {
        let s = sector_with(&[(5, 100, 100, 8), (197, 100, 100, 3), (198, 100, 100, 1)]);
        let h = parse_smart(&s);
        assert_eq!(h.reallocated_sectors.get(), Some(&8));
        assert_eq!(h.pending_sectors.get(), Some(&3));
        assert_eq!(h.uncorrectable_sectors.get(), Some(&1));
    }

    #[test]
    fn temperature_reads_from_the_low_byte() {
        // Drives commonly pack min/max into the upper bytes.
        let s = sector_with(&[(194, 100, 100, 0x0018_0020_002A)]);
        let h = parse_smart(&s);
        assert_eq!(h.temperature_c.get(), Some(&42.0));
    }

    #[test]
    fn falls_back_to_airflow_temperature() {
        let s = sector_with(&[(190, 100, 100, 38)]);
        let h = parse_smart(&s);
        assert_eq!(h.temperature_c.get(), Some(&38.0));
    }

    #[test]
    fn ssd_life_uses_the_normalised_value() {
        let s = sector_with(&[(231, 87, 87, 0)]);
        let h = parse_smart(&s);
        assert_eq!(h.life_remaining_percent.get(), Some(&87));
    }

    /// A spinning disk has no endurance attribute; 253 means "not populated" and must
    /// not be reported as 253% life remaining.
    #[test]
    fn missing_endurance_attribute_is_unavailable() {
        let s = sector_with(&[(9, 95, 95, 100)]);
        let h = parse_smart(&s);
        assert!(!h.life_remaining_percent.is_ok());
    }

    #[test]
    fn tbw_converts_from_512_byte_lbas() {
        // 1 TB == 1e12 / 512 LBAs.
        let s = sector_with(&[(241, 100, 100, 1_953_125_000)]);
        let h = parse_smart(&s);
        let tbw = *h.terabytes_written.get().unwrap();
        assert!((tbw - 1.0).abs() < 0.01, "got {tbw}");
    }

    /// Vendors that count attribute 241 in GB or 32 MiB units produce absurd totals.
    /// Reporting "4 million TB written" would destroy trust in the whole report.
    #[test]
    fn rejects_vendor_units_that_imply_impossible_writes() {
        let s = sector_with(&[(241, 100, 100, u64::MAX >> 16)]);
        let h = parse_smart(&s);
        assert!(
            !h.terabytes_written.is_ok(),
            "implausible TBW should be rejected, not printed"
        );
    }

    #[test]
    fn unused_attribute_slots_are_skipped() {
        let s = sector_with(&[(9, 95, 95, 100)]);
        let h = parse_smart(&s);
        assert_eq!(h.attributes.len(), 1, "empty slots must not become entries");
    }

    #[test]
    fn short_sector_does_not_panic() {
        let h = parse_smart(&[0u8; 8]);
        assert!(h.attributes.is_empty());
    }
}
