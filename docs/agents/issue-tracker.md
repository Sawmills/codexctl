# Issue tracker: Linear

Track codexctl work in Sawmills Linear. Use issue keys such as `SAW-1234` or full Linear issue URLs.
Use GitHub for code, pull requests, and CI.

## Workflow

- Use the installed `linear` skill for authenticated reads and authorized writes. Prefer the available Linear connector; use that skill's CLI fallback when needed.
- Before creating an issue, search Linear for an existing issue that covers the work. Reuse it when it matches.
- Read the issue description, comments, labels, status, and linked documents before implementation or triage. Follow pagination.
- When a skill says "publish to the issue tracker", create or update the authorized Linear issue in the Sawmills team.
- When a skill says "fetch the relevant ticket", read the named Linear issue and its relevant comments and documents.
- Use `docs/agents/triage-labels.md` for label names. Preserve unrelated labels and fields.
- Use Linear parent/sub-issue and blocking relations for maps, child tickets, and dependencies. Match actual team statuses before changing a status.
- Claim work through the authorized Linear assignment workflow. Do not take another owner's active issue.
- Maintain the single Codex session resume breadcrumb required by the `linear` skill during ticket implementation.
- Keep comments concise: result, evidence, blocker, and next action. Close an issue only when its acceptance criteria are met and closure is authorized.

## Scope

**PRs as a request surface: no.** Pull requests are code-review artifacts, not the issue triage queue.
This configuration does not create issues, labels, or change Linear state by itself.
