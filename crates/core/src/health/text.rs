//! Ages, durations and dates in user text, and the plain-text forms of a checkup and a boot
//! history. The text forms name no account or computer.

use std::cmp::Reverse;
use std::fmt::{Display, Write as _};

use chrono::{DateTime, Local, TimeZone, Utc};

use super::boot::{BootHistory, BootRecord, BootStats, BootType, LogAccess, Phase, SlowKind};
use super::checkup::{Check, CheckState, Checkup, Grade};

/// "1 day", "3 days": `noun` gets an "s" unless `count` is 1.
pub(crate) fn plural(count: u64, noun: &str) -> String {
    format!("{count} {noun}{}", if count == 1 { "" } else { "s" })
}

/// Whole days from `from` to `now` (0 for a time in the future).
pub(crate) fn days_between(from: DateTime<Utc>, now: DateTime<Utc>) -> i64 {
    (now - from).num_days().max(0)
}

/// How long ago `from` was: "just now", "5 minutes ago", "17 hours ago", "3 days ago".
pub(crate) fn age_text(from: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let age = now - from;
    let minutes = age.num_minutes();
    if minutes < 1 {
        "just now".into()
    } else if minutes < 60 {
        format!("{} ago", plural(minutes as u64, "minute"))
    } else if age.num_hours() < 48 {
        format!("{} ago", plural(age.num_hours() as u64, "hour"))
    } else {
        format!("{} ago", plural(age.num_days() as u64, "day"))
    }
}

/// A date in the local time zone: "2026-09-28".
pub(crate) fn local_date(t: DateTime<Utc>) -> String {
    t.with_timezone(&Local).format("%Y-%m-%d").to_string()
}

/// A date and time in the local time zone: "2026-09-28 10:12".
pub(crate) fn local_datetime(t: DateTime<Utc>) -> String {
    t.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string()
}

/// A duration: "41.2 s" below a minute, "1 min 5 s" from a minute, "–" when missing.
pub fn duration_text(ms: Option<u64>) -> String {
    let Some(ms) = ms else {
        return "–".into();
    };
    if ms < 60_000 {
        format!("{:.1} s", ms as f64 / 1000.0)
    } else {
        let seconds = (ms + 500) / 1000;
        format!("{} min {} s", seconds / 60, seconds % 60)
    }
}

/// Trend of start times, with ±15 % as the threshold; `None` without enough starts.
pub fn trend_text(stats: &BootStats) -> Option<String> {
    let trend = stats.trend.as_ref()?;
    let pct = trend.change_pct;
    Some(if pct >= 15.0 {
        format!(
            "Starts are getting slower: {:.0}% slower than earlier starts",
            pct
        )
    } else if pct <= -15.0 {
        format!("Starts are getting faster: {:.0}% faster", -pct)
    } else {
        "Start times are steady".to_string()
    })
}

fn grade_text(grade: Grade) -> &'static str {
    match grade {
        Grade::Good => "well protected",
        Grade::Fair => "needs attention",
        Grade::AtRisk => "at risk",
    }
}

fn check_line(out: &mut String, check: &Check) {
    let (icon, chip) = match check.state {
        CheckState::Attention => ("⚠", check.severity.chip()),
        CheckState::Good => ("✓", ""),
        CheckState::Unknown => ("○", "Not checked"),
        CheckState::Checking => ("◐", "Checking"),
        CheckState::NotApplicable => ("–", ""),
    };
    let _ = writeln!(
        out,
        "  {icon} {chip:<13} {:<32} {}",
        check.title, check.summary
    );
}

/// Plain text of a checkup: the score, then the checks to fix (most severe first), the
/// checks that could not run and the passed ones, then the notes and errors.
pub fn checkup_text<Tz: TimeZone>(checkup: &Checkup, tz: &Tz) -> String
where
    Tz::Offset: Display,
{
    let mut out = String::new();
    let score = &checkup.score;
    let mut head = format!(
        "Security checkup  ·  score {} ({})",
        score.value,
        grade_text(score.grade)
    );
    if score.to_fix == 0 && score.unknown == 0 {
        head.push_str("  ·  nothing to fix");
    } else {
        let _ = write!(head, "  ·  {} to fix", score.to_fix);
        if score.unknown > 0 {
            let _ = write!(head, ", {} not checked", score.unknown);
        }
    }
    let _ = writeln!(out, "{head}");
    let _ = writeln!(
        out,
        "Checked {} in {:.1} s  ·  read-only: nothing on this PC is changed",
        checkup.taken_at.with_timezone(tz).format("%Y-%m-%d %H:%M"),
        checkup.duration_ms as f64 / 1000.0
    );
    let mut attention: Vec<&Check> = checkup
        .checks
        .iter()
        .filter(|c| c.state == CheckState::Attention)
        .collect();
    attention.sort_by_key(|c| Reverse(c.severity));
    let unknown = checkup
        .checks
        .iter()
        .filter(|c| matches!(c.state, CheckState::Unknown | CheckState::Checking));
    let passed = checkup
        .checks
        .iter()
        .filter(|c| matches!(c.state, CheckState::Good | CheckState::NotApplicable));
    out.push('\n');
    for check in attention {
        check_line(&mut out, check);
        for fact in &check.facts {
            if fact.label.is_empty() {
                let _ = writeln!(out, "{:17}{}", "", fact.value);
            } else {
                let _ = writeln!(out, "{:17}{}: {}", "", fact.label, fact.value);
            }
        }
    }
    for check in unknown.chain(passed) {
        check_line(&mut out, check);
    }
    if !checkup.notes.is_empty() || !checkup.errors.is_empty() {
        out.push('\n');
    }
    for note in &checkup.notes {
        let _ = writeln!(out, "Note: {note}");
    }
    for error in &checkup.errors {
        let _ = writeln!(
            out,
            "Could not read {}: {}",
            error.source.title(),
            error.message
        );
    }
    out
}

/// [`checkup_text`] with times in the local time zone.
pub fn checkup_text_local(checkup: &Checkup) -> String {
    checkup_text(checkup, &Local)
}

fn boot_type_text(kind: BootType) -> &'static str {
    match kind {
        BootType::Full => "Full start",
        BootType::FastStartup => "Fast Startup",
        BootType::Hibernate => "Resume",
        BootType::Unknown => "–",
    }
}

pub(crate) fn kind_text(kind: SlowKind) -> &'static str {
    match kind {
        SlowKind::App => "App",
        SlowKind::Driver => "Driver",
        SlowKind::Service => "Service",
        SlowKind::Device => "Device",
        SlowKind::Windows => "Windows",
        SlowKind::Prefetch => "Prefetch",
        SlowKind::Policy => "Policy",
    }
}

/// "Last full start 41.2 s (desktop after 18.0 s)  ·  Typical 38.5 s over 24 starts". Windows
/// times only full starts, so those are the starts listed.
pub fn summary_text(history: &BootHistory) -> String {
    let Some(latest) = history.boots.first() else {
        return "No full starts are recorded yet.".into();
    };
    format!(
        "Last full start {} (desktop after {})  ·  Typical {} over {}",
        duration_text(Some(latest.boot_ms)),
        duration_text(Some(latest.main_path_ms)),
        duration_text(history.stats.median_ms),
        plural(u64::from(history.stats.count), "start")
    )
}

fn boot_line<Tz: TimeZone>(boot: &BootRecord, tz: &Tz) -> String
where
    Tz::Offset: Display,
{
    let when = boot
        .started_at
        .unwrap_or(boot.logged_at)
        .with_timezone(tz)
        .format("%Y-%m-%d %H:%M");
    let mut line = format!(
        "{when}  ·  {}  ·  {} (desktop {})",
        boot_type_text(boot.boot_type),
        duration_text(Some(boot.boot_ms)),
        duration_text(Some(boot.main_path_ms))
    );
    if let Some(apps) = boot.startup_apps {
        let _ = write!(line, "  ·  {}", plural(u64::from(apps), "startup app"));
    }
    if boot.degraded {
        line.push_str("  ·  ⚠ slower than usual");
    }
    if boot.after_update {
        line.push_str("  ·  after an update");
    }
    if boot.after_unexpected_shutdown {
        line.push_str("  ·  after an unexpected shutdown");
    }
    line
}

/// Plain text of a boot history: the summary and trend, what slowed starts, each start and
/// each shutdown.
pub fn boot_text<Tz: TimeZone>(history: &BootHistory, tz: &Tz) -> String
where
    Tz::Offset: Display,
{
    let mut out = String::new();
    match history.access {
        LogAccess::NeedsAdmin => {
            out.push_str(
                "Windows keeps its startup records where only administrators can read them. \
                 Run optctl from an administrator terminal to see them.\n",
            );
            return out;
        }
        LogAccess::LogMissing => {
            out.push_str("This copy of Windows does not keep startup performance records.\n");
            return out;
        }
        LogAccess::LogDisabled => out.push_str(
            "Windows is not recording startup performance on this PC: its \
             Diagnostics-Performance log is turned off.\n",
        ),
        LogAccess::Ok => {}
    }
    let _ = writeln!(out, "Boot history  ·  {}", summary_text(history));
    if let Some(trend) = trend_text(&history.stats) {
        let _ = writeln!(out, "{trend}");
    }
    if history.fast_startup == Some(true) {
        out.push_str(
            "Fast Startup is on: Windows times only full starts (restarts), so most starts are \
             not listed.\n",
        );
    }
    if let Some(last) = history.unexpected_shutdowns.first() {
        let _ = writeln!(
            out,
            "⚠ Windows was shut down unexpectedly {} (crash or power loss), last on {}.",
            plural(history.unexpected_shutdowns.len() as u64, "time"),
            last.with_timezone(tz).format("%Y-%m-%d")
        );
    }
    for (phase, heading, records, noun) in [
        (
            Phase::Startup,
            "Slows startup",
            history.boots.len(),
            "start",
        ),
        (
            Phase::Shutdown,
            "Slows shutdown",
            history.shutdowns.len(),
            "shutdown",
        ),
    ] {
        let items: Vec<_> = history
            .slow_items
            .iter()
            .filter(|i| i.phase == phase)
            .collect();
        if items.is_empty() {
            continue;
        }
        let _ = writeln!(out, "\n{heading}:");
        for item in items {
            let _ = writeln!(
                out,
                "  {:<8} {}  ·  {} of the last {}  ·  usually adds {}",
                kind_text(item.kind),
                item.title,
                item.count,
                plural(records as u64, noun),
                duration_text(Some(item.median_degradation_ms))
            );
        }
    }
    if !history.boots.is_empty() {
        out.push_str("\nRecent starts:\n");
        for boot in &history.boots {
            let _ = writeln!(out, "  {}", boot_line(boot, tz));
        }
    }
    if !history.shutdowns.is_empty() {
        out.push_str("\nShutdowns:\n");
        for shutdown in &history.shutdowns {
            let when = shutdown
                .started_at
                .unwrap_or(shutdown.logged_at)
                .with_timezone(tz)
                .format("%Y-%m-%d %H:%M");
            let mut line = format!("  {when}  ·  {}", duration_text(Some(shutdown.shutdown_ms)));
            if shutdown.degraded {
                line.push_str("  ·  ⚠ slower than usual");
            }
            let _ = writeln!(out, "{line}");
        }
    }
    for note in &history.notes {
        let _ = writeln!(out, "Note: {note}");
    }
    for error in &history.errors {
        let _ = writeln!(out, "Could not read: {error}");
    }
    out
}

/// [`boot_text`] with times in the local time zone.
pub fn boot_text_local(history: &BootHistory) -> String {
    boot_text(history, &Local)
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::super::boot::tests::sample_history;
    use super::super::boot::Trend;
    use super::super::checkup::{CheckId, Score, Severity, Source, SourceError};
    use super::super::probe::fixtures;
    use super::*;

    fn at(text: &str) -> DateTime<Utc> {
        text.parse().unwrap()
    }

    #[test]
    fn plurals_and_ages() {
        assert_eq!(plural(1, "day"), "1 day");
        assert_eq!(plural(0, "day"), "0 days");
        assert_eq!(plural(3, "security update"), "3 security updates");
        let now = at("2026-09-28T12:00:00Z");
        assert_eq!(age_text(now - Duration::seconds(30), now), "just now");
        assert_eq!(age_text(now - Duration::minutes(1), now), "1 minute ago");
        assert_eq!(age_text(now - Duration::minutes(59), now), "59 minutes ago");
        assert_eq!(age_text(now - Duration::hours(17), now), "17 hours ago");
        assert_eq!(age_text(now - Duration::hours(47), now), "47 hours ago");
        assert_eq!(age_text(now - Duration::hours(48), now), "2 days ago");
        assert_eq!(days_between(now - Duration::hours(47), now), 1);
        assert_eq!(days_between(now + Duration::days(3), now), 0);
    }

    #[test]
    fn durations() {
        assert_eq!(duration_text(None), "–");
        assert_eq!(duration_text(Some(0)), "0.0 s");
        assert_eq!(duration_text(Some(999)), "1.0 s");
        assert_eq!(duration_text(Some(41_200)), "41.2 s");
        assert_eq!(duration_text(Some(59_949)), "59.9 s");
        assert_eq!(duration_text(Some(60_000)), "1 min 0 s");
        assert_eq!(duration_text(Some(65_000)), "1 min 5 s");
        assert_eq!(duration_text(Some(125_600)), "2 min 6 s");
    }

    fn stats(change_pct: Option<f64>) -> BootStats {
        BootStats {
            count: 10,
            latest_ms: Some(41_200),
            median_ms: Some(38_500),
            full_count: 10,
            fast_count: 0,
            trend: change_pct.map(|change_pct| Trend {
                boot_type: Some(BootType::Full),
                recent_median_ms: 44_000,
                earlier_median_ms: 38_000,
                change_pct,
            }),
        }
    }

    #[test]
    fn trend_boundaries_are_fifteen_percent() {
        assert_eq!(trend_text(&stats(None)), None);
        assert_eq!(
            trend_text(&stats(Some(24.0))).unwrap(),
            "Starts are getting slower: 24% slower than earlier starts"
        );
        assert_eq!(
            trend_text(&stats(Some(15.0))).unwrap(),
            "Starts are getting slower: 15% slower than earlier starts"
        );
        assert_eq!(
            trend_text(&stats(Some(14.9))).unwrap(),
            "Start times are steady"
        );
        assert_eq!(
            trend_text(&stats(Some(-14.9))).unwrap(),
            "Start times are steady"
        );
        assert_eq!(
            trend_text(&stats(Some(-15.0))).unwrap(),
            "Starts are getting faster: 15% faster"
        );
        assert_eq!(
            trend_text(&stats(Some(-18.2))).unwrap(),
            "Starts are getting faster: 18% faster"
        );
    }

    fn small_checkup() -> Checkup {
        let good = |id: CheckId, summary: &str| Check::new(id, "").good(summary);
        let checks = vec![
            good(CheckId::Antivirus, "Microsoft Defender Antivirus is on"),
            Check::new(CheckId::SecurityIntelligence, "")
                .fact("Version", "1.459.440.0")
                .line("• one line")
                .attention(Severity::High, "9 days old"),
            good(CheckId::Firewall, "On for every network type"),
            Check::new(CheckId::Encryption, "").unknown("Needs administrator rights to check"),
            Check::new(CheckId::RemoteDesktop, "")
                .not_applicable("Not available on Windows 11 Home"),
            Check::new(CheckId::RemoteAssistance, "")
                .attention(Severity::Low, "Invitations are allowed"),
        ];
        Checkup {
            taken_at: at("2026-09-28T10:12:00Z"),
            duration_ms: 1300,
            elevated: false,
            other_user: Some(false),
            home_edition: true,
            score: Score {
                value: 77,
                grade: Grade::Fair,
                to_fix: 2,
                critical: 0,
                unknown: 1,
                checked: 4,
            },
            checks,
            update_scan: fixtures::scan_done(),
            update_scan_due: false,
            notes: vec!["Drive encryption can only be checked with administrator rights.".into()],
            errors: vec![SourceError {
                source: Source::Defender,
                message: "WMI did not answer".into(),
            }],
        }
    }

    #[test]
    fn checkup_text_golden() {
        let text = checkup_text(&small_checkup(), &Utc);
        let expected = "\
Security checkup  ·  score 77 (needs attention)  ·  2 to fix, 1 not checked
Checked 2026-09-28 10:12 in 1.3 s  ·  read-only: nothing on this PC is changed

  ⚠ Important     Virus definitions                9 days old
                 Version: 1.459.440.0
                 • one line
  ⚠ Optional      Remote Assistance                Invitations are allowed
  ○ Not checked   Drive encryption                 Needs administrator rights to check
  ✓               Antivirus                        Microsoft Defender Antivirus is on
  ✓               Firewall                         On for every network type
  –               Remote Desktop                   Not available on Windows 11 Home

Note: Drive encryption can only be checked with administrator rights.
Could not read Microsoft Defender: WMI did not answer
";
        assert_eq!(text, expected);
    }

    #[test]
    fn a_clean_checkup_says_nothing_to_fix() {
        let mut checkup = small_checkup();
        checkup.checks.retain(|c| c.state == CheckState::Good);
        checkup.score = Score {
            value: 100,
            grade: Grade::Good,
            to_fix: 0,
            critical: 0,
            unknown: 0,
            checked: 2,
        };
        checkup.notes.clear();
        checkup.errors.clear();
        let text = checkup_text(&checkup, &Utc);
        assert!(text
            .starts_with("Security checkup  ·  score 100 (well protected)  ·  nothing to fix\n"));
        assert!(!text.contains("Note:"));
    }

    #[test]
    fn checkup_text_names_no_account_or_computer() {
        let raw = fixtures::raw();
        let checks = super::super::checkup::evaluate(
            &raw,
            &fixtures::ctx(),
            &fixtures::scan_done(),
            fixtures::now(),
        );
        let mut checkup = small_checkup();
        checkup.checks = checks;
        let text = checkup_text(&checkup, &Utc);
        let user = fixtures::user();
        assert!(!text.contains(&user.domain));
        assert!(!text.contains(&user.sid));
    }

    #[test]
    fn boot_text_lists_starts_shutdowns_and_what_slowed_them() {
        let text = boot_text(&sample_history(), &Utc);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0],
            "Boot history  ·  Last full start 49.0 s (desktop after 24.5 s)  ·  Typical 44.5 s over 10 starts"
        );
        assert_eq!(lines[1], "Start times are steady");
        assert_eq!(
            lines[2],
            "Fast Startup is on: Windows times only full starts (restarts), so most starts are not \
             listed."
        );
        assert!(text.contains(
            "\nSlows startup:\n  App      Discord.exe  ·  10 of the last 10 starts  ·  usually adds 3.0 s\n"
        ));
        assert!(text.contains(
            "\nSlows shutdown:\n  Service  Contoso Service  ·  3 of the last 3 shutdowns  ·  usually adds 0.8 s\n"
        ));
        assert!(text.contains(
            "\n  2026-09-10 08:00  ·  Full start  ·  49.0 s (desktop 24.5 s)  ·  12 startup apps\n"
        ));
        assert!(text.contains("\n  2026-09-03 23:00  ·  12.3 s  ·  ⚠ slower than usual\n"));
    }

    #[test]
    fn boot_text_explains_missing_access() {
        let mut history = sample_history();
        history.access = LogAccess::NeedsAdmin;
        assert!(boot_text(&history, &Utc).starts_with(
            "Windows keeps its startup records where only administrators can read them."
        ));
        history.access = LogAccess::LogMissing;
        assert_eq!(
            boot_text(&history, &Utc),
            "This copy of Windows does not keep startup performance records.\n"
        );
        history.access = LogAccess::LogDisabled;
        let text = boot_text(&history, &Utc);
        assert!(text.starts_with("Windows is not recording startup performance on this PC"));
        assert!(text.contains("Recent starts:"));
        history.unexpected_shutdowns = vec![at("2026-09-05T20:00:00Z"), at("2026-09-02T20:00:00Z")];
        assert!(boot_text(&history, &Utc).contains(
            "⚠ Windows was shut down unexpectedly 2 times (crash or power loss), last on 2026-09-05."
        ));
    }

    #[test]
    fn summaries_of_boot_histories() {
        let history = sample_history();
        assert_eq!(
            summary_text(&history),
            "Last full start 49.0 s (desktop after 24.5 s)  ·  Typical 44.5 s over 10 starts"
        );
        let mut empty = history.clone();
        empty.boots.clear();
        assert_eq!(summary_text(&empty), "No full starts are recorded yet.");
        assert_eq!(kind_text(SlowKind::Prefetch), "Prefetch");
    }
}
