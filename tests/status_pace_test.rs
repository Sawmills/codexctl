use codexctl::status_pace::Pace;
use codexctl::{profile, status_json::AccountStatus};

#[test]
fn weekly_pace_starts_at_zero_elapsed() {
    let pace = Pace::weekly(Some(12.0), Some(604800), Some(1604800), true, 1000000).unwrap();
    assert_eq!((pace.elapsed_percent, pace.points), (0.0, 12.0));
}

#[test]
fn weekly_pace_at_midpoint_is_used_minus_fifty_points() {
    let pace = Pace::weekly(Some(37.0), Some(604800), Some(1302400), true, 1000000).unwrap();
    assert_eq!((pace.elapsed_percent, pace.points), (50.0, -13.0));
}

#[test]
fn weekly_pace_at_reset_has_one_hundred_percent_elapsed() {
    let pace = Pace::weekly(Some(92.0), Some(604800), Some(1000000), true, 1000000).unwrap();
    assert_eq!((pace.elapsed_percent, pace.points), (100.0, -8.0));
}

#[test]
fn weekly_pace_is_unknown_for_stale_or_failed_usage() {
    assert_eq!(
        Pace::weekly(Some(37.0), Some(604800), Some(1302400), false, 1000000),
        None
    );
}

#[test]
fn weekly_pace_does_not_guess_missing_data_or_non_weekly_windows() {
    for (used, seconds, reset) in [
        (None, Some(604800), Some(1302400)),
        (Some(37.0), None, Some(1302400)),
        (Some(37.0), Some(604800), None),
        (Some(37.0), Some(0), Some(1302400)),
        (Some(37.0), Some(86400), Some(1302400)),
        (Some(37.0), Some(18000), Some(1302400)),
    ] {
        assert_eq!(Pace::weekly(used, seconds, reset, true, 1000000), None);
    }
}

#[test]
fn weekly_pace_rejects_expired_or_invalid_observations() {
    for (used, reset) in [
        (37.0, 999999),
        (f64::NAN, 1302400),
        (f64::INFINITY, 1302400),
        (-1.0, 1302400),
        (101.0, 1302400),
    ] {
        assert_eq!(
            Pace::weekly(Some(used), Some(604800), Some(reset), true, 1000000),
            None
        );
    }
}

#[test]
fn status_json_reports_weekly_pace_and_clears_it_when_usage_fails() {
    let usage = serde_json::from_value(serde_json::json!({
        "rate_limit": {"primary_window": {
            "used_percent": 37, "window_minutes": 10080, "reset_at": 1302400
        }}
    }))
    .unwrap();
    let mut row = AccountStatus::local(&profile::Meta::default(), false);
    row.set_usage(&usage);
    row.set_pace_at(1000000);
    let json = serde_json::to_value(&row).unwrap();
    assert_eq!(json["pace_points"], -13.0);
    assert_eq!(json["elapsed_percent"], 50.0);
    row.error = Some("usage unavailable".into());
    row.set_pace_at(1000000);
    let json = serde_json::to_value(&row).unwrap();
    assert_eq!(json.get("pace_points"), Some(&serde_json::Value::Null));
    assert_eq!(json.get("elapsed_percent"), Some(&serde_json::Value::Null));
}

#[test]
fn status_json_stale_usage_retains_usage_but_has_no_pace() {
    let mut row = AccountStatus::local(&profile::Meta::default(), false);
    row.secondary_used_percent = Some(37.0);
    row.secondary_window_seconds = Some(604800);
    row.secondary_resets_at = codexctl::status_json::timestamp(Some(1302400));
    row.usage_stale = Some(true);
    row.set_pace_at(1000000);
    let json = serde_json::to_value(row).unwrap();
    assert_eq!(json["secondary_used_percent"], 37.0);
    assert_eq!(json.get("pace_points"), Some(&serde_json::Value::Null));
    assert_eq!(json.get("elapsed_percent"), Some(&serde_json::Value::Null));
}

#[test]
fn fleet_pace_is_an_equal_weight_mean_of_known_accounts() {
    assert_eq!(
        codexctl::status_pace::fleet_points([Some(12.0), None, Some(-38.0)].into_iter()),
        Some(-13.0)
    );
    assert_eq!(
        codexctl::status_pace::fleet_points([None, None].into_iter()),
        None
    );
}

#[test]
fn rounded_pace_keeps_the_direction_of_small_nonzero_values() {
    assert_eq!(
        codexctl::status_format::pace_cell(Some(0.25)).content(),
        "+0 ahead"
    );
    assert_eq!(
        codexctl::status_format::pace_cell(Some(-0.25)).content(),
        "-0 behind"
    );
    assert_eq!(
        codexctl::status_format::pace_cell(Some(0.0)).content(),
        "0 on pace"
    );
}
