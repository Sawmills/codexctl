//! Display formatting shared by connected and unconnected status tables.

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
