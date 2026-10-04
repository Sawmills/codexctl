// Copy commands and preserve disclosure state across refreshes.
function bindControls() {
  "use strict";
  const status = document.getElementById("copy-status");
  for (const button of document.querySelectorAll("[data-copy]")) {
    const label = button.textContent;
    button.addEventListener("click", async () => {
      const source = document.getElementById(button.dataset.copy);
      const text = [...source.childNodes]
        .filter((n) => !(n.classList && n.classList.contains("prompt")))
        .map((n) => n.textContent)
        .join("");
      try {
        await navigator.clipboard.writeText(text);
        if (status) status.textContent = "Copied. Paste it into your terminal.";
        button.textContent = "Copied";
        button.dataset.copied = "";
        setTimeout(() => {
          button.textContent = label;
          delete button.dataset.copied;
        }, 2000);
      } catch {
        if (status)
          status.textContent =
            "Copy is not available here. Select the command and copy it.";
      }
    });
  }

  // Keep open disclosures in the URL (?revoked=show, ?platform=linux).
  for (const details of document.querySelectorAll("details[data-param]")) {
    const { param, value } = details.dataset;
    if (new URL(location.href).searchParams.get(param) === value)
      details.open = true;
    details.addEventListener("toggle", () => {
      const next = new URL(location.href);
      if (details.open) next.searchParams.set(param, value);
      else next.searchParams.delete(param);
      history.replaceState(null, "", next);
    });
  }
}
bindControls();
