# Banked Resets and Account Selection

A banked reset clears an exhausted usage window. Reset approval and permission to
use paid credits are separate. OpenAI assigns each reset an expiry date.

## List and Redeem

```sh
codexctl resets
codexctl resets --redeem work
codexctl resets --redeem work --yes
```

These commands support server accounts and local profiles. Redemption requires an
exhausted usage window and uses the qualifying reset closest to expiry. `--yes`
skips confirmation. Explicit account selection with `use work` or
`codex --account work` never redeems a reset.

The account server's dashboard offers **Apply reset** when its operator enables
`CODEXCTL_CENTRAL_BROWSER_RESETS=1` (off by default). The action stays disabled until
OpenAI reports an applicable reset. Confirmation requires recent company sign-in.
Retries keep the selected reset and provider idempotency key across replicas.
See [server reset behavior](central-server.md#daily-use) for routing-unknown rows,
redemption settlement, and failure handling.

## Claim Expiring Local Resets

For local profiles, `--claim` redeems applicable resets that expire within three
days by default. Change the interval with `--within-days`:

```sh
codexctl resets --claim
codexctl resets --claim --within-days 7 --yes
```

An account must already have an exhausted window. A reset for an account with
remaining usage cannot be rescued by early redemption.

## Reset-Aware Selection

Local automatic selection prefers the eligible account whose weekly window
resets soonest within its billing class. Accounts that cannot bill credits rank
before accounts that can. Usage-based accounts are never selected automatically.

Within each billing class, automatic `use` selection skips a candidate at or above
95% usage when another candidate is below 95%. This applies to local and server
accounts. A server account at 10% can therefore rank before one at 97%, even if
the latter resets sooner. Fresh server `codex` launches use a separate policy:
they exclude exhausted windows at 100% and rank by live sessions and recent 429s.
See [launching and recovery](launching.md#recovery).

For local selection, choose the most-headroom-first order instead:

```sh
CODEXCTL_SELECT=most-available codexctl codex
```

## Local Recovery

Local `codexctl use` without an alias and local `codexctl codex` recovery rank
available options in this order:

1. An account with included headroom that cannot bill credits.
2. A reset that expires before the exhausted window naturally resets. Codexctl
   redeems it without prompting because it would otherwise lapse unused.
3. A reset worth keeping, with confirmation or `--allow-resets`.
4. An account that can bill credits.

Local `use` warns before selecting an account that can bill credits and continues
without billing confirmation. Active-session recovery asks before switching to
such an account because it can immediately spend credits. `--allow-billing`
approves that recovery; it does not approve reset redemption.

```sh
codexctl use --allow-resets
codexctl codex --allow-resets -- "start prompt"
codexctl codex --allow-resets --allow-billing -- "start prompt"
```

Server launches and automatic 429 recovery use included accounts without automatic
reset redemption or new billing approval. Their policy is separate from local
recovery. Use [explicit redemption](#list-and-redeem) when a server account needs a reset.
