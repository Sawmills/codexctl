(() => {
  "use strict";
  const byId = (id) => document.getElementById(id);
  const node = (tag, className, text) => {
    const el = document.createElement(tag);
    if (className) el.className = className;
    if (tag === "h1") el.id = "title";
    if (text !== undefined) el.textContent = text;
    return el;
  };
  const known = (n) => typeof n === "number" && Number.isFinite(n);
  const number = (n) =>
    known(n)
      ? new Intl.NumberFormat(undefined, { maximumFractionDigits: 1 }).format(n)
      : "Unknown";
  const duration = (seconds) => {
    if (!known(seconds) || seconds <= 0) return "Unknown window";
    for (const [unit, size] of [
      ["day", 86400],
      ["hour", 3600],
      ["minute", 60],
      ["second", 1],
    ]) {
      if (seconds % size === 0)
        return `${number(seconds / size)}-${unit} Window`;
    }
  };
  const countdown = (seconds) => {
    if (seconds <= 0) return "Reset due · awaiting observation";
    const d = Math.floor(seconds / 86400),
      h = Math.floor((seconds % 86400) / 3600),
      m = Math.ceil((seconds % 3600) / 60);
    return `Resets in ${d ? `${number(d)}d ${number(h)}h` : h ? `${number(h)}h ${number(m)}m` : `${number(m)}m`}`;
  };
  const localTime = (seconds) =>
    new Intl.DateTimeFormat(undefined, {
      month: "short",
      day: "numeric",
      hour: "numeric",
      minute: "2-digit",
      timeZoneName: "short",
    }).format(new Date(seconds * 1000));
  const shortDate = (seconds) =>
    new Intl.DateTimeFormat(undefined, {
      month: "short",
      day: "numeric",
    }).format(new Date(seconds * 1000));
  const code = (tag, className, text) => {
    const el = node(tag, className, text);
    el.translate = false;
    return el;
  };
  function resetTime(at, timers = resetNodes) {
    if (!known(at)) return node("span", "muted", "Reset time unknown");
    const el = node("time");
    el.dateTime = new Date(at * 1000).toISOString();
    el.title = localTime(at);
    el.setAttribute("aria-label", `Resets ${localTime(at)}`);
    timers.push([el, at]);
    return el;
  }
  let firstRender = true,
    pollTimer,
    inFlight = false,
    refreshFailed = false;
  const isStale = (a) =>
    refreshFailed ||
    a.usage_stale ||
    !known(a.usage_age_seconds) ||
    a.usage_age_seconds + (performance.now() - receivedAt) / 1000 >= 60;
  const headroom = (a) => {
    const windows = [a.primary, a.secondary].filter((w) =>
      known(w.left_percent),
    );
    if (a.state !== "available" || isStale(a) || !windows.length) return -1;
    return Math.min(...windows.map((w) => w.left_percent));
  };
  let snapshot,
    receivedAt = 0;
  const freshnessNodes = [];
  const resetNodes = [];
  const heroResetNodes = [];
  const machineSeenNodes = [];
  const currentTime = () =>
    known(snapshot?.server_time)
      ? snapshot.server_time + (performance.now() - receivedAt) / 1000
      : Date.now() / 1000;
  const recentMachines = (data) =>
    data.machines
      .filter(
        (m) =>
          m.status === "registered" &&
          known(m.last_seen_at) &&
          currentTime() - m.last_seen_at < 300 &&
          m.last_used_alias,
      )
      .sort((a, b) => b.last_seen_at - a.last_seen_at);
  const seenAgo = (at) => {
    const minutes = Math.floor(Math.max(0, currentTime() - at) / 60);
    return minutes < 1 ? "just now" : `${number(minutes)} min ago`;
  };
  function windowView(w, fallback, stale) {
    const wrap = node("div", `window${stale ? " stale" : ""}`);
    const heading = node("div", "window-heading");
    heading.append(
      node(
        "strong",
        "",
        known(w.window_seconds)
          ? duration(w.window_seconds)
          : `${fallback} · duration unknown`,
      ),
    );
    if (known(w.used_percent)) {
      const severity =
        w.used_percent >= 100 ? "bad" : w.used_percent >= 80 ? "warning" : "ok";
      heading.append(
        node(
          "span",
          "muted",
          w.used_percent >= 100
            ? "Exhausted"
            : w.used_percent >= 80
              ? "High usage"
              : "",
        ),
      );
      const bar = node(
        "progress",
        `${severity}${firstRender ? " animate-meter" : ""}`,
      );
      bar.setAttribute("aria-valuemin", "0");
      bar.setAttribute("aria-valuemax", "100");
      bar.setAttribute("aria-valuenow", String(Math.min(100, w.used_percent)));
      bar.max = 100;
      bar.value = Math.min(100, w.used_percent);
      bar.setAttribute(
        "aria-label",
        `${fallback} usage${stale ? ", last observation" : ""}`,
      );
      bar.setAttribute(
        "aria-valuetext",
        `${number(w.used_percent)} percent used, ${number(w.left_percent)} percent left`,
      );
      wrap.append(heading, bar);
    } else {
      wrap.append(heading, node("div", "unknown-bar"));
    }
    const values = node("div", "usage-values");
    values.append(
      node(
        "strong",
        "",
        known(w.left_percent) ? `${number(w.left_percent)}% left` : "Unknown",
      ),
      node(
        "span",
        "",
        known(w.used_percent)
          ? `${number(w.used_percent)}% used`
          : "Usage unknown",
      ),
    );
    wrap.append(values);
    const reset = node("div", "reset");
    reset.append(resetTime(w.resets_at));
    wrap.append(reset);
    return wrap;
  }
  function renderFeatured(accounts, machines) {
    const hero = byId("featured");
    hero.replaceChildren();
    heroResetNodes.length = 0;
    const recent = recentMachines({ machines });
    const best =
      accounts.find((a) => a.alias === recent[0]?.last_used_alias) ||
      accounts
        .filter((a) => headroom(a) > 0 && a.billing_class === "rate_limited")
        .sort((a, b) => headroom(b) - headroom(a))[0];
    const intro = node("div", "featured-identity");
    intro.append(
      node("p", "eyebrow", recent.length ? "IN USE NOW" : "SUGGESTED"),
    );
    if (!best) {
      intro.append(
        node(
          "h1",
          "",
          accounts.length
            ? "Capacity Is Unconfirmed"
            : "Your Accounts, Together",
        ),
        node(
          "p",
          "",
          accounts.length
            ? "No machine is active. Check the observations below before choosing an account."
            : "Connect a machine and migrate your profiles to get started.",
        ),
      );
      hero.append(intro);
      return;
    }
    intro.append(
      node("h1", "", best.label || "Server Account"),
      code("p", "alias", best.alias),
      node(
        "span",
        "pill inverse",
        best.plan ? `${best.plan.toUpperCase()} plan` : "Plan unknown",
      ),
    );
    if (recent.length) {
      const list = node("ul", "in-use");
      for (const machine of recent) {
        const item = node("li");
        item.append(
          node("span", "", `${machine.name} → `),
          code("code", "", machine.last_used_alias),
        );
        const seen = node(
          "time",
          "",
          `Last token delivery ${seenAgo(machine.last_seen_at)}`,
        );
        seen.title = localTime(machine.last_seen_at);
        seen.dateTime = new Date(machine.last_seen_at * 1000).toISOString();
        item.append(seen);
        list.append(item);
      }
      intro.append(
        list,
        node(
          "p",
          "hero-caption",
          "Token delivery within 5 min · running sessions may differ.",
        ),
      );
    } else
      intro.append(
        node(
          "p",
          "hero-caption",
          "No machine is active. Suggested by included headroom; nothing has been switched.",
        ),
      );
    const capacity = node("div", "featured-capacity"),
      w = best.secondary;
    capacity.append(
      node(
        "p",
        "eyebrow",
        known(w.window_seconds)
          ? duration(w.window_seconds)
          : "LONG WINDOW · DURATION UNKNOWN",
      ),
      node(
        "p",
        "big-number",
        known(w.left_percent) ? `${number(w.left_percent)}%` : "Unknown",
      ),
      node(
        "p",
        "remaining-label",
        isStale(best) ? "remaining · stale observation" : "remaining",
      ),
    );
    const reset = node("p", "hero-reset");
    reset.append(resetTime(w.resets_at, heroResetNodes));
    capacity.append(reset);
    hero.append(intro, capacity);
  }
  function render(data) {
    freshnessNodes.length = 0;
    resetNodes.length = 0;
    machineSeenNodes.length = 0;
    const active = new Set(recentMachines(data).map((m) => m.last_used_alias));
    const accounts = [...data.accounts].sort(
      (a, b) =>
        Number(active.has(b.alias)) - Number(active.has(a.alias)) ||
        headroom(b) - headroom(a) ||
        a.alias.localeCompare(b.alias),
    );
    byId("accounts").classList.toggle("large-list", data.accounts.length > 50);
    byId("accounts").setAttribute("aria-busy", "false");
    byId("account-count").textContent = number(accounts.length);
    renderFeatured(accounts, data.machines);
    byId("machine-count").textContent = number(data.machines.length);
    byId("accounts").replaceChildren();
    byId("machines").replaceChildren();
    if (!data.accounts.length)
      byId("accounts").append(
        node(
          "p",
          "notice",
          "No server accounts yet. Connect a machine and migrate your profiles to see them here.",
        ),
      );
    for (const a of accounts) {
      const card = node("article", "card"),
        top = node("div", "account-top"),
        identity = node("div", "account-identity"),
        name = node("div");
      name.append(
        node("h3", "", a.label || "Server Account"),
        code("p", "alias", a.alias),
      );
      const state =
        {
          available: "Available",
          unavailable: "Unavailable",
          renewal_pending: "Renewal pending",
        }[a.state] || "Unknown";
      if (active.has(a.alias))
        name.append(node("span", "active-label", "In use now"));
      identity.append(
        name,
        node(
          "span",
          `pill ${a.state === "unavailable" ? "bad" : a.state === "renewal_pending" ? "pending" : ""}`,
          state,
        ),
      );
      const meta = node("div", "account-meta");
      const billing =
        {
          rate_limited: "Included usage",
          usage_based: "Usage-based billing",
          unknown: "Billing unknown",
        }[a.billing_class] || "Billing unknown";
      meta.append(
        node(
          "span",
          "pill plan",
          a.plan ? `${a.plan.toUpperCase()} plan` : "Plan unknown",
        ),
        node("span", "", billing),
      );
      top.append(identity, meta);
      const windows = node("div", "windows");
      windows.append(
        windowView(a.primary, "Short window", a.usage_stale),
        windowView(a.secondary, "Long window", a.usage_stale),
      );
      const banked = node("div", "banked"),
        b = a.banked_resets;
      const inventory = known(b.count)
        ? `${number(b.count)} ${b.count === 1 ? "reset" : "resets"}`
        : "Banked resets unknown";
      const expiry = known(b.nearest_expiry)
        ? ` · next expires ${shortDate(b.nearest_expiry)}`
        : b.count === 0
          ? ""
          : " · expiry unknown";
      banked.append(
        node("span", "reset-chip", inventory + expiry),
        node(
          "p",
          "muted",
          known(b.redeemable_now)
            ? `${number(b.redeemable_now)} redeemable now`
            : "Redeemable count unknown",
        ),
      );
      if (b.stale)
        banked.append(node("span", "muted small", "Reset inventory stale"));
      const freshness = node("p", "freshness");
      freshnessNodes.push([freshness, a, windows]);
      card.append(top, windows, banked, freshness);
      byId("accounts").append(card);
    }
    if (!data.machines.length)
      byId("machines").append(
        node("p", "machine muted", "No registered machines yet."),
      );
    for (const m of data.machines) {
      const row = node("article", "machine"),
        icon = node("span", "machine-icon", ">_"),
        details = node("div", "machine-details");
      icon.setAttribute("aria-hidden", "true");
      const seen = node("p", "", "Last seen unknown");
      if (known(m.last_seen_at)) machineSeenNodes.push([seen, m.last_seen_at]);
      details.append(node("h3", "", m.name), seen);
      if (m.last_used_alias)
        details.append(node("p", "", `Last used ${m.last_used_alias}`));
      row.append(
        icon,
        details,
        node("span", "pill", m.status === "revoked" ? "Revoked" : "Registered"),
      );
      byId("machines").append(row);
    }
    firstRender = false;
    tick();
  }
  function tick() {
    const elapsed = Math.floor((performance.now() - receivedAt) / 1000);
    for (const [el, account, windows] of freshnessNodes) {
      const age = known(account.usage_age_seconds)
        ? account.usage_age_seconds + elapsed
        : null;
      const stale =
        refreshFailed || account.usage_stale || age === null || age >= 60;
      el.textContent =
        age === null
          ? "Usage unknown · no successful observation"
          : `Updated ${number(age)} s ago${stale ? " · Stale" : ""}`;
      if (account.usage_error) el.textContent += " · usage refresh unavailable";
      for (const w of windows.children) w.classList.toggle("stale", stale);
    }
    if (snapshot) {
      const ages = snapshot.accounts
        .map((a) => a.usage_age_seconds)
        .filter(known);
      byId("refresh-status").textContent = ages.length
        ? `Updated ${number(Math.min(...ages) + elapsed)} s ago · refreshes every 60 s`
        : "Usage freshness unknown";
    }
    for (const [el, at] of machineSeenNodes)
      el.textContent = `Last token delivery ${seenAgo(at)} · ${localTime(at)}`;
    if (snapshot) renderFeatured(snapshot.accounts, snapshot.machines);
    for (const [el, at] of [...resetNodes, ...heroResetNodes])
      el.textContent = countdown(at - currentTime());
  }
  async function pollAccounts() {
    if (inFlight) return;
    clearTimeout(pollTimer);
    inFlight = true;
    byId("retry").disabled = true;
    let again = true;
    try {
      const response = await fetch("/accounts/data", {
        credentials: "same-origin",
        cache: "no-store",
        signal: AbortSignal.timeout(45000),
      });
      if (response.status === 401 || response.status === 403) {
        snapshot = null;
        freshnessNodes.length = 0;
        resetNodes.length = 0;
        heroResetNodes.length = 0;
        machineSeenNodes.length = 0;
        byId("accounts").replaceChildren();
        byId("machines").replaceChildren();
        byId("account-count").textContent = "Unknown";
        byId("machine-count").textContent = "Unknown";
        byId("connection").textContent =
          "Your session ended or access changed. Sign in to continue.";
        byId("featured").replaceChildren(
          node("h1", "", "Sign In to View Your Accounts"),
        );
        byId("refresh-status").textContent = "Session ended";
        byId("retry").hidden = true;
        byId("sign-in").hidden = false;
        again = false;
        return;
      }
      if (!response.ok) throw new Error("unavailable");
      snapshot = await response.json();
      receivedAt = performance.now();
      refreshFailed = false;
      byId("retry").hidden = true;
      render(snapshot);
      byId("connection").textContent = "";
    } catch {
      refreshFailed = true;
      byId("retry").hidden = false;
      byId("accounts").setAttribute("aria-busy", "false");
      if (!snapshot) {
        byId("accounts").replaceChildren();
        byId("featured").replaceChildren(
          node("h1", "", "Your Overview Is Unavailable"),
          node("p", "", "Retry to load your accounts."),
        );
        byId("refresh-status").textContent = "Freshness unknown";
      }
      tick();
      byId("connection").textContent = snapshot
        ? "Refresh unavailable. Showing the last observation; retrying in 60 seconds."
        : "Your accounts could not be loaded. Retrying in 60 seconds.";
      if (snapshot)
        byId("connection").append(
          node("span", "pill warning stale-badge", "Stale"),
        );
    } finally {
      inFlight = false;
      byId("retry").disabled = false;
      if (again) pollTimer = setTimeout(pollAccounts, 60000);
    }
  }
  async function pollReady() {
    const pill = byId("server-status");
    try {
      const response = await fetch("/ready", {
        cache: "no-store",
        signal: AbortSignal.timeout(8000),
      });
      pill.textContent = response.ok ? "Server ready" : "Server not ready";
      pill.className = response.ok ? "pill ok" : "pill warning";
    } catch {
      pill.textContent = "Server status unknown";
      pill.className = "pill";
    }
    setTimeout(pollReady, 60000);
  }
  for (const button of document.querySelectorAll("[data-copy]")) {
    button.addEventListener("click", async () => {
      try {
        await navigator.clipboard.writeText(
          byId(button.dataset.copy).textContent,
        );
        byId("copy-status").textContent =
          "Command copied. Paste it into your terminal.";
        button.textContent = "Copied";
        setTimeout(() => {
          button.textContent = "Copy";
        }, 2000);
      } catch {
        byId("copy-status").textContent =
          "Copy unavailable. Select the command and copy it from the page.";
      }
    });
  }
  if (byId("accounts")) {
    byId("retry").addEventListener("click", pollAccounts);
    pollAccounts();
    setInterval(tick, 60000);
  }
  if (byId("server-status")) pollReady();
  const linux = document.querySelector("details");
  if (linux) {
    linux.open =
      new URL(location.href).searchParams.get("platform") === "linux";
    linux.addEventListener("toggle", () => {
      const url = new URL(location.href);
      if (linux.open) url.searchParams.set("platform", "linux");
      else url.searchParams.delete("platform");
      history.replaceState(null, "", url);
    });
  }
})();
