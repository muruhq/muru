// The development toolbar (design specification section 4.20): a badge
// on the page for the request that produced it, opening to the panels of
// its record. It reads and changes nothing, and runs no panel's script:
// the panels' HTML comes from the edge, already sanitised.

(() => {
  "use strict";

  const script = document.currentScript;

  if (!script || !script.dataset.request) {
    return;
  }

  const id = script.dataset.request;
  const dashboard = script.dataset.dashboard;
  const nonce = script.nonce;
  const base = script.src.replace(/toolbar\.js(\?.*)?$/, "");
  const record = `${base}requests/${encodeURIComponent(id)}`;
  const panelsPath = new URL(`${record}/panels/`, location.href).pathname;

  const element = (name, className, text) => {
    const made = document.createElement(name);

    if (className) {
      made.className = className;
    }

    if (text !== undefined) {
      made.textContent = text;
    }

    return made;
  };

  const host = element("div");
  host.id = "muru-toolbar";
  const root = host.attachShadow({ mode: "open" });
  const style = element("link");
  style.rel = "stylesheet";
  style.href = `${base}toolbar.css`;

  if (nonce) {
    style.nonce = nonce;
  }

  const bar = element("div", "toolbar collapsed");
  const badge = element("button", "badge", "muru");
  badge.type = "button";
  badge.title = "Muru: this page's request";
  const body = element("div", "body");
  const tabs = element("nav", "tabs");
  const content = element("div", "panel content");
  const footer = element("div", "footer");
  body.append(tabs, content, footer);
  bar.append(badge, body);
  root.append(style, bar);
  (document.body || document.documentElement).append(host);

  let current = null;
  let polls = 0;

  const millis = (ms) => (ms >= 1000 ? `${(ms / 1000).toFixed(2)}s` : `${ms.toFixed(1)}ms`);

  // The badge: status, total time, queue, worker, and the counts of log
  // records and errors.

  const summary = (found) => {
    badge.replaceChildren();
    const worker = found.from_worker;
    const logs = worker ? worker.logs.length + worker.logs_dropped : 0;
    const failed = (worker && worker.outcome && worker.outcome !== "completed") || found.status >= 500;
    const status = element("span", found.status >= 400 ? "status error" : "status", found.in_progress && !found.status ? "…" : String(found.status));
    badge.append(element("span", "name", "muru"), status);

    if (found.status) {
      badge.append(element("span", "figure", millis(found.total_ms)));
      badge.append(element("span", "figure muted", `queue ${millis(found.queue_ms)}`));
    }

    if (found.worker !== null && found.worker !== undefined) {
      badge.append(element("span", "figure muted", `worker ${found.group && found.group !== "default" ? `${found.group}/` : ""}${found.worker}`));
    }

    badge.append(element("span", logs ? "count warning" : "count muted", `${logs} log`));
    badge.append(element("span", failed ? "count error" : "count muted", `${failed ? 1 : 0} error`));
  };

  // The tabs, one for each panel of the record, each loaded when first
  // opened.

  const tabbed = (found) => {
    if (tabs.childElementCount) {
      return;
    }

    for (const panel of found.panels) {
      const tab = element("button", "tab", panel.title);
      tab.type = "button";
      tab.dataset.name = panel.name;
      tab.addEventListener("click", () => open(panel.name));
      tabs.append(tab);
    }

    footer.replaceChildren(element("span", "muted", `request ${id}`));

    if (dashboard) {
      const link = element("a", "", "open in dashboard");
      link.href = dashboard;
      link.target = "_blank";
      link.rel = "noopener";
      footer.append(link);
    }
  };

  const show = async (address) => {
    try {
      const response = await fetch(address, { credentials: "same-origin", cache: "no-store" });
      content.innerHTML = response.ok ? await response.text() : "";

      if (!response.ok) {
        content.append(element("p", "muted", "This panel is no longer kept."));
      }
    } catch {
      content.replaceChildren(element("p", "muted", "The run did not answer."));
    }
  };

  const open = (name) => {
    current = name;

    for (const tab of tabs.children) {
      tab.classList.toggle("open", tab.dataset.name === name);
    }

    show(`${record}/panels/${encodeURIComponent(name)}.html`);
  };

  // A link inside a panel to the panel's own address reloads the panel,
  // for sorting and paging; any other link goes where it says.

  content.addEventListener("click", (event) => {
    const link = event.target.closest && event.target.closest("a[href]");

    if (!link) {
      return;
    }

    const address = new URL(link.getAttribute("href"), location.href);

    if (address.origin === location.origin && address.pathname.startsWith(panelsPath)) {
      event.preventDefault();
      show(address.href);
    }
  });

  badge.addEventListener("click", () => {
    bar.classList.toggle("collapsed");

    if (!bar.classList.contains("collapsed") && !current && tabs.firstElementChild) {
      open(tabs.firstElementChild.dataset.name);
    }
  });

  // The record, polled while it is still being written.

  const load = async () => {
    let found;

    try {
      const response = await fetch(`${record}.json`, { credentials: "same-origin", cache: "no-store" });

      if (!response.ok) {
        badge.replaceChildren(element("span", "name", "muru"), element("span", "muted", "record no longer kept"));
        return;
      }

      found = await response.json();
    } catch {
      return;
    }

    summary(found);
    tabbed(found);

    if (found.in_progress && polls < 120) {
      polls += 1;
      setTimeout(async () => {
        await load();

        if (current) {
          open(current);
        }
      }, 1000);
    }
  };

  load();
})();
