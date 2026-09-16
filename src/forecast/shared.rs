//! Constant demand, reallocated to usable seats within a comparable quota group.
//! This is an equal-capacity scenario, not a conversion from percentages to tokens.
use super::{Projection, WEEK};

fn comparable(a: &Projection, b: &Projection) -> bool {
    a.sample.plan == b.sample.plan
        && a.sample.windows.len() == b.sample.windows.len()
        && a.sample
            .windows
            .iter()
            .zip(&b.sample.windows)
            .all(|(a, b)| a.seconds == b.seconds)
}

pub(super) fn availability(projections: &[Projection], now: i64) -> Vec<(i64, i64, usize)> {
    if projections.is_empty() {
        return Vec::new();
    }
    let mut groups: Vec<Vec<&Projection>> = Vec::new();
    for p in projections {
        if let Some(group) = groups.iter_mut().find(|g| comparable(g[0], p)) {
            group.push(p);
        } else {
            groups.push(vec![p]);
        }
    }
    let timelines: Vec<_> = groups.iter().map(|g| simulate(g, now)).collect();
    let mut boundaries: Vec<_> = timelines
        .iter()
        .flatten()
        .flat_map(|(a, b, _)| [*a, *b])
        .collect();
    boundaries.sort_unstable();
    boundaries.dedup();
    boundaries
        .windows(2)
        .map(|pair| {
            let count = timelines
                .iter()
                .map(|t| {
                    let i = t.partition_point(|(_, b, _)| *b <= pair[0]);
                    t.get(i).map_or(0, |(_, _, n)| *n)
                })
                .sum();
            (pair[0], pair[1], count)
        })
        .collect()
}

fn simulate(group: &[&Projection], now: i64) -> Vec<(i64, i64, usize)> {
    let weekly = group[0]
        .sample
        .windows
        .iter()
        .position(|w| w.seconds == WEEK)
        .unwrap();
    // Total demand does not disappear when a source account is blocked.
    let demand: f64 = group.iter().map(|p| p.rates[weekly]).sum();
    let mut used: Vec<Vec<f64>> = group.iter().map(|p| p.used_at(now)).collect();
    let mut resets: Vec<Vec<i64>> = group
        .iter()
        .map(|p| {
            p.sample
                .windows
                .iter()
                .map(|w| {
                    if w.reset > now {
                        w.reset
                    } else {
                        w.reset + ((now - w.reset) / w.seconds + 1) * w.seconds
                    }
                })
                .collect()
        })
        .collect();
    let mut credits: Vec<_> = group
        .iter()
        .map(|p| {
            let mut expiries = p.reset_expiries.clone();
            expiries.sort_unstable();
            expiries
        })
        .collect();
    let end = now + WEEK;
    let mut at = now;
    let mut out = Vec::new();
    while at < end {
        for i in 0..group.len() {
            credits[i].retain(|expiry| *expiry > at);
            if used[i].iter().any(|u| *u >= 100.0 - 1e-9) && !credits[i].is_empty() {
                credits[i].remove(0);
                // Forecast assumption: one credit clears all exhausted main
                // windows and restarts their clocks. Other windows stay intact.
                for j in 0..used[i].len() {
                    if used[i][j] >= 100.0 - 1e-9 {
                        used[i][j] = 0.0;
                        resets[i][j] = at + group[i].sample.windows[j].seconds;
                    }
                }
            }
        }
        let active: Vec<_> = used
            .iter()
            .map(|windows| windows.iter().all(|u| *u < 100.0 - 1e-9))
            .collect();
        let count = active.iter().filter(|a| **a).count();
        // Balance demand equally between available seats. Each seat retains its
        // observed ratio of short-window burn to weekly burn.
        let weekly_rate = if count > 0 {
            demand / count as f64
        } else {
            0.0
        };
        let rates: Vec<Vec<f64>> = group
            .iter()
            .enumerate()
            .map(|(i, p)| {
                p.rates
                    .iter()
                    .enumerate()
                    .map(|(j, r)| {
                        if !active[i] {
                            0.0
                        } else if j == weekly {
                            weekly_rate
                        } else if p.rates[weekly] > 0.0 {
                            weekly_rate * r / p.rates[weekly]
                        } else {
                            0.0
                        }
                    })
                    .collect()
            })
            .collect();
        let mut next = end;
        for i in 0..group.len() {
            for j in 0..used[i].len() {
                next = next.min(resets[i][j]);
                if rates[i][j] > 0.0 {
                    // Bound the float before integer conversion to avoid overflow
                    // for very small observed rates. Resolution is one second.
                    let delay = ((100.0 - used[i][j]) / rates[i][j]).ceil().max(1.0);
                    next = next.min(at + delay.min((end - at) as f64) as i64);
                }
            }
        }
        out.push((at, next, count));
        for i in 0..group.len() {
            for j in 0..used[i].len() {
                used[i][j] = (used[i][j] + rates[i][j] * (next - at) as f64).min(100.0);
                if resets[i][j] == next {
                    used[i][j] = 0.0;
                    resets[i][j] += group[i].sample.windows[j].seconds;
                }
            }
        }
        at = next;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forecast::{Sample, Window};
    fn seat(name: &str, used: f64, rate: f64) -> Projection {
        Projection {
            sample: Sample {
                seat: ("w".into(), name.into()),
                login_uid: None,
                alias: name.into(),
                plan: "pro".into(),
                at: 0,
                windows: vec![Window {
                    seconds: WEEK,
                    used,
                    reset: WEEK,
                }],
            },
            rates: vec![rate],
            provisional: false,
            unknown_short_pace: false,
            weekly_from_history: true,
            reset_expiries: Vec::new(),
        }
    }
    #[test]
    fn banked_resets_apply_at_exhaustion_once_each() {
        let mut a = seat("a", 90.0, 1.0);
        a.reset_expiries = vec![500, 50];
        let timeline = availability(&[a], 0);
        assert_eq!(
            timeline[..4],
            [(0, 10, 1), (10, 110, 1), (110, 210, 1), (210, WEEK, 0)]
        );
    }

    #[test]
    fn expiry_at_exhaustion_is_too_late_and_credits_are_seat_local() {
        let mut a = seat("a", 90.0, 1.0);
        a.reset_expiries = vec![10];
        assert_eq!(availability(&[a], 0)[1], (10, WEEK, 0));
        let a = seat("a", 100.0, 1.0);
        let mut b = seat("b", 0.0, 0.0);
        b.sample.plan = "plus".into();
        b.reset_expiries = vec![WEEK];
        assert_eq!(availability(&[a, b], 0), vec![(0, WEEK, 1)]);
    }

    #[test]
    fn banked_reset_clears_only_exhausted_windows_and_restarts_clock() {
        let mut a = seat("a", 40.0, 1.0);
        a.sample.windows.push(Window {
            seconds: 20,
            used: 100.0,
            reset: 5,
        });
        a.rates.push(0.0);
        a.reset_expiries = vec![WEEK];
        let timeline = availability(&[a], 0);
        assert_eq!(timeline[0], (0, 20, 1)); // old short reset at 5 replaced
        assert_eq!(timeline[3], (60, 80, 0)); // weekly 40% retained
    }

    #[test]
    fn natural_reset_at_exhaustion_does_not_waste_credit() {
        let mut a = seat("a", 90.0, 1.0);
        a.sample.windows[0].reset = 10;
        a.reset_expiries = vec![WEEK];
        let timeline = availability(&[a], 0);
        assert_eq!(
            timeline[..4],
            [(0, 10, 1), (10, 110, 1), (110, 210, 1), (210, WEEK, 0)]
        );
    }

    #[test]
    fn blocked_seat_demand_moves_to_remaining_seat() {
        let a = seat("a", 100.0, 1.0);
        let b = seat("b", 0.0, 1.0);
        let intervals = availability(&[a, b], 0);
        assert_eq!(intervals[0], (0, 50, 1));
        assert_eq!(intervals[1], (50, WEEK, 0));
    }
    #[test]
    fn different_plans_do_not_exchange_percentage_demand() {
        let a = seat("a", 100.0, 1.0);
        let mut b = seat("b", 0.0, 1.0);
        b.sample.plan = "plus".into();
        assert_eq!(availability(&[a, b], 0)[0], (0, 100, 1));
    }
    #[test]
    fn reset_restores_service_and_demand_continues_without_backlog() {
        let mut a = seat("a", 100.0, 1.0);
        a.sample.windows[0].reset = 20;
        let timeline = availability(&[a], 0);
        assert_eq!(timeline[..3], [(0, 20, 0), (20, 120, 1), (120, WEEK, 0)]);
    }
    #[test]
    fn short_window_blocks_and_redirects_work() {
        let mut a = seat("a", 0.0, 1.0);
        let mut b = seat("b", 0.0, 1.0);
        for p in [&mut a, &mut b] {
            p.sample.windows.push(Window {
                seconds: 500,
                used: 0.0,
                reset: 200,
            });
            p.rates.push(2.0);
        }
        a.sample.windows[1].used = 100.0;
        assert_eq!(availability(&[a, b], 0)[0], (0, 25, 1));
    }
    #[test]
    fn unknown_own_demand_seat_receives_work_after_reset() {
        let mut a = seat("a", 100.0, 0.0);
        a.sample.windows[0].reset = 10;
        let b = seat("b", 0.0, 1.0);
        assert_eq!(
            availability(&[a, b], 0),
            vec![(0, 10, 1), (10, 190, 2), (190, 200, 1), (200, WEEK, 0)]
        );
    }

    #[test]
    fn tiny_and_zero_rates_terminate_without_overflow() {
        let a = seat("a", 0.0, 1e-300);
        assert_eq!(availability(&[a], 0), vec![(0, WEEK, 1)]);
        assert_eq!(
            availability(&[seat("b", 100.0, 0.0)], 0),
            vec![(0, WEEK, 0)]
        );
    }
}
