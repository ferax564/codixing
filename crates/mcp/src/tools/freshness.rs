//! MCP handler for the `audit_freshness` tool.

use codixing_core::{Engine, FreshnessOptions, FreshnessTier};
use serde_json::Value;

pub(crate) fn call_audit_freshness(engine: &Engine, args: &Value) -> (String, bool) {
    let threshold_days = args
        .get("threshold_days")
        .and_then(|v| v.as_u64())
        .unwrap_or(21);

    let include_pattern = args
        .get("include")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let exclude_pattern = args
        .get("exclude")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let options = FreshnessOptions {
        threshold_days,
        include_pattern,
        exclude_pattern,
    };

    let report = engine.audit_freshness(options);

    if report.entries.is_empty() {
        return (
            format!(
                "## File Freshness Audit\n\nAll {} indexed file(s) are fresh — no stale or orphaned files detected.",
                report.files_audited
            ),
            false,
        );
    }

    // Split by tier.
    let critical: Vec<_> = report
        .entries
        .iter()
        .filter(|e| e.tier == FreshnessTier::Critical)
        .collect();
    let warning: Vec<_> = report
        .entries
        .iter()
        .filter(|e| e.tier == FreshnessTier::Warning)
        .collect();
    let info: Vec<_> = report
        .entries
        .iter()
        .filter(|e| e.tier == FreshnessTier::Info)
        .collect();

    let mut out = String::from("## File Freshness Audit\n");

    // Critical section.
    if !critical.is_empty() {
        out.push_str(&format!(
            "\n### Critical (orphan + stale)\nFiles with no importers AND not modified in {}+ days:\n\n",
            threshold_days
        ));
        out.push_str("| File | Last Modified | Days Old | Orphan? | Reason |\n");
        out.push_str("|------|--------------|----------|---------|--------|\n");
        for e in &critical {
            let date_str = ts_to_date(e.last_modified_ts);
            let days_str = if e.days_old == u64::MAX {
                "very old".to_string()
            } else {
                e.days_old.to_string()
            };
            let orphan_str = e
                .orphan_confidence
                .as_ref()
                .map(|c| c.as_str())
                .unwrap_or("yes");
            out.push_str(&format!(
                "| `{}` | {} | {} | {} | {} |\n",
                e.file_path, date_str, days_str, orphan_str, e.reason
            ));
        }
    }

    // Warning section.
    if !warning.is_empty() {
        out.push_str(&format!(
            "\n### Warning (stale but connected)\nFiles not modified in {}+ days but still imported:\n\n",
            threshold_days
        ));
        out.push_str("| File | Last Modified | Days Old | Importers | Reason |\n");
        out.push_str("|------|--------------|----------|-----------|--------|\n");
        for e in &warning {
            let date_str = ts_to_date(e.last_modified_ts);
            let days_str = if e.days_old == u64::MAX {
                "very old".to_string()
            } else {
                e.days_old.to_string()
            };
            out.push_str(&format!(
                "| `{}` | {} | {} | {} | {} |\n",
                e.file_path, date_str, days_str, e.importer_count, e.reason
            ));
        }
    }

    // Info section.
    if !info.is_empty() {
        out.push_str(
            "\n### Info (recently orphaned)\nFiles with no importers but modified recently:\n\n",
        );
        out.push_str("| File | Last Modified | Days Old | Reason |\n");
        out.push_str("|------|--------------|----------|--------|\n");
        for e in &info {
            let date_str = ts_to_date(e.last_modified_ts);
            let days_str = if e.days_old == u64::MAX {
                "very old".to_string()
            } else {
                e.days_old.to_string()
            };
            out.push_str(&format!(
                "| `{}` | {} | {} | {} |\n",
                e.file_path, date_str, days_str, e.reason
            ));
        }
    }

    // Summary.
    out.push_str(&format!(
        "\n### Summary\n\
         - {} file(s) audited\n\
         - {} critical (orphan + stale)\n\
         - {} warning (stale but connected)\n\
         - {} info (recently orphaned)\n",
        report.files_audited,
        critical.len(),
        warning.len(),
        info.len(),
    ));

    (out, false)
}

/// Convert a Unix timestamp to a `YYYY-MM-DD` string.
/// Returns `"unknown"` for timestamps at or below zero.
fn ts_to_date(ts: i64) -> String {
    if ts <= 0 {
        return "unknown".to_string();
    }
    let days_since_epoch = (ts as u64) / 86_400;

    // Civil-from-days (Howard Hinnant algorithm).
    let z = days_since_epoch as i64 + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02}")
}
