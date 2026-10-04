// Refresh server-rendered markup; account decisions remain on the server.
(() => {
  "use strict";
  const byId = (id) => document.getElementById(id);
  if (!byId("overview")) return;
  let received = 0,
    inFlight = false,
    ended = false;
  function expire(message) {
    const root = byId("overview");
    if (!root) return;
    root.classList.add("overview-expired");
    byId("answer-current").hidden = true;
    byId("answer-fallback").hidden = false;
    byId("connection").hidden = false;
    byId("connection-text").textContent = message;
    // Retain the observations, but remove any claim that they prove capacity.
    for (const row of root.querySelectorAll(".ledger tbody tr"))
      row.classList.add("stale");
    for (const row of root.querySelectorAll(".recommended"))
      row.classList.remove("recommended");
    for (const note of root.querySelectorAll(
      ".note.pick, .all-clear, #attention-title .count",
    ))
      note.hidden = true;
    for (const state of root.querySelectorAll(".ledger .state")) {
      if (
        ["Available", "Nearly exhausted", "Exhausted", "Stale usage"].includes(
          state.textContent,
        )
      ) {
        state.textContent = "Last observed";
        state.className = "state idle";
      }
    }
  }
  async function poll() {
    if (inFlight || ended) return;
    inFlight = true;
    const started = performance.now();
    byId("retry").disabled = true;
    try {
      const response = await fetch("/accounts/data", {
        headers: { Accept: "text/html" },
        credentials: "same-origin",
        cache: "no-store",
        signal: AbortSignal.timeout(45000),
      });
      if (response.status === 401 || response.status === 403) {
        ended = true;
        byId("overview").replaceChildren();
        const main = document.createElement("main");
        main.className = "wrap";
        main.id = "main";
        const text = document.createElement("h1");
        text.textContent =
          "Your session ended or access changed. Sign in to continue.";
        const link = document.createElement("a");
        link.href = "/accounts/sign-in";
        link.textContent = "Sign in with company SSO";
        main.append(text, link);
        byId("overview").append(main);
        return;
      }
      if (!response.ok) throw new Error("Refresh unavailable");
      const doc = new DOMParser().parseFromString(
        await response.text(),
        "text/html",
      );
      const next = doc.getElementById("overview");
      if (!next) throw new Error("Invalid overview");
      const focused = document.activeElement;
      const focusId = focused?.id;
      const focusCopy = focused?.dataset.copy;
      byId("overview").replaceWith(next);
      received = started; // Include request time in the age, conservatively.
      bindControls();
      bindRetry();
      if (focusId) byId(focusId)?.focus({ preventScroll: true });
      else if (focusCopy) {
        [...document.querySelectorAll("[data-copy]")]
          .find((el) => el.dataset.copy === focusCopy)
          ?.focus({ preventScroll: true });
      }
      tick();
    } catch {
      expire(
        "Refresh failed. Last observed figures are not proof of headroom.",
      );
    } finally {
      inFlight = false;
      if (byId("retry")) byId("retry").disabled = false;
    }
  }
  function bindRetry() {
    byId("retry").addEventListener("click", poll);
  }
  function tick() {
    if (ended) return;
    const root = byId("overview");
    const elapsed = (performance.now() - received) / 1000;
    if (
      elapsed >= Number(root.dataset.validFor) &&
      !root.classList.contains("overview-expired")
    ) {
      expire(
        "Observations have aged. Cannot confirm headroom; refreshes every 60 seconds.",
      );
    }
    for (const observed of root.querySelectorAll("[data-age]")) {
      if (observed.dataset.age === "") continue;
      const age = Math.floor(Number(observed.dataset.age) + elapsed);
      observed.textContent = `Last observed ${age < 60 ? `${age} s` : `${Math.floor(age / 60)} min`} ago`;
    }
    const now = Number(root.dataset.serverTime) + elapsed;
    for (const state of root.querySelectorAll("[data-seen]")) {
      if (now - Number(state.dataset.seen) >= 300) {
        state.className = "state idle";
        state.textContent = "Idle";
      }
    }
  }
  bindRetry();
  setInterval(poll, 60000);
  setInterval(tick, 1000);
  document.addEventListener("visibilitychange", () => {
    if (!document.hidden) {
      tick();
      poll();
    }
  });
})();
