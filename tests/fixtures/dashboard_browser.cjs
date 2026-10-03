const { chromium } = require("playwright");
const AxeBuilder = require("@axe-core/playwright").default;
const http = require("node:http");
const fs = require("node:fs");
const assert = require("node:assert/strict");
const root = process.argv[2] || "/tmp/b16-render";
const out =
  process.argv[3] ||
  require("node:path").join(require("node:os").homedir(), "b16-screens");
fs.mkdirSync(out, { recursive: true });
const now = Math.floor(Date.now() / 1000);
const window = (used, seconds, reset) => ({
  used_percent: used,
  left_percent: used === null ? null : Math.max(0, 100 - used),
  window_seconds: seconds,
  resets_at: reset,
});
const snapshot = {
  version: 1,
  server_time: now,
  accounts: [
    {
      alias: "studio",
      label: "Everyday building",
      plan: "pro",
      state: "available",
      billing_class: "rate_limited",
      primary: window(24, 18000, now + 12642),
      secondary: window(42, 604800, now + 278302),
      usage_age_seconds: 8,
      usage_stale: false,
      usage_error: null,
      banked_resets: {
        count: 2,
        redeemable_now: 0,
        nearest_expiry: now + 1252800,
        stale: false,
      },
    },
    {
      alias: "deep-work",
      label: "Long-running projects",
      plan: "pro",
      state: "available",
      billing_class: "unknown",
      primary: window(86, 18000, now + 3982),
      secondary: window(100, 604800, now + 97420),
      usage_age_seconds: 12,
      usage_stale: false,
      usage_error: null,
      banked_resets: {
        count: 1,
        redeemable_now: 1,
        nearest_expiry: now + 684000,
        stale: false,
      },
    },
    {
      alias: "research",
      label: "Ideas in progress",
      plan: "team",
      state: "renewal_pending",
      billing_class: "unknown",
      primary: window(null, null, null),
      secondary: window(null, null, null),
      usage_age_seconds: null,
      usage_stale: true,
      usage_error: null,
      banked_resets: {
        count: null,
        redeemable_now: null,
        nearest_expiry: null,
        stale: false,
      },
    },
  ],
  machines: [
    {
      name: "Studio MacBook",
      status: "registered",
      last_seen_at: now - 35,
      last_used_alias: "studio",
    },
    {
      name: "Linux Build Machine",
      status: "registered",
      last_seen_at: now - 120,
      last_used_alias: "reserve",
    },
    { name: "Previous workstation", status: "revoked", last_seen_at: null },
  ],
};
snapshot.accounts.push(
  {
    ...structuredClone(snapshot.accounts[0]),
    alias: "reserve",
    label: "Extra Capacity",
    primary: window(12, 18000, now + 14100),
    secondary: window(16, 604800, now + 431000),
    banked_resets: {
      count: 0,
      redeemable_now: 0,
      nearest_expiry: null,
      stale: false,
    },
  },
  {
    ...structuredClone(snapshot.accounts[0]),
    alias: "sprint",
    label: "Feature Sprint",
    primary: window(84, 18000, now + 2400),
    secondary: window(78, 604800, now + 109000),
  },
  {
    ...structuredClone(snapshot.accounts[0]),
    alias: "offline",
    label: "Login Needs Attention",
    state: "unavailable",
    usage_age_seconds: 184,
    usage_stale: true,
    billing_class: "unknown",
  },
  {
    ...structuredClone(snapshot.accounts[2]),
    alias: "flex",
    label: "Flexible Research",
    state: "available",
    billing_class: "usage_based",
    usage_stale: false,
    usage_age_seconds: 18,
    primary: window(36, null, null),
    secondary: window(null, null, null),
  },
);
let polls = 0,
  denied = false,
  unavailable = false;
const server = http.createServer((req, res) => {
  if (req.url === "/accounts/data") {
    polls++;
    res.writeHead(denied ? 401 : unavailable ? 503 : 200, {
      "content-type": "application/json",
      "cache-control": "no-store",
    });
    res.end(
      JSON.stringify(
        denied
          ? { error: "browser_sign_in_required" }
          : unavailable
            ? { error: "unavailable" }
            : snapshot,
      ),
    );
    return;
  }
  if (req.url === "/ready") {
    res.writeHead(200);
    res.end();
    return;
  }
  const file = req.url === "/accounts" ? "accounts.html" : "landing.html";
  res.writeHead(200, {
    "content-type": "text/html",
    "content-security-policy": fs.readFileSync(`${root}/${file}.csp`, "utf8"),
    "cache-control": "no-store",
  });
  res.end(fs.readFileSync(`${root}/${file}`));
});
(async () => {
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const origin = `http://127.0.0.1:${server.address().port}`;
  const browser = await chromium.launch({ headless: true });
  const errors = [];
  for (const scheme of ["light", "dark"]) {
    const context = await browser.newContext({
      colorScheme: scheme,
      viewport: { width: 1440, height: 1100 },
      timezoneId: "America/New_York",
      permissions: ["clipboard-read", "clipboard-write"],
    });
    const page = await context.newPage();
    page.on("pageerror", (e) => errors.push(String(e)));
    page.on("console", (m) => {
      if (
        m.type() === "error" &&
        !m.text().includes("401 (Unauthorized)") &&
        !m.text().includes("503 (Service Unavailable)")
      )
        errors.push(m.text());
    });
    for (const [route, name] of [
      ["/", "landing"],
      ["/accounts", "accounts"],
    ]) {
      await page.goto(origin + route);
      await page
        .getByText(name === "landing" ? "Server ready" : "Everyday building", {
          exact: true,
        })
        .first()
        .waitFor();
      if (name === "accounts")
        assert(
          await page
            .locator(".big-number")
            .evaluate((el) => parseFloat(getComputedStyle(el).fontSize) >= 56),
        );
      const a11y = await new AxeBuilder({ page })
        .withTags(["wcag2a", "wcag2aa", "wcag21aa"])
        .analyze();
      assert.deepEqual(
        a11y.violations.map((v) => ({
          id: v.id,
          nodes: v.nodes.map((n) => n.target),
        })),
        [],
      );
      assert(
        await page.evaluate(
          () => document.documentElement.scrollWidth <= innerWidth,
        ),
      );
      await page.screenshot({
        path: `${out}/${name}-${scheme}.png`,
        fullPage: true,
      });
      if (name === "landing") {
        await page
          .getByRole("button", { name: "Copy macOS install command" })
          .click();
        assert.equal(
          await page.evaluate(() => navigator.clipboard.readText()),
          "brew install sawmills/tap/codexctl",
        );
        await page.getByText("Linux / arm64 & x86_64", { exact: true }).click();
        await page
          .getByRole("button", { name: "Copy Linux install commands" })
          .click();
        const command = await page.evaluate(() =>
          navigator.clipboard.readText(),
        );
        assert(command.includes("sha256sum --check"));
        assert(command.includes("set -eu"));
      }
      await page.setViewportSize({ width: 390, height: 844 });
      if (name === "landing")
        await page.locator("details").evaluate((e) => (e.open = false));
      assert(
        await page.evaluate(
          () => document.documentElement.scrollWidth <= innerWidth,
        ),
      );
      await page.screenshot({
        path: `${out}/${name}-${scheme}-phone.png`,
        fullPage: true,
      });
      if (name === "accounts")
        await page.screenshot({
          path: `${out}/accounts-${scheme}-phone-first-screen.png`,
        });
      await page.setViewportSize({ width: 1440, height: 1100 });
    }
    // Keyboard, reduced-motion, empty, and suggested states use the same bundled page.
    await page.emulateMedia({ reducedMotion: "reduce" });
    const activeMachines = structuredClone(snapshot.machines);
    snapshot.machines.forEach((m) => (m.last_seen_at = null));
    await page.goto(origin + "/accounts");
    await page.getByText("SUGGESTED", { exact: true }).waitFor();
    assert.equal(
      await page
        .locator("progress")
        .first()
        .evaluate(
          (el) =>
            getComputedStyle(el, "::-webkit-progress-value").animationName,
        ),
      "none",
    );
    assert.equal(
      await page.locator("#featured h1").textContent(),
      "Extra Capacity",
    );
    if (scheme === "light")
      await page.screenshot({
        path: `${out}/accounts-suggested-light.png`,
        fullPage: true,
      });
    const savedAccounts = snapshot.accounts;
    snapshot.accounts = [];
    snapshot.machines = [];
    await page.goto(origin + "/accounts");
    await page
      .getByText(
        "No server accounts yet. Connect a machine and migrate your profiles to see them here.",
      )
      .waitFor();
    snapshot.accounts = savedAccounts;
    snapshot.machines = activeMachines;
    await page.goto(origin + "/");
    await page.keyboard.press("Tab");
    assert.equal(await page.locator(":focus").textContent(), "Skip to content");
    // Exercise actual DOM escaping, polling and session-loss clearing.
    await page.clock.install();
    await page.goto(origin + "/accounts");
    await page
      .getByText("Everyday building", { exact: true })
      .first()
      .waitFor();
    const before = polls;
    snapshot.accounts[0].label = "<img src=x onerror=alert(1)>";
    await page.clock.fastForward(61000);
    await page
      .getByRole("heading", {
        name: "<img src=x onerror=alert(1)>",
        exact: true,
      })
      .first()
      .waitFor();
    assert.equal(polls, before + 1);
    assert.equal(await page.locator("#accounts img").count(), 0);
    snapshot.accounts[0].label = "Everyday building";
    await page.clock.fastForward(61000);
    await page
      .getByText("Everyday building", { exact: true })
      .first()
      .waitFor();
    unavailable = true;
    await page.clock.fastForward(61000);
    await page.getByRole("button", { name: "Retry Now" }).waitFor();
    assert(
      (await page.locator(".freshness").first().textContent()).includes(
        "Stale",
      ),
    );
    if (scheme === "light")
      await page.screenshot({
        path: `${out}/accounts-error-stale.png`,
        fullPage: true,
      });
    unavailable = false;
    await page.getByRole("button", { name: "Retry Now" }).click();
    await page
      .getByRole("button", { name: "Retry Now" })
      .waitFor({ state: "hidden" });
    denied = true;
    await page.clock.fastForward(61000);
    await page
      .getByText("Your session ended or access changed. Sign in to continue.")
      .waitFor();
    assert.equal(await page.locator("#accounts article").count(), 0);
    snapshot.accounts[0].label = "Everyday building";
    denied = false;
    await context.close();
  }
  assert.deepEqual(errors, []);
  await browser.close();
  server.close();
  console.log(
    "Light/dark desktop/phone screenshots, WCAG A/AA checks, copy, polling, DOM escaping, and session loss passed.",
  );
})().catch((e) => {
  console.error(e);
  server.close();
  process.exit(1);
});
