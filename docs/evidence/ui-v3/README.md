# Account server UI v3 evidence

These screenshots show the production Rust templates and renderer using synthetic
account and machine fixtures. Approval and connected pages came through the local
OIDC enrollment protocol test, including a consumed synthetic approval. No live
account credentials or production services were used.

Each of landing, accounts, approval, connected, healthy, blocked, stale, and empty
has four captures: 1440 px desktop and 390 px phone, in light and dark mode.

The browser test checks every capture for page overflow and WCAG A/AA violations,
then checks visible keyboard focus, command copying, URL-synced disclosures,
reduced motion, 60-second refresh, observation expiry, reset-action suppression
on refresh failure, retry, POST sign-out, and clearing private data on 401/403.
It also verifies that the account overview works without JavaScript.

Reproduce with Playwright, its Chromium browser, and `@axe-core/playwright`
installed in an external Node tooling directory:

```bash
B22_RENDER_DIR=/tmp/b22-render cargo test --lib central::dashboard
B22_RENDER_DIR=/tmp/b22-render cargo test --test central_managed_test dashboard_v3_
NODE_PATH=/path/to/tooling/node_modules node tests/fixtures/dashboard_browser.cjs \
  /tmp/b22-render /tmp/b22-screens
```

The JSON contract of `/accounts/data` remains available. The browser requests
`Accept: text/html` on that endpoint to refresh the same Rust-rendered view used
by `/accounts`. This keeps recommendation and attention policy in one module;
a second JavaScript renderer would require both callers to maintain identical
selection, escaping, and stale-data rules. Both representations revalidate the
company session after collecting observations, return errors on failed snapshots,
and only read account state. Fetch cancellation is bounded to 45 seconds.

The page conservatively withdraws its recommendation when its earliest fresh
observation reaches 60 seconds, even if another observation remains fresh.
It retains the last observed figures until the next refresh. Automatic selection
uses the CLI's default weekly-reset ordering, without reading the account server
process's `CODEXCTL_SELECT` override.

CSP keeps `default-src 'none'`, hash-pinned styles/scripts, same-origin forms, and
frame/base restrictions. Fonts add only `font-src 'self'`. Enrollment adds the
hash-pinned copy script. Referrer policies, session checks, approval tokens, and
sign-out origin checks are preserved. Native progress meters avoid inline styles.

This is local build and browser evidence. Deployment and production validation
follow HQ's independent review and operator notice.
