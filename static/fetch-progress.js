// Shows which sources are fetching on the Sources and engine pages: a "fetching" or
// "queued" badge on their rows, and a progress bar while "Fetch all now" runs. It polls
// /sources/activity while anything is fetching or a fetch form is waiting for its answer.
// The page swap that inline-forms.js does afterwards brings the final status.
(() => {
  "use strict";

  const FAST = 1000;
  let timer = null;

  function schedule(delay) {
    clearTimeout(timer);
    timer = setTimeout(tick, delay);
  }

  // Badges show their server-rendered status again once a source is idle.
  function mark(row, state) {
    const badge = row.querySelector(".badge[data-status]");
    row.classList.toggle("is-fetching", state === "fetching");
    row.classList.toggle("is-queued", state === "queued");
    if (!badge) return;
    badge.textContent = state ?? badge.dataset.status;
  }

  function render(activity) {
    const fetching = new Set(activity.fetching);
    const queued = new Set(activity.queued);
    const labels = [];
    for (const row of document.querySelectorAll("tr[data-source]")) {
      const id = row.dataset.source;
      const state = fetching.has(id) ? "fetching" : queued.has(id) ? "queued" : null;
      mark(row, state);
      if (state === "fetching") labels.push(row.dataset.label);
    }

    const box = document.querySelector(".fetch-progress");
    if (!box) return;
    const batch = activity.batch;
    box.hidden = !batch;
    if (!batch) return;
    const bar = box.querySelector("progress");
    bar.max = batch.total;
    bar.value = batch.done;
    let text = `${batch.done} of ${batch.total} fetched · ${batch.new_items} new`;
    if (batch.failed) text += ` · ${batch.failed} failed`;
    if (batch.skipped) text += ` · ${batch.skipped} skipped`;
    const names = [...new Set(labels)];
    if (names.length) {
      text += ` · now: ${names.slice(0, 3).join(", ")}`;
      if (names.length > 3) text += ` +${names.length - 3}`;
    }
    box.querySelector(".fetch-progress-text").textContent = text;
  }

  async function tick() {
    let activity;
    try {
      const response = await fetch("/sources/activity", { credentials: "same-origin" });
      if (!response.ok) return; // Logged out: stop polling.
      activity = await response.json();
    } catch {
      schedule(FAST * 5);
      return;
    }
    render(activity);
    const busy = activity.fetching.length || activity.queued.length || activity.batch
      || document.querySelector('form[aria-busy="true"][action^="/sources/fetch"], form[aria-busy="true"][action="/sources/interval"]');
    if (busy) schedule(FAST);
  }

  document.addEventListener("submit", (event) => {
    const form = event.target;
    const action = form.getAttribute?.("action") ?? "";
    if (!action.startsWith("/sources/fetch") && action !== "/sources/interval") return;
    // Show the request right away; the next poll corrects it.
    const id = form.querySelector('input[name="source_id"]')?.value;
    const rows = id
      ? [...document.querySelectorAll("tr[data-source]")].filter((r) => r.dataset.source === id)
      : action === "/sources/fetch-all"
        ? document.querySelectorAll("tr[data-source]:not(.is-disabled)")
        : [];
    rows.forEach((row) => mark(row, "queued"));
    // The last fetch's result would read as this one's.
    document.querySelectorAll(".notice[role=status]").forEach((n) => { n.hidden = true; });
    schedule(200);
  });

  if (document.querySelector("tr[data-source]")) tick();
})();
