# codexctl UI v3 Audit: Before and After

The audit score rose from 17/20 to 20/20. The first audit ran after the clarify, distill, and typeset passes. The second ran after the fixes listed below. Both rounds used the same evidence: the Impeccable detector (`impeccable detect --json design/v3`), plus a Playwright scan of every page. The scan covered light and dark themes at 1440 px and 390 px and measured text contrast, interactive target size, heading order, and horizontal overflow.

## Scores

| # | Dimension | Before | After | Key finding |
| --- | --- | --- | --- | --- |
| 1 | Accessibility | 3 | 4 | Tertiary text on the recommended row measured 4.48:1; standalone text controls measured 18 to 23 px tall |
| 2 | Performance | 4 | 4 | Two self-hosted fonts (57 KB and 31 KB), no images, 1.5 KB of script |
| 3 | Responsive design | 3 | 4 | Same target-size finding; no overflow at 390 px in either round |
| 4 | Theming | 4 | 4 | Every color is a token; dark mode swaps tokens only |
| 5 | Implementation integrity | 3 | 4 | Detector flagged decorative stripes and cramped padding |
| | **Total** | **17/20 (Good)** | **20/20 (Excellent)** | |

## Findings and Fixes

| ID | Severity | Finding (before) | Evidence | Fix (after) |
| --- | --- | --- | --- | --- |
| A1 | P1 | `--ink-3` on `--accent-soft` (the recommended ledger row) failed WCAG AA in light mode | 4.48:1 on `overview.html` and `overview-blocked.html` | `--ink-3` darkened from `#686c73` to `#63676e`. Rescan: zero contrast failures on all pages, both themes, both widths |
| A2 | P2 | Standalone text controls were below the 24 px target minimum (WCAG 2.5.8) | Sign out 20 px, Sign in 20 px, revoked disclosure 20 px, footer links 18 px, wordmark 23 px | Minimum 24 px for standalone text controls, 44 px on coarse pointers. Rescan: one remaining target under 24 px, an inline link inside a sentence, which 2.5.8 exempts |
| A3 | P2 | The machines table lost table semantics on phones, where its rows restyle as blocks | `display: block` without ARIA roles | Explicit `role="table"`, `row`, `cell`, and `columnheader`, matching the account ledger |
| A4 | P2 | The unknown-usage meter used a repeating gradient | Detector: `repeating-stripes-gradient` | Unknown is now a dotted rule: same meaning, no surface pattern |
| A5 | P3 | Detector reported cramped padding on attention items, setup steps, and approval facts | Detector: `cramped-padding` | Logical `padding-block` became physical `padding`; attention items also gained 4 px (12 to 16 px) to match the ledger rhythm |
| A6 | P3 | The approval and connected pages had no skip link | Markup | Skip link added, as on the other pages |
| T1 | P3 | Detector reported tight body leading | Detector: `tight-leading` 1.27x; the real computed value was 1.45 | Body leading raised to 1.5 |

## Remaining Detector Findings (Explained)

| Finding | Why it stays |
| --- | --- |
| `overused-font`: Instrument Sans | The v2 world established it and the brief asks to keep what v2 did right. Its variable width carries the condensed 44 px figures (`font-stretch: 82%`), a role a fixed-width face cannot fill. Operate surfaces are well served by a workhorse UI face. |
| `monotonous-spacing`: about 4 px used 11 of 17 times per page | The 4 px values are the tight groups inside a cell (label to value, value to meter). Separation between groups uses 16, 24, and 48 px. |

## Earlier Passes

These passes ran before the first audit and are not part of the score.

- **Clarify.** Rewrote the possessive "Nightly agents's", the refresh-failed answer ("the best match was"), and the renewal-pending fix text. Moved the approval warning under the code it refers to.
- **Distill.** Removed "Updated 8 s ago" from every fresh ledger row (the answer block states freshness once). Removed per-row "Last observed" when the page notice already says the refresh failed. Removed the duplicate reset command from the attention list when the answer already gives it. Cut the selection rule from three sentences to one. (The plan column left the ledger earlier, in the JTBD structure; plan sits as a tag next to the alias.)
- **Typeset.** Body leading 1.5; dark mode opens body tracking by 0.01em; type roles unchanged from v2 apart from a larger answer heading (36 px) and figures (44 px).
- **Polish.** Added the healthy state (`overview-healthy.html`) so the empty attention lane is designed, not assumed. Changed the heading from "Switch to" to "Use", because the recommendation can already be in use. Kept an in-use recommendation first in the ledger. Removed dead CSS.

## Not Verified

- Screen reader output was not tested with VoiceOver or NVDA; semantics were checked in markup only.
- Touch was emulated (Playwright `isMobile`, `hasTouch`) in headless Chromium 149; no physical device or WebKit run.
- 200% browser zoom was not tested directly; the layout reflows to one column below 900 px.

## Independent Finish Review

A fresh Impeccable finish reviewer (no shared context with the build) read the direction contract, the product record, and every capture. It returned `fix` with seven material findings. Two fix rounds followed, and the final verdict was `ship`. That verdict scores the seven fixes; it is not a second review of the whole surface.

| # | Finding | Fix | Verdict |
| --- | --- | --- | --- |
| 1 | The nothing-left heading said no account had room, while a stale account showed 61% and a usage-based account showed 92% | Heading "No included account has confirmed room"; the stale account is named with its age and a bare `codexctl use`, which checks live usage | Resolved (round 2) |
| 2 | A second reset command contradicted the closest-to-expiry rule | Other reset-ready accounts explain why the answer spends a different reset first; no command | Resolved |
| 3 | The healthy page told the visitor to switch to the account already in use | Status heading ("is the best account now"), a secondary Copy for "another machine", and an effect line with its subject | Resolved (round 2) |
| 4 | The reviewer read `codexctl login <alias>` as starting a second sign-in | Rejected on evidence: `docs/central-server.md` says the same command resumes on the starting machine and other machines cannot resume it. The copy now says exactly that | Resolved |
| 5 | Cobalt marked "In use" and "Renewal pending" as well as action | "In use" is ink; "Renewal pending" is ink-2 with a hollow dot; cobalt now marks only action and "use this" | Resolved |
| 6 | "That machine" did not say which machine | The effect line names the machine that stopped | Resolved |
| 7 | The ledger caption hid the stale-last order | "In use first, then the recommendation, then most room; stale and unknown last" | Resolved |
