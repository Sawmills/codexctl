# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Users

Engineers who use OpenAI Codex through codexctl and have connected one or more machines to their company's account server. Confirmed priority, when the jobs compete for space: the engineer whose Codex has just stopped and who needs, within five seconds, the account to switch to and the command to run. Second: the owner of many server accounts, who checks daily that logins work, usage is not running out, and banked resets are not about to expire unused. Third: an engineer connecting a new machine. Each company user sees only their own server accounts and machines.

## Product Purpose

The web app is the browser view of a codexctl account server. It shows a signed-in company user their server accounts (usage in the 5-hour and 7-day windows, banked resets, login state) and their machines (which account each one last received a token for), and it hosts machine connection: install, connect, approve. It never changes an account: switching, redeeming, and login renewal happen in the terminal. Success is an engineer back at work after one copied command.

## Positioning

codexctl keeps several OpenAI Codex logins on a private account server that every machine of one person shares, and it chooses between them by included usage, never silently spending credits. The web app is the only place that shows all of a person's accounts and machines together.

## Operating Context

- Visits start from the terminal: a usage-limit error, a `codexctl connect` that opened the browser, or a daily check.
- The engineer acts in the terminal; the page supplies the command. Copy-to-clipboard is the main interaction.
- Data comes from `GET /accounts/data` and refreshes every 60 seconds. Usage older than 60 seconds is stale and is not proof of headroom.
- A machine's "in use" state means a token delivery in the last 5 minutes. The server cannot see live connectivity or running sessions.

## Capabilities and Constraints

- Server-rendered HTML with a strict content security policy (hash-pinned inline style and script, `default-src 'none'`). No SPA framework, no third-party requests.
- Account states from the server: `available`, `renewal_pending`, `unavailable` (login needs attention, or a routing refusal). Billing classes: `rate_limited` (included usage), `usage_based` (bills credits), `unknown`.
- Recommendation rule, matching automatic selection in `codexctl use` from v0.1.37 (PR #76, open on 2026-10-03): available accounts with included usage (`rate_limited` billing), fresh data, at least one reported window, and every reported window below 100% qualify; an absent window (for example a weekly-only account) does not disqualify. If any qualifying account has a window below 95% used, accounts with any window at 95% or more drop out. The soonest 7-day reset wins, then the lower availability score (5-hour used counts double). Usage-based and unknown billing never qualify. Source: `remote::select_for_activation` and `remote::select` in PR #76 at `268fcfe`.
- From v0.1.37, `codexctl use <alias>` moves running Codex sessions on that machine to the selected account within 60 seconds. Before v0.1.37, running sessions kept their startup account.
- `codexctl reset <alias>` redeems the soonest-expiring banked reset, and only for an account with an exhausted window. `codexctl login <alias>` renews a server account's login.
- Terms follow `GLOSSARY.md`: company user, machine, account server, server account, alias, login renewal. Avoid "device" except for the OpenAI device code and the `codexctl devices` command.

## Brand Commitments

- Name: `codexctl`, always lowercase.
- Company-neutral copy. codexctl is open source (Apache-2.0) and other companies run their own account servers: say "company SSO", never a specific company, in templates.
- Voice: plain, short, literal. Commands are shown exactly as typed.

## Evidence on Hand

- Real data shape: `src/central/dashboard.rs` (`/accounts/data`).
- Current templates: `src/central/dashboard/`, `src/central/enrollment/`.
- Previous prototype and its design rules: `design/prototype/`, `design/README.md`.
- No customer quotes, metrics, or logos exist; none may be invented. Fixture data in prototypes is illustrative.

## Product Principles

1. Answer first. Each page opens on the answer to its visitor's question, with the action next to it.
2. Never imply more certainty than the data has. Stale is stale; unknown is unknown; "in use" is a token delivery, not a live session.
3. Never nudge toward spending credits. Included usage first; usage-based accounts are named honestly and never recommended.
4. The terminal is where actions happen. The page gives exact commands and says what they will do.

## Accessibility & Inclusion

WCAG 2.2 AA: text contrast 4.5:1, non-text 3:1, full keyboard operation with visible focus, color never the only signal, respects `prefers-color-scheme` and `prefers-reduced-motion`, no horizontal scroll at 390 px.
