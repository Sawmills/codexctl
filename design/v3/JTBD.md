# codexctl Web UI v3: Jobs to Be Done

This document derives the web app's jobs from the people who arrive and the moment they arrive in, not from the pages that exist today. The ranking decides the information architecture at the end. Terms follow [`CONTEXT.md`](../../CONTEXT.md): company user, machine, account server, server account, alias, login renewal.

## Who Arrives, and When

Three people reach the app. Two of them are the same engineer on different days.

1. **The stuck engineer.** Codex has just stopped in the middle of work. The terminal says "You've hit your usage limit", shows HTTP 429, or the provider helper returned no token. They switch to the browser with one hand still on the keyboard. They have seconds of patience and want a command to paste.
2. **The engineer on a new machine.** A new laptop, a fresh dev box, or a colleague's advice to "use codexctl". They are calm but unfamiliar, and they follow the page literally.
3. **The owner of many accounts** (Amir today, any heavy user tomorrow). Several server accounts, several machines, banked resets that expire. They come at the start of the day, before a long agent run, or after an agent pane stalls, and they scan for anything wrong.

## The Jobs, Ranked

| Rank | Job | Who | How often | Cost of a slow answer |
| --- | --- | --- | --- | --- |
| 1 | Get unblocked now | Stuck engineer | Several times a week per engineer | Work stops; the engineer guesses an alias or pays for credits |
| 2 | Check that every account is healthy | Owner | Daily | A broken login or an expired reset is found only when it blocks job 1 |
| 3 | Connect a new machine | New-machine engineer | Once per machine | Setup stalls; the engineer asks a colleague |
| 4 | Confirm a machine approval is safe | New-machine engineer | Once per machine | A wrong approval gives a stranger token delivery |

Jobs 3 and 4 are one flow (install, connect, approve, done), but they happen on different pages and ask different questions, so they stay separate rows.

## Job 1: Get Unblocked Now

- **Trigger.** Codex stops with a usage limit, a 429, or a refused token.
- **Question in their head.** "Which account still has room, and what do I type?"
- **Answer the page must give.** One account, named by label and alias, with how much it has left in each window, and the exact command: `codexctl use <alias>`. The page must also say what that command does to the work in flight: running Codex sessions on that machine move to the new account within 60 seconds.
- **How the page picks.** The same rule as automatic selection in `codexctl use` (v0.1.37): only accounts with included usage (never usage-based billing), fresh data, and every window below 100% qualify. Accounts at or above 95% used in any window drop out while another qualifying account is below 95%. Among the rest, the soonest 7-day reset wins, then the most headroom. The page states this rule in one line and never switches anything itself.
- **When nothing qualifies.** The page says so plainly, then gives the best next step in this order:
  1. An exhausted included account with a redeemable banked reset: `codexctl reset <alias>`, then `codexctl use <alias>`.
  2. No reset to redeem: the time the soonest window resets, and on which account.
  3. Usage-based accounts exist: name them, say that they bill credits, and say that `codexctl use <alias>` asks before it bills.
- **When the data is stale.** A stale figure is not proof of headroom. The page says "Cannot confirm headroom" and offers bare `codexctl use`, which checks live usage before it switches.
- **Action.** Copy one command.
- **Must not be on screen first.** Install steps, plan tags, revoked machines, documentation links, marketing copy, a decorative terminal, and any figure that does not change the choice.

## Job 2: Check That Every Account Is Healthy

- **Trigger.** Start of day, before a long unattended run, or an agent pane that stalled.
- **Question in their head.** "Is anything broken, running out, or about to be wasted? Which machine uses what?"
- **Answer the page must give.** First, a short list of exceptions with the fix next to each one:

  | Exception | Fix shown |
  | --- | --- |
  | Login needs attention (`state == "unavailable"`) | `codexctl login <alias>` |
  | Renewal pending (`state == "renewal_pending"`) | Finish the OpenAI sign-in on the machine that started it, or `codexctl login <alias> --cancel` |
  | Exhausted with a redeemable reset | `codexctl reset <alias>` |
  | Exhausted, no reset | When the window resets |
  | Nearly exhausted (under 20% left) | When the window resets |
  | Stale usage | Last observed time; the server refreshes every 60 seconds |
  | Banked reset expiring within 3 days | Expiry date, and that a reset can only be spent once a window is exhausted |

  Then the full account list, one row per account with both windows, banked resets, and state. Then the machines: which account each one last received a token for, and when.
- **Action.** Copy a fix command, or read and leave satisfied.
- **Must not be on screen.** A badge on every healthy row, a "stale" tag repeated on every row when the whole page is stale (one page notice instead), live-connectivity claims for machines (the server only knows the last token delivery), and revoked machines outside a closed disclosure.

## Job 3: Connect a New Machine

- **Trigger.** A new machine, or a teammate's recommendation.
- **Question in their head.** "What do I run, in what order, and how do I know it worked?"
- **Answer the page must give.** Two commands and one browser step: install codexctl (Homebrew on macOS; a pinned, checksum-verified script on Linux), then `codexctl connect --server <this server> --name "<machine>"` with this server already filled in, then approve in the browser window that command opens.
- **Action.** Copy, paste, return.
- **Must not be on screen.** Account data (the visitor is signed out), feature lists that restate the product, decorative terminals, and the Linux script for macOS visitors (it stays in a disclosure).
- **Second visitor on the same page.** A returning engineer whose browser session expired lands here too, often in the middle of job 1. "Sign in with company SSO" must be the first control on the page, so that visitor never reads the setup steps.

## Job 4: Confirm a Machine Approval Is Safe

- **Trigger.** `codexctl connect` opened the browser.
- **Question in their head.** "Is this my machine and my account?"
- **Answer the page must give.** The signed-in company identity, the machine name, and the code to compare with the terminal, large enough to compare character by character.
- **Action.** Approve, or close the page to cancel.
- **After approval.** One line that the machine is connected, and the next command: `codexctl use` to pick an account. First machine with local profiles: `codexctl migrate` moves them to the account server.
- **Must not be on screen.** Brand storytelling, a second call to action, anything that competes with the code.

## Information Architecture

The ranking gives this structure. Every page renders from data the server already has; the one small addition is listed below.

| Route | Serves | Order on the page |
| --- | --- | --- |
| `/` (signed out) | Job 3, and the sign-in door for jobs 1 and 2 | Sign in first, then three setup steps |
| `/accounts` (signed in) | Job 1, then job 2 | The answer and its command, then "Needs attention", then all accounts, then machines |
| `/auth/approve` | Job 4 | Identity, machine, code, approve |
| Connected page | Job 4 follow-up | Confirmation, next command |

Changes from v2:

- The overview opens on the answer to job 1 instead of on machine columns. Machines move to the end as a compact list, because the account rows already carry every figure the machine columns repeated.
- A "Needs attention" list is new. It serves job 2 by putting every exception and its fix in one place, so the owner does not scan seven rows to find two problems.
- The landing page puts sign-in before setup, because a returning engineer in the middle of job 1 lands there when the browser session has expired.
- The plan column leaves the account table. Plan does not change any decision; billing class does, and it stays next to the account name.

The recommendation, the attention list, and the sort all derive in the browser from `GET /accounts/data` as it exists in `src/central/dashboard.rs`. One small server addition would sharpen the overview: a `routing_refused` flag, so that a routing refusal reads "Routing refused" instead of the login hint (both are `state == "unavailable"` today). The prototype shows the login hint, which is correct for the current data.
