//! Crash and hardware-error history.
//!
//! This is the part of a machine's past a seller cannot tidy up by reinstalling
//! Windows and wiping the desktop. Two independent sources:
//!
//! * **Minidumps** in `C:\Windows\Minidump` — one file per bugcheck (blue screen),
//!   timestamped. A machine with fifteen dumps over the last two months is unstable
//!   regardless of how well it behaves during a ten-minute inspection.
//!
//! * **WHEA-Logger events** — the Windows Hardware Error Architecture channel, which
//!   records machine-check exceptions, memory errors and PCIe errors. *Corrected*
//!   errors are the interesting ones: the machine keeps running, the owner never sees
//!   a crash, but the hardware is degrading.
//!
//! Both are read-only. Neither is affected by a fresh Windows install of the *user*
//! profile, though a full OS reinstall does clear them — which is itself worth knowing,
//! and why the storage odometer is cross-referenced against them.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::model::{Reading, Unavailable};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrashEvent {
    /// ISO-8601 UTC.
    pub timestamp: String,
    pub source: String,
    pub event_id: u32,
    pub level: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrashHistory {
    /// One per blue screen, from the minidump directory.
    pub minidump_count: usize,
    pub most_recent_minidump: Reading<String>,
    /// Minidumps written in the last 30 days: recent instability, not ancient history.
    pub minidumps_last_30_days: usize,
    /// A full kernel dump is present (`C:\Windows\MEMORY.DMP`).
    pub full_memory_dump_present: bool,

    /// Hardware errors from the WHEA-Logger provider.
    pub whea_events: Reading<Vec<CrashEvent>>,
    /// WHEA entries logged as errors rather than warnings/informational.
    pub whea_uncorrected_count: usize,

    /// Kernel-Power event 41: the machine lost power or halted without a clean
    /// shutdown. A high count points at a failing PSU, battery, or overheating.
    pub unexpected_shutdowns: Reading<Vec<CrashEvent>>,
}

pub fn probe() -> CrashHistory {
    let (minidump_count, most_recent, recent_30d) = scan_minidumps(r"C:\Windows\Minidump");
    let whea = query_provider("Microsoft-Windows-WHEA-Logger", None);
    let unexpected_shutdowns = query_provider("Microsoft-Windows-Kernel-Power", Some(41));

    let whea_uncorrected_count = whea
        .get()
        .map(|events| {
            events
                .iter()
                .filter(|e| e.level.eq_ignore_ascii_case("Error") || e.level == "1" || e.level == "2")
                .count()
        })
        .unwrap_or(0);

    CrashHistory {
        minidump_count,
        // "No crashes recorded" is a fact about the machine's history, not a missing
        // capability. Reporting it as unsupported hardware would be nonsense next to
        // a crash count of zero.
        most_recent_minidump: match most_recent {
            Some(ts) => Reading::value(ts),
            None => Reading::missing(Unavailable::NotApplicable),
        },
        minidumps_last_30_days: recent_30d,
        full_memory_dump_present: Path::new(r"C:\Windows\MEMORY.DMP").exists(),
        whea_events: whea,
        whea_uncorrected_count,
        unexpected_shutdowns,
    }
}

/// Count minidumps, find the newest, and count those within 30 days.
fn scan_minidumps(dir: &str) -> (usize, Option<String>, usize) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        // No directory usually means no crashes have ever been recorded.
        Err(_) => return (0, None, 0),
    };

    let now = std::time::SystemTime::now();
    let thirty_days = std::time::Duration::from_secs(30 * 24 * 3600);

    let mut count = 0;
    let mut recent = 0;
    let mut newest: Option<std::time::SystemTime> = None;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()).map(|e| e.eq_ignore_ascii_case("dmp"))
            != Some(true)
        {
            continue;
        }
        count += 1;

        if let Ok(modified) = entry.metadata().and_then(|m| m.modified()) {
            if now.duration_since(modified).map(|d| d < thirty_days).unwrap_or(false) {
                recent += 1;
            }
            if newest.map(|n| modified > n).unwrap_or(true) {
                newest = Some(modified);
            }
        }
    }

    let newest_iso = newest.map(|t| {
        let dt: chrono::DateTime<chrono::Utc> = t.into();
        dt.to_rfc3339()
    });

    (count, newest_iso, recent)
}

// ---------------------------------------------------------------------------
// WHEA-Logger via the Windows Event Log API
// ---------------------------------------------------------------------------

/// Cap on events pulled back. A machine with thousands of corrected errors is already
/// conclusively diagnosed by the first few hundred.
const MAX_EVENTS: usize = 200;

/// Query the System channel for a provider, newest first, optionally filtered to one
/// event ID.
pub fn query_provider(provider: &str, event_id: Option<u32>) -> Reading<Vec<CrashEvent>> {
    use windows::core::PCWSTR;
    use windows::Win32::System::EventLog::{
        EvtClose, EvtNext, EvtQuery, EvtQueryChannelPath, EvtQueryReverseDirection, EvtRender,
        EvtRenderEventXml, EVT_HANDLE,
    };

    let channel = crate::win::to_wide("System");
    let query = crate::win::to_wide(&build_query(provider, event_id));

    let mut events = Vec::new();

    unsafe {
        let handle = match EvtQuery(
            None,
            PCWSTR(channel.as_ptr()),
            PCWSTR(query.as_ptr()),
            (EvtQueryChannelPath.0 | EvtQueryReverseDirection.0) as u32,
        ) {
            Ok(h) => h,
            // ERROR_ACCESS_DENIED reading the System channel; everything else means the
            // channel or provider is unavailable.
            Err(e) => {
                let err = crate::win::WinError::from_win("EvtQuery", &e);
                return if err.code == 5 {
                    Reading::missing(Unavailable::RequiresElevation)
                } else {
                    Reading::failed(err)
                };
            }
        };

        // EvtNext takes a raw isize array even though the handles are EVT_HANDLE.
        let mut batch = [0isize; 16];
        while events.len() < MAX_EVENTS {
            let mut returned: u32 = 0;
            if EvtNext(handle, &mut batch, 5_000, 0, &mut returned).is_err() {
                // ERROR_NO_MORE_ITEMS is the normal terminator.
                break;
            }
            if returned == 0 {
                break;
            }

            for item in batch.iter().take(returned as usize) {
                let event_handle = EVT_HANDLE(*item);
                let mut needed: u32 = 0;
                let mut property_count: u32 = 0;
                // First call sizes the buffer and is expected to fail.
                let _ = EvtRender(
                    None,
                    event_handle,
                    EvtRenderEventXml.0 as u32,
                    0,
                    None,
                    &mut needed,
                    &mut property_count,
                );

                if needed > 0 {
                    let mut buf = vec![0u8; needed as usize];
                    if EvtRender(
                        None,
                        event_handle,
                        EvtRenderEventXml.0 as u32,
                        needed,
                        Some(buf.as_mut_ptr() as *mut _),
                        &mut needed,
                        &mut property_count,
                    )
                    .is_ok()
                    {
                        let chars: Vec<u16> = buf
                            .chunks_exact(2)
                            .map(|c| u16::from_le_bytes([c[0], c[1]]))
                            .collect();
                        let xml = crate::win::wide_to_string(&chars);
                        if let Some(event) = parse_event_xml(&xml) {
                            events.push(event);
                        }
                    }
                }
                let _ = EvtClose(event_handle);
            }
        }

        let _ = EvtClose(handle);
    }

    Reading::value(events)
}

/// Build an XPath event query for one provider, optionally narrowed to an event ID.
pub fn build_query(provider: &str, event_id: Option<u32>) -> String {
    match event_id {
        Some(id) => format!(
            "*[System[Provider[@Name='{provider}'] and (EventID={id})]]"
        ),
        None => format!("*[System[Provider[@Name='{provider}']]]"),
    }
}

/// Pull the fields we care about out of a rendered event XML document.
///
/// Deliberately attribute-scraping rather than full XML parsing: the System block has a
/// fixed shape, and this avoids pulling a parser over a hot path that may run hundreds
/// of times.
pub fn parse_event_xml(xml: &str) -> Option<CrashEvent> {
    let timestamp = extract_attribute(xml, "TimeCreated", "SystemTime")?;
    let event_id = extract_element(xml, "EventID")
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);
    let level = extract_element(xml, "Level").unwrap_or_else(|| "Unknown".to_string());
    let source = extract_attribute(xml, "Provider", "Name")
        .unwrap_or_else(|| "Microsoft-Windows-WHEA-Logger".to_string());

    Some(CrashEvent {
        timestamp,
        source,
        event_id,
        level: level_name(&level),
    })
}

/// Windows event levels: 1 Critical, 2 Error, 3 Warning, 4 Information.
fn level_name(raw: &str) -> String {
    match raw {
        "1" => "Critical".into(),
        "2" => "Error".into(),
        "3" => "Warning".into(),
        "4" => "Information".into(),
        other => other.to_string(),
    }
}

fn extract_element(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let start = xml.find(&open)?;
    let gt = xml[start..].find('>')? + start + 1;
    let close = format!("</{tag}>");
    let end = xml[gt..].find(&close)? + gt;
    Some(xml[gt..end].trim().to_string())
}

fn extract_attribute(xml: &str, tag: &str, attribute: &str) -> Option<String> {
    let open = format!("<{tag}");
    let start = xml.find(&open)?;
    let element_end = xml[start..].find('>')? + start;
    let element = &xml[start..element_end];
    let needle = format!("{attribute}='");
    let (needle, quote) = if element.contains(&needle) {
        (needle, '\'')
    } else {
        (format!("{attribute}=\""), '"')
    };
    let value_start = element.find(&needle)? + needle.len();
    let value_end = element[value_start..].find(quote)? + value_start;
    Some(element[value_start..value_end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<Event xmlns='http://schemas.microsoft.com/win/2004/08/events/event'><System><Provider Name='Microsoft-Windows-WHEA-Logger' Guid='{c26c4f3c-3f66-4e99-8f8a-39405cfed220}'/><EventID>17</EventID><Version>0</Version><Level>3</Level><Task>0</Task><Opcode>0</Opcode><Keywords>0x8000000000000000</Keywords><TimeCreated SystemTime='2026-03-14T09:21:44.1234567Z'/><EventRecordID>5512</EventRecordID><Channel>System</Channel><Computer>DESKTOP-A4CG2OG</Computer></System></Event>"#;

    #[test]
    fn parses_a_whea_event() {
        let e = parse_event_xml(SAMPLE).expect("should parse");
        assert_eq!(e.event_id, 17);
        assert_eq!(e.timestamp, "2026-03-14T09:21:44.1234567Z");
        assert_eq!(e.source, "Microsoft-Windows-WHEA-Logger");
        assert_eq!(e.level, "Warning");
    }

    #[test]
    fn maps_numeric_levels_to_names() {
        assert_eq!(level_name("1"), "Critical");
        assert_eq!(level_name("2"), "Error");
        assert_eq!(level_name("3"), "Warning");
        assert_eq!(level_name("4"), "Information");
    }

    #[test]
    fn handles_double_quoted_attributes() {
        let xml = r#"<Event><System><Provider Name="Test"/><EventID>41</EventID><Level>1</Level><TimeCreated SystemTime="2026-01-01T00:00:00Z"/></System></Event>"#;
        let e = parse_event_xml(xml).expect("should parse");
        assert_eq!(e.source, "Test");
        assert_eq!(e.event_id, 41);
        assert_eq!(e.level, "Critical");
    }

    #[test]
    fn malformed_xml_returns_none_not_panic() {
        assert!(parse_event_xml("").is_none());
        assert!(parse_event_xml("<Event><System></System></Event>").is_none());
        assert!(parse_event_xml("<<<>>>").is_none());
    }

    #[test]
    fn missing_minidump_directory_is_zero_not_an_error() {
        let (count, newest, recent) = scan_minidumps(r"C:\definitely\not\a\real\path");
        assert_eq!(count, 0);
        assert_eq!(recent, 0);
        assert!(newest.is_none());
    }

    #[test]
    fn probing_this_machine_does_not_panic() {
        let h = probe();
        // Whatever the machine's history, the counts must be self-consistent.
        assert!(h.minidumps_last_30_days <= h.minidump_count);
    }

    #[test]
    fn builds_provider_queries() {
        assert_eq!(
            build_query("Microsoft-Windows-WHEA-Logger", None),
            "*[System[Provider[@Name='Microsoft-Windows-WHEA-Logger']]]"
        );
        assert_eq!(
            build_query("Microsoft-Windows-Kernel-Power", Some(41)),
            "*[System[Provider[@Name='Microsoft-Windows-Kernel-Power'] and (EventID=41)]]"
        );
    }

    /// Proves the query -> render -> parse pipeline works end to end.
    ///
    /// WHEA is silent on a healthy machine, so "no hardware errors" and "the parser is
    /// broken" produce identical output. Querying a provider that always has entries is
    /// the only way to tell those apart.
    #[test]
    fn event_pipeline_returns_real_parsed_events() {
        let r = query_provider("Service Control Manager", None);
        match r {
            Reading::Ok { value, .. } => {
                assert!(!value.is_empty(), "System log had no Service Control Manager events");
                let e = &value[0];
                assert!(
                    e.timestamp.starts_with("20"),
                    "timestamp not parsed: {:?}",
                    e.timestamp
                );
                assert!(e.event_id > 0, "event id not parsed: {:?}", e);
                assert!(!e.level.is_empty(), "level not parsed");
            }
            Reading::Missing { note, .. } => {
                // Reading the System channel can require elevation; that is acceptable,
                // any other failure is not.
                assert!(
                    note.contains("administrator"),
                    "event pipeline failed unexpectedly: {note}"
                );
            }
        }
    }
}
