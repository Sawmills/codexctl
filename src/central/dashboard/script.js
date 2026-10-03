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
        return `${number(seconds / size)}-${unit} window`;
    }
  };
  const countdown = (seconds) => {
    if (seconds <= 0) return "Reset due · awaiting observation";
    const minutes = Math.ceil(seconds / 60),
      d = Math.floor(minutes / 1440),
      h = Math.floor((minutes % 1440) / 60),
      m = minutes % 60;
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
  const pageIsStale = () =>
    refreshFailed ||
    (snapshot?.accounts.length > 0 && snapshot.accounts.every(isStale));
  const headroom = (a) => {
    if (
      a.state !== "available" ||
      isStale(a) ||
      !known(a.secondary.left_percent) ||
      [a.primary, a.secondary].some(
        (w) => known(w.used_percent) && w.used_percent >= 100,
      )
    )
      return -1;
    return a.secondary.left_percent;
  };
  const category = (a, active) => {
    if (active.has(a.alias)) return 0;
    if (a.state === "unavailable") return 6;
    if (a.state === "renewal_pending") return 5;
    if (isStale(a)) return 4;
    if (
      [a.primary, a.secondary].some(
        (w) => known(w.used_percent) && w.used_percent >= 100,
      )
    )
      return 3;
    if (!known(a.secondary.used_percent)) return 4;
    if (
      [a.primary, a.secondary].some(
        (w) => known(w.used_percent) && w.used_percent >= 80,
      )
    )
      return 2;
    return 1;
  };
  const ageText = (seconds) =>
    seconds < 60
      ? `${number(Math.floor(seconds))} s ago`
      : `${number(Math.floor(seconds / 60))} min ago`;
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
    const heading = node("div", "window-heading"),
      label = node("div", "window-label");
    label.append(
      node(
        "span",
        "",
        known(w.window_seconds)
          ? duration(w.window_seconds)
          : `${fallback}, duration unknown`,
      ),
      node(
        "strong",
        "",
        known(w.left_percent) ? `${number(w.left_percent)}% left` : "Unknown",
      ),
    );
    heading.append(label);
    if (known(w.resets_at)) heading.append(resetTime(w.resets_at));
    wrap.append(heading);
    const severity =
      w.used_percent >= 100 ? "bad" : w.used_percent >= 80 ? "warning" : "ok";
    if (known(w.used_percent)) {
      const bar = node(
        "progress",
        `${severity}${firstRender ? " animate-meter" : ""}`,
      );
      bar.max = 100;
      bar.value = Math.min(100, w.used_percent);
      for (const [key, value] of Object.entries({
        "aria-valuemin": "0",
        "aria-valuemax": "100",
        "aria-valuenow": String(bar.value),
        "aria-label": `${fallback} usage${stale ? ", last observation" : ""}`,
        "aria-valuetext": `${number(w.used_percent)} percent used, ${number(w.left_percent)} percent left`,
      }))
        bar.setAttribute(key, value);
      wrap.append(bar);
    } else wrap.append(node("div", "unknown-bar"));
    const caption = node("div", "window-caption");
    caption.append(
      node(
        "span",
        "",
        known(w.used_percent)
          ? `${number(w.used_percent)}% used`
          : "Usage unknown",
      ),
    );
    const note = node(
      "span",
      severity,
      w.used_percent >= 100
        ? "Exhausted"
        : w.used_percent >= 80
          ? "High usage"
          : known(w.resets_at)
            ? ""
            : "Reset time unknown",
    );
    caption.append(note);
    wrap.append(caption);
    return wrap;
  }
  function accountPill(account, alias, inverse = false) {
    const pill = node("span", `account-pill${inverse ? " inverse" : ""}`);
    if (account?.label) pill.append(node("span", "", account.label));
    pill.append(code("span", "alias", alias));
    return pill;
  }
  function featuredRow(account, machine) {
    const row = node("div", "hero-machine"),
      identity = node("div", "hero-machine-identity");
    identity.append(
      node(
        "h2",
        "hero-machine-name",
        machine ? machine.name : account.label || "Server account",
      ),
    );
    const chips = node("div", "hero-account-chips");
    chips.append(
      accountPill(
        machine ? account : null,
        machine?.last_used_alias || account.alias,
        true,
      ),
    );
    if (account?.plan)
      chips.append(
        node("span", "pill inverse plan", `${account.plan.toUpperCase()} plan`),
      );
    identity.append(chips);
    if (machine) {
      const seen = node(
        "time",
        "delivery-time",
        `Last delivery ${seenAgo(machine.last_seen_at)}`,
      );
      seen.title = localTime(machine.last_seen_at);
      seen.dateTime = new Date(machine.last_seen_at * 1000).toISOString();
      identity.append(seen);
    }
    const long = account?.secondary,
      short = account?.primary,
      capacity = node("div", "hero-capacity");
    capacity.append(
      node(
        "p",
        "capacity-label",
        known(long?.window_seconds)
          ? duration(long.window_seconds)
          : "Long window, duration unknown",
      ),
    );
    const value = node("p", "hero-value");
    value.append(
      node(
        "strong",
        "big-number",
        known(long?.left_percent) ? `${number(long.left_percent)}%` : "Unknown",
      ),
      node("span", "", known(long?.left_percent) ? "left" : ""),
    );
    capacity.append(value, resetTime(long?.resets_at, heroResetNodes));
    if (known(short?.left_percent))
      capacity.append(
        node(
          "p",
          "short-capacity",
          `${known(short.window_seconds) ? duration(short.window_seconds) : "Short window, duration unknown"} · ${number(short.left_percent)}% left`,
        ),
      );
    if (account && isStale(account) && !pageIsStale())
      capacity.append(node("span", "pill inverse", "Stale"));
    row.append(identity, capacity);
    return row;
  }
  function renderFeatured(accounts, machines) {
    const hero = byId("featured");
    hero.replaceChildren();
    heroResetNodes.length = 0;
    const recent = recentMachines({ machines });
    hero.classList.toggle("single-machine", recent.length <= 1);
    if (recent.length) {
      hero.append(node("h1", "", "In use now"));
      for (const machine of recent)
        hero.append(
          featuredRow(
            accounts.find((a) => a.alias === machine.last_used_alias),
            machine,
          ),
        );
      hero.append(
        node(
          "p",
          "hero-caption",
          "Token delivery within 5 min · running sessions may differ.",
        ),
      );
      return;
    }
    const best = accounts
      .filter((a) => headroom(a) > 0 && a.billing_class === "rate_limited")
      .sort((a, b) => headroom(b) - headroom(a))[0];
    if (best) {
      hero.append(
        node("h1", "", "Suggested"),
        featuredRow(best, null),
        node(
          "p",
          "hero-caption",
          "No machine is active. Suggested by included headroom; nothing has been switched.",
        ),
      );
    } else
      hero.append(
        node(
          "h1",
          "",
          accounts.length
            ? "Capacity is unconfirmed"
            : "Your accounts, together",
        ),
        node(
          "p",
          "hero-caption",
          accounts.length
            ? "No machine is active. Check the observations below before choosing an account."
            : "Connect a machine and migrate your profiles to get started.",
        ),
      );
  }
  function render(data) {
    freshnessNodes.length = 0;
    resetNodes.length = 0;
    machineSeenNodes.length = 0;
    const active = new Map();
    for (const m of recentMachines(data))
      if (!active.has(m.last_used_alias))
        active.set(m.last_used_alias, active.size);
    const accounts = [...data.accounts].sort(
      (a, b) =>
        category(a, active) - category(b, active) ||
        (active.has(a.alias) && active.has(b.alias)
          ? active.get(a.alias) - active.get(b.alias)
          : (b.secondary.left_percent ?? -1) -
            (a.secondary.left_percent ?? -1)) ||
        a.alias.localeCompare(b.alias),
    );
    byId("identity-email").textContent = data.identity.email;
    byId("browser-identity").hidden = false;
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
        node("h3", "", a.label || "Server account"),
        code("p", "alias", a.alias),
      );
      const state =
        {
          available: "Available",
          unavailable: "Unavailable",
          renewal_pending: "Renewal pending",
        }[a.state] || "Unknown";

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
      if (active.has(a.alias))
        meta.append(node("span", "active-label", "In use now"));
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
      if (b.count === 0)
        banked.append(node("span", "muted", "No banked resets"));
      else {
        const inventory = known(b.count)
          ? `${number(b.count)} ${b.count === 1 ? "reset" : "resets"}`
          : "Banked resets unknown";
        const expiry = known(b.nearest_expiry)
          ? ` · next expires ${shortDate(b.nearest_expiry)}`
          : " · expiry unknown";
        banked.append(node("span", "reset-chip", inventory + expiry));
        if (known(b.redeemable_now) && b.redeemable_now > 0)
          banked.append(
            node(
              "strong",
              "redeemable",
              `${number(b.redeemable_now)} redeemable now`,
            ),
          );
      }
      if (b.stale)
        banked.append(node("span", "muted small", "Reset inventory stale"));
      const freshness = node("p", "freshness");
      freshnessNodes.push([freshness, a, windows]);
      card.append(top, windows, banked, freshness);
      byId("accounts").append(card);
    }
    const machines = [...data.machines].sort(
      (a, b) => (b.last_seen_at ?? 0) - (a.last_seen_at ?? 0),
    );
    const registered = machines.filter((m) => m.status !== "revoked"),
      revoked = machines.filter((m) => m.status === "revoked");
    byId("machine-count").textContent = number(registered.length);
    if (!registered.length)
      byId("machines").append(
        node("p", "machine muted", "No registered machines yet."),
      );
    function machineRow(m) {
      const row = node("article", "machine"),
        icon = node("span", "machine-icon", ">_"),
        details = node("div", "machine-details");
      icon.setAttribute("aria-hidden", "true");
      const seen = node("p", "", "Last seen unknown");
      if (known(m.last_seen_at)) machineSeenNodes.push([seen, m.last_seen_at]);
      details.append(node("h3", "", m.name), seen);
      row.append(icon, details);
      if (m.last_used_alias)
        row.append(
          accountPill(
            data.accounts.find((a) => a.alias === m.last_used_alias),
            m.last_used_alias,
          ),
        );
      else
        row.append(
          node(
            "span",
            "muted small",
            m.status === "revoked" ? "Revoked" : "Account unknown",
          ),
        );
      return row;
    }
    for (const m of registered) byId("machines").append(machineRow(m));
    if (revoked.length) {
      const disclosure = node("details", "revoked-machines");
      disclosure.id = "revoked-machines";
      disclosure.open =
        new URL(location.href).searchParams.get("revoked") === "show";
      disclosure.append(
        node("summary", "", `Show revoked (${number(revoked.length)})`),
      );
      for (const m of revoked) disclosure.append(machineRow(m));
      disclosure.addEventListener("toggle", () => {
        const url = new URL(location.href);
        if (disclosure.open) url.searchParams.set("revoked", "show");
        else url.searchParams.delete("revoked");
        history.replaceState(null, "", url);
      });
      byId("machines").append(disclosure);
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
      el.replaceChildren(
        node(
          "span",
          "",
          age === null ? "Usage freshness unknown" : `Updated ${ageText(age)}`,
        ),
      );
      if (stale && !pageIsStale())
        el.append(node("span", "pill warning", "Stale"));
      if (account.usage_error) el.title = "Usage refresh unavailable";
      for (const w of windows.children) w.classList.toggle("stale", stale);
    }
    if (snapshot) {
      if (!refreshFailed) {
        byId("connection").textContent = pageIsStale()
          ? "All usage observations are stale. Refreshes every 60 seconds."
          : "";
        if (pageIsStale())
          byId("connection").append(
            node("span", "pill warning stale-badge", "Stale"),
          );
      }
      const ages = snapshot.accounts
        .map((a) => a.usage_age_seconds)
        .filter(known);
      byId("refresh-status").textContent = ages.length
        ? `Updated ${ageText(Math.min(...ages) + elapsed)} · refreshes every 60 s`
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
        byId("identity-email").textContent = "";
        byId("browser-identity").hidden = true;
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
          node("h1", "", "Sign in to view your accounts"),
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
      byId("connection").textContent = "";
      render(snapshot);
    } catch {
      refreshFailed = true;
      byId("retry").hidden = false;
      byId("accounts").setAttribute("aria-busy", "false");
      if (!snapshot) {
        byId("accounts").replaceChildren();
        byId("featured").replaceChildren(
          node("h1", "", "Your overview is unavailable"),
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
  const linux = document.querySelector(".setup details");
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
