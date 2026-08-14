//! Firmware probe — machine identity from SMBIOS, and firmware-persistence red flags
//! from the ACPI table list.
//!
//! Both providers are readable from ring 3 without elevation.
//!
//! Two jobs:
//!
//! 1. **Identity.** Model, serial, SKU, BIOS date, chassis type. Cross-referencing the
//!    BIOS date against SSD power-on hours and battery manufacture date is how a
//!    "barely used, bought last year" claim gets tested.
//!
//! 2. **WPBT detection.** The Windows Platform Binary Table lets firmware inject an
//!    executable that Windows runs at every boot — surviving a disk wipe or an SSD
//!    swap. It is the mechanism behind Absolute/Computrace anti-theft. A used laptop
//!    still enrolled with a previous owner's IT department can be remotely locked or
//!    wiped, and no amount of reinstalling Windows removes it. For some buyers this is
//!    the single most important thing in the report.

use serde::{Deserialize, Serialize};

use windows::Win32::System::SystemInformation::FIRMWARE_TABLE_PROVIDER;

use crate::model::{Reading, Unavailable};
use crate::win::{WinError, WinResult};

// Provider signatures are the four-character codes packed big-endian, matching the
// multi-character literals ('RSMB', 'ACPI') used in the Win32 documentation.
const PROVIDER_RSMB: FIRMWARE_TABLE_PROVIDER = FIRMWARE_TABLE_PROVIDER(0x5253_4D42);
const PROVIDER_ACPI: FIRMWARE_TABLE_PROVIDER = FIRMWARE_TABLE_PROVIDER(0x4143_5049);

// ---------------------------------------------------------------------------
// Raw firmware table access
// ---------------------------------------------------------------------------

fn get_firmware_table(provider: FIRMWARE_TABLE_PROVIDER, table_id: u32) -> WinResult<Vec<u8>> {
    use windows::Win32::System::SystemInformation::GetSystemFirmwareTable;

    unsafe {
        let needed = GetSystemFirmwareTable(provider, table_id, None);
        if needed == 0 {
            return Err(WinError::last("GetSystemFirmwareTable(size)"));
        }
        let mut buf = vec![0u8; needed as usize];
        let written = GetSystemFirmwareTable(provider, table_id, Some(&mut buf));
        if written == 0 || written > needed {
            return Err(WinError::last("GetSystemFirmwareTable(read)"));
        }
        buf.truncate(written as usize);
        Ok(buf)
    }
}

fn enum_firmware_tables(provider: FIRMWARE_TABLE_PROVIDER) -> WinResult<Vec<[u8; 4]>> {
    use windows::Win32::System::SystemInformation::EnumSystemFirmwareTables;

    unsafe {
        let needed = EnumSystemFirmwareTables(provider, None);
        if needed == 0 {
            return Err(WinError::last("EnumSystemFirmwareTables(size)"));
        }
        let mut buf = vec![0u8; needed as usize];
        let written = EnumSystemFirmwareTables(provider, Some(&mut buf));
        if written == 0 {
            return Err(WinError::last("EnumSystemFirmwareTables(read)"));
        }
        buf.truncate(written as usize);

        Ok(buf
            .chunks_exact(4)
            .map(|c| [c[0], c[1], c[2], c[3]])
            .collect())
    }
}

// ---------------------------------------------------------------------------
// SMBIOS parsing
// ---------------------------------------------------------------------------

/// One decoded SMBIOS structure: its type, its formatted bytes, and its string table.
#[derive(Debug, Clone)]
pub struct SmbiosStructure {
    pub struct_type: u8,
    pub formatted: Vec<u8>,
    pub strings: Vec<String>,
}

impl SmbiosStructure {
    /// SMBIOS string references are 1-based; 0 means "not set".
    pub fn string_at(&self, offset: usize) -> Option<String> {
        let idx = *self.formatted.get(offset)? as usize;
        if idx == 0 {
            return None;
        }
        let s = self.strings.get(idx - 1)?.trim();
        if s.is_empty() || is_placeholder(s) {
            None
        } else {
            Some(s.to_string())
        }
    }

    pub fn byte_at(&self, offset: usize) -> Option<u8> {
        self.formatted.get(offset).copied()
    }

    pub fn word_at(&self, offset: usize) -> Option<u16> {
        let lo = *self.formatted.get(offset)? as u16;
        let hi = *self.formatted.get(offset + 1)? as u16;
        Some(lo | (hi << 8))
    }

    pub fn dword_at(&self, offset: usize) -> Option<u32> {
        let b = self.formatted.get(offset..offset + 4)?;
        Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
}

/// OEMs ship placeholder strings constantly. Treating "To Be Filled By O.E.M." as a
/// serial number would put meaningless text in a report a buyer relies on.
fn is_placeholder(s: &str) -> bool {
    let l = s.trim().to_ascii_lowercase();
    l.is_empty()
        || l == "none"
        || l == "n/a"
        || l == "default string"
        || l == "not specified"
        || l == "not applicable"
        || l == "unknown"
        || l == "system serial number"
        || l == "system manufacturer"
        || l == "system product name"
        || l == "0"
        || l.starts_with("to be filled")
        || l.starts_with("oem_")
        || l.chars().all(|c| c == '0' || c == '.' || c == ' ')
}

/// Parse the raw SMBIOS blob returned by the `RSMB` provider.
pub fn parse_smbios(raw: &[u8]) -> Vec<SmbiosStructure> {
    // RawSMBIOSData header: Used20CallingMethod, Major, Minor, DmiRevision, Length(u32)
    if raw.len() < 8 {
        return Vec::new();
    }
    let length = u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]) as usize;
    let data = match raw.get(8..8 + length.min(raw.len() - 8)) {
        Some(d) => d,
        None => return Vec::new(),
    };

    let mut out = Vec::new();
    let mut pos = 0usize;

    while pos + 4 <= data.len() {
        let struct_type = data[pos];
        let formatted_len = data[pos + 1] as usize;

        // A header shorter than 4 bytes means the table is corrupt; stop rather than
        // walk off into unrelated memory.
        if formatted_len < 4 || pos + formatted_len > data.len() {
            break;
        }

        let formatted = data[pos..pos + formatted_len].to_vec();

        // The string set follows the formatted area, NUL-separated, double-NUL ended.
        let mut sp = pos + formatted_len;
        let mut strings = Vec::new();

        if sp + 1 < data.len() && data[sp] == 0 && data[sp + 1] == 0 {
            // A structure with no strings is terminated by *two* NULs. Consuming only
            // one here misaligns every subsequent structure — which silently truncated
            // the walk before Type 17 and made the machine look like it had no RAM.
            sp += 2;
        } else {
            let mut current = Vec::new();
            while sp < data.len() {
                let b = data[sp];
                sp += 1;
                if b == 0 {
                    if current.is_empty() {
                        // Terminating NUL of the set; the previous string's own NUL
                        // was already consumed.
                        break;
                    }
                    strings.push(String::from_utf8_lossy(&current).to_string());
                    current.clear();
                } else {
                    current.push(b);
                }
            }
        }

        out.push(SmbiosStructure {
            struct_type,
            formatted,
            strings,
        });

        // Type 127 is the end-of-table marker.
        if struct_type == 127 {
            break;
        }

        pos = sp;
    }

    out
}

// ---------------------------------------------------------------------------
// Public shape
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FormFactor {
    Laptop,
    Desktop,
    AllInOne,
    Tablet,
    Server,
    Unknown,
}

/// SMBIOS Type 3 chassis type codes.
fn chassis_form_factor(code: u8) -> FormFactor {
    // Bit 7 is a "chassis lock present" flag, not part of the type.
    match code & 0x7F {
        8 | 9 | 10 | 14 | 31 | 32 => FormFactor::Laptop, // portable/laptop/notebook/sub-notebook/convertible
        11 | 30 => FormFactor::Tablet,
        13 => FormFactor::AllInOne,
        3 | 4 | 5 | 6 | 7 | 15 | 16 => FormFactor::Desktop,
        17 | 23 | 28 => FormFactor::Server,
        _ => FormFactor::Unknown,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemIdentity {
    pub manufacturer: Reading<String>,
    pub product_name: Reading<String>,
    pub version: Reading<String>,
    pub serial_number: Reading<String>,
    pub sku: Reading<String>,
    pub family: Reading<String>,
    pub uuid: Reading<String>,

    pub baseboard_manufacturer: Reading<String>,
    pub baseboard_product: Reading<String>,
    pub baseboard_serial: Reading<String>,

    pub bios_vendor: Reading<String>,
    pub bios_version: Reading<String>,
    pub bios_release_date: Reading<String>,

    pub form_factor: FormFactor,
    pub chassis_type_code: Reading<u8>,
    pub smbios_version: Reading<String>,
}

/// A firmware-resident executable injection point.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FirmwarePersistence {
    /// WPBT is in the live ACPI table list: firmware is injecting an executable that
    /// Windows runs at every boot, surviving a disk wipe or drive swap.
    pub wpbt_present: bool,
    /// `wpbbin.exe` exists on disk. Windows writes this when it executes a WPBT
    /// payload, so it is evidence the mechanism has been used — possibly historically,
    /// or by a benign OEM driver installer. It is **not** vendor-specific and on its
    /// own says nothing about Absolute/Computrace.
    pub wpbt_launcher_present: bool,
    /// Every ACPI table signature we saw, for the detailed view.
    pub acpi_tables: Vec<String>,
    /// Files specific to the Absolute/Computrace agent. Only these justify naming it.
    pub absolute_agent_artifacts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FirmwareReport {
    pub identity: SystemIdentity,
    pub persistence: FirmwarePersistence,
}

/// Read and parse the SMBIOS table once.
///
/// Both the identity report and the memory probe are built from these same structures,
/// so the firmware read is done once and shared rather than paid for twice.
pub fn read_smbios() -> (Vec<SmbiosStructure>, Option<String>) {
    match get_firmware_table(PROVIDER_RSMB, 0) {
        Ok(raw) => {
            let version = smbios_version(&raw);
            (parse_smbios(&raw), version)
        }
        Err(_) => (Vec::new(), None),
    }
}

/// Build the firmware report from already-parsed SMBIOS structures.
pub fn probe_from(structs: &[SmbiosStructure], version: Option<String>) -> FirmwareReport {
    FirmwareReport {
        identity: build_identity(structs, version),
        persistence: probe_persistence(),
    }
}

pub fn probe() -> FirmwareReport {
    let (structs, version) = read_smbios();
    probe_from(&structs, version)
}

fn smbios_version(raw: &[u8]) -> Option<String> {
    if raw.len() < 3 {
        return None;
    }
    Some(format!("{}.{}", raw[1], raw[2]))
}

fn build_identity(structs: &[SmbiosStructure], version: Option<String>) -> SystemIdentity {
    let find = |t: u8| structs.iter().find(|s| s.struct_type == t);

    let sys = find(1);
    let board = find(2);
    let bios = find(0);
    let chassis = find(3);

    // SMBIOS Type 1 field offsets (SMBIOS spec 7.2).
    let str_or_missing = |s: Option<&SmbiosStructure>, off: usize| -> Reading<String> {
        match s.and_then(|s| s.string_at(off)) {
            Some(v) => Reading::value(v),
            None => Reading::missing(Unavailable::ImplausibleValue(
                "absent or an OEM placeholder".into(),
            )),
        }
    };

    let chassis_code = chassis.and_then(|c| c.byte_at(0x05));

    SystemIdentity {
        manufacturer: str_or_missing(sys, 0x04),
        product_name: str_or_missing(sys, 0x05),
        version: str_or_missing(sys, 0x06),
        serial_number: str_or_missing(sys, 0x07),
        sku: str_or_missing(sys, 0x19),
        family: str_or_missing(sys, 0x1A),
        uuid: sys
            .and_then(|s| s.formatted.get(0x08..0x18).map(format_uuid))
            .map(Reading::value)
            .unwrap_or_else(Reading::unsupported),

        baseboard_manufacturer: str_or_missing(board, 0x04),
        baseboard_product: str_or_missing(board, 0x05),
        baseboard_serial: str_or_missing(board, 0x07),

        bios_vendor: str_or_missing(bios, 0x04),
        bios_version: str_or_missing(bios, 0x05),
        bios_release_date: str_or_missing(bios, 0x08),

        form_factor: chassis_code.map(chassis_form_factor).unwrap_or(FormFactor::Unknown),
        chassis_type_code: chassis_code.into(),
        smbios_version: version.into(),
    }
}

/// SMBIOS stores the first three UUID groups little-endian (spec 7.2.1).
fn format_uuid(b: &[u8]) -> String {
    if b.len() < 16 {
        return String::new();
    }
    format!(
        "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        b[3], b[2], b[1], b[0], b[5], b[4], b[7], b[6], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

fn probe_persistence() -> FirmwarePersistence {
    let tables = enum_firmware_tables(PROVIDER_ACPI).unwrap_or_default();

    let signatures: Vec<String> = tables
        .iter()
        .map(|t| String::from_utf8_lossy(t).trim().to_string())
        .collect();

    let wpbt_present = tables.iter().any(|t| t == b"WPBT");

    FirmwarePersistence {
        wpbt_present,
        wpbt_launcher_present: std::path::Path::new(WPBT_LAUNCHER).exists(),
        acpi_tables: signatures,
        absolute_agent_artifacts: find_absolute_artifacts(),
    }
}

/// The generic payload Windows extracts from a WPBT table. Any vendor can use it.
const WPBT_LAUNCHER: &str = r"C:\Windows\System32\wpbbin.exe";

/// Look for the user-mode half of Absolute/Computrace specifically.
///
/// The firmware half drops these two; their presence alongside WPBT upgrades the
/// finding from "this machine has a firmware injection point" to "anti-theft tracking
/// is actively installed". Deliberately narrow — a report that accuses a seller of
/// shipping corporate tracking software had better be right.
fn find_absolute_artifacts() -> Vec<String> {
    const CANDIDATES: [&str; 2] = [
        r"C:\Windows\System32\rpcnet.exe",
        r"C:\Windows\System32\rpcnetp.exe",
    ];

    CANDIDATES
        .iter()
        .filter(|p| std::path::Path::new(p).exists())
        .map(|p| p.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal but valid SMBIOS blob: header + one Type 1 structure.
    fn synthetic_smbios() -> Vec<u8> {
        let mut table = Vec::new();
        // Type 1, length 0x1B, handle 0x0001
        table.extend_from_slice(&[0x01, 0x1B, 0x01, 0x00]);
        table.push(0x01); // 0x04 Manufacturer -> string 1
        table.push(0x02); // 0x05 Product Name -> string 2
        table.push(0x00); // 0x06 Version -> unset
        table.push(0x03); // 0x07 Serial -> string 3
        table.extend_from_slice(&[0xAA; 16]); // 0x08..0x18 UUID
        table.push(0x00); // 0x18 Wake-up type
        table.push(0x04); // 0x19 SKU -> string 4
        table.push(0x00); // 0x1A Family -> unset
        // String set
        for s in ["MSI Corp.", "MS-N014", "To Be Filled By O.E.M.", "SKU-7"] {
            table.extend_from_slice(s.as_bytes());
            table.push(0);
        }
        table.push(0); // terminating NUL

        let mut raw = vec![0x00, 0x03, 0x04, 0x00];
        raw.extend_from_slice(&(table.len() as u32).to_le_bytes());
        raw.extend_from_slice(&table);
        raw
    }

    #[test]
    fn parses_structure_and_string_table() {
        let structs = parse_smbios(&synthetic_smbios());
        assert_eq!(structs.len(), 1);
        let s = &structs[0];
        assert_eq!(s.struct_type, 1);
        assert_eq!(s.string_at(0x04).as_deref(), Some("MSI Corp."));
        assert_eq!(s.string_at(0x05).as_deref(), Some("MS-N014"));
        assert_eq!(s.string_at(0x19).as_deref(), Some("SKU-7"));
    }

    /// Regression: a structure with no strings is terminated by *two* NUL bytes.
    /// Consuming only one desynchronises the walk, silently truncating the table.
    /// That bug made a desktop with three DIMMs report zero memory devices, because
    /// Type 17 sits after the first string-less structure.
    #[test]
    fn structure_without_strings_does_not_desync_the_walk() {
        let mut table = Vec::new();
        // Type 126 (inactive), length 4, no strings.
        table.extend_from_slice(&[126, 0x04, 0x10, 0x00]);
        table.extend_from_slice(&[0x00, 0x00]);
        // Type 17, length 8, one string referenced at offset 7.
        table.extend_from_slice(&[17, 0x08, 0x11, 0x00, 0x00, 0x00, 0x00, 0x01]);
        table.extend_from_slice(b"DIMM 0\0\0");

        let mut raw = vec![0x00, 0x03, 0x07, 0x00];
        raw.extend_from_slice(&(table.len() as u32).to_le_bytes());
        raw.extend_from_slice(&table);

        let structs = parse_smbios(&raw);
        assert_eq!(structs.len(), 2, "string-less structure desynced the walk");
        assert_eq!(structs[0].struct_type, 126);
        assert_eq!(structs[1].struct_type, 17, "Type 17 became unreachable");
        assert_eq!(structs[1].strings, vec!["DIMM 0".to_string()]);
    }

    /// Guards the same bug against real firmware: every machine with RAM has at least
    /// one Type 17 structure, so an empty result means the walk is truncating.
    #[test]
    fn memory_devices_are_reachable_on_this_machine() {
        let (structs, _) = read_smbios();
        assert!(
            structs.iter().any(|s| s.struct_type == 17),
            "no Type 17 memory devices found; parsed {} structures: {:?}",
            structs.len(),
            structs.iter().map(|s| s.struct_type).collect::<Vec<_>>()
        );
    }

    #[test]
    fn unset_string_index_yields_none() {
        let structs = parse_smbios(&synthetic_smbios());
        assert_eq!(structs[0].string_at(0x06), None, "index 0 means 'not set'");
    }

    /// The important one: an OEM placeholder must never reach the report as a serial.
    #[test]
    fn oem_placeholders_are_rejected() {
        let structs = parse_smbios(&synthetic_smbios());
        assert_eq!(structs[0].string_at(0x07), None, "placeholder leaked through");

        assert!(is_placeholder("To Be Filled By O.E.M."));
        assert!(is_placeholder("Default string"));
        assert!(is_placeholder("System Serial Number"));
        assert!(is_placeholder("  none "));
        assert!(is_placeholder("0000000"));
        assert!(!is_placeholder("5CD9174TXK"));
    }

    #[test]
    fn malformed_table_does_not_panic_or_overrun() {
        assert!(parse_smbios(&[]).is_empty());
        assert!(parse_smbios(&[0, 1, 2]).is_empty());
        // Claims a huge length with no data behind it.
        let raw = vec![0x00, 0x03, 0x04, 0x00, 0xFF, 0xFF, 0xFF, 0x7F, 0x01, 0x1B];
        let _ = parse_smbios(&raw); // must simply return, not panic
    }

    #[test]
    fn uuid_first_three_groups_are_little_endian() {
        let b: Vec<u8> = (0u8..16).collect();
        // First group reverses 00 01 02 03 -> 03020100
        assert!(format_uuid(&b).starts_with("03020100-0504-0706-0809-"));
    }

    #[test]
    fn chassis_codes_classify_portables_as_laptops() {
        for code in [8u8, 9, 10, 14, 31] {
            assert_eq!(chassis_form_factor(code), FormFactor::Laptop, "code {code}");
        }
        assert_eq!(chassis_form_factor(3), FormFactor::Desktop);
        assert_eq!(chassis_form_factor(13), FormFactor::AllInOne);
        // High bit is a lock flag and must be masked off before matching.
        assert_eq!(chassis_form_factor(10 | 0x80), FormFactor::Laptop);
    }

    #[test]
    fn acpi_enumeration_works_on_this_machine() {
        let tables = enum_firmware_tables(PROVIDER_ACPI);
        assert!(tables.is_ok(), "ACPI enumeration failed: {:?}", tables.err());
        // Every x86 Windows machine has at least FACP and DSDT.
        let sigs: Vec<String> = tables
            .unwrap()
            .iter()
            .map(|t| String::from_utf8_lossy(t).to_string())
            .collect();
        assert!(!sigs.is_empty(), "no ACPI tables reported");
    }

    #[test]
    fn smbios_is_readable_on_this_machine() {
        let raw = get_firmware_table(PROVIDER_RSMB, 0);
        assert!(raw.is_ok(), "RSMB read failed: {:?}", raw.err());
        let structs = parse_smbios(&raw.unwrap());
        assert!(!structs.is_empty(), "parsed no SMBIOS structures");
        // Type 0 (BIOS) and Type 1 (System) are mandatory in the spec.
        assert!(structs.iter().any(|s| s.struct_type == 0), "no BIOS structure");
        assert!(structs.iter().any(|s| s.struct_type == 1), "no System structure");
    }
}
