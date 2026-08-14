//! Core types shared by every probe.
//!
//! The single most important idea here is [`Reading`]. Hardware inspection is full of
//! metrics that legitimately do not exist on a given machine: AMD exposes different
//! registers than Intel, many laptops have no fan tachometer, a large share of OEMs
//! report a battery cycle count of zero because the pack has no cycle counter at all.
//!
//! A tool that renders those as `0` actively misleads the person deciding whether to
//! hand over money. So no probe in this crate returns a bare value — it returns either
//! a reading or a specific reason the reading could not be taken, and the UI is
//! obligated to surface that reason.

use serde::{Deserialize, Serialize};

/// Why a metric could not be read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum Unavailable {
    /// The silicon genuinely does not expose this. Not an error — do not nag the user.
    NotSupportedByHardware,
    /// Exists, but we are not elevated. Actionable: relaunch as administrator.
    RequiresElevation,
    /// Exists, but the PawnIO driver or a required module is not loaded.
    DriverMissing(String),
    /// Vendor runtime absent (e.g. no NVIDIA driver, so no nvml.dll).
    VendorLibraryMissing(String),
    /// We asked and the call failed. Carries the OS-level detail for the log.
    QueryFailed(String),
    /// Reported by the source but self-evidently wrong (e.g. 0 mWh design capacity).
    ImplausibleValue(String),
    /// Meaningless in this context (e.g. battery metrics on a desktop).
    NotApplicable,
}

impl Unavailable {
    /// Short human-readable phrase for the UI.
    pub fn describe(&self) -> String {
        match self {
            Self::NotSupportedByHardware => "not supported by this hardware".into(),
            Self::RequiresElevation => "requires administrator".into(),
            Self::DriverMissing(m) => format!("driver not loaded ({m})"),
            Self::VendorLibraryMissing(l) => format!("vendor library missing ({l})"),
            Self::QueryFailed(e) => format!("query failed: {e}"),
            Self::ImplausibleValue(v) => format!("reported an implausible value ({v})"),
            Self::NotApplicable => "not applicable".into(),
        }
    }
}

/// A metric that may or may not be readable on this machine.
///
/// Serialises to `{"ok": true, "value": ...}` or
/// `{"ok": false, "reason": {...}, "note": "..."}` so the UI can branch without guessing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Reading<T> {
    Ok {
        ok: bool,
        value: T,
    },
    Missing {
        ok: bool,
        reason: Unavailable,
        note: String,
    },
}

impl<T> Reading<T> {
    pub fn value(v: T) -> Self {
        Reading::Ok { ok: true, value: v }
    }

    pub fn missing(reason: Unavailable) -> Self {
        let note = reason.describe();
        Reading::Missing {
            ok: false,
            reason,
            note,
        }
    }

    pub fn unsupported() -> Self {
        Self::missing(Unavailable::NotSupportedByHardware)
    }

    pub fn failed(e: impl std::fmt::Display) -> Self {
        Self::missing(Unavailable::QueryFailed(e.to_string()))
    }

    pub fn get(&self) -> Option<&T> {
        match self {
            Reading::Ok { value, .. } => Some(value),
            Reading::Missing { .. } => None,
        }
    }

    pub fn is_ok(&self) -> bool {
        matches!(self, Reading::Ok { .. })
    }

    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Reading<U> {
        match self {
            Reading::Ok { value, .. } => Reading::value(f(value)),
            Reading::Missing { reason, note, .. } => Reading::Missing {
                ok: false,
                reason,
                note,
            },
        }
    }
}

impl<T> From<Option<T>> for Reading<T> {
    /// `None` means the hardware did not offer it. Use [`Reading::failed`] when a call
    /// actually errored — the distinction matters to the person reading the report.
    fn from(o: Option<T>) -> Self {
        match o {
            Some(v) => Reading::value(v),
            None => Reading::unsupported(),
        }
    }
}

/// Convenience: treat a `Result` as a reading, recording the error text on failure.
impl<T, E: std::fmt::Display> From<Result<T, E>> for Reading<T> {
    fn from(r: Result<T, E>) -> Self {
        match r {
            Ok(v) => Reading::value(v),
            Err(e) => Reading::failed(e),
        }
    }
}

/// How seriously a buyer should take a finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Nothing to negotiate over.
    Ok,
    /// Worth knowing and worth mentioning, but not disqualifying.
    Watch,
    /// A real defect with a cost attached.
    Problem,
    /// Walk away, or do not proceed until resolved.
    Critical,
}

/// How much we trust the threshold behind a finding.
///
/// This maps onto the three evidence layers in the plan: silicon spec (high),
/// model cohort (varies with sample size), and heuristic inference (low).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// Compared against a manufacturer-guaranteed figure.
    SpecGrounded,
    /// Compared against a cohort of real runs on this laptop model.
    CohortGrounded,
    /// Inferred from behaviour without an authoritative threshold.
    Heuristic,
}

/// One conclusion in the report.
///
/// Every finding must be able to justify itself: what we measured, what we compared it
/// against, and where that comparison value came from. A verdict the buyer cannot
/// defend to the seller is worthless.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    pub id: String,
    pub title: String,
    pub severity: Severity,
    pub confidence: Confidence,
    /// What we actually measured, phrased for a human.
    pub observed: String,
    /// The threshold or reference we judged it against.
    pub expected: String,
    /// Where `expected` came from (spec sheet, cohort percentile, heuristic rule).
    pub basis: String,
    /// What the buyer should do about it.
    pub recommendation: String,
    /// Rough remediation cost in USD, when it is a known repair.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_cost_usd: Option<u32>,
}

impl Finding {
    pub fn new(
        id: impl Into<String>,
        title: impl Into<String>,
        severity: Severity,
        confidence: Confidence,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            severity,
            confidence,
            observed: String::new(),
            expected: String::new(),
            basis: String::new(),
            recommendation: String::new(),
            estimated_cost_usd: None,
        }
    }

    pub fn observed(mut self, s: impl Into<String>) -> Self {
        self.observed = s.into();
        self
    }

    pub fn expected(mut self, s: impl Into<String>) -> Self {
        self.expected = s.into();
        self
    }

    pub fn basis(mut self, s: impl Into<String>) -> Self {
        self.basis = s.into();
        self
    }

    pub fn recommend(mut self, s: impl Into<String>) -> Self {
        self.recommendation = s.into();
        self
    }

    pub fn cost(mut self, usd: u32) -> Self {
        self.estimated_cost_usd = Some(usd);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_readings_carry_a_human_reason() {
        let r: Reading<u32> = Reading::missing(Unavailable::RequiresElevation);
        assert!(!r.is_ok());
        assert!(r.get().is_none());
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("requires administrator"), "got {json}");
    }

    #[test]
    fn ok_readings_serialise_transparently() {
        let r = Reading::value(42u32);
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("42"), "got {json}");
        assert_eq!(r.get(), Some(&42));
    }

    #[test]
    fn option_none_means_unsupported_not_error() {
        let r: Reading<u32> = None.into();
        match r {
            Reading::Missing { reason, .. } => {
                assert_eq!(reason, Unavailable::NotSupportedByHardware)
            }
            _ => panic!("expected missing"),
        }
    }

    #[test]
    fn severity_orders_worst_last() {
        assert!(Severity::Critical > Severity::Problem);
        assert!(Severity::Problem > Severity::Watch);
        assert!(Severity::Watch > Severity::Ok);
    }
}
