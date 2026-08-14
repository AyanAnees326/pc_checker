//! Battery capacity history, parsed from `powercfg /batteryreport /xml`.
//!
//! This is the single most decision-relevant battery feature in the tool, and it is
//! not something BatteryInfoView shows.
//!
//! A live health reading is a point with no context. A pack sitting at 85% that was
//! 95% three months ago is failing fast and will need replacing within the year; a
//! pack stable at 85% for two years is simply middle-aged and fine. Same number,
//! opposite conclusions — only the trend separates them.
//!
//! Schema confirmed by generating a real report on Windows 11 (26100). The capacity
//! series lives in `HistoryEntry` *attributes*, not child elements:
//!
//! ```xml
//! <HistoryEntry StartDate="2024-08-05T01:33:49Z" EndDate="2024-08-12T00:00:00Z"
//!               DesignCapacity="0" FullChargeCapacity="0" CycleCount="0"
//!               BatteryChanged="0" ... />
//! ```

use std::process::Command;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::model::{Reading, Unavailable};

/// One weekly-ish observation of the pack's capacity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapacitySample {
    pub period_start: String,
    pub period_end: String,
    pub design_capacity_mwh: u64,
    pub full_charge_capacity_mwh: u64,
    pub health_percent: f64,
    pub cycle_count: Option<u32>,
    /// Windows saw a different pack in this period.
    pub battery_changed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatteryHistory {
    pub samples: Vec<CapacitySample>,
    /// The report recorded a pack swap. Either the battery was replaced (relevant to
    /// how the machine was treated and whether the pack is OEM) or the seller has
    /// already had to deal with a failure.
    pub battery_swap_detected: bool,
    pub health_first: Reading<f64>,
    pub health_last: Reading<f64>,
    /// Fitted slope of health over time. Negative means degrading.
    pub degradation_percent_per_year: Reading<f64>,
    pub observation_days: u32,
}

/// Run `powercfg /batteryreport` and parse the result.
///
/// The report is written to the process temp directory and deleted immediately after
/// parsing — this tool runs on machines it does not own and should leave nothing behind.
pub fn probe() -> Reading<BatteryHistory> {
    let path = std::env::temp_dir().join(format!("pc_checker_batteryreport_{}.xml", std::process::id()));

    let result = run_powercfg(&path);
    let parsed = match result {
        Ok(()) => match std::fs::read_to_string(&path) {
            Ok(xml) => match parse(&xml) {
                Some(h) if !h.samples.is_empty() => Reading::value(h),
                Some(_) => Reading::missing(Unavailable::NotApplicable),
                None => Reading::missing(Unavailable::QueryFailed(
                    "battery report XML did not parse".into(),
                )),
            },
            Err(e) => Reading::failed(e),
        },
        Err(e) => Reading::failed(e),
    };

    let _ = std::fs::remove_file(&path);
    parsed
}

fn run_powercfg(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(windows)]
    use std::os::windows::process::CommandExt;
    #[cfg(windows)]
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let mut cmd = Command::new("powercfg");
    cmd.args(["/batteryreport", "/xml", "/output"]).arg(path);

    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);

    let output = cmd.output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "powercfg exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

/// Parse the battery report XML.
///
/// Matches on local tag names so the `http://schemas.microsoft.com/battery/2012`
/// namespace does not need to be threaded through every lookup.
pub fn parse(xml: &str) -> Option<BatteryHistory> {
    let doc = roxmltree::Document::parse(xml).ok()?;

    let mut samples = Vec::new();
    let mut battery_swap_detected = false;

    for node in doc
        .descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == "HistoryEntry")
    {
        let design = attr_u64(&node, "DesignCapacity").unwrap_or(0);
        let full = attr_u64(&node, "FullChargeCapacity").unwrap_or(0);
        let changed = attr_u64(&node, "BatteryChanged").unwrap_or(0) != 0;

        if changed {
            battery_swap_detected = true;
        }

        // Periods with no battery present report zeroes. On a desktop that is every
        // row; on a laptop it means the pack was out. Either way there is no health
        // figure to derive, so the row is not a data point.
        if design == 0 || full == 0 {
            continue;
        }

        samples.push(CapacitySample {
            period_start: node.attribute("StartDate").unwrap_or_default().to_string(),
            period_end: node.attribute("EndDate").unwrap_or_default().to_string(),
            design_capacity_mwh: design,
            full_charge_capacity_mwh: full,
            health_percent: (full as f64 / design as f64) * 100.0,
            cycle_count: attr_u64(&node, "CycleCount")
                .filter(|&c| c > 0)
                .map(|c| c as u32),
            battery_changed: changed,
        });
    }

    let health_first = samples.first().map(|s| s.health_percent);
    let health_last = samples.last().map(|s| s.health_percent);
    let (slope, days) = fit_degradation(&samples);

    Some(BatteryHistory {
        battery_swap_detected,
        health_first: health_first.into(),
        health_last: health_last.into(),
        degradation_percent_per_year: match slope {
            Some(v) => Reading::value(v),
            None => Reading::missing(Unavailable::QueryFailed(
                "not enough history to establish a trend".into(),
            )),
        },
        observation_days: days,
        samples,
    })
}

fn attr_u64(node: &roxmltree::Node, name: &str) -> Option<u64> {
    node.attribute(name)?.trim().parse::<u64>().ok()
}

/// Least-squares fit of health against time, expressed as percent per year.
///
/// Returns `None` when there are too few points or the window is too short for a
/// slope to mean anything — an honest "unknown" beats an extrapolation from two
/// weeks of data.
fn fit_degradation(samples: &[CapacitySample]) -> (Option<f64>, u32) {
    // Only fit across a continuous run of the *same* pack; a swap resets the baseline
    // and would otherwise show up as a huge fake improvement.
    let segment: Vec<&CapacitySample> = match samples.iter().rposition(|s| s.battery_changed) {
        Some(idx) => samples[idx..].iter().collect(),
        None => samples.iter().collect(),
    };

    let points: Vec<(f64, f64)> = segment
        .iter()
        .filter_map(|s| {
            let t = parse_date(&s.period_start)?;
            Some((t.timestamp() as f64, s.health_percent))
        })
        .collect();

    if points.len() < 2 {
        return (None, 0);
    }

    let t0 = points[0].0;
    let t_last = points[points.len() - 1].0;
    let span_days = ((t_last - t0) / 86_400.0).round().max(0.0) as u32;

    // Under a month of observation cannot support a per-year rate.
    if span_days < 28 {
        return (None, span_days);
    }

    let n = points.len() as f64;
    let xs: Vec<f64> = points.iter().map(|p| (p.0 - t0) / 86_400.0).collect(); // days
    let ys: Vec<f64> = points.iter().map(|p| p.1).collect();

    let mean_x = xs.iter().sum::<f64>() / n;
    let mean_y = ys.iter().sum::<f64>() / n;

    let mut num = 0.0;
    let mut den = 0.0;
    for i in 0..points.len() {
        let dx = xs[i] - mean_x;
        num += dx * (ys[i] - mean_y);
        den += dx * dx;
    }

    if den.abs() < f64::EPSILON {
        return (None, span_days);
    }

    // Slope is percent-per-day; report percent-per-year.
    (Some((num / den) * 365.25), span_days)
}

fn parse_date(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(start: &str, end: &str, design: u64, full: u64, changed: u8) -> String {
        format!(
            r#"<HistoryEntry StartDate="{start}" EndDate="{end}" DesignCapacity="{design}" FullChargeCapacity="{full}" CycleCount="0" BatteryChanged="{changed}" />"#
        )
    }

    fn report(entries: &str) -> String {
        format!(
            r#"<?xml version="1.0"?>
<BatteryReport xmlns="http://schemas.microsoft.com/battery/2012">
  <History>{entries}</History>
</BatteryReport>"#
        )
    }

    #[test]
    fn parses_capacity_history_from_attributes() {
        let xml = report(&format!(
            "{}{}",
            entry("2026-01-01T00:00:00Z", "2026-01-08T00:00:00Z", 50000, 47000, 0),
            entry("2026-02-01T00:00:00Z", "2026-02-08T00:00:00Z", 50000, 46000, 0),
        ));
        let h = parse(&xml).unwrap();
        assert_eq!(h.samples.len(), 2);
        assert!((h.samples[0].health_percent - 94.0).abs() < 0.01);
        assert!((h.samples[1].health_percent - 92.0).abs() < 0.01);
    }

    /// A desktop's report is all zero rows. Those must not become 0%-health samples.
    #[test]
    fn zero_capacity_rows_are_not_data_points() {
        let xml = report(&entry(
            "2024-08-05T01:33:49Z",
            "2024-08-12T00:00:00Z",
            0,
            0,
            0,
        ));
        let h = parse(&xml).unwrap();
        assert!(h.samples.is_empty(), "zero rows must be skipped entirely");
        assert!(!h.degradation_percent_per_year.is_ok());
    }

    #[test]
    fn detects_a_battery_swap() {
        let xml = report(&format!(
            "{}{}",
            entry("2026-01-01T00:00:00Z", "2026-01-08T00:00:00Z", 50000, 40000, 0),
            entry("2026-02-01T00:00:00Z", "2026-02-08T00:00:00Z", 50000, 49500, 1),
        ));
        let h = parse(&xml).unwrap();
        assert!(h.battery_swap_detected, "BatteryChanged=1 must be surfaced");
    }

    /// Health jumping up after a swap must not be fitted as "improving" — the trend
    /// is only meaningful within one pack's lifetime.
    #[test]
    fn trend_ignores_history_before_a_swap() {
        let mut entries = String::new();
        // Old pack degrading badly.
        entries.push_str(&entry("2026-01-01T00:00:00Z", "2026-01-08T00:00:00Z", 50000, 30000, 0));
        // New pack installed, then slowly degrading over 3 months.
        entries.push_str(&entry("2026-02-01T00:00:00Z", "2026-02-08T00:00:00Z", 50000, 50000, 1));
        entries.push_str(&entry("2026-03-01T00:00:00Z", "2026-03-08T00:00:00Z", 50000, 49500, 0));
        entries.push_str(&entry("2026-05-01T00:00:00Z", "2026-05-08T00:00:00Z", 50000, 49000, 0));

        let h = parse(&report(&entries)).unwrap();
        let slope = *h.degradation_percent_per_year.get().unwrap();
        assert!(
            slope < 0.0,
            "post-swap trend should degrade, not show a jump: {slope}"
        );
        assert!(slope > -20.0, "slope implausibly steep: {slope}");
    }

    #[test]
    fn refuses_to_extrapolate_from_a_short_window() {
        let xml = report(&format!(
            "{}{}",
            entry("2026-01-01T00:00:00Z", "2026-01-08T00:00:00Z", 50000, 47000, 0),
            entry("2026-01-08T00:00:00Z", "2026-01-15T00:00:00Z", 50000, 46900, 0),
        ));
        let h = parse(&xml).unwrap();
        assert!(
            !h.degradation_percent_per_year.is_ok(),
            "two weeks is not enough to state a per-year rate"
        );
    }

    #[test]
    fn fits_a_realistic_degradation_rate() {
        // 100% -> 90% over one year should fit to about -10 %/year.
        let mut entries = String::new();
        for month in 0..12 {
            let full = 50_000 - (month * 417); // ~10% over 12 months
            entries.push_str(&entry(
                &format!("2026-{:02}-01T00:00:00Z", month + 1),
                &format!("2026-{:02}-08T00:00:00Z", month + 1),
                50_000,
                full as u64,
                0,
            ));
        }
        let h = parse(&report(&entries)).unwrap();
        let slope = *h.degradation_percent_per_year.get().unwrap();
        assert!(
            (slope + 10.0).abs() < 2.0,
            "expected about -10 %/year, got {slope}"
        );
        assert!(h.observation_days >= 300, "got {}", h.observation_days);
    }

    #[test]
    fn malformed_xml_returns_none_not_panic() {
        assert!(parse("not xml at all").is_none());
        assert!(parse("").is_none());
    }
}
