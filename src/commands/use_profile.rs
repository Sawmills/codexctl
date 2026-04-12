use anyhow::{Result, bail};

use crate::api;
use crate::profile;

pub fn run(alias: Option<&str>) -> Result<()> {
    match alias {
        Some(a) => {
            let email = profile::switch_to(a)?;
            println!("switched to {} ({})", a, email);
        }
        None => {
            let best = find_most_available()?;
            let email = profile::switch_to(&best)?;
            println!("auto-selected most available: {} ({})", best, email);
        }
    }
    Ok(())
}

fn find_most_available() -> Result<String> {
    let profiles = profile::list_profiles()?;
    if profiles.is_empty() {
        bail!("no profiles saved. Use 'codexctl save' to save the current account.");
    }

    let rt = tokio::runtime::Runtime::new()?;
    let client = reqwest::Client::new();

    let results = rt.block_on(async {
        let futs: Vec<_> = profiles
            .iter()
            .map(|p| {
                let client = client.clone();
                let alias = p.meta.alias.clone();
                let auth = api::read_auth_json(&p.auth_json_path());
                async move {
                    let token = match auth {
                        Ok(a) => a.access_token,
                        Err(_) => return (alias, f64::MAX),
                    };
                    match api::fetch_usage_async(&client, &token).await {
                        Ok(usage) => {
                            // Skip usage-based accounts — they don't have rate limits
                            let plan = usage.plan_type.as_deref().unwrap_or("");
                            if plan.contains("usage_based") {
                                return (alias, f64::MAX);
                            }

                            let h5 = usage
                                .rate_limit
                                .as_ref()
                                .and_then(|r| r.primary())
                                .map(|w| w.used_percent)
                                .unwrap_or(0.0);
                            let d7 = usage
                                .rate_limit
                                .as_ref()
                                .and_then(|r| r.secondary())
                                .map(|w| w.used_percent)
                                .unwrap_or(0.0);
                            let score = if h5 >= 100.0 && d7 >= 100.0 {
                                900.0
                            } else if d7 >= 100.0 {
                                700.0 + h5
                            } else if h5 >= 100.0 {
                                500.0 + d7
                            } else {
                                h5 * 2.0 + d7
                            };
                            (alias, score)
                        }
                        Err(_) => (alias, f64::MAX),
                    }
                }
            })
            .collect();
        futures::future::join_all(futs).await
    });

    let best = results
        .iter()
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .filter(|(_, score)| *score < f64::MAX);

    match best {
        Some((alias, _)) => Ok(alias.clone()),
        None => bail!("no usable accounts found (all expired or errored)"),
    }
}
