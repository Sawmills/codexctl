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
const round2 = new Set([
  "landing-light.png",
  "landing-light-phone.png",
  "accounts-error-stale.png",
  "accounts-dark-phone-first-screen.png",
]);
async function screenshot(page, options) {
  const filename = require("node:path").basename(options.path);
  if (process.argv[4] === "check") return;
  if (process.argv[4] !== "round2" || round2.has(filename))
    await page.screenshot(options);
}
async function checkLayout(page, name) {
  if (name === "landing") {
    for (const flag of await page.locator(".command-token").all()) {
      assert.equal(
        await flag.evaluate((el) => {
          const r = document.createRange();
          r.selectNodeContents(el);
          return r.getClientRects().length;
        }),
        1,
        "flags cannot split",
      );
    }
  } else {
    for (const row of await page.locator(".hero-machine-identity").all()) {
      assert(
        await row.evaluate((el) => {
          const account = el
            .querySelector(".account-pill")
            .getBoundingClientRect();
          const plan = el.querySelector(".plan").getBoundingClientRect();
          return Math.abs(account.top - plan.top) < 4;
        }),
        "plan belongs beside the account chip",
      );
    }
  }
  assert(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  );
}
const now = Math.floor(Date.now() / 1000);
const window = (used, seconds, reset) => ({
  used_percent: used,
  left_percent: used === null ? null : Math.max(0, 100 - used),
  window_seconds: seconds,
  resets_at: reset,
});
const snapshot = {
  version: 1,
  identity: { email: "preview@sawmills.ai" },
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
      name: "Linux build machine",
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
    label: "Extra capacity",
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
    label: "Feature sprint",
    primary: window(84, 18000, now + 2400),
    secondary: window(78, 604800, now + 109000),
  },
  {
    ...structuredClone(snapshot.accounts[0]),
    alias: "offline",
    label: "Login needs attention",
    state: "unavailable",
    usage_age_seconds: 184,
    usage_stale: true,
    billing_class: "unknown",
  },
  {
    ...structuredClone(snapshot.accounts[2]),
    alias: "flex",
    label: "Flexible research",
    state: "available",
    billing_class: "usage_based",
    usage_stale: false,
    usage_age_seconds: 18,
    primary: window(36, null, null),
    secondary: window(null, null, null),
  },
);
let signedOut = false;
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
  if (req.url === "/accounts/sign-out") {
    assert.equal(req.method, "POST");
    assert.equal(req.headers.origin, `http://${req.headers.host}`);
    signedOut = true;
    res.writeHead(303, { location: "/" });
    res.end();
    return;
  }
  if (req.url === "/ready") {
    res.writeHead(200);
    res.end();
    return;
  }
  const file =
    new URL(req.url, "http://localhost").pathname === "/accounts"
      ? "accounts.html"
      : "landing.html";
  res.writeHead(200, {
    "content-type": "text/html",
    "referrer-policy": "same-origin",
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
      if (name === "accounts") {
        assert.equal(
          await page.locator(".hero-machine").count(),
          2,
          "one hero row per machine",
        );
        const rows = await page.locator(".hero-machine").allTextContents();
        assert(
          rows[0].includes("Studio MacBook") &&
            rows[0].includes("studio") &&
            rows[0].includes("58%") &&
            rows[0].includes("76%"),
        );
        assert(
          rows[1].includes("Linux build machine") &&
            rows[1].includes("reserve") &&
            rows[1].includes("84%") &&
            rows[1].includes("88%"),
        );
        assert.deepEqual(
          await page.locator("#accounts .alias").allTextContents(),
          [
            "studio",
            "reserve",
            "sprint",
            "deep-work",
            "flex",
            "research",
            "offline",
          ],
        );
        const heights = await page
          .locator("#accounts > article")
          .evaluateAll((cards) =>
            cards.map((c) => c.getBoundingClientRect().height),
          );
        assert(
          heights.every((h) => h <= 260),
          `card heights must fit 260px: ${heights}`,
        );
        assert.equal(
          await page.locator("#identity-email").textContent(),
          "preview@sawmills.ai",
        );
        assert.equal(
          await page.getByRole("button", { name: "Sign out" }).count(),
          1,
        );
        const reserve = page
          .locator("#accounts > article")
          .filter({ has: page.locator(".alias", { hasText: /^reserve$/ }) });
        assert((await reserve.textContent()).includes("No banked resets"));
        assert(!(await reserve.textContent()).includes("0 redeemable"));
        const offline = page
          .locator("#accounts > article")
          .filter({ has: page.locator(".alias", { hasText: /^offline$/ }) });
        assert((await offline.textContent()).includes("Updated 3 min ago"));
        assert.equal(
          await offline.locator(".pill", { hasText: /^Stale$/ }).count(),
          1,
        );
        assert.equal(await page.locator("#machines > article").count(), 2);
        await page.getByText("Show revoked (1)", { exact: true }).click();
        await page.getByText("Previous workstation", { exact: true }).waitFor();
        await page.getByText("Show revoked (1)", { exact: true }).click();
      }
      await checkLayout(page, name);
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
      await page.evaluate(() => scrollTo(0, 0));
      await screenshot(page, {
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
      await page.setViewportSize({ width: 768, height: 1024 });
      await checkLayout(page, name);
      await page.setViewportSize({ width: 390, height: 844 });
      await checkLayout(page, name);
      await page.evaluate(() => scrollTo(0, 0));
      if (name === "landing")
        await page.locator("details").evaluate((e) => (e.open = false));
      assert(
        await page.evaluate(
          () => document.documentElement.scrollWidth <= innerWidth,
        ),
      );
      if (name === "landing") {
        assert.equal(
          await page
            .locator(".terminal")
            .evaluate((el) => getComputedStyle(el).transform),
          "none",
        );
        for (const flag of await page.locator(".command-token").all()) {
          assert.equal(
            await flag.evaluate((el) => {
              const r = document.createRange();
              r.selectNodeContents(el);
              return r.getClientRects().length;
            }),
            1,
            "command flags stay intact",
          );
        }
      }
      await screenshot(page, {
        path: `${out}/${name}-${scheme}-phone.png`,
        fullPage: true,
      });
      if (name === "accounts")
        await screenshot(page, {
          path: `${out}/accounts-${scheme}-phone-first-screen.png`,
        });
      await page.setViewportSize({ width: 1440, height: 1100 });
    }
    await page.getByRole("button", { name: "Sign out" }).click();
    await page.waitForURL(origin + "/");
    assert(signedOut, "native sign-out form submits with same-origin evidence");
    // Distinguish a single older account from stale observations across the page.
    const originalAccounts = structuredClone(snapshot.accounts);
    snapshot.accounts[0].usage_stale = true;
    await page.goto(origin + "/accounts");
    await page.locator("#featured .pill", { hasText: /^Stale$/ }).waitFor();
    assert(
      await page
        .locator("#featured .pill", { hasText: /^Stale$/ })
        .evaluate((el) => el.getBoundingClientRect().width < 70),
      "stale pill hugs text",
    );
    snapshot.accounts.forEach((a) => (a.usage_stale = true));
    await page.goto(origin + "/accounts");
    await page
      .getByText("All usage observations are stale.", { exact: false })
      .waitFor();
    assert.equal(
      await page
        .locator("#accounts .pill, #featured .pill")
        .filter({ hasText: /^Stale$/ })
        .count(),
      0,
    );
    snapshot.accounts = originalAccounts;
    // Keyboard, reduced-motion, empty, and suggested states use the same bundled page.
    await page.emulateMedia({ reducedMotion: "reduce" });
    const activeMachines = structuredClone(snapshot.machines);
    snapshot.machines.forEach((m) => (m.last_seen_at = null));
    await page.goto(origin + "/accounts");
    await page.getByText("Suggested", { exact: true }).waitFor();
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
      await page.locator("#featured h2").textContent(),
      "Extra capacity",
    );
    if (scheme === "light")
      await screenshot(page, {
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
    await page.getByRole("button", { name: "Retry now" }).waitFor();
    assert.equal(
      await page.locator("#connection .pill", { hasText: /^Stale$/ }).count(),
      1,
    );
    assert.equal(
      await page
        .locator("#accounts .pill, #featured .pill")
        .filter({ hasText: /^Stale$/ })
        .count(),
      0,
      "one page-wide stale banner",
    );
    if (scheme === "light")
      await screenshot(page, {
        path: `${out}/accounts-error-stale.png`,
        fullPage: true,
      });
    unavailable = false;
    await page.getByRole("button", { name: "Retry now" }).click();
    await page
      .getByRole("button", { name: "Retry now" })
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
