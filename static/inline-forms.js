// Submits forms marked `data-inline` in the background and swaps in the page the server
// answers with, so admin actions don't reload the page. Without this script the forms
// work as ordinary posts.
(() => {
  "use strict";

  let status;
  function announce(text) {
    if (!status) {
      status = document.createElement("p");
      status.className = "visually-hidden";
      status.setAttribute("role", "status");
      document.body.append(status);
    }
    status.textContent = "";
    // A fresh text node after clearing makes screen readers repeat the same message.
    setTimeout(() => { status.textContent = text; }, 50);
  }

  // The id of the source or engine/outlet a form acts on, to find its counterpart later.
  function target(form) {
    const field = form.querySelector('input[name="source_id"], input[name="owner_id"]');
    return field ? field.name + "=" + field.value : null;
  }

  // After the swap, focus the same form's button again, or else the one that took the
  // submitted form's place in the list (e.g. the next suggestion).
  function refocus(main, action, key, index) {
    const forms = [...main.querySelectorAll(`form[data-inline][action="${action}"]`)];
    const form = forms.find((f) => key && target(f) === key)
      ?? forms[Math.min(index, forms.length - 1)];
    const button = form?.querySelector('button[type="submit"]');
    if (button) button.focus({ preventScroll: true });
  }

  document.addEventListener("submit", async (event) => {
    const form = event.target;
    if (!(form instanceof HTMLFormElement) || !form.hasAttribute("data-inline")) return;
    event.preventDefault();
    const main = document.getElementById("main");
    const action = form.getAttribute("action");
    const key = target(form);
    const index = [...main.querySelectorAll(`form[data-inline][action="${action}"]`)].indexOf(form);
    const open = [...main.querySelectorAll("details[open][id]")].map((d) => d.id);
    const buttons = form.querySelectorAll("button");
    buttons.forEach((b) => { b.disabled = true; });
    form.setAttribute("aria-busy", "true");

    let response, html;
    try {
      response = await fetch(form.action, {
        method: "POST",
        body: new URLSearchParams(new FormData(form, event.submitter)),
        credentials: "same-origin",
      });
      html = await response.text();
    } catch {
      form.submit(); // Network trouble: fall back to an ordinary post.
      return;
    }
    const doc = new DOMParser().parseFromString(html, "text/html");
    const next = doc.getElementById("main");
    if (!next) {
      form.submit();
      return;
    }

    // Close the confirmation popover (it lives in the top layer) before its DOM goes.
    form.closest("[popover]")?.hidePopover?.();
    main.replaceWith(next);
    document.title = doc.title;
    for (const id of open) {
      const details = document.getElementById(id);
      if (details instanceof HTMLDetailsElement) details.open = true;
    }
    if (response.redirected && response.url !== location.href) {
      history.replaceState(null, "", response.url + location.hash);
    }
    const error = next.querySelector(".form-error, .error-page");
    if (error) {
      error.scrollIntoView({ block: "nearest" });
      announce(error.textContent.trim());
    } else {
      refocus(next, action, key, index);
      announce(form.dataset.inline || "Saved.");
    }
  });
})();
