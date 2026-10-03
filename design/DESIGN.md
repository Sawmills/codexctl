---
name: codexctl web UI
description: The browser view of a codexctl account server, a quiet ledger that answers which account to use and which command to run.
colors:
  accent: "#2443c4"
  accent-ink: "#ffffff"
  accent-soft: "#e8ecfb"
  paper: "#f7f7f4"
  surface: "#ffffff"
  sunken: "#efefea"
  ink: "#18191b"
  ink-2: "#4f5258"
  ink-3: "#63676e"
  line: "#e3e3dd"
  line-strong: "#cfcfc8"
  ok: "#1d7646"
  warn: "#8a5300"
  warn-fill: "#b06d00"
  warn-soft: "#fbf1df"
  bad: "#b3261e"
  meter: "#2c2e33"
  meter-track: "#e3e3dc"
  accent-dark: "#93a6ff"
  accent-ink-dark: "#0d1330"
  accent-soft-dark: "#1f2645"
  paper-dark: "#121314"
  surface-dark: "#1a1b1d"
  sunken-dark: "#1f2123"
  ink-dark: "#ececea"
  ink-2-dark: "#b0b2b7"
  ink-3-dark: "#8f9298"
  line-dark: "#27292c"
  line-strong-dark: "#383b3f"
  ok-dark: "#5cc98e"
  warn-dark: "#f0b44c"
  warn-fill-dark: "#e3a33a"
  warn-soft-dark: "#2a2213"
  bad-dark: "#ff8a80"
  meter-dark: "#d4d5d8"
  meter-track-dark: "#2c2e32"
typography:
  answer:
    fontFamily: "Instrument Sans, ui-sans-serif, system-ui, sans-serif"
    fontSize: "2.25rem"
    fontWeight: 600
    lineHeight: 1.08
    letterSpacing: "-0.03em"
    fontVariation: '"wdth" 90'
  figure:
    fontFamily: "Instrument Sans, ui-sans-serif, system-ui, sans-serif"
    fontSize: "2.75rem"
    fontWeight: 600
    lineHeight: 1
    letterSpacing: "-0.03em"
    fontFeature: '"tnum" 1'
    fontVariation: '"wdth" 82'
  headline:
    fontFamily: "Instrument Sans, ui-sans-serif, system-ui, sans-serif"
    fontSize: "1.75rem"
    fontWeight: 600
    lineHeight: 1.15
    letterSpacing: "-0.02em"
  title-lg:
    fontFamily: "Instrument Sans, ui-sans-serif, system-ui, sans-serif"
    fontSize: "1.25rem"
    fontWeight: 600
    letterSpacing: "-0.01em"
  title:
    fontFamily: "Instrument Sans, ui-sans-serif, system-ui, sans-serif"
    fontSize: "1rem"
    fontWeight: 600
    letterSpacing: "-0.01em"
  body:
    fontFamily: "Instrument Sans, ui-sans-serif, system-ui, sans-serif"
    fontSize: "0.875rem"
    fontWeight: 400
    lineHeight: 1.5
    fontFeature: '"tnum" 1'
  body-sm:
    fontFamily: "Instrument Sans, ui-sans-serif, system-ui, sans-serif"
    fontSize: "0.8125rem"
    fontWeight: 400
    lineHeight: 1.5
  label:
    fontFamily: "Instrument Sans, ui-sans-serif, system-ui, sans-serif"
    fontSize: "0.75rem"
    fontWeight: 500
    lineHeight: 1.5
  tag:
    fontFamily: "Instrument Sans, ui-sans-serif, system-ui, sans-serif"
    fontSize: "11px"
    fontWeight: 600
    lineHeight: "18px"
    letterSpacing: "0.03em"
  mono-command:
    fontFamily: "JetBrains Mono, ui-monospace, SF Mono, Menlo, monospace"
    fontSize: "0.8125rem"
    fontWeight: 400
    lineHeight: 1.7
  mono-alias:
    fontFamily: "JetBrains Mono, ui-monospace, SF Mono, Menlo, monospace"
    fontSize: "0.75rem"
    fontWeight: 400
  mono-code:
    fontFamily: "JetBrains Mono, ui-monospace, SF Mono, Menlo, monospace"
    fontSize: "2.25rem"
    fontWeight: 600
    lineHeight: 1.2
    letterSpacing: "0.12em"
rounded:
  meter: "2px"
  tag: "4px"
  control: "6px"
  block: "10px"
  round: "50%"
spacing:
  s1: "4px"
  s2: "8px"
  s3: "12px"
  s4: "16px"
  s5: "24px"
  s6: "32px"
  s7: "48px"
  s8: "64px"
components:
  button-primary:
    backgroundColor: "{colors.accent}"
    textColor: "{colors.accent-ink}"
    typography: "{typography.body}"
    rounded: "{rounded.control}"
    padding: "0 16px"
    height: "40px"
  button-secondary:
    backgroundColor: "{colors.surface}"
    textColor: "{colors.ink}"
    typography: "{typography.body}"
    rounded: "{rounded.control}"
    padding: "0 16px"
    height: "40px"
  button-small:
    backgroundColor: "{colors.surface}"
    textColor: "{colors.ink}"
    typography: "{typography.body-sm}"
    rounded: "{rounded.control}"
    padding: "0 12px"
    height: "28px"
  button-copied:
    backgroundColor: "{colors.ok}"
    textColor: "{colors.accent-ink}"
    rounded: "{rounded.control}"
  tag-plan:
    backgroundColor: "{colors.sunken}"
    textColor: "{colors.ink-2}"
    typography: "{typography.tag}"
    rounded: "{rounded.tag}"
    padding: "0 5px"
  command-block:
    backgroundColor: "{colors.sunken}"
    textColor: "{colors.ink}"
    typography: "{typography.mono-command}"
    rounded: "{rounded.block}"
    padding: "12px 0 12px 16px"
  notice:
    backgroundColor: "{colors.warn-soft}"
    textColor: "{colors.ink}"
    rounded: "{rounded.control}"
    padding: "12px 12px 12px 16px"
  ledger-row-recommended:
    backgroundColor: "{colors.accent-soft}"
    textColor: "{colors.ink}"
    rounded: "{rounded.control}"
  meter:
    backgroundColor: "{colors.meter-track}"
    rounded: "{rounded.meter}"
    height: "4px"
  device-code:
    backgroundColor: "{colors.sunken}"
    textColor: "{colors.ink}"
    typography: "{typography.mono-code}"
    rounded: "{rounded.block}"
    padding: "24px 16px"
---

# Design System: codexctl web UI

## Overview

**Creative North Star: "The Quiet Ledger"**

The web UI reads like a well-kept ledger on warm paper. Ink text, hairline rules, and one cobalt accent do all the work; there are no cards, no shadows, and no decorative surfaces. Hierarchy comes from size, weight, and three gray steps of ink, never from boxes or fills. The few filled surfaces (command blocks, the plan tag, the device code, the single page notice, and the recommended ledger row) are filled because they hold something to copy, compare, or act on.

The system is dense but calm. Body text is 14 px with tabular figures so every column of numbers aligns. Numbers that answer the visitor's question are set large and condensed, using the variable width axis of Instrument Sans. Commands are set in JetBrains Mono exactly as typed, and they never wrap mid-token. Status color appears only for exceptions and always sits beside a word, so a monochrome reading loses nothing.

v3 keeps the v2 visual world recorded in `README.md` and changes structure, not material. The token deltas from v2 are: `ink-3` darkened from `#686c73` to `#63676e` (light) and `#8e9197` to `#8f9298` (dark) so tertiary text passes AA on the recommended row; `--t-display` replaced by a fixed 36 px answer heading; figures grown from 40 px to 44 px; body leading raised to 1.5; a 0.01em body tracking in dark mode; dark `surface`, `sunken`, `line`, `line-strong`, `accent-soft`, and `meter-track` slightly lifted; `bad-soft` removed; the unknown meter changed from a stripe to a dotted rule; and cobalt withdrawn from "In use" and "Renewal pending".

**Key Characteristics:**

- Warm paper ground, near-black ink, three ink steps for hierarchy.
- One cobalt accent for action and "use this"; amber and red for exceptions only, each with a word.
- Hairlines between rows and one stronger rule under each section heading; no cards, no shadows.
- Instrument Sans with tabular figures; condensed width for the answer heading and figures only.
- JetBrains Mono for aliases, commands, the device code, and the wordmark only.
- Dark mode swaps tokens; no component has dark-specific rules.

## Colors

A warm-neutral paper and ink palette with a single cobalt accent and two exception hues.

### Primary

- **Signal Cobalt** (`accent`): the only accent. Primary buttons, links, command flags, the "Recommended" and "Redeemable now" notes, the wordmark caret, and the focus ring. Its pale wash, **Cobalt Wash** (`accent-soft`), tints the recommended ledger row and text selection. **Accent Ink** (`accent-ink`) is text on a cobalt or green fill.

### Secondary

- **Exception Amber** (`warn`): text for low usage (below 20% left), stale usage, an expiring banked reset, and the usage-based billing note. **Amber Fill** (`warn-fill`) is the meter fill for low usage; **Amber Wash** (`warn-soft`) is the page notice ground.
- **Exhausted Red** (`bad`): text and dot for an exhausted window and "Login needs attention", and the bold clause in the answer's context line.

### Tertiary

- **Ledger Green** (`ok`): the dot of "Available" (the word stays ink) and the copied state of a Copy button. Never a text color for content.

### Neutral

- **Warm Paper** (`paper`): page ground.
- **White Sheet** (`surface`): secondary button ground.
- **Sunken Paper** (`sunken`): command blocks, plan tags, the device code.
- **Ledger Ink** (`ink`): primary text, headings, "In use".
- **Ink Two** (`ink-2`): secondary text, context lines, descriptions, "Renewal pending", stale figures.
- **Ink Three** (`ink-3`): tertiary text: column headers, counts, captions, reset times, aliases, footer. The lowest pair it sits on is the recommended row wash.
- **Hairline** (`line`): row dividers and figure-band rules. **Strong Rule** (`line-strong`): the rule under a section heading, button borders, the dotted unknown meter.
- **Meter Ink** (`meter`) on **Meter Track** (`meter-track`): the default meter.

Each `-dark` token is the counterpart of the same name under `prefers-color-scheme: dark`.

### Named Rules

**The One Cobalt Rule.** Cobalt means "act" or "use this". It never marks a status: "In use" is ink with an ink dot, and "Renewal pending" is ink-2 with a hollow dot.

**The Word Beside Every Color Rule.** Amber, red, and green never appear without a word that carries the same meaning. Healthy rows are ink on paper.

## Typography

**Display Font:** Instrument Sans (variable width 75 to 100%, weight 400 to 700; fallback ui-sans-serif, system-ui)
**Body Font:** Instrument Sans
**Label/Mono Font:** JetBrains Mono (fallback ui-monospace, SF Mono, Menlo)

**Character:** A plain workhorse UI sans whose width axis lets large numbers stand condensed and tight, paired with a mono that appears only where text is typed into a terminal. Both are self-hosted Latin subsets; the pages make no third-party request.

### Hierarchy

- **Answer** (600, 36 px, 1.08, width 90%): the one H1 that answers the page's question ("Use Everyday building", the landing headline). Drops to 28 px below 1040 px.
- **Figure** (600, 44 px, 1, width 82%, tabular): the two window figures under the answer, with a 20 px ink-3 `%` unit. 36 px on phones.
- **Headline** (600, 28 px, 1.15): H1 on single-panel pages (approve, connected).
- **Title Large** (600, 20 px): the landing's steps heading and figure units.
- **Title** (600, 16 px): section headings, ledger usage values, step headings, ledes and context lines (at weight 400).
- **Body** (400, 14 px, 1.5, tabular): all running and table text.
- **Body Small** (400, 13 px): descriptions, captions, hints, small buttons.
- **Label** (500, 12 px): column headers, notes under an account name, reset times, status details.
- **Tag** (600, 11 px, 18 px line, 0.03em): plan tag only.
- **Mono** (12 px aliases in ink-3; 13 px commands at 1.7 leading, 16 px weight 500 in the answer's command; 36 px weight 600 at 0.12em for the device code; 15 px weight 600 for the wordmark).

Headings use weight 600, -0.01em tracking, and balanced wrapping. Paragraphs use pretty wrapping. Rule lines cap at 62ch; ledes cap at 38ch.

### Named Rules

**The Tabular Figures Rule.** The body enables `tnum`, so every number in every column aligns. Mono and code reset it to normal.

**The Mono Means Typed Rule.** JetBrains Mono appears only for text a person types or reads back to a terminal: aliases, commands, the device code, and the wordmark.

**The Condensed Answer Rule.** The width axis narrows only the answer heading (90%) and figures (82%). Nothing else is condensed.

## Layout

Content sits in a centered column of 1200 px plus a fluid gutter (16 to 40 px). The spacing scale is a 4 px grid (4, 8, 12, 16, 24, 32, 48, 64). The 4 px and 8 px steps group items inside a cell (label to value, value to meter); 16, 24, and 48 px separate groups. Sections sit 48 px apart; main content has 32 px above and 64 px below. Ledger cells have 16 px vertical padding; machine rows 12 px.

Two-column pages split on a 12-part grid with a 48 to 64 px gap: the accounts overview puts the answer in 7 parts and "Needs attention" in 5; the signed-out landing puts sign-in in 5 and the connection steps in 7. Single-task pages (approve, connected) use one 440 px panel, top-centered.

Breakpoints: at 1040 px the lead gap tightens to 32 px and the answer drops to 28 px; at 900 px two-column pages stack (answer, then attention); at 860 px the account ledger and machines table restyle each row as a two-column grid with inline labels while explicit ARIA roles keep table semantics; at 640 px vertical rhythm tightens and figures drop to 36 px. No page scrolls sideways at 390 px; only long commands scroll inside their block.

### Named Rules

**The Lines, Not Boxes Rule.** Structure comes from one strong rule under each section heading and hairlines between rows, never from bordered containers.

## Elevation & Depth

The system is flat. There are no shadows anywhere. Depth is tonal: content sits on warm paper, and anything to copy or compare sits on sunken paper one step darker. Hairlines separate rows; the only lift is a 1 px press on an active button.

### Named Rules

**The Flat Paper Rule.** No `box-shadow`, no elevation tokens. A surface is either paper, sunken paper, or a wash (cobalt for the recommended row, amber for the notice).

## Shapes

Corners are gently rounded and consistent by role: 6 px for controls, the notice, and the recommended row; 10 px for blocks (command, device code); 4 px for the plan tag; 2 px for the meter. Dots (state, step numbers, the done mark) are full circles. Borders are 1 px hairlines; the only thicker strokes are the 1.5 px hollow state dot, the 1.5 px disclosure chevron drawn with borders, and the 2 px dotted unknown meter. The wordmark ends in a solid cobalt text caret.

## Components

### Buttons

Restrained and solid: they read as controls without ornament.

- **Shape:** gently rounded (6 px), 40 px tall, 1 px border.
- **Primary:** cobalt fill and border, accent-ink text, weight 600, 16 px side padding. One per decision: the answer's Copy, Sign in, Connect machine.
- **Secondary:** white sheet ground, strong-rule border, ink text. Hover darkens the border to ink-3.
- **Small:** 28 px tall, 12 px side padding, 13 px text. Copy buttons inside lists, Retry now.
- **Hover / Focus / Active:** primary hover mixes 14% ink into cobalt; 150 ms color transitions on the system ease-out; focus is a 2 px cobalt outline offset 2 px; active presses down 1 px.
- **Copied:** for 2 seconds after a copy, the label reads "Copied" and the button turns green (border and text, or fill on primary).
- **Link button:** inline text with a strong-rule underline that turns to the text color on hover (Sign out).

### State Indicator

A 7 px dot and one word, weight 500. Available: green dot, ink word. In use: ink dot, ink word. Nearly exhausted and stale: amber. Exhausted and login needs attention: red. Renewal pending: ink-2 word with a hollow dot. Idle: ink-3 with a hollow dot.

### Meter

A 4 px bar whose fill is what is left, the same quantity as the figure beside it. Meter ink by default, amber fill below 20% left, gray at 55% opacity inside a stale row. Unknown is a 2 px dotted rule with the word "Unknown". Always `aria-hidden`; the number carries the value.

### Command Block

Sunken paper, 10 px corners, mono 13 px at 1.7 leading, `white-space: pre` with horizontal scroll inside the block. Multi-line commands use `\` continuations; flags are cobalt, placeholders ink-2 italic, the `$ ` prompt ink-3 and unselectable (not copied). The Copy button sits at the right edge. In the answer block, the command grows to 16 px weight 500.

### Plan Tag

Sunken paper, 4 px corners, 11 px weight 600 with 0.03em tracking, hugging its text. Carries the plan only (PRO, TEAM, PLUS).

### Notice

Amber wash, 6 px corners, a bold amber lead word and a next-step button. One per page for a page-wide condition (refresh failed, signed out); never repeated on rows.

### Account Ledger

A full-width table: account (name, mono alias, plan tag, one-line notes), 5-hour window, 7-day window, banked resets, state. Column headers are 12 px ink-3; cells are top-aligned with a hairline above. The recommended row carries a cobalt wash with 6 px rounded ends. Below 860 px each row becomes a two-column grid with the state top right.

### Answer Figures

Two figures side by side in a band ruled above and below by hairlines: condensed 44 px value, 4 px meter, 13 px ink-2 label with the reset time in ink-3.

### Section Heading

Title left, quiet ink-3 meta right, an ink-3 count beside the title, and one strong rule underneath.

### Disclosure

A native `details` summary in ink-2 weight 500 with a 6 px border-drawn chevron that rotates on open. Its open state syncs to the URL.

## Do's and Don'ts

### Do:

- **Do** open every page on the answer to its visitor's question, with the action beside it.
- **Do** fill meters with what is left, matching the figure next to them.
- **Do** put a word beside every amber, red, or green mark.
- **Do** show "Unknown" with a dotted rule, and keep stale figures in ink-2 with "Last observed" and an age.
- **Do** set commands exactly as typed in a sunken block with a Copy button; scroll long lines inside the block.
- **Do** keep standalone text controls at least 24 px tall, and 44 px on coarse pointers.
- **Do** swap tokens for dark mode; never add dark-specific component rules.

### Don't:

- **Don't** use cobalt for a status such as "In use" or "Renewal pending"; it marks action and "use this" only.
- **Don't** wrap content in bordered or shadowed cards; use hairlines and the section rule.
- **Don't** add shadows or elevation of any kind.
- **Don't** put eyebrow or kicker labels above headings; a heading and a count are enough.
- **Don't** condense any text other than the answer heading and figures.
- **Don't** use mono for prose, labels, or numbers that are not typed into a terminal.
- **Don't** repeat a page-wide notice or badge on every row.
- **Don't** let a command token break across lines.
- **Don't** use a patterned or striped fill to mean "unknown".
