//! Display formatting shared by connected and unconnected status tables.

/// Display signed percentage points. Ahead of pace carries a warning color.
pub fn pace_cell(points: Option<f64>) -> comfy_table::Cell {
    let Some(points) = points else {
        return comfy_table::Cell::new("-");
    };
    let rounded = points.round();
    if points > 0.0 {
        comfy_table::Cell::new(format!("+{rounded:.0} ahead")).fg(comfy_table::Color::Yellow)
    } else if points < 0.0 {
        comfy_table::Cell::new(format!("{rounded:.0} behind"))
    } else {
        comfy_table::Cell::new("0 on pace")
    }
}

/// Format an OpenAI credit balance for display, without changing its raw value.
pub fn format_credit_balance(balance: &str) -> String {
    let display = match balance.parse::<f64>() {
        Ok(value) if value.is_finite() => {
            let number = format!("{value:.2}");
            let (integer, fraction) = number.split_once('.').unwrap_or((&number, "00"));
            let mut grouped = String::new();
            for (index, digit) in integer.chars().rev().enumerate() {
                if index > 0 && index.is_multiple_of(3) && digit.is_ascii_digit() {
                    grouped.push(',');
                }
                grouped.push(digit);
            }
            format!("{}.{fraction}", grouped.chars().rev().collect::<String>())
        }
        _ => balance.to_string(),
    };
    let display: String = display
        .chars()
        .filter(|c| !c.is_control())
        .take(80)
        .collect();
    format!("{display} credits")
}

pub fn format_window_reset(reset: Option<i64>) -> String {
    match reset.filter(|&ts| chrono::DateTime::from_timestamp(ts, 0).is_some()) {
        Some(reset_ts) => {
            let now = chrono::Utc::now().timestamp();
            let diff_secs = reset_ts - now;
            if diff_secs <= 0 {
                "now".to_string()
            } else if diff_secs >= 86400 {
                format!(
                    "in {} ({})",
                    format_duration(diff_secs),
                    format_reset_timestamp(reset_ts)
                )
            } else {
                format!("in {}", format_duration(diff_secs))
            }
        }
        None => "-".to_string(),
    }
}

fn format_reset_timestamp(reset_ts: i64) -> String {
    chrono::DateTime::from_timestamp(reset_ts, 0)
        .map(|dt| {
            let local = dt.with_timezone(&chrono::Local);
            local.format("%a %b %d %H:%M").to_string()
        })
        .unwrap_or_else(|| "-".to_string())
}

pub fn format_duration(secs: i64) -> String {
    let days = secs / 86400;
    let hours = (secs % 86400) / 3600;
    let minutes = (secs % 3600) / 60;

    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes:02}m")
    } else {
        format!("{minutes}m")
    }
}
