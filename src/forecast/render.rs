//! Static terminal layout. Prediction and persisted history stay in the parent module.
use super::{Report, WEEK};
use comfy_table::{Cell, Color, ContentArrangement, Table, presets::NOTHING};
use crossterm::style::Stylize;

struct View {
    out: String,
    width: usize,
    color: bool,
}

impl View {
    fn text(&mut self, text: &str, tone: Color) {
        let mut table = table(self.width, NOTHING, false);
        table.add_row([text]);
        for column in table.column_iter_mut() {
            column.set_padding((0, 0));
        }
        for line in table.to_string().lines() {
            self.out.push_str("  ");
            self.out.push_str(&paint(line.trim_end(), tone, self.color));
            self.out.push('\n');
        }
    }

    fn blank(&mut self) {
        self.out.push('\n');
    }
    fn rule(&mut self) {
        self.text(&"─".repeat(self.width), Color::DarkGrey);
    }
    fn section(&mut self, title: &str) {
        self.blank();
        self.text(title, Color::Cyan);
        self.rule();
    }
    fn table(&mut self, table: Table) {
        for line in table.to_string().lines() {
            self.out.push_str("  ");
            self.out.push_str(line.trim_end());
            self.out.push('\n');
        }
    }
}

fn paint(text: &str, tone: Color, color: bool) -> String {
    if color {
        let color = match tone {
            Color::Cyan => crossterm::style::Color::Cyan,
            Color::DarkGrey => crossterm::style::Color::DarkGrey,
            Color::Yellow => crossterm::style::Color::Yellow,
            Color::Green => crossterm::style::Color::Green,
            Color::DarkGreen => crossterm::style::Color::DarkGreen,
            Color::Red => crossterm::style::Color::Red,
            _ => crossterm::style::Color::Reset,
        };
        format!("{}", text.with(color))
    } else {
        text.into()
    }
}

fn table(width: usize, preset: &str, color: bool) -> Table {
    let mut table = Table::new();
    table
        .load_preset(preset)
        .force_no_tty()
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_width(width as u16);
    if color {
        table.enforce_styling();
    }
    table
}

fn date(at: i64, format: &str) -> String {
    chrono::DateTime::from_timestamp(at, 0)
        .map(|d| d.with_timezone(&chrono::Local).format(format).to_string())
        .unwrap_or_else(|| "unknown time".into())
}

fn duration(seconds: i64) -> String {
    let minutes = (seconds + 59) / 60;
    let mut parts = Vec::new();
    if minutes >= 1440 {
        parts.push(format!("{}d", minutes / 1440));
    }
    if minutes % 1440 >= 60 {
        parts.push(format!("{}h", minutes % 1440 / 60));
    }
    if minutes % 60 > 0 || parts.is_empty() {
        parts.push(format!("{}m", minutes % 60));
    }
    parts.join(" ")
}

fn minimum(intervals: &[(i64, i64, usize)], start: i64, end: i64) -> Option<usize> {
    intervals
        .iter()
        .filter(|(a, b, _)| *a < end && *b > start)
        .map(|(_, _, n)| *n)
        .min()
}

/// Compact default: hourly minima retain even one-second gaps.
pub(super) fn compact(report: &Report, now: i64, width: usize, color: bool) -> String {
    let mut view = View {
        out: String::new(),
        width: width.saturating_sub(4).clamp(12, 112),
        color,
    };
    let intervals = report.availability(now);
    let outages = report.outages(now);
    let total: i64 = outages.iter().map(|(a, b)| b - a).sum();
    let banked: usize = report
        .projections
        .iter()
        .map(|p| p.reset_expiries.len())
        .sum();
    let incomplete = report.excluded > 0
        || report.samples.len() != report.projections.len()
        || report.projections.iter().any(|p| {
            p.sample
                .windows
                .iter()
                .zip(&p.rates)
                .any(|(w, r)| w.seconds == WEEK && *r == 0.0)
        });
    let low_confidence = incomplete
        || report.history_span < 2 * 86400
        || report.reset_inventory_unknown > 0
        || banked > 0
        || report.projections.iter().any(|p| p.provisional);
    view.blank();
    view.text("Account availability  /  next 7 days", Color::Cyan);
    view.text(
        &format!(
            "{} → {} · local time",
            date(now, "%a %d %b %H:%M"),
            date(now + WEEK, "%a %d %b %H:%M")
        ),
        Color::DarkGrey,
    );
    let summary = match outages.first() {
        Some((start, _)) => format!(
            "First gap {} · Total gaps {}",
            date(*start, "%a %d %H:%M"),
            duration(total)
        ),
        None if intervals.is_empty() => "First gap unknown · Total gaps unknown".into(),
        None => "No gaps predicted in the next 7 days".into(),
    };
    view.text(&summary, if total > 0 { Color::Red } else { Color::Reset });
    view.text(
        if low_confidence || intervals.is_empty() {
            "Confidence: low / provisional scenario"
        } else {
            "Confidence: limited / past demand may change"
        },
        Color::Yellow,
    );
    view.blank();
    heatmap(&mut view, &intervals, now, incomplete);
    view.blank();
    view.text(
        &format!(
            "Banked resets: {banked} available · Auto-use assumed · History: {}",
            duration(report.history_span)
        ),
        Color::Reset,
    );
    if banked > 0 {
        view.text(
            &format!(
                "Without banked resets: {} total gaps. Simulation only.",
                duration(report.gap_seconds_without_banked_resets(now))
            ),
            Color::DarkGrey,
        );
    }
    if intervals.is_empty() {
        view.text("Collecting history. No usable forecast yet.", Color::Yellow);
    }
    if incomplete {
        view.text(
            "Coverage incomplete: ? marks unknown capacity; × marks a gap in known accounts.",
            Color::Yellow,
        );
    }
    if report.reset_inventory_unknown > 0 {
        view.text(
            "Reset inventory incomplete; unverified credits excluded.",
            Color::Yellow,
        );
    }
    if report.projections.iter().any(|p| p.unknown_short_pace) {
        view.text(
            "Short-window pace unknown for some accounts.",
            Color::Yellow,
        );
    }
    if let Some(error) = &report.history_error {
        view.text(&format!("History unavailable: {error}"), Color::Yellow);
    }
    if !outages.is_empty() {
        view.blank();
        for (a, b) in outages.iter().take(3) {
            view.text(
                &format!(
                    "Gap {} → {} ({}){}",
                    date(*a, "%a %d %H:%M:%S"),
                    date(*b, "%a %d %H:%M:%S"),
                    duration(b - a),
                    if *b == now + WEEK {
                        " [horizon ends]"
                    } else {
                        ""
                    }
                ),
                Color::Red,
            );
        }
        if outages.len() > 3 {
            view.text(
                &format!(
                    "{} more gaps; use --details for daily totals.",
                    outages.len() - 3
                ),
                Color::DarkGrey,
            );
        }
    }
    view.blank();
    view.text(
        "Assumes steady demand and balanced routing. Details: codexctl forecast --details",
        Color::DarkGrey,
    );
    view.text(
        report
            .sampling_status
            .as_deref()
            .unwrap_or("Sampling status unknown."),
        Color::DarkGrey,
    );
    view.blank();
    view.out
}

fn heatmap(view: &mut View, intervals: &[(i64, i64, usize)], now: i64, incomplete: bool) {
    // Split hours into bands on narrow terminals; never collapse hourly data.
    let hours = if view.width >= 64 {
        24
    } else if view.width >= 40 {
        12
    } else {
        6
    };
    view.text(
        "Each square = 1 hour; columns = hours after row start.",
        Color::DarkGrey,
    );
    for band in (0..24).step_by(hours) {
        view.blank();
        let stacked = view.width < 40;
        let indent = if stacked { 0 } else { 15 };
        let mut header = " ".repeat(indent);
        for h in band..band + hours {
            header.push_str(&if h % 3 == 0 {
                format!("{h:02}")
            } else {
                "  ".into()
            });
        }
        view.text(header.trim_end(), Color::DarkGrey);
        for day in 0..7 {
            let start = now + day * 86400;
            let label = date(start, "%a %d %H:%M");
            if stacked {
                view.text(&label, Color::DarkGrey);
            }
            let mut line = if stacked {
                String::new()
            } else {
                format!("{label:<15}")
            };
            for h in band..band + hours {
                let a = start + h as i64 * 3600;
                let count = minimum(intervals, a, a + 3600);
                let (glyph, tone) = match count {
                    Some(0) => ("×", Color::Red),
                    _ if incomplete || count.is_none() => ("?", Color::DarkGrey),
                    Some(1) => ("▪", Color::Yellow),
                    Some(2..=3) => ("■", Color::DarkGreen),
                    Some(_) => ("◆", Color::Green),
                    None => ("?", Color::DarkGrey),
                };
                line.push_str(&paint(glyph, tone, view.color));
                line.push(' ');
            }
            view.out.push_str("  ");
            view.out.push_str(line.trim_end());
            view.out.push('\n');
        }
    }
    let mut legend_width = 0;
    view.out.push_str("  ");
    for (label, tone) in [
        ("× gap", Color::Red),
        ("▪ 1 account", Color::Yellow),
        ("■ 2-3", Color::DarkGreen),
        ("◆ 4+", Color::Green),
        ("? unknown", Color::DarkGrey),
    ] {
        let width = label.chars().count();
        if legend_width > 0 {
            if legend_width + 2 + width > view.width {
                view.out.push_str("\n  ");
                legend_width = 0;
            } else {
                view.out.push_str("  ");
                legend_width += 2;
            }
        }
        view.out.push_str(&paint(label, tone, view.color));
        legend_width += width;
    }
    view.out.push('\n');
    view.text(
        "Lowest count in each hour. Even a brief gap gets ×.",
        Color::DarkGrey,
    );
}

pub(super) fn render(report: &Report, now: i64, width: usize, color: bool) -> String {
    let mut view = View {
        out: String::new(),
        width: width.saturating_sub(4).clamp(12, 112),
        color,
    };
    let intervals = report.availability(now);
    let low = minimum(&intervals, now, now + WEEK);
    let outages = report.outages(now);
    let unknown_weekly = report
        .projections
        .iter()
        .filter(|p| {
            p.sample
                .windows
                .iter()
                .zip(&p.rates)
                .any(|(w, rate)| w.seconds == WEEK && *rate == 0.0)
        })
        .count();
    let unknown =
        report.excluded + report.samples.len() - report.projections.len() + unknown_weekly;
    let banked: usize = report
        .projections
        .iter()
        .map(|p| p.reset_expiries.len())
        .sum();
    let provisional = report.projections.iter().any(|p| p.provisional)
        || banked > 0
        || report.reset_inventory_unknown > 0;
    let current = report
        .samples
        .iter()
        .filter(|s| s.windows.iter().all(|w| w.used < 100.0))
        .count();
    let measured = report
        .projections
        .iter()
        .filter(|p| p.weekly_from_history)
        .count();

    view.blank();
    view.text("CODEX / QUOTA FORECAST", Color::Cyan);
    view.text(
        &format!(
            "{} → {} · local time",
            date(now, "%a %d %b %H:%M"),
            date(now + WEEK, "%a %d %b %H:%M")
        ),
        Color::DarkGrey,
    );
    view.rule();
    view.blank();
    view.text("AT LEAST ONE ACCOUNT ALL WEEK?", Color::Reset);
    view.text(
        &format!("RESETS ASSUMED ON / {banked} verified banked credits modeled"),
        Color::Cyan,
    );
    match (low, outages.first()) {
        (None, _) => {
            view.text("? COLLECTING HISTORY", Color::Yellow);
            view.text(
                "Collecting history. There is no usable forecast yet.",
                Color::Reset,
            );
        }
        (_, Some((start, end))) => {
            view.text("! RISK / GAP IN THE CURRENT MODEL", Color::Red);
            view.text(
                &format!(
                    "{} → {}{}",
                    date(*start, "%a %d %b %H:%M"),
                    date(*end, "%a %d %b %H:%M"),
                    if *end == now + WEEK {
                        " (forecast ends)"
                    } else {
                        ""
                    }
                ),
                Color::Reset,
            );
            let total: i64 = outages.iter().map(|(a, b)| b - a).sum();
            let longest = outages.iter().map(|(a, b)| b - a).max().unwrap_or(0);
            view.text(
                &format!(
                    "Total gaps this week: {} · Longest: {}",
                    duration(total),
                    duration(longest)
                ),
                Color::Red,
            );
            let length = duration(end - start);
            view.text(
                &format!("First gap: {length} with zero modeled accounts available."),
                Color::Reset,
            );
        }
        (Some(count), _) => {
            if unknown > 0 || provisional {
                view.text("~ LIKELY COVERED / EARLY ESTIMATE", Color::Yellow);
            } else {
                view.text("+ COVERED IN THE CURRENT MODEL", Color::Green);
            }
            view.text(
                &format!("Model minimum: {count} available throughout the next seven days."),
                Color::Reset,
            );
        }
    }
    if banked > 0 && !intervals.is_empty() {
        let with: i64 = outages.iter().map(|(a, b)| b - a).sum();
        view.text(
            &format!(
                "Total gaps: {} without banked resets → {} with resets",
                duration(report.gap_seconds_without_banked_resets(now)),
                duration(with)
            ),
            Color::Cyan,
        );
        view.text("Assumes immediate use at exhaustion: clears exhausted windows and restarts their clocks. No resets are redeemed.", Color::DarkGrey);
    }
    if report.reset_inventory_unknown > 0 {
        view.text(
            &format!(
                "Reset inventory incomplete for {} accounts; unverified credits excluded.",
                report.reset_inventory_unknown
            ),
            Color::Yellow,
        );
    }
    if unknown > 0 {
        view.text(
            &format!("Coverage incomplete: {unknown} accounts need data or a usable rate."),
            Color::Yellow,
        );
    }
    if provisional {
        view.text(
            "Confidence: low. PROVISIONAL rates or reset assumptions apply.",
            Color::Yellow,
        );
    }
    if !provisional {
        view.text(
            if unknown > 0 || report.history_span < 2 * 86400 {
                "Confidence: low. Short or incomplete usage history."
            } else {
                "Confidence: limited. Past demand may change."
            },
            Color::Yellow,
        );
    }
    view.text(
        "Shared workload: demand moves to usable accounts with matching plans and windows.",
        Color::DarkGrey,
    );
    view.blank();
    view.text(
        &format!(
            "NOW {current}/{} usable   WEEK LOW {}   HISTORY {}",
            report.samples.len(),
            low.map_or("?".into(), |n| n.to_string()),
            duration(report.history_span)
        ),
        Color::Cyan,
    );
    view.text(
        &format!(
            "{} of {} accounts modeled · {measured} measured weekly rates · {} observations",
            report.projections.len(),
            report.samples.len(),
            report.history_samples
        ),
        Color::DarkGrey,
    );
    if let Some(error) = &report.history_error {
        view.text(&format!("History unavailable: {error}"), Color::Yellow);
    }

    if !intervals.is_empty() {
        view.section("DAILY COVERAGE / next seven 24-hour periods");
        chart(&mut view, &intervals, now);
    }

    if !report.samples.is_empty() {
        view.section("ACCOUNTS / rolling 7-day allowance");
        accounts(&mut view, report);
    }
    view.blank();
    view.rule();
    if report.projections.iter().any(|p| p.unknown_short_pace) {
        view.text(
            "Short-window pace unknown for some accounts; no further short-window use assumed.",
            Color::Yellow,
        );
    }
    if unknown_weekly > 0 {
        view.text(
            "Pace after reset: unknown for some seats. Their own demand is excluded; they can receive shared work.",
            Color::Yellow,
        );
    }
    if report.usage_based > 0 {
        view.text(
            &format!(
                "{} paid-credit seats excluded by design.",
                report.usage_based
            ),
            Color::DarkGrey,
        );
    }
    if report.duplicates > 0 {
        view.text(
            &format!("{} duplicate aliases counted once.", report.duplicates),
            Color::DarkGrey,
        );
    }
    view.text("Scenario: equal capacity within each group, steady demand, balanced routing. No transfer across groups; no backlog after a gap.", Color::DarkGrey);
    view.text(
        "Short windows and verified banked resets count. Paid credits and model-specific limits excluded.",
        Color::DarkGrey,
    );
    view.text(
        report
            .sampling_status
            .as_deref()
            .unwrap_or("Sampling status unknown. Run status to save an observation."),
        Color::DarkGrey,
    );
    view.blank();
    view.out
}

fn chart(view: &mut View, intervals: &[(i64, i64, usize)], now: i64) {
    let day = WEEK / 7;
    view.text(
        &format!("24h periods from {} local time.", date(now, "%a %d %H:%M")),
        Color::DarkGrey,
    );
    let wide = view.width >= 64;
    let mut t = table(view.width, NOTHING, view.color);
    if wide {
        t.set_header([
            "Period starts",
            "Time with an account",
            "Fewest available",
            "Gap time",
        ]);
    } else {
        t.set_header(["Period starts", "Fewest available", "Gap time"]);
    }
    for i in 0..7 {
        let start = now + i * day;
        let end = start + day;
        let low = minimum(intervals, start, end).unwrap();
        let gap: i64 = intervals
            .iter()
            .filter(|(_, _, n)| *n == 0)
            .map(|(a, b, _)| (end.min(*b) - start.max(*a)).max(0))
            .sum();
        let coverage = 100.0 * (day - gap) as f64 / day as f64;
        let tone = if gap > 0 { Color::Yellow } else { Color::Green };
        let mut row = vec![Cell::new(date(start, "%a %d %H:%M"))];
        if wide {
            // Any nonzero gap leaves a visible break, even below display precision.
            let filled = ((day - gap) as f64 / day as f64 * 16.0).floor() as usize;
            let percent = if gap > 0 && coverage >= 99.95 {
                "<100".into()
            } else {
                format!("{coverage:.1}")
            };
            row.push(
                Cell::new(format!(
                    "{}{} {percent}%",
                    "━".repeat(filled),
                    "·".repeat(16 - filled)
                ))
                .fg(tone),
            );
        }
        row.push(Cell::new(low).fg(if low == 0 { Color::Red } else { Color::Cyan }));
        row.push(
            Cell::new(if gap == 0 {
                "None".into()
            } else {
                duration(gap)
            })
            .fg(tone),
        );
        t.add_row(row);
    }
    view.table(t);
    view.text(
        "Gap time = total time with zero modeled accounts, including short limits.",
        Color::DarkGrey,
    );
}

fn accounts(view: &mut View, report: &Report) {
    let mut accounts: Vec<_> = report.samples.iter().collect();
    accounts.sort_by(|a, b| {
        let left = a.windows.iter().find(|w| w.seconds == WEEK).unwrap();
        let right = b.windows.iter().find(|w| w.seconds == WEEK).unwrap();
        right
            .used
            .total_cmp(&left.used)
            .then_with(|| a.alias.cmp(&b.alias))
    });
    let tabular = view.width >= 74
        && accounts
            .iter()
            .all(|s| s.alias.is_ascii() && s.alias.len() <= view.width.saturating_sub(48));
    let mut t = table(view.width, NOTHING, view.color);
    t.set_header(
        ["Account", "Weekly left", "Now", "Weekly reset"].map(|s| Cell::new(s).fg(Color::Cyan)),
    );
    for sample in accounts {
        let weekly = sample.windows.iter().find(|w| w.seconds == WEEK).unwrap();
        let left = 100.0 - weekly.used;
        let tone = if left <= 0.0 {
            Color::Red
        } else if left <= 20.0 {
            Color::Yellow
        } else {
            Color::Green
        };
        let filled = ((left / 100.0 * 8.0).ceil() as usize).min(8);
        let meter = format!(
            "{}{} {left:.1}%",
            "━".repeat(filled),
            "─".repeat(8 - filled)
        );
        let empty = if left <= 0.0 {
            "Empty now"
        } else if sample.windows.iter().any(|w| w.used >= 100.0) {
            "Short limit"
        } else {
            "Ready"
        };
        let reset = date(weekly.reset, "%a %d %H:%M");
        if tabular {
            t.add_row([
                Cell::new(&sample.alias),
                Cell::new(meter).fg(tone),
                Cell::new(empty),
                Cell::new(reset),
            ]);
        } else {
            view.text(&sample.alias, Color::Reset);
            view.text(&format!("{meter} left"), tone);
            view.text(&format!("{empty} · Reset: {reset}"), Color::DarkGrey);
            view.blank();
        }
    }
    if tabular {
        view.table(t);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forecast::{Sample, Window};
    const NOW: i64 = 1_800_000_000;

    fn fixture() -> Report {
        let sample = Sample {
            seat: ("workspace".into(), "seat".into()),
            login_uid: None,
            alias: "very-long-account-name+personal@example.test".into(),
            plan: "pro".into(),
            at: NOW,
            windows: vec![Window {
                seconds: WEEK,
                used: 10.0,
                reset: NOW + 86400,
            }],
        };
        let mut old = sample.clone();
        old.at -= 86400;
        old.windows[0].used = 5.0;
        Report::build(vec![sample], &[old], 0)
    }

    #[test]
    fn heatmap_legend_matches_cell_colors_and_plain_output() {
        for color in [false, true] {
            let mut view = View {
                out: String::new(),
                width: 76,
                color,
            };
            heatmap(&mut view, &[(NOW, NOW + WEEK, 2)], NOW, false);
            for (label, tone) in [
                ("× gap", Color::Red),
                ("▪ 1 account", Color::Yellow),
                ("■ 2-3", Color::DarkGreen),
                ("◆ 4+", Color::Green),
                ("? unknown", Color::DarkGrey),
            ] {
                assert!(view.out.contains(&paint(label, tone, color)));
            }
            assert_eq!(view.out.contains('\u{1b}'), color);
        }
    }

    #[test]
    fn compact_heatmap_keeps_short_gaps_unknowns_and_narrow_layout() {
        let intervals = [(NOW, NOW + 1, 0), (NOW + 1, NOW + WEEK, 2)];
        for width in [24, 40, 60, 80, 120] {
            let mut view = View {
                out: String::new(),
                width: width - 4,
                color: false,
            };
            heatmap(&mut view, &intervals, NOW, false);
            assert!(view.out.contains('×'));
            assert_eq!(view.out.matches('■').count(), 168); // 167 cells plus legend
            assert!(view.out.lines().all(|l| l.chars().count() <= width));
            let output = compact(&fixture(), NOW, width, false);
            assert!(output.lines().all(|l| l.chars().count() <= width));
            assert!(!output.contains("Weekly left"));
        }
        let mut view = View {
            out: String::new(),
            width: 76,
            color: false,
        };
        heatmap(&mut view, &intervals, NOW, true);
        assert!(!view.out.contains("■ ■"));
        assert!(view.out.contains("? ?"));
        let empty = compact(&Report::default(), NOW, 80, false);
        assert!(empty.contains("Total gaps unknown"));
    }

    #[test]
    fn layout_fits_narrow_and_wide_terminals_without_losing_identity() {
        for width in [24, 40, 48, 51, 52, 53, 54, 55, 56, 57, 58, 60, 80, 120] {
            let output = fixture().render(NOW, width);
            for line in output.lines() {
                assert!(line.chars().count() <= width, "width {width}: {line:?}");
            }
            let compact: String = output.chars().filter(|c| !c.is_whitespace()).collect();
            assert!(
                compact.contains("very-long-account-name+personal@example.test"),
                "width {width}: {output}"
            );
            assert!(compact.contains("WEEKLOW1"));
            assert!(!output.contains('\u{1b}'));
        }
    }

    #[test]
    fn qualified_answer_distinguishes_history_and_missing_coverage() {
        let mut report = fixture();
        let ready = report.render(NOW, 80);
        assert!(ready.contains("COVERED IN THE CURRENT MODEL"));
        assert!(ready.contains("WEEK LOW 1"));
        assert!(ready.contains("Shared workload:"));
        report.excluded = 1;
        let partial = report.render(NOW, 80);
        assert!(partial.contains("EARLY ESTIMATE"));
        assert!(partial.contains("Coverage incomplete"));
        assert!(!partial.contains("COVERED IN THE CURRENT MODEL"));
        report.excluded = 0;
        report.projections[0].provisional = true;
        assert!(report.render(NOW, 80).contains("PROVISIONAL"));
        assert!(
            !report
                .render(NOW, 80)
                .contains("COVERED IN THE CURRENT MODEL")
        );
    }

    #[test]
    fn interval_minimum_keeps_a_short_outage_and_excludes_the_right_endpoint() {
        let intervals = [
            (NOW, NOW + 100, 2),
            (NOW + 100, NOW + 101, 0),
            (NOW + 101, NOW + WEEK, 2),
        ];
        assert_eq!(minimum(&intervals, NOW, NOW + 86400), Some(0));
        assert_eq!(minimum(&intervals, NOW, NOW + 100), Some(2));
        assert_eq!(minimum(&intervals, NOW + 101, NOW + 86400), Some(2));
    }

    #[test]
    fn zero_history_is_not_a_prediction_of_zero_accounts() {
        let output = Report::default().render(NOW, 80);
        assert!(output.contains("COLLECTING HISTORY"));
        assert!(output.contains("WEEK LOW ?"));
        assert!(!output.contains("RISK"));
    }

    #[test]
    fn larger_fleets_keep_exact_daily_counts() {
        let base = fixture();
        let samples = (0..12)
            .map(|i| {
                let mut sample = base.samples[0].clone();
                sample.seat.1 = format!("seat-{i}");
                sample.alias = format!("account-{i}");
                sample
            })
            .collect();
        let report = Report::build(samples, &[], 0);
        let output = report.render(NOW, 80);
        assert!(output.contains("WEEK LOW 12"));
        assert!(output.contains("Fewest available"));
    }

    #[test]
    fn headline_exposes_total_risk_and_period_rows_include_start_times() {
        let mut report = fixture();
        report.samples[0].windows[0].used = 100.0;
        report.projections[0].sample.windows[0].used = 100.0;
        let output = report.render(NOW, 120);
        assert!(output.contains("Total gaps this week: 1d"));
        assert!(output.contains("Longest: 1d"));
        assert!(output.contains(&date(NOW + 3 * 86400, "%a %d %H:%M")));
    }

    #[test]
    fn one_second_gap_stays_visible_in_coverage_bar() {
        let mut view = View {
            out: String::new(),
            width: 112,
            color: false,
        };
        chart(
            &mut view,
            &[(NOW, NOW + 1, 0), (NOW + 1, NOW + WEEK, 1)],
            NOW,
        );
        assert!(view.out.contains("<100%"));
        assert!(view.out.contains("1m"));
    }

    #[test]
    fn gap_lengths_use_readable_units() {
        assert_eq!(duration(1), "1m");
        assert_eq!(duration(65 * 60), "1h 5m");
        assert_eq!(duration(WEEK - 60), "6d 23h 59m");
    }

    #[test]
    fn weekly_reset_dates_and_period_anchor_are_explicit() {
        let mut report = fixture();
        let reset = NOW + WEEK - 60;
        report.samples[0].windows[0].reset = reset;
        let output = report.render(NOW, 120);
        assert!(output.contains(&date(reset, "%a %d %H:%M")));
        assert!(output.contains(&format!(
            "24h periods from {} local time.",
            date(NOW, "%a %d %H:%M")
        )));
        for width in 51..=57 {
            let output = report.render(NOW, width);
            assert!(output.contains("Fewest available"));
        }
    }

    #[test]
    fn wide_names_use_stacked_layout() {
        let mut report = fixture();
        report.samples[0].alias = "測試賬戶@example.test".into();
        let output = report.render(NOW, 80);
        assert!(output.contains("測試賬戶@example.test"));
        assert!(output.contains("Reset:"));
    }

    #[test]
    fn color_changes_only_presentation() {
        let mut report = fixture();
        report.samples[0].alias = "primary@example.test".into();
        let plain = report.render(NOW, 80);
        let colored = report.render_terminal(NOW, 80, true);
        assert!(colored.contains('\u{1b}'));
        let terminal = |text: &str| {
            let mut parser = vt100::Parser::new(200, 80, 0);
            parser.process(text.replace('\n', "\r\n").as_bytes());
            parser
                .screen()
                .contents()
                .lines()
                .map(str::trim_end)
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert_eq!(terminal(&plain), terminal(&colored));
    }
}
