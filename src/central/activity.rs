//! Recent token delivery per launch session, used as a bounded session signal.
use super::vault::Device;
use std::{collections::BTreeMap, sync::Mutex};

#[derive(Clone)]
pub(super) struct LastUse {
    pub alias: String,
    /// The owning account key; a borrower's use counts on the lender's account.
    pub account: String,
    pub seen_at: i64,
}

#[derive(Default)]
pub(super) struct Activity {
    entries: Mutex<BTreeMap<(String, String), LastUse>>,
    devices: Mutex<BTreeMap<(String, String), LastUse>>,
}
impl Activity {
    pub(super) fn delivered(
        &self,
        device: &Device,
        alias: String,
        session_id: String,
        account: String,
    ) {
        let seen_at = chrono::Utc::now().timestamp();
        let mut entries = self.entries.lock().expect("machine activity lock");
        entries.retain(|_, entry| entry.seen_at >= seen_at.saturating_sub(10 * 60));
        entries.insert(
            (device.user.clone(), session_id),
            LastUse {
                alias: alias.clone(),
                account: account.clone(),
                seen_at,
            },
        );
        drop(entries);
        self.devices.lock().expect("machine activity lock").insert(
            (device.user.clone(), device.id.clone()),
            LastUse {
                alias,
                account,
                seen_at,
            },
        );
    }
    pub(super) fn last_use(&self, device: &Device) -> Option<LastUse> {
        self.devices
            .lock()
            .expect("machine activity lock")
            .get(&(device.user.clone(), device.id.clone()))
            .cloned()
    }

    /// Recent launch sessions of every company user on one account.
    pub(super) fn live_sessions(&self, account: &str, window: i64) -> usize {
        let cutoff = chrono::Utc::now().timestamp().saturating_sub(window);
        self.entries
            .lock()
            .expect("machine activity lock")
            .values()
            .filter(|entry| entry.account == account && entry.seen_at >= cutoff)
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
        activity.delivered(&device, "first".into(), "launch-a".into(), "key".into());
        activity.delivered(&device, "first".into(), "launch-b".into(), "key".into());
        assert_eq!(activity.live_sessions("key", 60), 2);
        let borrower = Device {
            id: "other".into(),
            user: "borrower".into(),
            ..device
        };
        activity.delivered(
            &borrower,
            "lender/first".into(),
            "launch-c".into(),
            "key".into(),
        );
        assert_eq!(
            activity.live_sessions("key", 60),
            3,
            "every user counts on the account"
        );
    }
}
