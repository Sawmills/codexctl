#!/usr/bin/env python3
"""Render the codexctl UI v3 overview prototypes from fixture data.

The recommendation, the "Needs attention" list, and the ledger order are
derived here with the same rules the server-rendered page will use, so the
fixtures double as a worked example for the Rust templates.

Run: python3 design/build_v3.py   (writes design/v3/overview*.html)
"""

from __future__ import annotations

import html
from dataclasses import dataclass, field, replace
from pathlib import Path

OUT = Path(__file__).parent / "v3"
NOW_TITLE = "Oct 3, 9:39:52 AM EDT"
SWITCH_THRESHOLD_LEFT = 5  # 95% used or more
LOW_LEFT = 20


def e(text: str) -> str:
    return html.escape(text, quote=True)


@dataclass
class Window:
    left: int
    reset_in: str  # "3h 31m"
    iso: str
    local: str  # "1:11 PM"
    title: str  # "Oct 3, 1:11 PM EDT"


@dataclass
class Banked:
    count: int
    redeemable: int = 0
    expires: str = ""  # nearest expiry, "Oct 19"
    expires_in_days: int = 99


@dataclass
class Account:
    label: str
    alias: str
    plan: str
    five: Window | None
    seven: Window | None
    banked: Banked | None
    state: str = "available"  # available | renewal_pending | unavailable
    billing: str = "rate_limited"  # rate_limited | usage_based
    stale: bool = False
    age: str = "8 s ago"
    in_use_on: str | None = None
    pending_note: str = ""

    @property
    def windows(self):
        return [("5-hour", self.five), ("7-day", self.seven)]

    @property
    def exhausted(self):
        return any(w and w.left <= 0 for _, w in self.windows)

    @property
    def min_left(self):
        known = [w.left for _, w in self.windows if w]
        return min(known) if known else None


@dataclass
class Machine:
    name: str
    alias: str | None
    seen: str
    seen_iso: str
    seen_title: str
    state: str  # live | idle | revoked


@dataclass
class Scene:
    file: str
    nav: str
    accounts: list[Account]
    machines: list[Machine]
    refresh_failed: bool = False
    failed_age: str = ""
    notes: dict = field(default_factory=dict)


# ---------- Rules ----------


def recommend(accounts: list[Account], ignore_stale: bool = False):
    """Same rule as automatic selection in `codexctl use` (v0.1.37)."""
    pool = [
        a
        for a in accounts
        if a.state == "available"
        and a.billing == "rate_limited"
        and (ignore_stale or not a.stale)
        and a.five
        and a.seven
        and not a.exhausted
    ]
    if any(a.min_left > SWITCH_THRESHOLD_LEFT for a in pool):
        pool = [a for a in pool if a.min_left > SWITCH_THRESHOLD_LEFT]
    pool.sort(key=lambda a: (a.seven.iso, -a.min_left))
    return pool[0] if pool else None


def reset_target(accounts: list[Account]):
    """Exhausted included account whose banked reset expires soonest."""
    pool = [
        a
        for a in accounts
        if a.state == "available"
        and a.billing == "rate_limited"
        and a.exhausted
        and a.banked
        and a.banked.redeemable > 0
    ]
    pool.sort(key=lambda a: a.banked.expires_in_days)
    return pool[0] if pool else None


def category(a: Account, pick: Account | None):
    if a.in_use_on:
        return 0
    if a is pick:
        return 1
    if a.state == "unavailable":
        return 7
    if a.state == "renewal_pending":
        return 6
    if a.stale:
        return 5
    if a.exhausted:
        return 4
    if a.min_left is not None and a.min_left < LOW_LEFT:
        return 3
    return 2


def issues(scene: Scene, pick: Account | None):
    out = []
    target = None if (pick or scene.refresh_failed) else reset_target(scene.accounts)
    for a in scene.accounts:
        if a.state == "unavailable":
            out.append((0, a, "bad", "Login needs attention",
                        "The account server cannot refresh this login. Renew it from any connected machine.",
                        f"codexctl login {a.alias}"))
        elif a.state == "renewal_pending":
            out.append((1, a, "pending", "Renewal pending",
                        f"{a.pending_note} Only the machine that started it can finish it; this command resumes it there.",
                        f"codexctl login {a.alias}"))
        elif a.exhausted:
            name, w = next((n, w) for n, w in a.windows if w and w.left <= 0)
            when = f'<time datetime="{w.iso}" title="{e(w.title)}">{e(w.local)}</time>'
            if a is target:
                n = a.banked.redeemable
                text = (f"{n} banked reset{'s' if n > 1 else ''} ready. "
                        "The next step on this page spends the one closest to expiry.")
                cmd = None
            elif target and a.banked and a.banked.redeemable > 0:
                text = (f"{a.banked.redeemable} banked reset ready, expiring {e(a.banked.expires)}. "
                        f"The next step spends {e(target.label)}’s first because it expires sooner.")
                cmd = None
            elif a.banked and a.banked.redeemable > 0:
                n = a.banked.redeemable
                text = (f"{n} banked reset{'s' if n > 1 else ''} can clear the {name} window now. "
                        f"Otherwise it resets at {when}.")
                cmd = f"codexctl reset {a.alias}"
            else:
                text, cmd = f"The {name} window resets at {when}.", None
            out.append((0, a, "bad", "Exhausted", text, cmd))
        elif a.stale and not scene.refresh_failed:
            out.append((2, a, "warn", "Stale usage",
                        f"Last observed {a.age}. The server retries every 60 seconds.", None))
        elif a.min_left is not None and a.min_left < LOW_LEFT and not a.stale:
            name, w = min(((n, w) for n, w in a.windows if w), key=lambda x: x[1].left)
            out.append((2, a, "warn", f"{w.left}% left",
                        f'The {name} window resets in <time datetime="{w.iso}" title="{e(w.title)}">{e(w.reset_in)}</time>.',
                        None))
        if (a.banked and a.banked.expires_in_days <= 3 and not a.exhausted
                and a.state == "available"):
            n = a.banked.count
            lead = (f"The next of {n} banked resets expires" if n > 1
                    else "The banked reset expires")
            out.append((2, a, "warn", f"Reset expires {a.banked.expires}",
                        f"{lead} in {a.banked.expires_in_days} days. A reset can be spent only after a window is exhausted.",
                        None))
    out.sort(key=lambda i: i[0])
    return out


# ---------- Markup ----------


def alias(a: str) -> str:
    return f'<span class="alias" translate="no">{e(a)}</span>'


def command(cid: str, lines: list[str], primary=False, prompt=False) -> str:
    body = "\n".join(
        (f'<span class="prompt">$ </span>' if prompt else "") + e(line) for line in lines
    )
    label = " and ".join(lines)
    cls = "button primary copy" if primary else "button small copy"
    return (
        f'<div class="command">\n'
        f'  <pre><code id="{cid}" translate="no">{body}</code></pre>\n'
        f'  <button class="{cls}" type="button" data-copy="{cid}" aria-label="Copy {e(label)}">Copy</button>\n'
        f"</div>"
    )


def figure(name: str, w: Window) -> str:
    return (
        f'<div class="figure">\n'
        f'  <span class="figure-value">{w.left}<small>%</small></span>\n'
        f'  <div class="meter{" warn" if w.left < LOW_LEFT else ""}" aria-hidden="true"><i style="--left: {w.left}%"></i></div>\n'
        f'  <span class="figure-label">{name} left · <span>resets in <time datetime="{w.iso}" title="{e(w.title)}">{e(w.reset_in)}</time></span></span>\n'
        f"</div>"
    )


RULE = (
    'Chosen like <code translate="no">codexctl use</code>: included usage only, '
    "no account at 95% used or more while another is below, soonest 7-day reset first."
)


def stuck_line(scene: Scene) -> str:
    for m in scene.machines:
        if m.state != "live":
            continue
        a = next((x for x in scene.accounts if x.alias == m.alias), None)
        if a and a.exhausted:
            name, w = next((n, w) for n, w in a.windows if w and w.left <= 0)
            return (f'<p class="context">{e(m.name)} is on {e(a.label)}: <b>{name} window exhausted</b> '
                    f'until <time datetime="{w.iso}" title="{e(w.title)}">{e(w.local)}</time>.</p>')
    return ""


def stuck_machine(scene: Scene):
    for m in scene.machines:
        a = next((x for x in scene.accounts if x.alias == m.alias), None)
        if m.state == "live" and a and a.exhausted:
            return m.name
    return None


def effect(scene: Scene, label: str) -> str:
    where = stuck_machine(scene)
    run = f"Run it on {e(where)}." if where else "Run it on the machine that stopped."
    return f'<p class="effect">{run} Its running Codex sessions move to {e(label)} within 60 seconds.</p>'


def answer(scene: Scene) -> str:
    pick = None if scene.refresh_failed else recommend(scene.accounts)
    updated = f'Updated <time datetime="2026-10-03T13:39:52Z" title="{NOW_TITLE}">8 s ago</time>.'
    parts = []
    if pick and pick.in_use_on:
        parts += [
            f'<h1 id="answer-title">{e(pick.label)} {alias(pick.alias)} is the best account now</h1>',
            f'<p class="context">{e(pick.in_use_on)} already uses it. Nothing needs a switch.</p>',
            f'<div class="figures">\n{figure("5-hour", pick.five)}\n{figure("7-day", pick.seven)}\n</div>',
            '<p class="next-step">To move another machine to it, run this there:</p>',
            command("cmd-use", [f"codexctl use {pick.alias}"], prompt=True),
            f'<p class="effect">On that machine, running Codex sessions move to {e(pick.label)} within 60 seconds.</p>',
            f'<p class="rule">{RULE} {updated}</p>',
        ]
    elif pick:
        parts += [
            f'<h1 id="answer-title">Use {e(pick.label)} {alias(pick.alias)}</h1>',
            stuck_line(scene),
            f'<div class="figures">\n{figure("5-hour", pick.five)}\n{figure("7-day", pick.seven)}\n</div>',
            command("cmd-use", [f"codexctl use {pick.alias}"], primary=True, prompt=True),
            effect(scene, pick.label),
            f'<p class="rule">{RULE} {updated}</p>',
        ]
    elif scene.refresh_failed:
        last = recommend(scene.accounts, ignore_stale=True)
        parts += [
            '<h1 id="answer-title">Cannot confirm which account has room</h1>',
            f'<p class="context">The last observation is {e(scene.failed_age)} old. At that time, '
            f'the best match was {e(last.label)} {alias(last.alias)}.</p>',
            f'<div class="figures stale">\n{figure("5-hour", last.five)}\n{figure("7-day", last.seven)}\n</div>',
            command("cmd-use", ["codexctl use"], primary=True, prompt=True),
            '<p class="effect">Without an alias, <code translate="no">codexctl use</code> checks live usage before it switches. '
            "Running Codex sessions on the machine you run it on move within 60 seconds.</p>",
            f'<p class="rule">{RULE}</p>',
        ]
    else:
        target = reset_target(scene.accounts)
        parts += [
            '<h1 id="answer-title">No included account has confirmed room</h1>',
            stuck_line(scene),
        ]
        if target:
            b = target.banked
            name, w = next((n, w) for n, w in target.windows if w and w.left <= 0)
            parts += [
                f'<p class="next-step">Spend a banked reset on <b>{e(target.label)}</b> {alias(target.alias)}. '
                f"Its next reset expires {e(b.expires)}, sooner than any other.</p>",
                command("cmd-use", [f"codexctl reset {target.alias}", f"codexctl use {target.alias}"],
                        primary=True, prompt=True),
                f'<p class="effect">The reset clears the exhausted {name} window. '
                + (f"Run it on {e(stuck_machine(scene))}; its" if stuck_machine(scene) else "Its")
                + f" running Codex sessions move to {e(target.label)} within 60 seconds.</p>",
            ]
        alt = []
        for a in scene.accounts:
            if (a.stale and a.state == "available" and a.billing == "rate_limited"
                    and not a.exhausted and a.min_left):
                alt.append(f"<li>{e(a.label)} {alias(a.alias)} showed at least {a.min_left}% left in both windows {e(a.age)}, too old to confirm. "
                           'Bare <code translate="no">codexctl use</code> checks live usage and picks it if the room is still there.</li>')
        soon = sorted(
            ((w, a, n) for a in scene.accounts if a.billing == "rate_limited" and a.state == "available"
             for n, w in a.windows if w and w.left <= 0),
            key=lambda x: x[0].iso,
        )
        if soon:
            w, a, n = soon[0]
            alt.append(f"<li>Or wait: the {n} window on {e(a.label)} resets at "
                       f'<time datetime="{w.iso}" title="{e(w.title)}">{e(w.local)}</time> (in {e(w.reset_in)}).</li>')
        for a in scene.accounts:
            if a.billing == "usage_based" and a.state == "available":
                alt.append(f"<li>{e(a.label)} {alias(a.alias)} has room but bills credits. "
                           f'<code translate="no">codexctl use {e(a.alias)}</code> asks before it bills.</li>')
        if alt:
            parts.append('<ul class="alternatives">' + "".join(alt) + "</ul>")
        parts.append(f'<p class="rule">{updated}</p>')
    body = "\n".join(p for p in parts if p)
    return f'<section class="answer" aria-labelledby="answer-title">\n{body}\n</section>'


def attention(scene: Scene, pick) -> str:
    items = issues(scene, pick)
    if not items:
        inner = ('<div class="all-clear"><span class="state ok">Nothing needs attention</span>'
                 f"<p>All {len(scene.accounts)} accounts can deliver tokens.</p></div>")
    else:
        rows = []
        for i, (_, a, kind, word, text, cmd) in enumerate(items):
            fix = command(f"fix-{a.alias}-{i}", [cmd]) if cmd else ""
            rows.append(
                f'<li class="issue">\n'
                f'  <div class="issue-head"><span class="issue-account"><b>{e(a.label)}</b>{alias(a.alias)}</span>'
                f'<span class="state {kind}">{e(word)}</span></div>\n'
                f"  <p>{text}</p>\n  {fix}\n</li>"
            )
        inner = "<ul>\n" + "\n".join(rows) + "\n</ul>"
    count = f' <span class="count">{len(items)}</span>' if items else ""
    return (
        f'<section class="attention" aria-labelledby="attention-title">\n'
        f'<div class="section-head"><h2 id="attention-title">Needs attention{count}</h2></div>\n'
        f"{inner}\n"
        f'<p id="copy-status" class="copy-status" role="status" aria-live="polite"></p>\n'
        f"</section>"
    )


def usage_cell(name: str, w: Window | None) -> str:
    label = f'<span class="cell-label" aria-hidden="true">{name}</span>'
    if w is None:
        return (f'<td role="cell" class="cell-usage">{label}<div class="usage unknown">'
                f'<div class="usage-top"><span class="usage-value">Unknown</span></div>'
                f'<div class="meter unknown" aria-hidden="true"></div></div></td>')
    tone = " bad" if w.left <= 0 else " warn" if w.left < LOW_LEFT else ""
    meter = " warn" if 0 < w.left < LOW_LEFT else ""
    return (
        f'<td role="cell" class="cell-usage">{label}<div class="usage{tone}">'
        f'<div class="usage-top"><span class="usage-value">{w.left}%<small>left</small></span>'
        f'<span class="usage-reset">resets in <time datetime="{w.iso}" title="{e(w.title)}">{e(w.reset_in)}</time></span></div>'
        f'<div class="meter{meter}" aria-hidden="true"><i style="--left: {w.left}%"></i></div></div></td>'
    )


def banked_cell(b: Banked | None) -> str:
    label = '<span class="cell-label" aria-hidden="true">Banked resets</span>'
    if b is None:
        inner = "<span>Unknown</span>"
    elif b.count == 0:
        inner = "<span>None</span>"
    else:
        parts = [f"<b>{b.count}</b>"]
        if b.redeemable:
            parts.append('<span class="redeemable">Redeemable now</span>')
        cls = ' class="expiring"' if b.expires_in_days <= 3 else ""
        nxt = "next expires" if b.count > 1 else "expires"
        parts.append(f"<span{cls}>{nxt} {e(b.expires)}</span>")
        inner = "".join(parts)
    return f'<td role="cell" class="cell-banked">{label}<div class="banked">{inner}</div></td>'


def state_cell(a: Account, scene: Scene) -> str:
    if a.state == "unavailable":
        s, d = '<span class="state bad">Login needs attention</span>', f"Last observed {a.age}"
    elif a.state == "renewal_pending":
        s, d = '<span class="state pending">Renewal pending</span>', "Waiting for OpenAI sign-in"
    elif scene.refresh_failed:
        word = "Exhausted" if a.exhausted else "Available"
        kind = "bad" if a.exhausted else "ok"
        s, d = f'<span class="state {kind}">{word}</span>', ""
    elif a.stale:
        s, d = '<span class="state warn">Stale usage</span>', f"Last observed {a.age}"
    elif a.exhausted:
        s, d = '<span class="state bad">Exhausted</span>', ""
    elif a.min_left is not None and a.min_left < LOW_LEFT:
        s, d = '<span class="state warn">Nearly exhausted</span>', ""
    else:
        s = '<span class="state ok">Available</span>'
        d = "Never chosen automatically" if a.billing == "usage_based" else ""
    detail = f'<span class="status-detail">{e(d)}</span>' if d else ""
    return f'<td role="cell" class="cell-state"><div class="status">{s}{detail}</div></td>'


def ledger(scene: Scene, pick) -> str:
    rows = []
    target = None if (pick or scene.refresh_failed) else reset_target(scene.accounts)
    pick = pick or target
    order = sorted(
        scene.accounts,
        key=lambda a: (category(a, pick), a is not pick,
                       -(a.min_left if a.min_left is not None else -1), a.alias),
    )
    for a in order:
        notes = []
        if a.in_use_on:
            notes.append(f'<span class="note live">In use on {e(a.in_use_on)}</span>')
        if a is pick:
            word = "Spend a reset here" if a is target else "Recommended"
            notes.append(f'<span class="note pick">{word}</span>')
        if a.billing == "usage_based":
            notes.append('<span class="note billing">Usage-based · bills credits</span>')
        cls = []
        if a is pick:
            cls.append("recommended")
        if a.stale or scene.refresh_failed:
            cls.append("stale")
        cls_attr = f' class="{" ".join(cls)}"' if cls else ""
        rows.append(
            f'<tr role="row"{cls_attr}>\n'
            f'<td role="cell" class="cell-account"><div class="account-name"><strong>{e(a.label)}</strong>'
            f'<span class="line">{alias(a.alias)}<span class="tag">{e(a.plan)}</span></span>{"".join(notes)}</div></td>\n'
            f"{usage_cell('5-hour', a.five)}\n{usage_cell('7-day', a.seven)}\n{banked_cell(a.banked)}\n{state_cell(a, scene)}\n"
            f"</tr>"
        )
    sub = ("Last observation, in use first" if scene.refresh_failed
           else "In use first, then the next step, then most room; stale and unknown last" if target
           else "In use first, then the recommendation, then most room; stale and unknown last" if pick
           else "In use first, then most room; stale and unknown last")
    return f"""<section class="section" aria-labelledby="accounts-title">
<div class="section-head">
  <h2 id="accounts-title">Accounts <span class="count">{len(scene.accounts)}</span></h2>
  <p>{sub}</p>
</div>
<table class="ledger" role="table" aria-labelledby="accounts-title">
<thead role="rowgroup"><tr role="row">
  <th role="columnheader" scope="col">Account</th>
  <th role="columnheader" scope="col" class="col-usage">5-hour window</th>
  <th role="columnheader" scope="col" class="col-usage">7-day window</th>
  <th role="columnheader" scope="col" class="col-banked">Banked resets</th>
  <th role="columnheader" scope="col" class="col-state">State</th>
</tr></thead>
<tbody role="rowgroup">
{chr(10).join(rows)}
</tbody>
</table>
</section>"""


def machines(scene: Scene) -> str:
    active = [m for m in scene.machines if m.state != "revoked"]
    revoked = [m for m in scene.machines if m.state == "revoked"]
    rows = []
    for m in active:
        a = next((x for x in scene.accounts if x.alias == m.alias), None)
        acct = f"{e(a.label)}{alias(a.alias)}" if a else '<span class="seen">Account unknown</span>'
        word = "In use" if m.state == "live" else "Idle"
        rows.append(
            f'<tr role="row"{" class=\"idle\"" if m.state == "idle" else ""}><td role="cell">{e(m.name)}</td>'
            f'<td role="cell" class="cell-account">{acct}</td>'
            f'<td role="cell" class="cell-seen"><span class="seen"><time datetime="{m.seen_iso}" title="{e(m.seen_title)}">{e(m.seen)}</time></span></td>'
            f'<td role="cell" class="cell-state"><span class="state {m.state}">{word}</span></td></tr>'
        )
    rev = ""
    if revoked:
        items = "".join(
            f'<li><b>{e(m.name)}</b><span>Revoked</span><span>Last token delivery '
            f'<time datetime="{m.seen_iso}" title="{e(m.seen_title)}">{e(m.seen)}</time></span></li>'
            for m in revoked
        )
        n = len(revoked)
        rev = (f'<details class="revoked" data-param="revoked" data-value="show">'
               f"<summary>{n} revoked machine{'s' if n > 1 else ''}</summary><ul>{items}</ul></details>")
    return f"""<section class="section" aria-labelledby="machines-title">
<div class="section-head">
  <h2 id="machines-title">Machines <span class="count">{len(active)}</span></h2>
  <p>The account each machine last received a token for</p>
</div>
<table class="machines" role="table" aria-labelledby="machines-title">
<thead role="rowgroup"><tr role="row"><th role="columnheader" scope="col">Machine</th><th role="columnheader" scope="col">Account</th><th role="columnheader" scope="col" class="col-seen">Last token delivery</th><th role="columnheader" scope="col" class="col-state">State</th></tr></thead>
<tbody role="rowgroup">
{chr(10).join(rows)}
</tbody>
</table>
<p class="machines-note">“In use” means a token delivery in the last 5 minutes. The server cannot see whether a session is still running.</p>
{rev}
</section>"""


NAV = [
    ("index.html", "Signed out"),
    ("overview.html", "Accounts"),
    ("overview-healthy.html", "Accounts, all healthy"),
    ("overview-blocked.html", "Accounts, nothing left"),
    ("overview-stale.html", "Accounts, refresh failed"),
    ("approve.html", "Approve machine"),
    ("connected.html", "Connected"),
]


def page(scene: Scene) -> str:
    pick = None if scene.refresh_failed else recommend(scene.accounts)
    notice = ""
    if scene.refresh_failed:
        notice = (
            '<div class="notice" role="status">'
            f"<p><strong>Refresh failed.</strong> Showing the last observation from {e(scene.failed_age)} ago. "
            "Retrying every 60 seconds.</p>"
            '<button class="button small" type="button">Retry now</button></div>'
        )
    nav = "\n".join(
        f'<a href="{href}"{" aria-current=\"page\"" if href == scene.file else ""}>{label}</a>'
        for href, label in NAV
    )
    title = "Accounts · codexctl"
    return f"""<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <meta name="color-scheme" content="light dark" />
    <meta name="theme-color" content="#f7f7f4" media="(prefers-color-scheme: light)" />
    <meta name="theme-color" content="#121314" media="(prefers-color-scheme: dark)" />
    <title>{title}</title>
    <link rel="preload" href="fonts/instrument-sans.woff2" as="font" type="font/woff2" crossorigin />
    <link rel="stylesheet" href="style.css" />
  </head>
  <body>
    <a class="skip" href="#main">Skip to content</a>
    <header class="topbar">
      <div class="wrap topbar-inner">
        <a class="wordmark" href="overview.html" translate="no" aria-label="codexctl accounts">codexctl<span class="caret" aria-hidden="true"></span></a>
        <div class="identity">
          <span class="email" translate="no" title="amir@example.com">amir@example.com</span>
          <form action="index.html" method="get"><button class="link-button" type="submit">Sign out</button></form>
        </div>
      </div>
    </header>
    <main id="main" class="wrap">
{notice}
<div class="lead">
{answer(scene)}
{attention(scene, pick)}
</div>
{ledger(scene, pick)}
{machines(scene)}
    </main>
    <footer class="wrap footer">
      <span translate="no">codexctl v0.1.37</span>
      <a href="https://github.com/Sawmills/codexctl/blob/main/docs/central-server.md">Documentation</a>
      <nav class="proto-nav" aria-label="Prototype pages">
        <span>Prototype:</span>
{nav}
      </nav>
    </footer>
    <script src="script.js"></script>
  </body>
</html>
"""


# ---------- Fixtures ----------


def w(left, reset_in, iso, local, title):
    return Window(left, reset_in, iso, local, title)


BASE = [
    Account("Feature sprint", "sprint", "PRO",
            w(0, "1h 12m", "2026-10-03T14:52:00Z", "10:52 AM", "Oct 3, 10:52 AM EDT"),
            w(22, "1d 20h", "2026-10-05T09:40:00Z", "Oct 5, 5:40 AM", "Oct 5, 5:40 AM EDT"),
            Banked(1, redeemable=1, expires="Oct 19", expires_in_days=16),
            in_use_on="Studio MacBook"),
    Account("Extra capacity", "reserve", "PRO",
            w(88, "3h 55m", "2026-10-03T17:35:00Z", "1:35 PM", "Oct 3, 1:35 PM EDT"),
            w(84, "4d 23h", "2026-10-08T12:40:00Z", "Oct 8, 8:40 AM", "Oct 8, 8:40 AM EDT"),
            Banked(0), in_use_on="Linux build machine"),
    Account("Everyday building", "studio", "PRO",
            w(76, "3h 31m", "2026-10-03T17:11:00Z", "1:11 PM", "Oct 3, 1:11 PM EDT"),
            w(58, "3d 5h", "2026-10-06T18:40:00Z", "Oct 6, 2:40 PM", "Oct 6, 2:40 PM EDT"),
            Banked(2, expires="Oct 5", expires_in_days=2)),
    Account("Team credits", "credits", "TEAM",
            w(92, "2h 22m", "2026-10-03T16:02:00Z", "12:02 PM", "Oct 3, 12:02 PM EDT"),
            w(95, "6d 1h", "2026-10-09T15:00:00Z", "Oct 9, 11:00 AM", "Oct 9, 11:00 AM EDT"),
            Banked(0), billing="usage_based"),
    Account("Nightly agents", "nightly", "PLUS",
            w(4, "38m", "2026-10-03T14:18:00Z", "10:18 AM", "Oct 3, 10:18 AM EDT"),
            w(40, "2d 7h", "2026-10-05T20:50:00Z", "Oct 5, 4:50 PM", "Oct 5, 4:50 PM EDT"),
            Banked(0)),
    Account("Code review", "review", "TEAM",
            w(61, "2h 50m", "2026-10-03T16:30:00Z", "12:30 PM", "Oct 3, 12:30 PM EDT"),
            w(70, "3d 23h", "2026-10-07T13:00:00Z", "Oct 7, 9:00 AM", "Oct 7, 9:00 AM EDT"),
            Banked(1, expires="Oct 21", expires_in_days=18), stale=True, age="7 min ago"),
    Account("Weekend", "weekend", "PLUS", None, None, None, state="renewal_pending",
            pending_note="An OpenAI sign-in started 4 min ago."),
    Account("Night shift", "night", "PRO", None, None, None, state="unavailable", age="2 h ago"),
]

MACHINES = [
    Machine("Studio MacBook", "sprint", "Just now", "2026-10-03T13:39:40Z", "Oct 3, 9:39 AM EDT", "live"),
    Machine("Linux build machine", "reserve", "2 min ago", "2026-10-03T13:37:40Z", "Oct 3, 9:37 AM EDT", "live"),
    Machine("Travel Air", "studio", "Sep 30, 6:12 PM", "2026-09-30T22:12:00Z", "Sep 30, 6:12 PM EDT", "idle"),
    Machine("Old iMac", None, "Aug 12", "2026-08-12T15:02:00Z", "Aug 12, 11:02 AM EDT", "revoked"),
]


def by_alias(accts, alias_, **changes):
    return [replace(a, **changes) if a.alias == alias_ else a for a in accts]


blocked = BASE
blocked = by_alias(blocked, "reserve",
                   five=w(0, "1h 40m", "2026-10-03T15:20:00Z", "11:20 AM", "Oct 3, 11:20 AM EDT"))
blocked = by_alias(blocked, "studio",
                   five=w(0, "2h 5m", "2026-10-03T15:45:00Z", "11:45 AM", "Oct 3, 11:45 AM EDT"),
                   seven=w(31, "3d 5h", "2026-10-06T18:40:00Z", "Oct 6, 2:40 PM", "Oct 6, 2:40 PM EDT"),
                   banked=Banked(2, redeemable=2, expires="Oct 5", expires_in_days=2))
blocked = by_alias(blocked, "nightly",
                   five=w(0, "38m", "2026-10-03T14:18:00Z", "10:18 AM", "Oct 3, 10:18 AM EDT"))

stale = [replace(a, stale=True, age="4 min ago") if a.state == "available" else a for a in BASE]

healthy = [a for a in BASE if a.alias in ("sprint", "reserve", "studio", "credits")]
healthy = by_alias(healthy, "sprint",
                   five=w(64, "2h 40m", "2026-10-03T16:20:00Z", "12:20 PM", "Oct 3, 12:20 PM EDT"),
                   banked=Banked(1, expires="Oct 19", expires_in_days=16))
healthy = by_alias(healthy, "studio", banked=Banked(2, expires="Oct 17", expires_in_days=14))

SCENES = [
    Scene("overview.html", "Accounts", BASE, MACHINES),
    Scene("overview-healthy.html", "Accounts, all healthy", healthy, MACHINES),
    Scene("overview-blocked.html", "Accounts, nothing left", blocked, MACHINES),
    Scene("overview-stale.html", "Accounts, refresh failed", stale, MACHINES,
          refresh_failed=True, failed_age="4 min"),
]

if __name__ == "__main__":
    for scene in SCENES:
        (OUT / scene.file).write_text(page(scene))
        pick = None if scene.refresh_failed else recommend(scene.accounts)
        print(f"{scene.file}: pick={pick.alias if pick else None}")
