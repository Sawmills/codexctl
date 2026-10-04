//! Recent token delivery per registered machine, used as a bounded session signal.
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
}
impl Activity {
    pub(super) fn delivered(&self, device: &Device, alias: String) {
        self.entries.lock().expect("machine activity lock").insert(
            (device.user.clone(), device.id.clone()),
            LastUse {
                alias,
                seen_at: chrono::Utc::now().timestamp(),
            },
        );
    }
    pub(super) fn last_use(&self, device: &Device) -> Option<LastUse> {
        self.entries
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
            .filter(|((entry_user, _), entry)| {
                entry_user == user && entry.alias == alias && entry.seen_at >= cutoff
            })
            .count()
    }
}
