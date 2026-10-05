# AWS RDS Trust Bundle

`global-bundle.pem` is the official AWS RDS global CA bundle, downloaded without
modification on 2026-10-05 from:

<https://truststore.pki.rds.amazonaws.com/global/global-bundle.pem>

SHA-256: `fe45bbebf92ad3e27a583bbb2ddd1553c521ed4d49af5514dc0a40372ea5395c`.

Both staging overlays generate `ConfigMap/codexctl-rds-ca` from this public bundle.
The stable name lets separately submitted migration and backfill Jobs use the
same trust roots as the HA broker. File-mode StatefulSet settings stay unchanged.
Mount the ConfigMap read-only at `/etc/codexctl/rds-ca` and set
`CODEXCTL_CENTRAL_DB_CA_FILE=/etc/codexctl/rds-ca/global-bundle.pem` in each
PostgreSQL client container. The existing Rust connector verifies the certificate
chain and database hostname. Do not disable TLS or certificate verification.

To rotate trust roots, download the official bundle, validate its certificates,
update this checksum, and review the rendered overlays in a PR. After merge and
Argo reconciliation, restart HA pods through a reviewed GitOps change because
the connector loads trust roots at process startup. New migration Jobs load the
updated bundle. Keep the previous roots until all RDS certificates have rotated.
