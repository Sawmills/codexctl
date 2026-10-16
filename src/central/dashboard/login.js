(() => {
  const root = document.querySelector("[data-browser-login]");
  const form = document.getElementById("login-status-form");
  if (!root || !form) return;
  const labels = {
    starting: "Starting login",
    pending: "Finish your OpenAI sign-in",
    verifying: "Checking the sign-in",
    completed: "Login completed",
    canceled: "Login canceled",
    expired: "Login expired",
    failed: "Login failed",
  };
  const active = (status) =>
    ["starting", "pending", "verifying"].includes(status);
  let inFlight = false;
  let timer;
  async function poll() {
    if (inFlight) return;
    clearTimeout(timer);
    inFlight = true;
    try {
      const response = await fetch(form.action, {
        method: "POST",
        credentials: "same-origin",
        cache: "no-store",
        headers: { Accept: "application/json" },
        body: new URLSearchParams(new FormData(form)),
        signal: AbortSignal.timeout(10000),
      });
      if (!response.ok) {
        if ([401, 403, 404].includes(response.status))
          root.dataset.status = "expired";
        throw new Error("Login status unavailable");
      }
      const view = await response.json();
      root.dataset.status = view.status;
      document.getElementById("login-state").textContent =
        labels[view.status] || "Login status unavailable";
      const code = typeof view.userCode === "string" ? view.userCode : "";
      document.getElementById("login-code").textContent = code;
      document.getElementById("login-challenge").hidden = !code;
      document.getElementById("login-cancel-form").hidden = !active(
        view.status,
      );
      document.getElementById("login-result").textContent =
        view.status === "completed"
          ? `Server account ${view.landedAlias || view.alias} is ready.`
          : view.status === "failed"
            ? "Return to accounts and start a new login."
            : "";
    } catch {
      document.getElementById("login-code").textContent = "";
      document.getElementById("login-challenge").hidden = true;
      document.getElementById("login-state").textContent =
        root.dataset.status === "expired"
          ? "This browser sign-in ended or the login is unavailable. Return to accounts."
          : "Cannot load login status. Try Check status.";
    } finally {
      inFlight = false;
      if (active(root.dataset.status)) timer = setTimeout(poll, 2000);
    }
  }
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    poll();
  });
  if (active(root.dataset.status)) timer = setTimeout(poll, 1000);
})();
