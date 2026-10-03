# Planned Google Workspace cutover

These preparation manifests are **not referenced** by the staging kustomization
or Application. Merging them does not change staging's Clerk configuration or
its SSM secret source. Do not apply them until the separate cutover is approved.

1. Create the Internal Google web client using the
   [SSO runbook](../../../../docs/central-server.md), register the exact callback,
   and replace the client-ID placeholder in `sso.yaml`.
2. Provision/verify `ClusterSecretStore/aws-secrets-manager` and the plaintext
   Google client secret at `/app/codexctl/oidc-client-secret` in Secrets Manager.
   Preserve the current Clerk secret in SSM for recovery planning.
3. Back up account server state and the vault key. Verify the allowlisted
   company-user ID and email against `codexctl-central users --state /data/state`.
   The raw Clerk subject is not stored and is not required. Only
   `amir@sawmills.ai` is authorized here, with company-user ID
   `6334462fa3dac53bdd9836507666cdde920a73768c3b52b632297d08130e1fd4`.
   Do not add `assistbot@sawmills.ai` or `test-cli-integration@sawmills.ai`.
4. In a separate reviewed cutover change, replace the active staging `sso.yaml`
   and `externalsecret.yaml` with these completed manifests and select the
   reviewed image. Render staging with Kustomize and verify the Google client,
   secret source and the single allowlist entry before the planned sync/restart.
5. Confirm Google sign-in retains Amir's company-user ID, server accounts and
   existing machine credentials, and emits one `SSO_IDENTITY_LINKED` event.
   Remove `clerk_migration` from the active configuration and restart after the
   link. An already-linked Clerk identity remains refused after issuer rollback;
   follow the state-backup recovery plan in ADR 0003.
