# Plan: Staging SSO Cutover from Clerk to Google Workspace (SAW-12467)

**Decision.** Cut staging over in four reviewed config PRs with no image change: a precursor PR moves only the client-secret source; PR A switches `sso.yaml` to Google with the one-user `clerk_migration` allowlist; Amir signs in once; PR B removes `clerk_migration`. A revert PR for PR A is ready before the window. Google OIDC support shipped in #81, so the pinned staging image already contains it. Machine tokens do not change, so lanes keep working except for the restart pauses.

## Preconditions (each with its owner)

1. Google OAuth client `codexctl-staging` (External per A26, Web application, redirect `https://codexctl.ue1.staging.plat.sm-svc.com/auth/callback`) exists; ID and secret in 1Password only. Owner: lane t5A (SAW-12454 A15), computer use in Dia.
2. Secret: SSM SecureString `/app/codexctl/google-oidc-client-secret` through the existing `ClusterSecretStore/aws-parameter-store` (HQ Q-SSO-STORE: `aws-secrets-manager` does not exist). Written 2026-10-07 as version 1 from `op read` through stdin; its SHA-256 matches 1Password. The Clerk secret stays at `/app/codexctl/oidc-client-secret` for rollback.
3. The image pin that ships #137 and #139 has rolled out and passed its checks. The window then changes configuration only.
4. Surgical rollback tooling (G3): ready. The broker image has `flock`, `sed`, `grep`, and `mv` (no `jq`, `python3`, or Perl JSON module). `users.json` is compact serde JSON that omits an empty `oidc_identity`, so the link adds exactly `,"oidc_identity":"<digest>"` to Amir's record. The clear below removes that string; dry-run in the pod on sample text passed on 2026-10-07, and `flock` on `users.lock` was taken.
5. Operator (G6): one Claude Code session named by HQ before the window. Its credentials: AWS SSO (`aws-sso-login`) for `kubie`/`kubectl`, the repository `gh` identity for merges, `op read` of one field for the hash check. It never uses codexctl tokens, so a server outage cannot block it. Revert merger (G2): HQ w5C:t41; the operator merges if HQ does not answer in 5 minutes.

## PRs

- **Precursor (G1, #147, merged).** Adds the key `google-oidc-client-secret` to `ExternalSecret/codexctl-secrets` from the new SSM path. The Clerk key `oidc-client-secret` is unchanged and nothing reads the new key yet, so a pod restart before PR A keeps Clerk sign-in working. Gate before PR A: `Ready=True`, and `sha256` of the new Secret key equals `sha256` of the 1Password field (hashes only, never the value).
- **PR A (#148).** Replace `sso.yaml` with the Google config and the real client ID, and patch the staging init container to copy `google-oidc-client-secret` to `/keys/oidc-client-secret`. Both change in one sync, and Argo applies the ConfigMap before the StatefulSet, so no pod pairs Google config with the Clerk secret. Bump `restarted-at`. Delete `deploy/k8s/cutovers/google-workspace/`. Evidence: `kubectl kustomize deploy/k8s/overlays/staging` shows the Google issuer, the client ID, `allowed_hosted_domains: ["sawmills.ai"]`, exactly one allowlist entry, the init copy, and an unchanged image digest.
- **PR A also fences staging-ha (G5).** `overlays/staging-ha` reuses the names `codexctl-sso` and `codexctl-secrets` in the same namespace and still points to Clerk. Argo applies only `overlays/staging` today (Application `codexctl`), but a later HA apply would overwrite the Google config. PR A updates `staging-ha/sso.yaml` and `staging-ha/codexctl-externalsecret.yaml` to the same Google values.
- **Revert PR (G2, #149).** Reverts PR A (Clerk `sso.yaml`, the init copy of the Clerk key, the staging-ha files) and bumps `restarted-at`. The staged Google key stays; nothing reads it under Clerk. CI green before the window.
- **PR B.** Remove `clerk_migration` (both overlays) and bump `restarted-at`.

## Window

Argo Application `codexctl` auto-syncs with prune and selfHeal, so a merge is a deploy (G7). No manual sync.

1. Fleet notice at least 10 minutes before the first restart. Each restart can pause the server for up to `terminationGracePeriodSeconds` 600 plus startup; the notice says so (G6).
2. Take the backup of `/data/state` and the vault key, and write a marker file with the backup time. `codexctl-central users --state /data/state` must show company user `6334462f…e1fd4`, `amir@sawmills.ai`, enabled.
3. Merge the precursor. Wait for its gate (G1).
4. Merge PR A. Watch the Application `status.sync.revision` reach the merge commit, a new pod UID, and `/ready` 200.
5. Amir signs in at the dashboard with Google, only as `amir@sawmills.ai` (G4). Expected: one `SSO_IDENTITY_LINKED company_user=6334462f…` line, the same user in `codexctl devices`. Then `codexctl-central users` must list no new company-user ID. If one appears: `codexctl-central users --state /data/state --user <id> --disable`, stop, and decide rollback.
6. Domain refusal (A26: the Google client is External, so any Google account reaches the consent screen). Amir signs in once with a personal Google account, which carries no `hd` claim. Expected: HTTP 403 `company_identity_required` (`src/central/enrollment.rs`, hosted-domain check) and no new company-user ID in `codexctl-central users`. A wrong domain (`hd=other.example`) is already covered by `google_workspace_checks_the_signed_domain_and_verified_email`; the missing-`hd` case has no test, so this staging check is its proof.
7. A lane runs `codexctl codex --account <alias> exec "say ok"`; machine credentials still work.
8. Merge PR B. Watch the sync, the new pod UID, `/ready` 200, and one more lane test call. No second browser sign-in (G8).

## Rollback

- **Before the link (step 5):** merge the revert PR; Clerk works as before.
- **After the link (G3):** a Clerk sign-in for the linked user stays refused by design (ADR 0003). First record which files under `/data/state` changed since the step-2 marker (`find /data/state -newer <marker>`). Then the surgical clear, with `DIGEST` from the `SSO_IDENTITY_LINKED ... oidc_identity=<digest>` log line:

  ```sh
  kubie exec plat-staging codexctl kubectl exec codexctl-0 -c broker -- env DIGEST=<digest> \
    flock -w 10 /data/state/users.lock sh -ec '
      F=/data/state/users.json; P=",\"oidc_identity\":\"$DIGEST\""
      [ "$(grep -o -F "$P" "$F" | wc -l)" -eq 1 ] || { echo "expected one link"; exit 3; }
      umask 077; sed "s/$P//" "$F" > "$F.clear"
      [ "$(grep -c -F oidc_identity "$F.clear")" -eq 0 ] || exit 4
      mv "$F.clear" "$F"'
  ```

  The server's `registry_lock` uses `flock(2)`, so it waits and fails safe; `mv` within one directory is an atomic rename. Then merge the revert PR (#149). Amir's user ID is the Clerk-derived ID, so Clerk resolves him again. Full `/data/state` restore from the step-2 backup is the last resort only, because it discards every write since the marker. Never deploy a binary older than identity links against linked state.

## Done

Amir signs in with Google on staging; `codexctl devices` shows the same user and machines; lanes keep getting tokens; neither overlay has a Clerk issuer or `clerk_migration`.
