// Prototype behavior only: copy buttons. Everything else is static markup.
(() => {
  "use strict";
  const status = document.getElementById("copy-status");
  for (const button of document.querySelectorAll("[data-copy]")) {
    button.addEventListener("click", async () => {
      const source = document.getElementById(button.dataset.copy);
      try {
        await navigator.clipboard.writeText(source.textContent);
        if (status) status.textContent = "Copied. Paste it into your terminal.";
        button.textContent = "Copied";
        setTimeout(() => (button.textContent = "Copy"), 2000);
      } catch {
        if (status)
          status.textContent =
            "Copy is not available here. Select the command and copy it.";
      }
    });
  }
})();

// Keep open disclosures in the URL (?revoked=show, ?platform=linux) so a link
// reopens the same view.
(() => {
  "use strict";
  for (const details of document.querySelectorAll("details[data-param]")) {
    const { param, value } = details.dataset;
    const url = new URL(location.href);
    if (url.searchParams.get(param) === value) details.open = true;
    details.addEventListener("toggle", () => {
      const next = new URL(location.href);
      if (details.open) next.searchParams.set(param, value);
      else next.searchParams.delete(param);
      history.replaceState(null, "", next);
    });
  }
})();
