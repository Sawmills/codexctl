# codexctl UI v2 Design Direction

UI v2 turns the web app into a quiet ledger: one typeface family, hairlines instead of cards, one accent color, and numbers that read at a glance. Every page answers its main question in the first screen. On `/accounts` that question is "which account does each machine use, and how much does it have left?"

Prototype: [`prototype/`](prototype/) (published at https://codexctl-ui-v2.brisk.sm-svc.com/). Screenshots: [`prototype/shots/`](prototype/shots/), each page at 1440 px and 390 px in light and dark. The current UI for comparison is in [`current/`](current/).

## Reference Products

| Product | What we take |
| --- | --- |
| Linear | Quiet chrome, small type, hierarchy from weight and gray steps rather than boxes and fills. |
| Stripe Dashboard | Tabular figures, right-sized status words with a dot, tables that stay dense yet readable. |
| Tailscale admin console | An honest machine list: last seen times, plain idle and revoked states, no implied liveness. |

## Principles

1. **Information first.** The page opens on machines and their accounts. The big navy hero, the eyebrow labels, and the decorative terminal are gone.
2. **Lines, not boxes.** Sections use one strong rule under the heading and light hairlines between rows. The only filled surfaces are command blocks, the device code, and the single page notice.
3. **Color marks exceptions.** Healthy rows are ink on paper. Amber means low or stale, red means exhausted or broken, cobalt means "in use", "renewal pending", or an action. Every color has a word next to it, so color is never the only signal.
4. **The meter agrees with the number.** A meter fills with what is *left*, the same quantity as the figure beside it. The current UI shows "76% left" next to a bar that fills with the 24% used.
5. **Say what is unknown.** Unknown values read "Unknown" with a dashed track. Stale values keep their figures in gray with "Last observed 7 min ago". A page-wide refresh failure shows one notice at the top and never a badge on every row.

## Type

Instrument Sans (variable width and weight, with tabular figures) for text; JetBrains Mono for aliases, commands, and the device code. Both are OFL fonts and are self-hosted in `prototype/fonts/` (Latin subset, 57 KB and 31 KB), so the page makes no third-party request. The body enables `font-feature-settings: "tnum"` so columns of numbers align.

| Token | Size / line | Use |
| --- | --- | --- |
| `--t-xs` | 12 / 16 | Column headers, captions, reset times, aliases |
| `--t-sm` | 13 / 18 | Secondary meta, notices, hints |
| `--t-base` | 14 / 20 | Body and table text |
| `--t-md` | 16 / 24 | Section titles, usage figures in the table, lede |
| `--t-lg` | 20 / 28 | Unit next to big figures |
| `--t-xl` | 28 / 32 | Page titles (`Accounts`, enrollment) |
| `--t-figure` | 40 / 40 | Machine usage figures, 82% width, weight 600 |
| `--t-display` | 36 to 52 | Landing headline only, 88% width |

## Color Tokens

All text pairs pass WCAG AA (4.5:1). The lowest text pair is `--ink-3` on `--sunken` in light mode at 4.57:1. Meter fills pass 3:1 against their track.

| Token | Light | Dark | Use |
| --- | --- | --- | --- |
| `--bg` | `#f7f7f4` | `#121314` | Page |
| `--sunken` | `#efefea` | `#1d1f21` | Command blocks, tags, device code |
| `--ink` | `#18191b` | `#ececea` | Primary text, meter fill |
| `--ink-2` | `#4f5258` | `#b0b2b7` | Secondary text (7.3:1 / 8.8:1) |
| `--ink-3` | `#686c73` | `#8e9197` | Tertiary text (4.9:1 / 5.9:1) |
| `--line` / `--line-strong` | `#e3e3dd` / `#cfcfc8` | `#26282b` / `#34373b` | Row hairlines / section rules |
| `--accent` | `#2443c4` | `#93a6ff` | Links, primary button, in use, pending, redeemable |
| `--ok` | `#1d7646` | `#5cc98e` | Available dot only |
| `--warn` / `--warn-fill` | `#8a5300` / `#b06d00` | `#f0b44c` / `#e3a33a` | Low usage, stale data, usage-based billing |
| `--bad` | `#b3261e` | `#ff8a80` | Exhausted, login needs attention |

Dark mode follows `prefers-color-scheme` and only swaps tokens; no component has dark-specific rules.

## Space and Layout

A 4 px grid: 4, 8, 12, 16, 24, 32, 48, 64. Content width is 1200 px with a gutter of 16 to 40 px. Section gaps are 48 px; table rows have 16 px vertical padding. Radii are 6 px for controls and 10 px for blocks. At 860 px the account table becomes stacked rows; at 640 px machines stack and copy buttons move above their command. No page scrolls sideways at 390 px; only long commands scroll inside their block, so a token never breaks.

## Components

| Component | Markup | Rules |
| --- | --- | --- |
| State | `<span class="state ok">` (also `warn`, `bad`, `pending`, `live`, `idle`) | A 7 px dot and one word. Pending and idle use a hollow dot. |
| Meter | `<div class="meter"><i style="--left: 76%">` | Fill is percent left; `warn` below 20% left; `unknown` is dashed; inside `.stale` the fill is gray. `aria-hidden`, because the number beside it carries the value. |
| Machine column | `article.machine` | Name, state, account label and alias, two figures (5-hour, 7-day), last token delivery. Idle machines show text instead of figures. |
| Account ledger | `table.ledger` with explicit ARIA roles | Columns: account, plan, 5-hour, 7-day, banked resets, state. Roles keep table semantics when phones restyle rows as grids. |
| Notice | `div.notice` | One per page, for refresh failure or a signed-out session, with the next step (`Retry now`, `Sign in again`). |
| Command | `div.command > pre` plus `button.copy` | `white-space: pre`, multi-line commands with `\` continuations, flags in accent. |
| Tag | `span.tag` | Plan only (`PRO`, `TEAM`). Hugs its text. |
| Disclosure | `details[data-param]` | Revoked machines and the Linux install. The open state syncs to the URL (`?revoked=show`, `?platform=linux`). |

## State Rules for `/accounts`

These rules use the fields in `GET /accounts/data` (`src/central/dashboard.rs`) and keep the current sort order.

| Shown state | Condition | Detail line |
| --- | --- | --- |
| Available (green dot) | `state == "available"`, fresh, no window at 100% | `Updated 8 s ago` |
| Exhausted (red) | Any window `used_percent >= 100` | `Updated …`; banked column shows `N redeemable now` in accent |
| Stale data (amber) | `usage_stale`, or age ≥ 60 s while the rest of the page is fresh | `Last observed 7 min ago` |
| Renewal pending (cobalt, hollow) | `state == "renewal_pending"` | `Waiting for OpenAI sign-in` |
| Login needs attention (red) | `state == "unavailable"` | `codexctl login <alias>`, then the last observation age |
| In use (machine, cobalt) | Registered machine with a token delivery in the last 5 minutes | The note under the machines states this rule |

A usage figure below 20% left turns amber. `billing_class == "usage_based"` reads "Usage-based" in amber, because recovery never selects it and it bills credits.

## Why This Is Cleaner

| Current UI | UI v2 |
| --- | --- |
| A navy hero repeats the machine and account data that appears again lower down. | Machines lead the page once, with the figures in place; the table shows every account once. |
| Seven bordered cards, each with five inner rules and a gray footer. | One table: a row per account, aligned columns, one hairline per row. |
| Bars fill with usage while the number shows what is left. | Bar and number both show what is left. |
| Eyebrow labels, uppercase pills, and badges on every element. | A title and a count per section; one state word per account. |
| Several surface colors (navy, cream, gray, peach). | One paper, one sunken gray, one accent. |
| A wrapped connect command that split `--name` across lines. | A three-line command with `\` continuations; tokens never split. |

## Implementation Notes for the Rust Lane

- Each table row and machine column is fixed markup filled from one JSON object. The current `script.js` renderer or a server template can produce it unchanged.
- Serve the two `.woff2` files from the binary (`include_bytes!`) under `/assets/` and add `font-src 'self'` to the CSP in `dashboard.rs`. No Google Fonts request is needed.
- Format times with `Intl.DateTimeFormat` and keep the `<time datetime>` plus `title` pattern. The prototype hardcodes fixture times.
- The footer version keeps coming from `CARGO_PKG_VERSION`; the Linux script pin must follow the release.
- The approval form keeps `method="post" action="/auth/approve"`; the prototype submits with GET to a static page.
- Remove the `proto-nav` footer links; they exist only to move between prototype pages.
- `state == "unavailable"` also covers a routing refusal (`routing_refused` in `server.rs`). If the server can expose the reason, show "Routing refused" for that case instead of the login hint.
