# CLI Forecast Visualization

## Recommendation

For codexctl maintainers: keep the existing `comfy-table` and `crossterm` dependencies for the static forecast. Spend the effort on a clear answer, aligned dates, compact account details, and visible uncertainty. Reserve Ratatui for a future interactive or continuously refreshed view.

The repository declares Rust 1.89, `comfy-table = "7"`, and `crossterm = "0.29"`. The lockfile selects comfy-table 7.2.2. At the start of this research, the forecast produced a string and applied terminal color separately. These facts come from [Cargo.toml](../../Cargo.toml), [Cargo.lock](../../Cargo.lock), [forecast.rs](../../src/forecast.rs), and [status.rs](../../src/commands/status.rs).

## Library Fit

**comfy-table 7.2.2 with crossterm 0.29: recommended.** Comfy-table supplies dynamic column layout, explicit width limits, terminal detection, rounded borders, and cell styling. Crossterm supplies terminal size and color support. Integration cost is low because both are installed. Use the table library for aligned account rows; keep forecast-specific timeline aggregation in a small rendering helper. [Table API](https://docs.rs/comfy-table/7.2.2/comfy_table/struct.Table.html), [Crossterm API](https://docs.rs/crossterm/0.29.0/crossterm/)

Both packages use the MIT license. Crossterm declares Rust 1.63. Comfy-table's changelog states that 7.2.0 moved to Rust 1.85. These declared requirements fit this repository's Rust 1.89 floor; final compatibility still needs the repository build. Comfy-table published 7.2.2 in January 2026 and 8.0.0 in August 2026. This display change does not require a major upgrade. [Comfy-table Registry Metadata](https://crates.io/api/v1/crates/comfy-table), [Crossterm Registry Metadata](https://crates.io/api/v1/crates/crossterm), [Comfy-table Changelog](https://github.com/Nukesor/comfy-table/blob/main/CHANGELOG.md)

**Ratatui 0.30.2: suitable for a richer future view.** Its inline viewport embeds widgets in a normal CLI flow. Its in-memory `TestBackend` can verify rendered cells without a terminal. The package uses MIT and declares Rust 1.88; its June 2026 release supplies recent maintenance evidence. [Viewport API](https://docs.rs/ratatui/0.30.2/ratatui/enum.Viewport.html), [TestBackend API](https://docs.rs/ratatui/0.30.2/ratatui/backend/struct.TestBackend.html), [Registry Metadata](https://crates.io/api/v1/crates/ratatui)

Ratatui would add widget layout and either terminal lifecycle management or conversion of its cell buffer to printable text. That is an integration cost assessment based on these APIs. The current command prints once and exits, so those capabilities do not justify another dependency for this change.

**textplots 0.8.7: useful for numeric plots.** It supports terminal line plots and explicit chart dimensions. Its MIT release dates to February 2025; registry metadata provides no declared minimum Rust version. Adoption would require a Rust 1.89 build check. We would still need separate summary, calendar labels, account layout, and uncertainty text. A discrete availability timeline better matches this task than a general line chart. [Plot API](https://docs.rs/textplots/0.8.7/textplots/), [Registry Metadata](https://crates.io/api/v1/crates/textplots)

**Charming: outside this command's needs.** Its supported renderers target HTML, images, and WebAssembly. Image rendering adds a JavaScript engine, and the project provides no minimum Rust version guarantee. It uses MIT or Apache-2.0. Consider it if a browser or exported image becomes a requirement. [Project Documentation](https://github.com/yuankunzhang/charming)

These checks establish API fit, license terms, and release evidence. They are not a dependency security audit. The visualization uses existing dependencies. History compression separately adds `flate2`, which requires the normal dependency and advisory checks.

## Proposed Presentation

These are design recommendations for this forecast, not usability study results:

1. Lead with the user's question: “At least one account for the next 7 days?” Answer with a qualified forecast verdict. Place incomplete coverage and provisional rates directly below it.
2. Show the lowest projected account count as a number. Readers should not need to infer it from the lowest filled chart row.
3. Align the chart to seven labeled intervals. Show the minimum availability within each interval so a brief outage cannot disappear in an average. State the interval boundaries and local time.
4. Use restrained cyan for headings, amber for uncertainty, and red for outages. Keep words or symbols beside color so monochrome output carries the same meaning.
5. Place compact account details below the summary: remaining allowance, expected exhaustion, reset time, and evidence quality. Preserve full account identity somewhere in the output.
6. Use a stacked layout on narrow terminals. Prefer a bounded display width on wide terminals. Test long account names and Unicode display widths.

Keep prediction semantics unchanged. Describe quota percentages rather than token counts. State that each account keeps its observed pace and that moving work between accounts changes the result. A polished display must not imply stronger evidence than the forecasting model supplies.

## Terminal Contract and Verification

Keep output in scrollback and usable through a pipe. Suppress color for redirected output and honor `NO_COLOR`. The convention defines a nonempty variable as disabling default color; the current application also disables it for an empty value. Preserve that existing behavior in this presentation change. [NO_COLOR Convention](https://no-color.org/), [Current Output Policy](../../src/commands/status.rs)

Check 40, 80, and 120 columns; long Unicode aliases; monochrome output; missing history; incomplete coverage; and a short outage inside a chart interval. Verify that narrower views preserve the verdict, warning, and outage timing. Use existing forecast fixtures to keep rendering tests separate from prediction changes.
