# Plan: Staging SSO Cutover from Clerk to Google Workspace (SAW-12467)

**Decision.** Cut staging over in two reviewed config PRs and two restarts, with no image change: PR A switches `sso.yaml` and `externalsecret.yaml` to the prepared Google manifests with the one-user `clerk_migration` allowlist; Amir signs in once; PR B removes `clerk_migration`. Google OIDC support shipped in #81, so the pinned staging image already contains it. Machine tokens do not change, so lanes keep working except for the restart pauses.

## Preconditions (each with its owner)

1. Google OAuth client `codexctl-staging` (Internal, Web application, redirect `https://codexctl.ue1.staging.plat.sm-svc.com/auth/callback`) exists; ID and secret in 1Password only. Owner: lane t5A (SAW-12454 A15), computer use in Dia.
2. Plaintext secret at Secrets Manager `/app/codexctl/oidc-client-secret`, and `ClusterSecretStore/aws-secrets-manager` can read it. The Clerk secret stays in SSM for rollback. Owner: platform admin.
3. Backup of `/data/state` and the vault key, taken immediately before PR A syncs. `codexctl-central users --state /data/state` must show company user `6334462f…e1fd4` with email `amir@sawmills.ai`, enabled. Owner: operator with cluster access (lanes have no k8s write).

## PR A: Google With the One-Time Link

- Replace `deploy/k8s/overlays/staging/sso.yaml` and `externalsecret.yaml` with the cutover manifests, with the real client ID in `sso.yaml`. Keep the `vault-key` and `metrics-token` sources unchanged; only `oidc-client-secret` moves to `aws-secrets-manager`.
- Bump `codexctl-restart-patch.yaml` `restarted-at`: a ConfigMap change alone does not restart the StatefulSet, and the init container copies the secret only at pod start.
- Evidence in the PR: `kustomize build deploy/k8s/overlays/staging` shows the Google issuer, the client ID, `allowed_hosted_domains: ["sawmills.ai"]`, exactly one allowlist entry, and the image digest unchanged. Delete `deploy/k8s/cutovers/google-workspace/` in the same PR.

## Window

1. Notice to the fleet with at least 10 minutes' warning. The operator and HQ do not depend on codexctl tokens during the window (B33 lesson).
2. Take the backup (precondition 3), merge PR A, and run the manual Argo sync. Wait for `/ready` 200.
3. Amir signs in at the dashboard with Google. Expected: the same company-user ID, server accounts and devices in `codexctl devices`, and one `SSO_IDENTITY_LINKED` event in the server log.
4. A lane requests a token (`codexctl codex --account <alias> exec "say ok"`), proving machine credentials still work.
5. Merge PR B (remove `clerk_migration`, bump `restarted-at`), sync, `/ready` 200, and Amir signs in again without a new link event.

## Rollback

- Before Amir's link (step 3): revert PR A and sync; Clerk works as before.
- After the link: a Clerk sign-in for the linked user stays refused by design (ADR 0003). Restore the step-2 backup of `/data/state` with the Clerk config, per the ADR 0003 state-backup procedure. Do not deploy a binary older than identity links against linked state.

## Done

Amir signs in with Google on staging; `codexctl devices` shows the same user and machines; lanes keep getting tokens; `sso.yaml` has no Clerk issuer and no `clerk_migration`.
