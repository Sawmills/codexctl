use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

const START: &str = "# BEGIN codexctl usage sampling";
const END: &str = "# END codexctl usage sampling";

fn entry(executable: &Path, home: &Path) -> Result<String> {
    fn quote(path: &Path) -> Result<String> {
        let value = path.to_str().context("cron paths must be valid UTF-8")?;
        // Cron interprets percent before the shell, even inside quotes.
        if value.contains(['\n', '\r', '%']) {
            bail!("cron paths cannot contain newlines or percent signs");
        }
        Ok(format!("'{}'", value.replace('\'', "'\\''")))
    }
    Ok(format!(
        "{START}\n0 * * * * HOME={} {} status >/dev/null\n{END}\n",
        quote(home)?,
        quote(executable)?
    ))
}

fn replace_managed(existing: &str, replacement: &str) -> Result<String> {
    let mut result = String::new();
    let mut inside = false;
    for line in existing.split_inclusive('\n') {
        match line.trim_end_matches(['\r', '\n']) {
            START if !inside => inside = true,
            END if inside => inside = false,
            START | END => bail!("malformed codexctl cron block; crontab left unchanged"),
            _ if !inside => result.push_str(line),
            _ => {}
        }
    }
    if inside {
        bail!("unfinished codexctl cron block; crontab left unchanged");
    }
    if !replacement.is_empty() {
        if !result.is_empty() && !result.ends_with('\n') {
            result.push('\n');
        }
        result.push_str(replacement);
    }
    Ok(result)
}

fn scheduled_executable() -> Result<PathBuf> {
    let current = std::env::current_exe().context("cannot locate codexctl executable")?;
    let Some(invoked) = std::env::args_os().next().map(PathBuf::from) else {
        return Ok(current);
    };
    let candidates = if invoked.is_absolute() {
        vec![invoked]
    } else if invoked.components().count() > 1 {
        vec![std::env::current_dir()?.join(invoked)]
    } else {
        std::env::var_os("PATH")
            .map(|path| {
                std::env::split_paths(&path)
                    .map(|dir| dir.join(&invoked))
                    .collect()
            })
            .unwrap_or_default()
    };
    let actual = std::fs::canonicalize(&current)?;
    for candidate in candidates {
        // Verify identity, but keep the stable symlink rather than its versioned target.
        if std::fs::canonicalize(&candidate).ok().as_ref() == Some(&actual) {
            return Ok(if candidate.is_absolute() {
                candidate
            } else {
                std::env::current_dir()?.join(candidate)
            });
        }
    }
    Ok(current)
}

fn read_crontab() -> Result<String> {
    let listed = Command::new("crontab")
        .arg("-l")
        .env("LC_ALL", "C")
        .output()
        .context("cannot run crontab; cron must be installed")?;
    if listed.status.success() {
        String::from_utf8(listed.stdout).context("crontab is not UTF-8; left unchanged")
    } else {
        let error = String::from_utf8_lossy(&listed.stderr);
        if listed.status.code() == Some(1) && error.contains("no crontab for") {
            Ok(String::new())
        } else {
            bail!("cannot read crontab; left unchanged: {error}");
        }
    }
}

// Reconcile against a fresh read before replacing the user's table. Crontab has
// no compare-and-swap API: an external writer after the final read can still race.
fn reconcile_crontab(
    replacement: &str,
    mut read: impl FnMut() -> Result<String>,
) -> Result<Option<String>> {
    let mut existing = read()?;
    for _ in 0..5 {
        let updated = replace_managed(&existing, replacement)?;
        let latest = read()?;
        if latest != existing {
            existing = latest;
            continue;
        }
        return Ok((updated != existing).then_some(updated));
    }
    bail!("crontab keeps changing; retry when other editors have finished")
}

pub fn run(install: bool, remove: bool) -> Result<()> {
    let replacement = if remove {
        String::new()
    } else {
        entry(
            &scheduled_executable()?,
            &dirs::home_dir().context("cannot determine home directory")?,
        )?
    };
    if !install && !remove {
        print!("{replacement}");
        println!("Runs at minute 00 of every hour in cron's timezone.");
        println!("Use --install to add it, or --remove to remove it.");
        println!("The machine must be awake. Reinstall if a move or upgrade changes this path.");
        return Ok(());
    }
    let updated = reconcile_crontab(&replacement, read_crontab)?;
    let Some(updated) = updated else {
        println!("Sampling schedule already matches.");
        return Ok(());
    };
    let mut child = Command::new("crontab")
        .arg("-")
        .stdin(Stdio::piped())
        .spawn()
        .context("cannot update crontab")?;
    child
        .stdin
        .take()
        .context("cannot open crontab input")?
        .write_all(updated.as_bytes())?;
    if !child.wait()?.success() {
        bail!("crontab rejected the sampling schedule");
    }
    if remove {
        println!("Removed codexctl sampling schedule.");
    } else {
        println!("Installed status sampling every hour. The machine must be awake.");
        println!(
            "Run `codexctl forecast` to see progress. Reinstall if a move or upgrade changes the installed path."
        );
    }
    Ok(())
}

/// Inspect only this tool's known schedules. Registration is not proof of a
/// successful sample; the forecast displays observation age separately.
pub fn status() -> String {
    let mut states = Vec::new();
    #[cfg(target_os = "macos")]
    {
        let domain = format!("gui/{}/ai.sawmills.codexctl-sampling", unsafe {
            libc::getuid()
        });
        let loaded = Command::new("launchctl")
            .args(["print", &domain])
            .output()
            .is_ok_and(|output| output.status.success());
        states.push(if loaded {
            "LaunchAgent loaded"
        } else {
            "LaunchAgent not loaded"
        });
    }
    let cron = if let (Ok(executable), Some(home)) = (scheduled_executable(), dirs::home_dir())
        && let Ok(expected) = entry(&executable, &home)
        && let Ok(output) = Command::new("crontab").arg("-l").output()
        && output.status.success()
        && String::from_utf8_lossy(&output.stdout).contains(&expected)
    {
        "cron registered hourly"
    } else {
        "no matching cron verified"
    };
    states.push(cron);
    format!("Sampling: {}.", states.join(" · "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconciles_intervening_jobs_on_install_and_remove() {
        let block = entry(Path::new("/bin/codexctl"), Path::new("/home/test")).unwrap();
        let other = "5 * * * * /usr/bin/true\n";
        for replacement in [&block[..], ""] {
            let initial = if replacement.is_empty() {
                block.clone()
            } else {
                String::new()
            };
            let latest = format!("{initial}{other}");
            let mut reads = [initial, latest.clone(), latest].into_iter();
            let updated = reconcile_crontab(replacement, || Ok(reads.next().unwrap()))
                .unwrap()
                .unwrap();
            assert!(updated.contains(other));
            assert_eq!(updated.contains(START), !replacement.is_empty());
        }
    }

    #[test]
    fn refuses_continuously_changing_crontab() {
        let mut revision = 0;
        assert!(
            reconcile_crontab("", || {
                revision += 1;
                Ok(format!("# revision {revision}\n"))
            })
            .is_err()
        );
    }

    #[test]
    fn install_is_idempotent_and_preserves_other_jobs() {
        let other = "# existing job\n5 * * * * /usr/bin/true\n";
        let block = entry(Path::new("/opt/my bin/codexctl"), Path::new("/home/test")).unwrap();
        let installed = replace_managed(other, &block).unwrap();
        assert!(installed.starts_with(other));
        assert_eq!(replace_managed(&installed, &block).unwrap(), installed);
        assert_eq!(replace_managed(&installed, "").unwrap(), other);
    }

    #[test]
    fn quotes_paths_and_rejects_cron_syntax() {
        let block = entry(Path::new("/a'b/codexctl"), Path::new("/home/test")).unwrap();
        assert!(block.contains("'/a'\\''b/codexctl' status"));
        assert!(entry(Path::new("/a%b/codexctl"), Path::new("/home/test")).is_err());
        assert!(entry(Path::new("/a\nb/codexctl"), Path::new("/home/test")).is_err());
        assert!(replace_managed(START, "").is_err());
        assert!(replace_managed(END, "").is_err());
    }
}
