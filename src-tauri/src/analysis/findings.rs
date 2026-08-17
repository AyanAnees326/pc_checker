//! Rules over the read-only probes.
//!
//! Each component evaluates independently, so a scan of one part of the machine never
//! depends on having scanned the rest. The aggregate [`evaluate`] is just a composition
//! of the per-component passes.

use crate::model::{Confidence, Finding, Reading, Severity};
use crate::probes::battery::BatteryReport;
use crate::probes::battery_history::BatteryHistory;
use crate::probes::crashes::CrashHistory;
use crate::probes::firmware::FirmwareReport;
use crate::probes::gpu::GpuReport;
use crate::probes::memory::{ChannelConfig, MemoryReport};
use crate::probes::storage::{DriveHealth, DriveReport};
use crate::probes::Inventory;

/// Hours in a year, for turning drive odometers into something a human can judge.
const HOURS_PER_YEAR: f64 = 8_766.0;

/// Worst first: the buyer should see deal-breakers without scrolling.
fn sorted(mut findings: Vec<Finding>) -> Vec<Finding> {
    findings.sort_by(|a, b| b.severity.cmp(&a.severity));
    findings
}

/// Evaluate every component at once.
pub fn evaluate(inv: &Inventory) -> Vec<Finding> {
    let mut all = Vec::new();
    all.extend(storage(&inv.drives));
    all.extend(battery(&inv.batteries, &inv.battery_history));
    all.extend(memory(&inv.memory));
    all.extend(firmware(&inv.firmware));
    all.extend(stability(&inv.crashes));
    if let Some(gpus) = inv.gpus.get() {
        all.extend(gpu(gpus));
    }
    sorted(all)
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

pub fn storage(drives: &[DriveReport]) -> Vec<Finding> {
    let mut out = Vec::new();

    for drive in drives {
        let label = drive
            .model
            .get()
            .cloned()
            .unwrap_or_else(|| format!("Drive {}", drive.index));

        let health = match drive.health.get() {
            Some(h) => h,
            None => continue,
        };

        match health {
            DriveHealth::Ata(ata) => {
                if let Some(&pending) = ata.pending_sectors.get() {
                    if pending > 0 {
                        out.push(
                            Finding::new(
                                format!("storage.pending.{}", drive.index),
                                format!("{label} has unreadable sectors"),
                                Severity::Problem,
                                Confidence::SpecGrounded,
                            )
                            .observed(format!("{pending} sector(s) pending reallocation"))
                            .expected("0")
                            .basis("SMART attribute 197; any non-zero value means data the drive cannot currently read")
                            .recommend("Treat this drive as failing. Do not store anything you care about on it, and price the machine assuming it needs replacing."),
                        );
                    }
                }

                if let Some(&uncorrectable) = ata.uncorrectable_sectors.get() {
                    if uncorrectable > 0 {
                        out.push(
                            Finding::new(
                                format!("storage.uncorrectable.{}", drive.index),
                                format!("{label} has uncorrectable sectors"),
                                Severity::Problem,
                                Confidence::SpecGrounded,
                            )
                            .observed(format!("{uncorrectable} uncorrectable sector(s)"))
                            .expected("0")
                            .basis("SMART attribute 198; these sectors have already lost data")
                            .recommend("Data loss has already occurred on this drive. Replace it."),
                        );
                    }
                }

                if let Some(&reallocated) = ata.reallocated_sectors.get() {
                    if reallocated > 50 {
                        out.push(
                            Finding::new(
                                format!("storage.realloc.{}", drive.index),
                                format!("{label} has extensive sector remapping"),
                                Severity::Problem,
                                Confidence::SpecGrounded,
                            )
                            .observed(format!("{reallocated} reallocated sectors"))
                            .expected("0")
                            .basis("SMART attribute 5; remapping at this scale usually accelerates")
                            .recommend("The drive is consuming its spare area. Budget for a replacement."),
                        );
                    } else if reallocated > 0 {
                        out.push(
                            Finding::new(
                                format!("storage.realloc.{}", drive.index),
                                format!("{label} has remapped sectors"),
                                Severity::Watch,
                                Confidence::SpecGrounded,
                            )
                            .observed(format!("{reallocated} reallocated sector(s)"))
                            .expected("0")
                            .basis("SMART attribute 5")
                            .recommend("A small count can stay stable for years, but re-check the trend if you keep the drive."),
                        );
                    }
                }

                if let Some(&life) = ata.life_remaining_percent.get() {
                    if life < 20 {
                        out.push(
                            Finding::new(
                                format!("storage.life.{}", drive.index),
                                format!("{label} is near the end of its write endurance"),
                                Severity::Problem,
                                Confidence::SpecGrounded,
                            )
                            .observed(format!("{life}% endurance remaining"))
                            .expected("above 20%")
                            .basis("SSD wear-levelling attribute, normalised by the drive's own controller")
                            .recommend("Assume this SSD needs replacing soon and negotiate accordingly."),
                        );
                    }
                }

                age_rule(&label, drive.index, ata.power_on_hours.get().copied(), &mut out);
            }

            DriveHealth::Nvme(nvme) => {
                if nvme.critical_warning != 0 {
                    out.push(
                        Finding::new(
                            format!("storage.critical.{}", drive.index),
                            format!("{label} is reporting a critical warning"),
                            Severity::Critical,
                            Confidence::SpecGrounded,
                        )
                        .observed(format!("critical warning bitmask 0x{:02X}", nvme.critical_warning))
                        .expected("0x00")
                        .basis("NVMe SMART/Health log page 02h, critical warning field")
                        .recommend("The controller is flagging an active fault. Do not buy on the assumption this drive is usable."),
                    );
                }

                if let Some(&used) = nvme.percentage_used.get() {
                    if used >= 90 {
                        out.push(
                            Finding::new(
                                format!("storage.used.{}", drive.index),
                                format!("{label} has consumed its rated endurance"),
                                Severity::Problem,
                                Confidence::SpecGrounded,
                            )
                            .observed(format!("{used}% of rated write endurance used"))
                            .expected("below 80%")
                            .basis("NVMe percentage-used field, computed by the drive's own controller")
                            .recommend("Price the machine as needing a new SSD."),
                        );
                    } else if used >= 80 {
                        out.push(
                            Finding::new(
                                format!("storage.used.{}", drive.index),
                                format!("{label} is well into its rated endurance"),
                                Severity::Watch,
                                Confidence::SpecGrounded,
                            )
                            .observed(format!("{used}% of rated write endurance used"))
                            .expected("below 80%")
                            .basis("NVMe percentage-used field")
                            .recommend("Usable, but factor a future SSD replacement into the price."),
                        );
                    }
                }

                if let (Some(&spare), Some(&threshold)) = (
                    nvme.available_spare_percent.get(),
                    nvme.available_spare_threshold_percent.get(),
                ) {
                    if threshold > 0 && spare < threshold {
                        out.push(
                            Finding::new(
                                format!("storage.spare.{}", drive.index),
                                format!("{label} has exhausted its spare blocks"),
                                Severity::Critical,
                                Confidence::SpecGrounded,
                            )
                            .observed(format!("{spare}% spare remaining"))
                            .expected(format!("at or above the drive's {threshold}% threshold"))
                            .basis("NVMe available-spare vs the controller's own threshold")
                            .recommend("This drive is at end of life by its manufacturer's definition."),
                        );
                    }
                }

                if let Some(&media_errors) = nvme.media_errors.get() {
                    if media_errors > 0 {
                        out.push(
                            Finding::new(
                                format!("storage.media.{}", drive.index),
                                format!("{label} has logged media errors"),
                                Severity::Problem,
                                Confidence::SpecGrounded,
                            )
                            .observed(format!("{media_errors} media/data-integrity error(s)"))
                            .expected("0")
                            .basis("NVMe media and data integrity errors counter")
                            .recommend("The drive has encountered data it could not return correctly. Replace it."),
                        );
                    }
                }

                age_rule(&label, drive.index, nvme.power_on_hours.get().copied(), &mut out);
            }
        }
    }

    sorted(out)
}

/// Power-on hours is the machine's odometer, and the hardest number for a seller to
/// fake. Reported as context rather than a defect: age alone is not a fault.
fn age_rule(label: &str, index: u32, hours: Option<u64>, out: &mut Vec<Finding>) {
    let hours = match hours {
        Some(h) if h > 0 => h,
        _ => return,
    };
    let years = hours as f64 / HOURS_PER_YEAR;
    let severity = if years >= 5.0 { Severity::Watch } else { Severity::Ok };

    out.push(
        Finding::new(
            format!("storage.age.{index}"),
            format!("{label} has {hours} power-on hours"),
            severity,
            Confidence::SpecGrounded,
        )
        .observed(format!("{hours} hours, about {years:.1} years of powered operation"))
        .expected("consistent with the age the seller states")
        .basis("SMART power-on hours, which a seller cannot reset by reinstalling Windows")
        .recommend(
            "Cross-check against the claimed age. A machine described as lightly used should not show years of runtime.",
        ),
    );
}

// ---------------------------------------------------------------------------
// Battery
// ---------------------------------------------------------------------------

pub fn battery(
    batteries: &Reading<Vec<BatteryReport>>,
    history: &Reading<BatteryHistory>,
) -> Vec<Finding> {
    let mut out = Vec::new();

    if let Some(packs) = batteries.get() {
        for (i, pack) in packs.iter().enumerate() {
            if let Some(&health) = pack.health_percent.get() {
                let (severity, advice) = if health < 60.0 {
                    (
                        Severity::Problem,
                        "Expect to replace the pack. Get a quote for this model and take it off the price.",
                    )
                } else if health < 80.0 {
                    (
                        Severity::Watch,
                        "Noticeably degraded. Usable, but runtime will be well below the original.",
                    )
                } else {
                    (Severity::Ok, "Healthy for a used machine.")
                };

                out.push(
                    Finding::new(
                        format!("battery.health.{i}"),
                        format!("Battery at {health:.0}% of design capacity"),
                        severity,
                        Confidence::SpecGrounded,
                    )
                    .observed(format!("{health:.1}% of design capacity"))
                    .expected("above 80% for a well-kept machine")
                    .basis("Full-charge capacity against design capacity, as reported by the pack")
                    .recommend(advice),
                );
            }
        }
    }

    if let Some(h) = history.get() {
        if h.battery_swap_detected {
            out.push(
                Finding::new(
                    "battery.swapped",
                    "The battery has been replaced at some point",
                    Severity::Watch,
                    Confidence::SpecGrounded,
                )
                .observed("Windows recorded a battery change in its power history")
                .expected("the original pack, for a machine described as original")
                .basis("BatteryChanged flag in the powercfg battery report history")
                .recommend(
                    "Ask what was replaced and whether the pack is an OEM part. Third-party packs vary a lot in quality.",
                ),
            );
        }

        if let Some(&rate) = h.degradation_percent_per_year.get() {
            if rate < -15.0 {
                out.push(
                    Finding::new(
                        "battery.degradation",
                        "Battery capacity is falling quickly",
                        Severity::Problem,
                        Confidence::SpecGrounded,
                    )
                    .observed(format!("about {rate:.0}% of design capacity lost per year"))
                    .expected("a gentler decline, typically under 10% per year")
                    .basis(format!(
                        "Least-squares fit over {} days of Windows capacity history",
                        h.observation_days
                    ))
                    .recommend(
                        "A pack degrading this fast will need replacing sooner than its current health suggests.",
                    ),
                );
            }
        }
    }

    sorted(out)
}

// ---------------------------------------------------------------------------
// Memory
// ---------------------------------------------------------------------------

pub fn memory(mem: &MemoryReport) -> Vec<Finding> {
    let mut out = Vec::new();

    if mem.populated_slots > 0 {
        match mem.channel_config {
            ChannelConfig::Single if mem.total_slots > mem.populated_slots => {
                out.push(
                    Finding::new(
                        "memory.single_channel",
                        "Memory is running in single channel",
                        Severity::Watch,
                        Confidence::Heuristic,
                    )
                    .observed(format!(
                        "{} of {} slots populated, all on one channel",
                        mem.populated_slots, mem.total_slots
                    ))
                    .expected("modules split across two channels")
                    .basis("Slot naming in the SMBIOS memory device table")
                    .recommend(
                        "Real-world performance is meaningfully below what this hardware can do. Adding a matched module is usually cheap.",
                    ),
                );
            }
            ChannelConfig::Asymmetric => {
                out.push(
                    Finding::new(
                        "memory.asymmetric",
                        "Memory modules do not match",
                        Severity::Watch,
                        Confidence::Heuristic,
                    )
                    .observed("populated modules differ in size, speed or part number")
                    .expected("identical modules")
                    .basis("SMBIOS memory device entries")
                    .recommend(
                        "Only part of the memory runs in dual channel. Worth knowing, not worth walking away over.",
                    ),
                );
            }
            _ => {}
        }
    }

    if mem.running_below_rated_speed {
        out.push(
            Finding::new(
                "memory.below_rated",
                "Memory is running below its rated speed",
                Severity::Watch,
                Confidence::SpecGrounded,
            )
            .observed("configured speed is below the modules' own rated speed")
            .expected("modules running at their rated speed")
            .basis("SMBIOS rated speed against configured speed")
            .recommend(
                "Usually just XMP/EXPO left disabled in firmware, which is a free fix rather than a fault.",
            ),
        );
    }

    if mem.all_soldered {
        out.push(
            Finding::new(
                "memory.soldered",
                "Memory is soldered and cannot be upgraded",
                Severity::Watch,
                Confidence::SpecGrounded,
            )
            .observed(format!("{} MB, all soldered to the board", mem.total_mb))
            .expected("socketed modules, if you intend to upgrade")
            .basis("SMBIOS form factor 0x0F (row of chips)")
            .recommend(
                "The memory this machine has is the memory it will always have. Judge the price on that basis.",
            ),
        );
    }

    sorted(out)
}

// ---------------------------------------------------------------------------
// Firmware
// ---------------------------------------------------------------------------

pub fn firmware(fw: &FirmwareReport) -> Vec<Finding> {
    let mut out = Vec::new();
    let p = &fw.persistence;

    if !p.absolute_agent_artifacts.is_empty() {
        out.push(
            Finding::new(
                "firmware.absolute",
                "Absolute/Computrace anti-theft tracking is installed",
                Severity::Critical,
                Confidence::SpecGrounded,
            )
            .observed(format!("found {}", p.absolute_agent_artifacts.join(", ")))
            .expected("no corporate anti-theft agent")
            .basis("Absolute agent executables present on disk")
            .recommend(
                "A previous owner's IT department may be able to lock or wipe this machine remotely, and reinstalling Windows does not remove it. Do not buy until the seller proves it is deactivated.",
            ),
        );
    } else if p.wpbt_present {
        out.push(
            Finding::new(
                "firmware.wpbt",
                "Firmware injects an executable at every boot",
                Severity::Watch,
                Confidence::SpecGrounded,
            )
            .observed("a WPBT table is present in ACPI")
            .expected("no firmware-injected executable, or a known OEM one")
            .basis("Windows Platform Binary Table in the live ACPI table list")
            .recommend(
                "Often a benign OEM driver installer, but it is also how anti-theft tracking persists across a disk wipe. Worth identifying before you buy.",
            ),
        );
    }

    sorted(out)
}

// ---------------------------------------------------------------------------
// Graphics
// ---------------------------------------------------------------------------

pub fn gpu(gpus: &[GpuReport]) -> Vec<Finding> {
    let mut out = Vec::new();

    let hardware: Vec<&GpuReport> = gpus.iter().filter(|g| !g.is_software).collect();
    if hardware.is_empty() && !gpus.is_empty() {
        out.push(
            Finding::new(
                "gpu.software_only",
                "No hardware graphics adapter is active",
                Severity::Problem,
                Confidence::SpecGrounded,
            )
            .observed("only a software renderer was enumerated")
            .expected("at least one hardware adapter")
            .basis("DXGI adapter enumeration")
            .recommend(
                "Either the GPU driver is not installed or the adapter has failed. Find out which before buying.",
            ),
        );
    }

    sorted(out)
}

// ---------------------------------------------------------------------------
// Stability
// ---------------------------------------------------------------------------

pub fn stability(c: &CrashHistory) -> Vec<Finding> {
    let mut out = Vec::new();

    if c.minidumps_last_30_days >= 3 {
        out.push(
            Finding::new(
                "crashes.recent",
                "The machine has been crashing recently",
                Severity::Problem,
                Confidence::SpecGrounded,
            )
            .observed(format!(
                "{} blue screens in the last 30 days ({} recorded in total)",
                c.minidumps_last_30_days, c.minidump_count
            ))
            .expected("no recent bugchecks")
            .basis("Crash dump files in C:\\Windows\\Minidump")
            .recommend(
                "A ten-minute inspection will not reproduce this. Ask the seller directly what the crashes were.",
            ),
        );
    }

    if c.whea_uncorrected_count > 0 {
        out.push(
            Finding::new(
                "crashes.whea",
                "Windows has logged hardware errors",
                Severity::Problem,
                Confidence::SpecGrounded,
            )
            .observed(format!("{} error-level WHEA events", c.whea_uncorrected_count))
            .expected("none")
            .basis("Microsoft-Windows-WHEA-Logger events in the System log")
            .recommend(
                "These indicate genuine hardware faults in the CPU, memory or PCIe devices, not software problems.",
            ),
        );
    }

    if let Some(shutdowns) = c.unexpected_shutdowns.get() {
        if shutdowns.len() >= 20 {
            out.push(
                Finding::new(
                    "crashes.power",
                    "The machine loses power without shutting down cleanly",
                    Severity::Watch,
                    Confidence::Heuristic,
                )
                .observed(format!("{} unexpected shutdowns recorded", shutdowns.len()))
                .expected("a handful at most")
                .basis("Kernel-Power event 41 in the System log")
                .recommend(
                    "Can be an unreliable mains supply rather than the machine, but on a laptop it may point at a failing battery or overheating. Ask.",
                ),
            );
        }
    }

    sorted(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probes::ata_smart::AtaHealth;

    fn ata(pending: Option<u64>, reallocated: Option<u64>, hours: Option<u64>) -> AtaHealth {
        AtaHealth {
            power_on_hours: hours.into(),
            power_cycles: Reading::unsupported(),
            reallocated_sectors: reallocated.into(),
            pending_sectors: pending.into(),
            uncorrectable_sectors: Reading::unsupported(),
            temperature_c: Reading::unsupported(),
            life_remaining_percent: Reading::unsupported(),
            terabytes_written: Reading::unsupported(),
            attributes: Vec::new(),
        }
    }

    fn drive(health: AtaHealth) -> DriveReport {
        DriveReport {
            index: 0,
            model: Reading::value("Test Drive".into()),
            serial: Reading::unsupported(),
            firmware: Reading::unsupported(),
            bus_type: "SATA".into(),
            removable: false,
            health: Reading::value(DriveHealth::Ata(health)),
        }
    }

    #[test]
    fn pending_sectors_are_a_problem() {
        let f = storage(&[drive(ata(Some(3), None, None))]);
        assert!(f.iter().any(|f| f.severity == Severity::Problem));
    }

    #[test]
    fn a_clean_drive_raises_no_defect() {
        let f = storage(&[drive(ata(Some(0), Some(0), None))]);
        assert!(
            f.iter().all(|f| f.severity == Severity::Ok),
            "clean drive should not produce defects: {f:?}"
        );
    }

    #[test]
    fn old_drives_are_flagged_as_context_not_failure() {
        let f = storage(&[drive(ata(None, None, Some(82_093)))]);
        let age = f.iter().find(|f| f.id == "storage.age.0").expect("age finding");
        assert_eq!(age.severity, Severity::Watch);
        assert!(age.observed.contains("9.4"), "got {}", age.observed);
    }

    #[test]
    fn a_young_drive_is_not_flagged() {
        let f = storage(&[drive(ata(None, None, Some(1_200)))]);
        let age = f.iter().find(|f| f.id == "storage.age.0").expect("age finding");
        assert_eq!(age.severity, Severity::Ok);
    }

    #[test]
    fn zero_power_on_hours_produces_no_age_finding() {
        let f = storage(&[drive(ata(None, None, Some(0)))]);
        assert!(f.iter().all(|f| f.id != "storage.age.0"));
    }

    #[test]
    fn components_evaluate_independently_of_each_other() {
        // A storage scan must not require battery, memory or firmware data.
        let f = storage(&[drive(ata(Some(1), None, Some(100)))]);
        assert!(!f.is_empty());
    }

    #[test]
    fn every_finding_can_justify_itself() {
        let inv = crate::probes::collect();
        for f in evaluate(&inv) {
            assert!(!f.title.is_empty(), "finding {} has no title", f.id);
            assert!(!f.observed.is_empty(), "finding {} states nothing observed", f.id);
            assert!(!f.basis.is_empty(), "finding {} cites no basis", f.id);
            assert!(!f.recommendation.is_empty(), "finding {} gives no recommendation", f.id);
        }
    }

    #[test]
    fn findings_are_sorted_worst_first() {
        let inv = crate::probes::collect();
        for pair in evaluate(&inv).windows(2) {
            assert!(
                pair[0].severity >= pair[1].severity,
                "findings out of order: {:?} before {:?}",
                pair[0].severity,
                pair[1].severity
            );
        }
    }
}
