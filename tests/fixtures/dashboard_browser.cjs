// Render first: B22_RENDER_DIR=/tmp/b22-render cargo test --lib central::dashboard
// B22_RENDER_DIR=/tmp/b22-render cargo test --test central_managed_test dashboard_v3_
// NODE_PATH=<playwright + @axe-core/playwright modules> node tests/fixtures/dashboard_browser.cjs /tmp/b22-render <screens-dir>
const { chromium } = require("playwright");
const AxeBuilder = require("@axe-core/playwright").default;
const http = require("node:http");
const fs = require("node:fs");
const path = require("node:path");
const assert = require("node:assert/strict");
const root = process.argv[2] || "/tmp/b22-render";
const out = process.argv[3] || "/tmp/b22-screens";
fs.mkdirSync(out, { recursive: true });
let responseMode = "accounts",
  polls = 0,
  signedOut = false;
const server = http.createServer((req, res) => {
  const route = new URL(req.url, "http://localhost").pathname;
  if (route.startsWith("/assets/fonts/")) {
    res.writeHead(200, { "content-type": "font/woff2" });
    res.end(
      fs.readFileSync(path.join("design/v3/fonts", path.basename(route))),
    );
    return;
  }
  if (route === "/accounts/sign-out") {
    assert.equal(req.method, "POST");
    assert.equal(req.headers.origin, `http://${req.headers.host}`);
    signedOut = true;
    res.writeHead(303, { location: "/landing" });
    res.end();
    return;
  }
  let name = route.slice(1) || "landing";
  if (route === "/accounts/data") {
    polls++;
    assert.equal(req.headers.accept, "text/html");
    if (["401", "403", "503"].includes(responseMode)) {
      res.writeHead(Number(responseMode));
      res.end();
      return;
    }
    name = responseMode;
  }
  if (!fs.existsSync(`${root}/${name}.html`)) {
    res.writeHead(404);
    res.end();
    return;
  }
  res.writeHead(200, {
    "content-type": "text/html",
    "content-security-policy": fs.readFileSync(
      `${root}/${name}.html.csp`,
      "utf8",
    ),
    "cache-control": "no-store",
  });
  res.end(fs.readFileSync(`${root}/${name}.html`));
});
(async () => {
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const origin = `http://127.0.0.1:${server.address().port}`;
  const browser = await chromium.launch({ headless: true });
  const errors = [];
  for (const scheme of ["light", "dark"]) {
    const context = await browser.newContext({
      colorScheme: scheme,
      permissions: ["clipboard-read", "clipboard-write"],
    });
    const page = await context.newPage();
    page.on("pageerror", (e) => errors.push(String(e)));
    page.on("console", (m) => {
      if (
        m.type() === "error" &&
        !/Failed to load resource.*(401|403|503|404)/.test(m.text())
      )
        errors.push(m.text());
    });
    page.on("request", (r) =>
      assert(r.url().startsWith(origin), "no external asset requests"),
    );
    for (const width of [1440, 390]) {
      await page.setViewportSize({ width, height: width === 390 ? 844 : 1000 });
      for (const name of [
        "landing",
        "accounts",
        "healthy",
        "blocked",
        "stale",
        "empty",
        "approval",
        "connected",
      ]) {
        await page.goto(`${origin}/${name}`);
        await page.evaluate(() => document.fonts.ready);
        assert(
          await page.evaluate(
            () => document.documentElement.scrollWidth <= innerWidth,
          ),
          `${name} overflows at ${width}`,
        );
        assert(
          await page.evaluate(() =>
            document.fonts.check('14px "Instrument Sans"'),
          ),
          "self-hosted font loaded",
        );
        const axe = await new AxeBuilder({ page })
          .withTags(["wcag2a", "wcag2aa", "wcag21aa", "wcag22aa"])
          .analyze();
        assert.deepEqual(
          axe.violations.map((v) => ({
            id: v.id,
            nodes: v.nodes.map((n) => n.target),
          })),
          [],
          `${name} ${scheme} ${width} accessibility`,
        );
        await page.keyboard.press("Tab");
        assert.equal(
          await page.locator(":focus").textContent(),
          "Skip to content",
        );
        const outline = await page
          .locator(":focus")
          .evaluate((el) => getComputedStyle(el).outlineStyle);
        assert.notEqual(outline, "none", "keyboard focus visible");
        await page.keyboard.press("Escape");
        await page.locator("h1:visible").first().click();
        await page.screenshot({
          path: `${out}/${name}-${scheme}-${width}.png`,
          fullPage: true,
        });
      }
    }
    await page.goto(`${origin}/accounts`);
    assert.equal(
      await page.locator("#cmd-use").textContent(),
      "codexctl use studio",
    );
    await page.locator('[data-copy="cmd-use"]').click();
    assert.equal(
      await page.evaluate(() => navigator.clipboard.readText()),
      "codexctl use studio",
    );
    assert.equal(await page.locator(".revoked").getAttribute("open"), null);
    await page.locator(".revoked summary").click();
    await page.waitForURL((url) => url.searchParams.get("revoked") === "show");
    await page.reload();
    assert.notEqual(await page.locator(".revoked").getAttribute("open"), null);
    await page.locator("button", { hasText: "Sign out" }).click();
    assert(signedOut);
    await page.goto(`${origin}/connected`);
    await page.locator('[data-copy="next-use"]').click();
    assert.equal(
      await page.evaluate(() => navigator.clipboard.readText()),
      "codexctl use",
    );
    await page.goto(`${origin}/landing?platform=linux`);
    await page.locator('[data-copy="linux-install"]').click();
    assert(
      (await page.evaluate(() => navigator.clipboard.readText())).includes(
        "sha256sum --check",
      ),
    );
    await page.emulateMedia({ reducedMotion: "reduce" });
    assert.equal(
      await page
        .locator(".button")
        .first()
        .evaluate((el) => getComputedStyle(el).transitionDuration),
      "0s",
    );
    await page.clock.install();
    await page.goto(`${origin}/accounts`);
    const before = polls;
    await page.clock.fastForward(40000);
    await page.waitForFunction(
      () => document.getElementById("overview").dataset.validFor === "52",
    );
    assert.equal(await page.locator("#answer-current").isVisible(), true);
    assert.equal(await page.locator("#answer-fallback").isVisible(), false);
    responseMode = "503";
    await page.clock.fastForward(60000);
    await page
      .getByText(
        "Refresh failed. Last observed figures are not proof of headroom.",
      )
      .waitFor();
    assert(polls > before);
    assert.equal(
      await page.locator(".fresh-action:visible").count(),
      0,
      "stale reset commands are hidden",
    );
    assert.equal(await page.locator("#connection:visible").count(), 1);
    responseMode = "healthy";
    await page.getByRole("button", { name: "Retry now" }).click();
    await page.locator(".all-clear").waitFor();
    assert.equal(
      await page.locator("#cmd-use").textContent(),
      "codexctl use studio",
    );
    for (const code of ["401", "403"]) {
      await page.goto(`${origin}/accounts`);
      responseMode = code;
      await page.clock.fastForward(61000);
      await page
        .getByText("Your session ended or access changed. Sign in to continue.")
        .waitFor();
      assert.equal(
        await page.locator(".ledger, .identity, .machines").count(),
        0,
      );
    }
    responseMode = "healthy";
    await page.goto(`${origin}/error`);
    await page
      .getByRole("heading", { name: "Your accounts could not be loaded" })
      .waitFor();
    await page.getByRole("button", { name: "Retry now" }).click();
    await page.locator("#cmd-use").waitFor();
    assert.equal(
      await page.locator("#cmd-use").textContent(),
      "codexctl use studio",
    );
    // The error page also recovers automatically without a manual reload.
    await page.goto(`${origin}/error`);
    await page.clock.fastForward(61000);
    await page.locator("#cmd-use").waitFor();
    responseMode = "accounts";
    await context.close();
  }
  // All essential page contents work without JavaScript.
  const noScript = await browser.newContext({ javaScriptEnabled: false });
  const page = await noScript.newPage();
  await page.goto(`${origin}/accounts`);
  assert.equal(
    await page.locator("#cmd-use").textContent(),
    "codexctl use studio",
  );
  assert.equal(await page.locator(".ledger tbody tr").count(), 8);
  assert.equal(
    await page.locator("noscript meta[http-equiv=refresh]").count(),
    1,
  );
  await noScript.close();
  assert.deepEqual(errors, []);
  await browser.close();
  server.close();
  console.log(
    "36 screenshots; desktop/390px light/dark; WCAG A/AA; focus; copy; disclosures; SSR without JS; polling; stale expiry; session loss passed.",
  );
})().catch((e) => {
  console.error(e);
  server.close();
  process.exit(1);
});
