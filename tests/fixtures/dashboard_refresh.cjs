// Focused regression for B22 review: run against Rust-exported fixture pages.
const { chromium } = require("playwright");
const fs = require("node:fs");
const assert = require("node:assert/strict");
const root = process.argv[2] || "/tmp/b22-render";
const html = fs.readFileSync(`${root}/accounts.html`, "utf8");
const csp = fs.readFileSync(`${root}/accounts.html.csp`, "utf8");
function view(validFor, generation = 0) {
  return html
    .replace(/data-valid-for="\d+"/, `data-valid-for="${validFor}"`)
    .replace('id="overview"', `id="overview" data-generation="${generation}"`);
}
(async () => {
  const browser = await chromium.launch();
  try {
    for (const validFor of [60, 15, 1]) {
      const page = await browser.newPage();
      await page.clock.install();
      let pending = null,
        polls = 0;
      await page.route("http://fixture.test/**", async (route) => {
        if (route.request().url().endsWith("/accounts/data")) {
          polls++;
          pending = route;
          return;
        }
        if (route.request().url().includes("/assets/fonts/")) {
          await route.fulfill({ status: 200, body: "" });
          return;
        }
        await route.fulfill({
          contentType: "text/html",
          headers: { "content-security-policy": csp },
          body: view(validFor),
        });
      });
      await page.goto("http://fixture.test/accounts");
      await page.evaluate(() => {
        window.answerFlashed = false;
        new MutationObserver(() => {
          if (!document.getElementById("answer-fallback").hidden)
            window.answerFlashed = true;
        }).observe(document.getElementById("overview").parentNode, {
          subtree: true,
          attributes: true,
          childList: true,
        });
      });
      const margin = Number(html.match(/data-refresh-margin="(\d+)"/)[1]);
      const due = Math.max(1, validFor - margin) * 1000;
      await page.clock.runFor(due + 1);
      await new Promise((resolve) => setTimeout(resolve, 50));
      assert(
        polls > 0,
        `must poll before ${validFor}s validity ends, not wait 60s`,
      );
      // A nearly-expired page must start immediately; other cases retain enough
      // time for a real upstream round trip without withdrawing the answer.
      if (validFor > 1) await page.clock.runFor(validFor === 60 ? 30000 : 5000);
      await pending.fulfill({
        contentType: "text/html",
        body: view(validFor === 60 ? 15 : 60, 1),
      });
      await page.waitForFunction(
        () =>
          document.getElementById("overview").dataset.generation === "1" &&
          document.getElementById("answer-current").hidden === false,
      );
      if (validFor > 1)
        assert.equal(
          await page.evaluate(() => window.answerFlashed),
          false,
          "fresh answer must not flash while refresh is in flight",
        );
      // The next successful cycle must also be scheduled from validity, not
      // from a fixed interval or the response completion time.
      await page.clock.runFor(validFor === 60 ? 1001 : 26000);
      await new Promise((resolve) => setTimeout(resolve, 50));
      assert.equal(polls, 2, "successful refresh rearms the adaptive timer");
      await pending.fulfill({ contentType: "text/html", body: view(60, 2) });
      await page.waitForFunction(
        () => document.getElementById("overview").dataset.generation === "2",
      );
      if (validFor > 1)
        assert.equal(await page.evaluate(() => window.answerFlashed), false);
      await page.close();
    }
    console.log(
      "Fresh and mid-aged observations refresh before expiry without flashing; near-expiry pages refresh promptly.",
    );
  } finally {
    await browser.close();
  }
})().catch((e) => {
  console.error(e);
  process.exit(1);
});
