//! CPU topology and identity probe.
//!
//! Vendor and brand string come straight from the `CPUID` instruction — no Win32 call
//! needed, and it works identically whether or not PawnIO is installed. Physical core
//! count comes from `GetLogicalProcessorInformationEx`; logical processor count from
//! `std::thread::available_parallelism`. This is what tells the stress orchestrator
//! how many threads to pin, and what tells `pawnio::` which vendor module to load.

use std::arch::x86_64::{__cpuid, __cpuid_count};

use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, GetLastError};
use windows::Win32::System::SystemInformation::{
    GetLogicalProcessorInformationEx, RelationProcessorCore, SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
};

use crate::model::Reading;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CpuVendor {
    Intel,
    Amd,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuTopology {
    pub vendor: CpuVendor,
    pub brand_string: Reading<String>,
    pub physical_cores: Reading<u32>,
    pub logical_processors: u32,
    /// From CPUID leaf 0x16 on Intel (authoritative), or a registry heuristic
    /// otherwise. The `cpu_specs.json` lookup in the analysis layer is the source of
    /// truth for "what should this clock be" — this field is only a display fallback
    /// for when a part isn't in that dataset yet.
    pub base_clock_mhz: Reading<u32>,
}

pub fn probe() -> CpuTopology {
    let vendor = detect_vendor();
    CpuTopology {
        vendor,
        brand_string: brand_string().into(),
        physical_cores: physical_core_count(),
        logical_processors: logical_processor_count(),
        base_clock_mhz: base_clock_mhz(vendor),
    }
}

/// CPUID leaf 0: EBX:EDX:ECX (in that order) spell the 12-character vendor string.
fn detect_vendor() -> CpuVendor {
    let regs = __cpuid(0);
    let mut bytes = [0u8; 12];
    bytes[0..4].copy_from_slice(&regs.ebx.to_le_bytes());
    bytes[4..8].copy_from_slice(&regs.edx.to_le_bytes());
    bytes[8..12].copy_from_slice(&regs.ecx.to_le_bytes());
    match &bytes {
        b"GenuineIntel" => CpuVendor::Intel,
        b"AuthenticAMD" => CpuVendor::Amd,
        _ => CpuVendor::Other,
    }
}

/// CPUID leaves 0x80000002-0x80000004: 48-byte NUL-padded ASCII brand string, four
/// registers per leaf, EAX:EBX:ECX:EDX order.
fn brand_string() -> Option<String> {
    let max_ext = __cpuid(0x8000_0000).eax;
    if max_ext < 0x8000_0004 {
        return None;
    }

    let mut bytes = [0u8; 48];
    for (i, leaf) in (0x8000_0002u32..=0x8000_0004).enumerate() {
        let regs = __cpuid(leaf);
        let off = i * 16;
        bytes[off..off + 4].copy_from_slice(&regs.eax.to_le_bytes());
        bytes[off + 4..off + 8].copy_from_slice(&regs.ebx.to_le_bytes());
        bytes[off + 8..off + 12].copy_from_slice(&regs.ecx.to_le_bytes());
        bytes[off + 12..off + 16].copy_from_slice(&regs.edx.to_le_bytes());
    }

    let s = String::from_utf8_lossy(&bytes);
    let trimmed = s.trim_matches(|c: char| c == '\0' || c.is_whitespace());
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// CPUID leaf 0x16 ("Processor Frequency Information", Intel-only): EAX = base
/// frequency in MHz directly, no bit-shifting or unit conversion needed.
fn intel_base_clock_mhz() -> Option<u32> {
    let max_leaf = __cpuid(0).eax;
    if max_leaf < 0x16 {
        return None;
    }
    let regs = __cpuid_count(0x16, 0);
    let mhz = regs.eax & 0xFFFF;
    if mhz == 0 {
        None
    } else {
        Some(mhz)
    }
}

fn base_clock_mhz(vendor: CpuVendor) -> Reading<u32> {
    match vendor {
        CpuVendor::Intel => intel_base_clock_mhz().into(),
        // AMD does not implement leaf 0x16. The registry's ~MHz value is a boot-time
        // snapshot, not a guaranteed-accurate rated clock, so it is intentionally not
        // wired up here — the spec dataset lookup is the real source for AMD parts,
        // and reporting a shaky number would just be a second, disagreeing "truth".
        _ => Reading::unsupported(),
    }
}

fn logical_processor_count() -> u32 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1)
}

/// Count physical cores by walking `GetLogicalProcessorInformationEx`'s
/// variable-length record stream, filtered to `RelationProcessorCore` — one record
/// per physical core regardless of SMT/hyperthreading width.
fn physical_core_count() -> Reading<u32> {
    unsafe {
        let mut len: u32 = 0;
        // First call is expected to fail with ERROR_INSUFFICIENT_BUFFER to report
        // the required size; any other outcome here means the buffer is real, which
        // one instance of this codebase has never actually observed.
        let first = GetLogicalProcessorInformationEx(RelationProcessorCore, None, &mut len);
        if first.is_ok() || GetLastError() != ERROR_INSUFFICIENT_BUFFER || len == 0 {
            return Reading::unsupported();
        }

        let mut buf = vec![0u8; len as usize];
        let ptr = buf.as_mut_ptr() as *mut SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX;
        if GetLogicalProcessorInformationEx(RelationProcessorCore, Some(ptr), &mut len).is_err() {
            return Reading::unsupported();
        }

        Reading::value(count_core_records(&buf))
    }
}

/// Walk the byte buffer `GetLogicalProcessorInformationEx` fills, advancing by each
/// record's own `Size` field — the only safe way to iterate this API's output, since
/// each `SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX` is a different length depending on
/// how many processor groups its `Processor.GroupMask` array holds.
fn count_core_records(buf: &[u8]) -> u32 {
    let mut count = 0u32;
    let mut offset = 0usize;

    while offset + 8 <= buf.len() {
        // Layout: LOGICAL_PROCESSOR_RELATIONSHIP Relationship (u32), DWORD Size (u32),
        // then a relationship-specific union.
        let size = u32::from_le_bytes(buf[offset + 4..offset + 8].try_into().unwrap()) as usize;
        if size == 0 || offset + size > buf.len() {
            break;
        }
        count += 1;
        offset += size;
    }

    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_a_real_x86_vendor_on_this_machine() {
        // Every x86_64 Windows machine is Intel or AMD; "Other" here would mean the
        // CPUID decode itself is broken.
        assert_ne!(detect_vendor(), CpuVendor::Other);
    }

    #[test]
    fn brand_string_is_readable_on_this_machine() {
        let s = brand_string();
        assert!(s.is_some(), "brand string should be available on any modern x86_64 CPU");
        assert!(!s.unwrap().is_empty());
    }

    #[test]
    fn logical_processor_count_is_plausible() {
        let n = logical_processor_count();
        assert!(n >= 1 && n <= 512, "got {n}");
    }

    #[test]
    fn physical_core_count_is_plausible_and_not_more_than_logical() {
        let topo = probe();
        if let Some(&phys) = topo.physical_cores.get() {
            assert!(phys >= 1);
            assert!(
                phys <= topo.logical_processors,
                "physical cores ({phys}) exceeded logical processors ({})",
                topo.logical_processors
            );
        }
    }

    #[test]
    fn count_core_records_stops_at_a_zero_size_record_rather_than_looping() {
        let buf = [0u8; 16]; // Relationship=0, Size=0 -> must not spin forever
        assert_eq!(count_core_records(&buf), 0);
    }

    #[test]
    fn count_core_records_walks_two_fixed_size_records() {
        let mut buf = vec![0u8; 32];
        buf[4..8].copy_from_slice(&16u32.to_le_bytes());
        buf[20..24].copy_from_slice(&16u32.to_le_bytes());
        assert_eq!(count_core_records(&buf), 2);
    }

    #[test]
    fn full_probe_does_not_panic_on_this_machine() {
        let topo = probe();
        assert!(topo.logical_processors >= 1);
    }
}
