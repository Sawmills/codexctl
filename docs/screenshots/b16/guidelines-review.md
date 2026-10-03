# B16 design self-review

Checked against `/home/amir/b16-design-spec.md` and
`/home/amir/web-interface-guidelines.md`, including HQ's subsequent machine-activity
clarification and all items in `/home/amir/b16-design-review-1.md` and `/home/amir/b16-design-review-2.md`. Synthetic preview: 7 server accounts; no local-profile invention.

## src/central/dashboard/accounts.html

- PASS — hierarchy: recent token deliveries in “In use now”; one row per machine with
  label, alias, plan, large long-window percentage, short-window capacity, reset and delivery age.
- PASS — no recent delivery: “Suggested” uses fresh included headroom, with no switch.
- PASS — loading skeleton, migration empty state, retryable error retaining stale
  data, session-ended state, explicit local-profile limitation.
- PASS — semantic headings, labeled section, native retry button and sign-in link.
- PASS — live regions for async errors and observation freshness; `aria-busy` on load.
- PASS — no account data embedded in HTML; SSO guards page and JSON separately.

## src/central/dashboard/landing.html

- FIX → PASS — replaced promotional terminal copy with concrete account-management actions.
- PASS — one headline, one primary SSO action, copyable macOS/Linux/connect commands.
- PASS — native buttons with specific accessible labels; clipboard success/error live region.
- PASS — pinned Linux release; checksum before extraction/install; shell failure stops install.
- PASS — readiness pill distinguishes ready, not ready and unknown.
- PASS — code uses `translate="no"`; `<machine>` and shell quotes remain literal code.
- PASS — Linux disclosure uses native keyboard interaction and URL `platform=linux` state.
- PASS — sentence case throughout, following HQ round 1 (overrides the general title-case rule).

## src/central/dashboard/page.html

- PASS — `lang="en"`, semantic header/main/footer, skip link, descriptive page title.
- PASS — brand uses `translate="no"`; decorative glyphs are hidden from assistive tech.
- PASS — zoom remains enabled; no maximum-scale or user-scalable restriction.
- PASS — theme-color metadata for both schemes; no external fonts, assets, or trackers.

## src/central/dashboard/style.css

- FIX → PASS — hero percentages: 56 px desktop (72 px for a single machine), 44 px phone.
- FIX → PASS — content-visibility applies only above 50 cards, avoiding blank preview cards.
- PASS — 1200 px maximum width; 1/2/3-column grid; 16 px phone gutters; safe-area insets.
- PASS — 4/8 px spacing, system UI type, tabular numerals, balanced headings.
- PASS — meaningful colors: brand healthy, amber ≥80%, red exhausted/unavailable, blue renewal.
- PASS — states also have text labels; light/dark axe WCAG A/AA checks found no violations.
- PASS — focus-visible outlines; no outline suppression; no sticky overlay obscures focus.
- PASS — hover feedback, disabled retry feedback; touch-action and intentional tap highlight.
- PASS — aliases/labels wrap, flex children shrink; phone overflow assertion passes.
- PASS — heading scroll margins; layout uses flex/grid rather than JavaScript measurement.
- PASS — meters reveal via transform once on first load; 500 ms, no uninterruptible loop,
  explicit transform origin, no transition-all; reduced motion disables animation.
- PASS — native dark color-scheme; no native select requiring an additional theme rule.

## src/central/dashboard/script.js

- PASS — sort: in-use by machine, available by long-window headroom, high usage,
  exhausted, unknown/stale usage, renewal pending, unavailable; browser order assertion.
- PASS — percent left is primary; used is secondary; declared durations only; Unknown
  remains text, never a fabricated 0% or dash.
- PASS — semantic progress bars, explicit min/max/now, names and percentage alternatives.
- PASS — countdowns update every minute; reset timestamps have local-time title/ARIA text.
- PASS — banked-reset inventory includes count/expiry and stale state; zero inventory
  is one muted line; redeemable count shown prominently only above zero.
- PASS — Intl number/date formatting; browser locale, no IP language detection.
- PASS — safe DOM text for user-controlled labels/aliases; hostile-looking label browser test.
- PASS — native buttons/links/disclosure supply keyboard actions; keyboard skip-link test.
- PASS — one poll at a time, bounded request timeout, retry button and automatic retry.
- PASS — expired/disabled session clears the previous account and machine data.
- PASS — no layout reads in production rendering; DOM updates in coherent batches.
- PASS — no hydration framework or server-rendered dates, so no hydration mismatch.
- PASS — active voice, second person, numerals, specific retry/sign-in labels and error next steps.

## Not applicable to these pages

- Forms: no text inputs, labels, input types/modes, autocomplete, spellcheck, paste
  handlers, checkboxes, validation, placeholders or unsaved input. Sign-out uses a native POST form.
- Images/media: no img, SVG, video, GIF, audio, captions, lazy media or autoplay loops.
- Compound controls/overlays: no custom composite controls, modals, drawers or sticky overlays.
- Drag/gesture: no drag, swipe or gesture-only operation; no autofocus.
- External assets: no preconnect/preload needed; fonts use the system stack.
- Destructive actions: none; dashboard does not select, redeem, remove or mutate accounts.
- Controlled inputs/hydration: none; no hydration suppression or expensive per-key rendering.

## Round 1 fixes

1. PASS — separate machine hero rows, no winner account; two-machine and suggested screenshots.
2. PASS — requested category order; exact seven-account order asserted.
3. PASS — compact label/left/reset row, meter and used caption; all desktop cards ≤260 px.
4. PASS — “No banked resets” and no zero redeemable noise; positive count emphasized.
5. PASS — sentence case in UI copy and window labels; user-provided labels preserved.
6. PASS — seconds below 60, minutes thereafter; Stale is a pill.
7. PASS — scoped identity email and Sign out; real HTTP isolation/CSRF checks and browser form check.
8. PASS — revoked disclosure, active machine account pills, URL-backed expanded state.
9. PASS — terminal has no decorative action arrows or rotation.
10. PASS — command pre-wrap with normal overflow wrapping and internal horizontal scrolling.
11. PASS — footer version comes from Cargo package metadata and links to documentation.

## Round 2 fixes

1. PASS — nonbreaking command flag spans; checked at 1440, 768 and 390 px.
2. PASS — whole-page stale observations use one banner; individual older observations
   keep their own pills; hero pills hug the text.
3. PASS — account and plan chips share a row at every checked width.
4. PASS — footer uses `CARGO_PKG_VERSION`; HTTP test checks the rendered footer on both
   routes against the build version. After rebasing on main, the build and footer are v0.1.35; release metadata was not edited.

HQ passed design review after the four requested round-2 captures. All 12 images
were then recaptured from the v0.1.35 build after rebasing on main, so the PR images
show the final layout and current build footer.

## Execution evidence

- Real Chromium, production bundled HTML/CSS/JS and exact CSP, synthetic JSON only.
- 1440 px and 390 px, light and dark; 7 accounts, 3 machines; error/stale and suggested views.
- Browser checks: axe WCAG A/AA, no horizontal overflow, per-machine hero, card heights ≤260 px, native sign-out, copy,
  60-second polling, retry, safe labels, session loss, empty and suggested states,
  keyboard skip link and reduced-motion behavior.
- This is a self-review and local browser evidence, not HQ's design/code review or deployment.
