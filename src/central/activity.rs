//! Recent token delivery per launch session, used as a bounded session signal.
use super::vault::Device;
use std::{collections::BTreeMap, sync::Mutex};

#[derive(Clone)]
pub(super) struct LastUse {
    pub alias: String,
    pub seen_at: i64,
}

#[derive(Default)]
pub(super) struct Activity {
    entries: Mutex<BTreeMap<(String, String), LastUse>>,
    devices: Mutex<BTreeMap<(String, String), LastUse>>,
}
impl Activity {
    pub(super) fn delivered(&self, device: &Device, alias: String, session_id: String) {
        let seen_at = chrono::Utc::now().timestamp();
        self.entries.lock().expect("machine activity lock").insert(
            (device.user.clone(), session_id),
            LastUse {
                alias: alias.clone(),
                seen_at,
            },
        );
        self.devices.lock().expect("machine activity lock").insert(
            (device.user.clone(), device.id.clone()),
            LastUse { alias, seen_at },
        );
    }
    pub(super) fn last_use(&self, device: &Device) -> Option<LastUse> {
        self.devices
            .lock()
            .expect("machine activity lock")
            .get(&(device.user.clone(), device.id.clone()))
            .cloned()
    }

    pub(super) fn live_sessions(&self, user: &str, alias: &str, window: i64) -> usize {
        let cutoff = chrono::Utc::now().timestamp().saturating_sub(window);
        self.entries
            .lock()
            .expect("machine activity lock")
            .iter()
            .filter(|((entry_user, _session_id), entry)| {
                entry_user == user && entry.alias == alias && entry.seen_at >= cutoff
            })
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_sessions_count_distinguishes_launch_ids_on_one_device() {
        let activity = Activity::default();
        let device = Device {
            id: "device".into(),
            tenant: "tenant".into(),
            user: "user".into(),
            token_hash: "hash".into(),
            revoked: false,
        };
        activity.delivered(&device, "first".into(), "launch-a".into());
        activity.delivered(&device, "first".into(), "launch-b".into());
        assert_eq!(activity.live_sessions("user", "first", 60), 2);
    }
}
