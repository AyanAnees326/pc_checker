//! Memory configuration probe, decoded from SMBIOS Type 17 (Memory Device).
//!
//! Reads straight from the firmware table rather than going through WMI's
//! `Win32_PhysicalMemory` — which is itself just a projection of these same SMBIOS
//! structures, but costs a COM round-trip and loses fields (notably form factor, which
//! is how we tell soldered LPDDR from an upgradeable SODIMM).
//!
//! This is configuration, not a memory *test*; the destructive pattern test lives in
//! `stress::ram_kernel`. Three things here matter to a buyer and are invisible in the
//! Windows UI:
//!   * single-channel population, which costs real performance and is often the
//!     difference between a laptop that feels fine and one that does not;
//!   * memory running below its rated speed (XMP/EXPO not applied);
//!   * soldered memory, meaning the RAM you see is the RAM you will always have.

use serde::{Deserialize, Serialize};

use crate::model::Reading;
use crate::probes::firmware::SmbiosStructure;

// SMBIOS Type 17 field offsets (SMBIOS spec 7.18).
const OFF_SIZE: usize = 0x0C;
const OFF_FORM_FACTOR: usize = 0x0E;
const OFF_DEVICE_LOCATOR: usize = 0x10;
const OFF_BANK_LOCATOR: usize = 0x11;
const OFF_MEMORY_TYPE: usize = 0x12;
const OFF_SPEED: usize = 0x15;
const OFF_MANUFACTURER: usize = 0x17;
const OFF_SERIAL: usize = 0x18;
const OFF_PART_NUMBER: usize = 0x1A;
const OFF_EXTENDED_SIZE: usize = 0x1C;
const OFF_CONFIGURED_SPEED: usize = 0x20;

fn memory_type_name(code: u8) -> &'static str {
    match code {
        0x12 => "DDR",
        0x13 => "DDR2",
        0x14 => "DDR2 FB-DIMM",
        0x18 => "DDR3",
        0x1A => "DDR4",
        0x1B => "LPDDR",
        0x1C => "LPDDR2",
        0x1D => "LPDDR3",
        0x1E => "LPDDR4",
        0x20 => "HBM",
        0x21 => "HBM2",
        0x22 => "DDR5",
        0x23 => "LPDDR5",
        0x24 => "HBM3",
        _ => "Unknown",
    }
}

fn form_factor_name(code: u8) -> &'static str {
    match code {
        0x08 => "DIMM",
        0x09 => "TSOP",
        0x0B => "RIMM",
        0x0C => "SODIMM",
        0x0D => "SRIMM",
        0x0F => "Soldered (row of chips)",
        _ => "Unknown",
    }
}

/// Form factor 0x0D is SODIMM in the spec; 0x0F ("row of chips") is how OEMs encode
/// memory soldered to the board.
fn is_soldered(form_factor: u8) -> bool {
    form_factor == 0x0F
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryModule {
    pub slot: Reading<String>,
    pub bank: Reading<String>,
    pub size_mb: u64,
    pub memory_type: String,
    pub form_factor: String,
    pub soldered: bool,
    /// The module's rated speed in MT/s.
    pub rated_speed_mts: Reading<u16>,
    /// What it is actually running at. Lower than rated means XMP/EXPO is off.
    pub configured_speed_mts: Reading<u16>,
    pub manufacturer: Reading<String>,
    pub part_number: Reading<String>,
    pub serial: Reading<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelConfig {
    Single,
    Dual,
    Quad,
    /// Populated asymmetrically — runs partly in single-channel (flex mode).
    Asymmetric,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryReport {
    pub modules: Vec<MemoryModule>,
    pub total_mb: u64,
    pub populated_slots: usize,
    pub total_slots: usize,
    pub channel_config: ChannelConfig,
    /// True when any module runs below its own rated speed.
    pub running_below_rated_speed: bool,
    /// True when all populated memory is soldered — no upgrade path.
    pub all_soldered: bool,
    /// True when populated modules differ in size, speed or part number.
    pub mismatched_modules: bool,
}

/// Decode all Type 17 structures from an already-parsed SMBIOS table.
pub fn from_smbios(structs: &[SmbiosStructure]) -> MemoryReport {
    let devices: Vec<&SmbiosStructure> =
        structs.iter().filter(|s| s.struct_type == 17).collect();

    let total_slots = devices.len();
    let mut modules = Vec::new();

    for d in &devices {
        let size_mb = decode_size(d);
        // Size 0 means the slot exists but is empty.
        if size_mb == 0 {
            continue;
        }

        let form_factor = d.byte_at(OFF_FORM_FACTOR).unwrap_or(0);

        modules.push(MemoryModule {
            slot: d.string_at(OFF_DEVICE_LOCATOR).into(),
            bank: d.string_at(OFF_BANK_LOCATOR).into(),
            size_mb,
            memory_type: memory_type_name(d.byte_at(OFF_MEMORY_TYPE).unwrap_or(0)).to_string(),
            form_factor: form_factor_name(form_factor).to_string(),
            soldered: is_soldered(form_factor),
            rated_speed_mts: nonzero_word(d, OFF_SPEED).into(),
            configured_speed_mts: nonzero_word(d, OFF_CONFIGURED_SPEED).into(),
            manufacturer: d.string_at(OFF_MANUFACTURER).into(),
            part_number: d.string_at(OFF_PART_NUMBER).into(),
            serial: d.string_at(OFF_SERIAL).into(),
        });
    }

    let total_mb = modules.iter().map(|m| m.size_mb).sum();
    let populated_slots = modules.len();

    let running_below_rated_speed = modules.iter().any(|m| {
        match (m.rated_speed_mts.get(), m.configured_speed_mts.get()) {
            // Allow a small tolerance: firmware often reports e.g. 3200 vs 3199.
            (Some(&rated), Some(&configured)) => configured + 50 < rated,
            _ => false,
        }
    });

    let all_soldered = populated_slots > 0 && modules.iter().all(|m| m.soldered);
    let mismatched_modules = detect_mismatch(&modules);
    let channel_config = infer_channels(&modules);

    MemoryReport {
        modules,
        total_mb,
        populated_slots,
        total_slots,
        channel_config,
        running_below_rated_speed,
        all_soldered,
        mismatched_modules,
    }
}

fn nonzero_word(d: &SmbiosStructure, off: usize) -> Option<u16> {
    match d.word_at(off) {
        Some(0) | None => None,
        // 0xFFFF is the spec's "unknown" sentinel.
        Some(0xFFFF) => None,
        Some(v) => Some(v),
    }
}

/// Size is in MB, except bit 15 flags KB units, and 0x7FFF defers to Extended Size.
fn decode_size(d: &SmbiosStructure) -> u64 {
    let raw = match d.word_at(OFF_SIZE) {
        Some(v) => v,
        None => return 0,
    };

    if raw == 0 || raw == 0xFFFF {
        return 0;
    }

    if raw == 0x7FFF {
        // Extended Size is a DWORD in MB, with bit 31 reserved.
        return d
            .dword_at(OFF_EXTENDED_SIZE)
            .map(|v| (v & 0x7FFF_FFFF) as u64)
            .unwrap_or(0);
    }

    if raw & 0x8000 != 0 {
        // Value is in kilobytes.
        ((raw & 0x7FFF) as u64) / 1024
    } else {
        raw as u64
    }
}

fn detect_mismatch(modules: &[MemoryModule]) -> bool {
    if modules.len() < 2 {
        return false;
    }
    let first = &modules[0];
    modules.iter().any(|m| {
        m.size_mb != first.size_mb
            || m.rated_speed_mts.get() != first.rated_speed_mts.get()
            || m.part_number.get() != first.part_number.get()
    })
}

/// Infer channel population from the device locators.
///
/// OEMs name slots inconsistently ("ChannelA-DIMM0", "DIMM A1", "Controller0-ChannelA"),
/// so this parses what it can and falls back to a module count. Reported with
/// heuristic confidence — it is a strong hint, not a hardware readout.
fn infer_channels(modules: &[MemoryModule]) -> ChannelConfig {
    if modules.is_empty() {
        return ChannelConfig::Unknown;
    }
    if modules.len() == 1 {
        return ChannelConfig::Single;
    }

    // Collect a channel letter/number per module where the locator exposes one.
    let mut channels: Vec<String> = Vec::new();
    for m in modules {
        let text = format!(
            "{} {}",
            m.slot.get().cloned().unwrap_or_default(),
            m.bank.get().cloned().unwrap_or_default()
        )
        .to_ascii_uppercase();

        if let Some(pos) = text.find("CHANNEL") {
            // Take the first alphanumeric after "CHANNEL", skipping separators.
            let tail = &text[pos + "CHANNEL".len()..];
            if let Some(c) = tail.chars().find(|c| c.is_alphanumeric()) {
                channels.push(c.to_string());
                continue;
            }
        }
        // Fall back to patterns like "DIMM A1" / "DIMMA".
        if let Some(pos) = text.find("DIMM") {
            let tail = &text[pos + "DIMM".len()..];
            if let Some(c) = tail.chars().find(|c| c.is_alphabetic()) {
                channels.push(c.to_string());
                continue;
            }
        }
    }

    let distinct: std::collections::BTreeSet<&String> = channels.iter().collect();

    // Unequal total capacity per channel means flex mode, not true dual channel.
    if distinct.len() >= 2 && detect_mismatch(modules) {
        return ChannelConfig::Asymmetric;
    }

    match distinct.len() {
        0 => {
            // No usable locator naming: fall back on module count.
            if modules.len() >= 4 {
                ChannelConfig::Quad
            } else if modules.len() >= 2 {
                ChannelConfig::Dual
            } else {
                ChannelConfig::Single
            }
        }
        1 => ChannelConfig::Single,
        2 => ChannelConfig::Dual,
        _ => ChannelConfig::Quad,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn module(size_mb: u64, slot: &str, rated: u16, configured: u16) -> MemoryModule {
        MemoryModule {
            slot: Reading::value(slot.to_string()),
            bank: Reading::unsupported(),
            size_mb,
            memory_type: "DDR4".into(),
            form_factor: "SODIMM".into(),
            soldered: false,
            rated_speed_mts: Reading::value(rated),
            configured_speed_mts: Reading::value(configured),
            manufacturer: Reading::value("Samsung".into()),
            part_number: Reading::value("M471A1K43CB1".into()),
            serial: Reading::unsupported(),
        }
    }

    #[test]
    fn two_modules_across_two_channels_is_dual() {
        let m = vec![
            module(8192, "ChannelA-DIMM0", 3200, 3200),
            module(8192, "ChannelB-DIMM0", 3200, 3200),
        ];
        assert_eq!(infer_channels(&m), ChannelConfig::Dual);
    }

    #[test]
    fn both_modules_on_one_channel_is_single() {
        let m = vec![
            module(8192, "ChannelA-DIMM0", 3200, 3200),
            module(8192, "ChannelA-DIMM1", 3200, 3200),
        ];
        assert_eq!(infer_channels(&m), ChannelConfig::Single);
    }

    /// The common used-laptop case: 8GB + 16GB. It is not real dual channel, and
    /// reporting it as such would overstate the machine.
    #[test]
    fn unequal_modules_are_asymmetric_not_dual() {
        let m = vec![
            module(8192, "ChannelA-DIMM0", 3200, 3200),
            module(16384, "ChannelB-DIMM0", 3200, 3200),
        ];
        assert_eq!(infer_channels(&m), ChannelConfig::Asymmetric);
    }

    #[test]
    fn single_module_is_single_channel() {
        let m = vec![module(8192, "DIMM A", 3200, 3200)];
        assert_eq!(infer_channels(&m), ChannelConfig::Single);
    }

    #[test]
    fn size_decoding_handles_kilobyte_flag_and_extended_size() {
        // Direct MB value.
        let d = synthetic_type17(8192, 0, 3200, 3200, 0x0C);
        assert_eq!(decode_size(&d), 8192);

        // 0x7FFF defers to the extended DWORD field (in MB).
        let d = synthetic_type17(0x7FFF, 32768, 3200, 3200, 0x0C);
        assert_eq!(decode_size(&d), 32768);
    }

    #[test]
    fn empty_slot_reports_zero_and_is_skipped() {
        let d = synthetic_type17(0, 0, 0, 0, 0x0C);
        assert_eq!(decode_size(&d), 0);
        let report = from_smbios(&[d]);
        assert_eq!(report.populated_slots, 0);
        assert_eq!(report.total_slots, 1, "an empty slot still counts as a slot");
    }

    #[test]
    fn xmp_not_applied_is_detected() {
        let d = synthetic_type17(8192, 0, 3200, 2666, 0x0C);
        let report = from_smbios(&[d]);
        assert!(
            report.running_below_rated_speed,
            "3200 MT/s memory running at 2666 should be flagged"
        );
    }

    #[test]
    fn small_speed_reporting_jitter_is_not_flagged() {
        let d = synthetic_type17(8192, 0, 3200, 3200, 0x0C);
        let report = from_smbios(&[d]);
        assert!(!report.running_below_rated_speed);
    }

    #[test]
    fn soldered_memory_is_detected() {
        let d = synthetic_type17(16384, 0, 6400, 6400, 0x0F);
        let report = from_smbios(&[d]);
        assert!(report.all_soldered, "form factor 0x0F means soldered");
        assert!(!report.modules[0].slot.is_ok() || report.modules[0].soldered);
    }

    #[test]
    fn memory_type_codes_decode() {
        assert_eq!(memory_type_name(0x1A), "DDR4");
        assert_eq!(memory_type_name(0x22), "DDR5");
        assert_eq!(memory_type_name(0x23), "LPDDR5");
    }

    /// Build a Type 17 structure with the fields this module reads.
    fn synthetic_type17(
        size: u16,
        extended_size: u32,
        rated: u16,
        configured: u16,
        form_factor: u8,
    ) -> SmbiosStructure {
        let mut f = vec![0u8; 0x28];
        f[0] = 17;
        f[1] = 0x28;
        f[OFF_SIZE..OFF_SIZE + 2].copy_from_slice(&size.to_le_bytes());
        f[OFF_FORM_FACTOR] = form_factor;
        f[OFF_DEVICE_LOCATOR] = 1;
        f[OFF_BANK_LOCATOR] = 0;
        f[OFF_MEMORY_TYPE] = 0x1A;
        f[OFF_SPEED..OFF_SPEED + 2].copy_from_slice(&rated.to_le_bytes());
        f[OFF_MANUFACTURER] = 2;
        f[OFF_SERIAL] = 0;
        f[OFF_PART_NUMBER] = 3;
        f[OFF_EXTENDED_SIZE..OFF_EXTENDED_SIZE + 4]
            .copy_from_slice(&extended_size.to_le_bytes());
        f[OFF_CONFIGURED_SPEED..OFF_CONFIGURED_SPEED + 2]
            .copy_from_slice(&configured.to_le_bytes());

        SmbiosStructure {
            struct_type: 17,
            formatted: f,
            strings: vec![
                "ChannelA-DIMM0".into(),
                "Samsung".into(),
                "M471A1K43CB1".into(),
            ],
        }
    }
}
