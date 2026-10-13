//! Preflight bounds for the stopped-file administrative backfill.
use super::*;

const BACKFILL_MAX_ROWS: usize = 50_000;
const TABLES: [&str; 6] = [
    "central_accounts",
    "central_users",
    "central_devices",
    "central_alias_tombstones",
    "account_loans",
    "account_loan_audit",
];

pub(super) fn check_rows(table: &str, count: usize) -> Result<usize> {
    if count > BACKFILL_MAX_ROWS {
        bail!("backfill row cap exceeded: {table} count {count}");
    }
    Ok(count)
}

pub(super) async fn target_counts(target: &PostgresStore) -> Result<BTreeMap<String, usize>> {
    let client = target.client().await?;
    let mut counts = BTreeMap::new();
    for table in TABLES {
        let exists: bool = client
            .query_one("SELECT to_regclass($1) IS NOT NULL", &[&table])
            .await?
            .get(0);
        let count = if exists {
            // Table names are fixed here, never derived from file or DB values.
            let count: i64 = client
                .query_one(&format!("SELECT count(*) FROM {table}"), &[])
                .await?
                .get(0);
            usize::try_from(count)?
        } else {
            0
        };
        counts.insert(table.to_owned(), check_rows(table, count)?);
    }
    Ok(counts)
}

// Counting discards row keys and values; payloads are decoded only after every
// source and target table has passed the preflight cap.
#[derive(Default)]
struct Rows(usize);

impl<'de> Deserialize<'de> for Rows {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct Counter;
        impl<'de> serde::de::Visitor<'de> for Counter {
            type Value = Rows;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a row array or map")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut rows: A,
            ) -> std::result::Result<Rows, A::Error> {
                let mut count = 0;
                while rows.next_element::<serde::de::IgnoredAny>()?.is_some() {
                    count += 1;
                }
                Ok(Rows(count))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut rows: A,
            ) -> std::result::Result<Rows, A::Error> {
                let mut count = 0;
                while rows
                    .next_entry::<serde::de::IgnoredAny, serde::de::IgnoredAny>()?
                    .is_some()
                {
                    count += 1;
                }
                Ok(Rows(count))
            }
        }
        deserializer.deserialize_any(Counter)
    }
}

#[derive(Default, Deserialize)]
struct FileLoanCounts {
    #[serde(default)]
    loans: Rows,
    #[serde(default)]
    loan_audit: Rows,
}

pub(super) fn source_counts(state: &Path, key: &Path) -> Result<BTreeMap<String, usize>> {
    let mut counts: BTreeMap<String, usize> = TABLES
        .into_iter()
        .map(|table| (table.to_owned(), 0))
        .collect();
    let accounts = state.join("accounts");
    if accounts.try_exists()? {
        let mut count = 0;
        for entry in std::fs::read_dir(accounts)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() && entry.path().join("vault.enc").try_exists()? {
                count += 1;
            }
        }
        counts.insert(
            "central_accounts".into(),
            check_rows("central_accounts", count)?,
        );
    }
    for (table, file) in [
        ("central_users", "users.json"),
        ("central_devices", "devices.json"),
        ("central_alias_tombstones", "renames.json"),
    ] {
        let path = state.join(file);
        if path.try_exists()? {
            let rows: Rows = serde_json::from_slice(&vault::private_read(&path)?)?;
            counts.insert(table.into(), check_rows(table, rows.0)?);
        }
    }
    let path = state.join("central-storage.enc");
    if path.try_exists()? {
        let bytes = vault::private_read(&path)?;
        let rows: FileLoanCounts = serde_json::from_slice(&vault::decrypt_bytes(key, &bytes)?)?;
        counts.insert(
            "account_loans".into(),
            check_rows("account_loans", rows.loans.0)?,
        );
        counts.insert(
            "account_loan_audit".into(),
            check_rows("account_loan_audit", rows.loan_audit.0)?,
        );
    }
    Ok(counts)
}

pub(super) async fn verify_devices(
    target: &PostgresStore,
    source: &BTreeMap<String, Vec<String>>,
) -> Result<()> {
    let tenants: Vec<_> = source.keys().map(String::as_str).collect();
    // Even retained rows in an undeclared tenant are rejected count-only;
    // known source tenants keep the DAL's existing active-row parity policy.
    let foreign: i64 = target
        .client()
        .await?
        .query_one(
            "SELECT count(*) FROM central_devices WHERE NOT(tenant=ANY($1::text[]))",
            &[&tenants],
        )
        .await?
        .get(0);
    if foreign != 0 {
        bail!("backfill source mismatch: central_devices foreign tenant count {foreign}");
    }
    for (tenant, ids) in source {
        crate::store::validate_alias(tenant)?;
        let ids: std::collections::BTreeSet<_> = ids.iter().map(String::as_str).collect();
        // This DAL always constrains rows with WHERE tenant=$1.
        for (id, _, _) in target.load_device_entity_revisions(tenant).await? {
            if !ids.contains(id.as_str()) {
                bail!("backfill source mismatch: central_devices extra row {id}");
            }
        }
    }
    Ok(())
}
