// Submits forms marked `data-inline` in the background and swaps in the page the server
// answers with, so admin actions don't reload the page, and runs the add-source and
// add-engine/outlet dialogs. Without this script the forms work as ordinary posts, and
// dialogs rendered `open` show in place. It also submits a `data-autosubmit` form (the
// timeline's filter toolbar) as soon as one of its dropdowns changes; without the
// script, its Search button applies them.
(() => {
  "use strict";

  // A dialog the server rendered open (a failed submission, or an owner to preselect)
  // becomes a modal again.
  function reopen(root) {
    for (const dialog of root.querySelectorAll("dialog[data-reopen][open]")) {
      dialog.removeAttribute("data-reopen");
      dialog.close();
      dialog.showModal();
    }
    // The owner to preselect has done its job; don't reopen the dialog on reload.
    const url = new URL(location.href);
    if (url.searchParams.has("owner") || url.hash === "#add-source") {
      url.searchParams.delete("owner");
      url.hash = "";
      history.replaceState(null, "", url);
    }
  }

  document.addEventListener("click", (event) => {
    // "Add source" next to an engine or outlet: open the dialog with it preselected.
    const link = event.target.closest("a[data-add-source]");
    const dialog = document.getElementById("add-source");
    if (link && dialog) {
      event.preventDefault();
      const select = dialog.querySelector('select[name="owner_id"]');
      if (select) select.value = link.dataset.addSource;
      dialog.showModal();
      return;
    }
    // `commandfor` buttons, for browsers without invoker commands.
    const button = event.target.closest("button[commandfor]");
    if (button && !("command" in HTMLButtonElement.prototype)) {
      const target = document.getElementById(button.getAttribute("commandfor"));
      if (!(target instanceof HTMLDialogElement)) return;
      if (button.getAttribute("command") === "show-modal") target.showModal();
      if (button.getAttribute("command") === "close") target.close();
    }
  });

  document.addEventListener("change", (event) => {
    const select = event.target;
    if (!(select instanceof HTMLSelectElement)) return;
    const form = select.closest("form[data-autosubmit]");
    if (form) form.requestSubmit();
  });

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

    // Close the confirmation popover or dialog (they live in the top layer) before its
    // DOM goes.
    form.closest("[popover]")?.hidePopover?.();
    const dialog = form.closest("dialog");
    dialog?.close();
    main.replaceWith(next);
    document.title = doc.title;
    for (const id of open) {
      const details = document.getElementById(id);
      if (details instanceof HTMLDetailsElement) details.open = true;
    }
    if (response.redirected && response.url !== location.href) {
      history.replaceState(null, "", response.url);
    }
    reopen(next);
    const error = next.querySelector(".form-error, .error-page");
    if (error) {
      error.scrollIntoView({ block: "nearest" });
      announce(error.textContent.trim());
    } else {
      // Back to the button that opened the dialog, or the form's counterpart.
      const opener = dialog?.id && next.querySelector(`[commandfor="${dialog.id}"][command="show-modal"]`);
      if (opener) opener.focus({ preventScroll: true });
      else refocus(next, action, key, index);
      // A result message on the page says more than the form's generic one.
      const status = next.querySelector(".notice[role=status]");
      announce(status ? status.textContent.trim() : form.dataset.inline || "Saved.");
    }
  });

  reopen(document);
})();
