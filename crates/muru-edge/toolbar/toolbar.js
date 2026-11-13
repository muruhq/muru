// The development toolbar (design specification section 4.20): a badge
// on the page for the request that produced it, opening to the panels of
// its record, and a "This page" tab with every request the page made. It
// reads and changes nothing, and runs no panel's script: the panels' HTML
// comes from the edge, already sanitised, and what the script draws
// itself goes on the page only as text.

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
  const prefixPath = new URL(base, location.href).pathname;
  const recordOf = (requestId) => `${base}requests/${encodeURIComponent(requestId)}`;

  // The page view's tab, beside the panels' own names; the most IDs one
  // request for summaries names, as the edge allows; and the size of the
  // browser's buffer of entries unless the page changed it.

  const PAGE = "page";
  const BATCH = 100;
  const BUFFER = 250;

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

  const button = (className, text, action) => {
    const made = element("button", className, text);
    made.type = "button";
    made.addEventListener("click", action);
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

  const millis = (ms) => (ms >= 1000 ? `${(ms / 1000).toFixed(2)}s` : `${ms.toFixed(1)}ms`);
  const failed = (found) => (found.from_worker && found.from_worker.outcome && found.from_worker.outcome !== "completed") || found.status >= 500;
  const workerName = (group, worker) => `${group && group !== "default" ? `${group}/` : ""}${worker}`;
  const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  const expanded = () => !bar.classList.contains("collapsed");

  // The requests the page made, by the URL and start that make each
  // one, and Muru's summary of each request ID: a summary, or null when
  // the record is no longer kept.

  const rows = new Map();
  const summaries = new Map();
  let bufferFull = false;

  // A record as JSON: the record, null when it is no longer kept, or
  // undefined when the run did not answer.

  const fetchRecord = async (requestId) => {
    try {
      const response = await fetch(`${recordOf(requestId)}.json`, { credentials: "same-origin", cache: "no-store" });
      return response.ok ? await response.json() : null;
    } catch {
      return undefined;
    }
  };

  // The badge: status, total time, queue, worker, and the counts of log
  // records and errors of the page's own request.

  const summary = (found) => {
    badge.replaceChildren(element("span", "name", "muru"));

    if (!found) {
      badge.append(element("span", "muted", "record no longer kept"));
      return;
    }

    const worker = found.from_worker;
    const logs = worker ? worker.logs.length + worker.logs_dropped : 0;
    const error = failed(found);
    badge.append(element("span", found.status >= 400 ? "status error" : "status", found.in_progress && !found.status ? "…" : String(found.status)));

    if (found.status) {
      badge.append(element("span", "figure", millis(found.total_ms)));
      badge.append(element("span", "figure muted", `queue ${millis(found.queue_ms)}`));
    }

    if (found.worker !== null && found.worker !== undefined) {
      badge.append(element("span", "figure muted", `worker ${workerName(found.group, found.worker)}`));
    }

    badge.append(element("span", logs ? "count warning" : "count muted", `${logs} log`));
    badge.append(element("span", error ? "count error" : "count muted", `${error ? 1 : 0} error`));
  };

  // The record the tabs show: the page's own request at first, or one
  // picked from the page view. Its panels are the tabs after "This
  // page". Each record is followed by one loop at a time, fetched again
  // each second for up to two minutes while it is being written.

  const own = { id, label: "", link: dashboard };
  const targets = new Map([[id, own]]);
  let shown = own;
  let current = null;
  let showing = 0;

  const panelsOf = (target) => (target.found ? target.found.panels : []);

  const tabbed = () => {
    const page = button("tab", `This page (${rows.size})`, () => choose(PAGE));
    page.dataset.name = PAGE;
    tabs.replaceChildren(page);

    if (shown === own) {
      tabs.append(element("span", "shown muted", "this page's request:"));
    } else {
      const label = element("span", "shown");
      label.append(element("span", "mono", shown.label), button("back", "this page's request", () => view(own, current)));
      tabs.append(label);
    }

    for (const panel of panelsOf(shown)) {
      const tab = button("tab", panel.title, () => choose(panel.name));
      tab.dataset.name = panel.name;
      tabs.append(tab);
    }

    for (const tab of tabs.querySelectorAll(".tab")) {
      tab.classList.toggle("open", tab.dataset.name === current);
    }

    footer.replaceChildren(element("span", "muted", `request ${shown.id}`));

    if (shown.link) {
      const link = element("a", "", "open in dashboard");
      link.href = shown.link;
      link.target = "_blank";
      link.rel = "noopener";
      footer.append(link);
    }
  };

  const show = async (address) => {
    const asked = ++showing;

    try {
      const response = await fetch(address, { credentials: "same-origin", cache: "no-store" });
      const html = response.ok ? await response.text() : "";

      if (asked !== showing) {
        return;
      }

      content.innerHTML = html;

      if (!response.ok) {
        content.append(element("p", "muted", "This panel is no longer kept."));
      }
    } catch {
      if (asked === showing) {
        content.replaceChildren(element("p", "muted", "The run did not answer."));
      }
    }
  };

  const choose = (name) => {
    if (!name) {
      return;
    }

    current = name;

    for (const tab of tabs.querySelectorAll(".tab")) {
      tab.classList.toggle("open", tab.dataset.name === name);
    }

    if (name === PAGE) {
      showing += 1;
      drawPage();
      refresh();
    } else {
      show(`${recordOf(shown.id)}/panels/${encodeURIComponent(name)}.html`);
    }
  };

  // A record fetched: the badge for the page's own, and the tabs when it
  // is the one shown, opening the tab asked for once it is in, or else
  // reloading the open panel.

  const refreshed = (target, again) => {
    if (target === own) {
      summary(own.found);
    }

    if (target !== shown) {
      return;
    }

    tabbed();

    if (target.found === null && current !== PAGE) {
      showing += 1;
      content.replaceChildren(element("p", "muted", "This request's record is no longer kept."));
      return;
    }

    if (target.opening !== undefined) {
      const name = target.opening;
      target.opening = undefined;
      const panels = panelsOf(target);

      if (expanded()) {
        choose(name === PAGE || panels.some((panel) => panel.name === name) ? name : panels[0] && panels[0].name);
      }
    } else if (again && current && current !== PAGE) {
      choose(current);
    } else if (!current && expanded()) {
      const panels = panelsOf(target);
      choose(panels[0] && panels[0].name);
    }
  };

  const follow = async (target) => {
    if (target.following) {
      return;
    }

    target.following = true;

    for (let polls = 0; polls <= 120; polls += 1) {
      const found = await fetchRecord(target.id);

      if (found === undefined) {
        break;
      }

      target.found = found;
      refreshed(target, polls > 0);

      if (!found || !found.in_progress) {
        break;
      }

      await sleep(1000);
    }

    target.following = false;
  };

  const view = (target, name) => {
    shown = target;
    target.opening = name;

    if (target.found !== undefined) {
      refreshed(target, false);
    }

    if (target.found === undefined || (target.found && target.found.in_progress)) {
      follow(target);
    }
  };

  // A link inside a panel to a record's panel reloads the panel in
  // place, for sorting and paging; any other link goes where it says.

  content.addEventListener("click", (event) => {
    const link = event.target.closest && event.target.closest("a[href]");

    if (!link) {
      return;
    }

    const address = new URL(link.getAttribute("href"), location.href);

    if (address.origin === location.origin && address.pathname.startsWith(`${prefixPath}requests/`) && address.pathname.includes("/panels/")) {
      event.preventDefault();
      show(address.href);
    }
  });

  badge.addEventListener("click", () => {
    bar.classList.toggle("collapsed");

    if (expanded() && !current) {
      const panels = panelsOf(shown);
      choose(panels[0] && panels[0].name);
    }
  });

  // The page view: every request the page made, from the browser's
  // Resource Timing entries. A cached response is not looked up, since
  // its Server-Timing names the request it was first cached from.

  const muruId = (entry) => {
    const found = (entry.serverTiming || []).find((timing) => timing.name === "muru");
    return found && found.description ? found.description : null;
  };

  const add = (entry) => {
    const address = new URL(entry.name, location.href);
    const sameOrigin = address.origin === location.origin;

    if (sameOrigin && address.pathname.startsWith(prefixPath)) {
      return;
    }

    const navigation = entry.entryType === "navigation";
    const cached = !navigation && (entry.deliveryType === "cache" || (sameOrigin && entry.transferSize === 0 && entry.decodedBodySize > 0));
    const key = navigation ? "navigation" : `${entry.name} ${entry.startTime}`;

    rows.set(key, {
      entry,
      address,
      sameOrigin,
      cached,
      requestId: cached ? null : muruId(entry) || (navigation ? id : null),
    });
  };

  const counted = () => {
    const page = tabs.querySelector(".tab");

    if (page) {
      page.textContent = `This page (${rows.size})`;
    }
  };

  // Muru's figures for the rows, fetched in batches while the tab is
  // open: those not yet asked for, and again each second those still in
  // progress.

  let fetching = false;
  let timer = null;

  const refresh = async () => {
    if (fetching || current !== PAGE) {
      return;
    }

    const wanted = new Set();

    for (const row of rows.values()) {
      const known = summaries.get(row.requestId);

      if (row.requestId && (known === undefined || (known && known.in_progress))) {
        wanted.add(row.requestId);
      }
    }

    if (!wanted.size) {
      return;
    }

    fetching = true;
    const asked = [...wanted];
    let answered = true;

    try {
      for (let at = 0; at < asked.length; at += BATCH) {
        const ids = asked.slice(at, at + BATCH).map(encodeURIComponent).join(",");
        const response = await fetch(`${base}requests?ids=${ids}`, { credentials: "same-origin", cache: "no-store" });

        if (!response.ok) {
          answered = false;
          break;
        }

        const found = await response.json();

        for (const request of found.requests) {
          summaries.set(request.request_id, request);
        }

        for (const gone of found.not_kept) {
          summaries.set(gone, null);
        }
      }
    } catch {
      answered = false;
    } finally {
      fetching = false;
    }

    drawPage();

    // Rows added meanwhile are asked for at once, and those in progress
    // after a second; a run that did not answer is asked again only
    // when the page makes another request.

    if (!answered) {
      return;
    }

    const unasked = [...rows.values()].some((row) => row.requestId && !summaries.has(row.requestId));
    const unfinished = [...summaries.values()].some((known) => known && known.in_progress);

    if (unasked) {
      refresh();
    } else if (unfinished && !timer) {
      timer = setTimeout(() => {
        timer = null;
        refresh();
      }, 1000);
    }
  };

  // A bar on the page's clock, in whole percent of the track, as the
  // panels place theirs.

  const placed = (className, start, duration, total, title) => {
    const from = Math.min(Math.max(Math.round((start * 100) / total), 0), 100);
    const width = Math.min(Math.max(Math.round((duration * 100) / total), 0), 100 - from);
    const made = element("span", `bar ${className} from-${from} for-${width}`);
    made.title = title;
    return made;
  };

  const cell = (text, className) => element("td", className, text);

  const COLUMNS = [["Request"], ["Status", "num"], ["Takes", "num"], ["On the page's clock"], ["Queue", "num"], ["Worker"], ["Logs", "num"], ["Errors", "num"], ["Connection"], ["Protocol"]];

  const LIMITS = [
    "Every request the browser reports for this page, except the toolbar's own.",
    "A request is listed when its response ends, so a streamed response or a long-running fetch appears only once it has finished.",
    "WebSockets are not listed.",
    `The browser holds ${BUFFER} of a page's requests by default until the toolbar starts, and drops the rest.`,
    "Redirects that led to the page are not shown.",
    "A response from the browser's cache is marked cache, since no request reached Muru.",
    "A request to another origin has the browser's figures only.",
  ];

  const drawRow = (row, total, labels) => {
    const entry = row.entry;
    const known = row.requestId ? summaries.get(row.requestId) : undefined;
    const ends = entry.responseEnd || performance.now();
    const takes = ends - entry.startTime;
    const line = element("tr", known ? "" : "plain");

    // What made the request, and where it went.

    const what = element("td", "what");
    what.title = entry.name;
    const path = row.sameOrigin ? `${row.address.pathname}${row.address.search}` : entry.name;
    what.append(element("span", "mono", path), element("span", "muted", ` ${entry.initiatorType || entry.entryType}`));
    line.append(what);

    const status = known ? (known.status ? String(known.status) : "…") : entry.responseStatus ? String(entry.responseStatus) : "";
    line.append(cell(status, known && known.status >= 400 ? "num error" : "num"));
    line.append(cell(entry.responseEnd ? millis(takes) : "…", "num"));

    // The browser's time, with the queue drawn where the request reached
    // Muru.

    const track = element("span", "track");
    track.append(placed("", entry.startTime, takes, total, `${millis(entry.startTime)} to ${millis(ends)}`));

    if (known && known.queue_ms > 0) {
      track.append(placed("queue", entry.requestStart || entry.startTime, known.queue_ms, total, `waited ${millis(known.queue_ms)} for a worker`));
    }

    const clock = element("td");
    clock.append(track);
    line.append(clock);

    // Muru's figures, or why there are none.

    if (known) {
      const worker = known.served_by_edge ? "edge" : known.worker !== null && known.worker !== undefined ? workerName(known.group, known.worker) : "-";
      line.append(cell(millis(known.queue_ms), "num"));
      line.append(cell(worker, "mono"));
      line.append(cell(String(known.logs), known.logs ? "num warning" : "num muted"));
      line.append(cell(String(known.errors), known.errors ? "num error" : "num muted"));
    } else {
      const why = row.cached ? "cache" : !row.sameOrigin ? "another origin" : !row.requestId ? "no record" : known === null ? "no longer kept" : "…";
      const reason = cell(why, "muted");
      reason.colSpan = 4;
      line.append(reason);
    }

    // The connection: Muru's label and position, and whether the browser
    // opened it for this request, when it gives the page the timings.

    const opened = !row.cached && entry.requestStart > 0 ? (entry.connectEnd > entry.connectStart ? "new" : "reused") : "";
    const label = known && known.connection ? `${labels.get(known.connection)} #${known.connection_position}` : "";
    const connection = cell([label, opened].filter(Boolean).join(" "), "mono");

    if (label) {
      connection.title = `connection ${known.connection}, request ${known.connection_position} on it`;
    }

    line.append(connection, cell(entry.nextHopProtocol || "", "mono"));

    // A row with a record opens it.

    if (known) {
      line.classList.add("pick");
      line.classList.toggle("shown", shown.id === known.request_id);

      line.addEventListener("click", () => {
        if (!targets.has(known.request_id)) {
          targets.set(known.request_id, { id: known.request_id, label: `${known.method} ${known.target}`, link: known.link });
        }

        view(targets.get(known.request_id), "timing");
      });
    }

    return line;
  };

  const drawPage = () => {
    const navigation = performance.getEntriesByType("navigation")[0];

    if (navigation) {
      add(navigation);
    }

    counted();

    if (current !== PAGE) {
      return;
    }

    const ordered = [...rows.values()].sort((one, other) => one.entry.startTime - other.entry.startTime);
    const total = Math.max(1, ...ordered.map((row) => row.entry.responseEnd || performance.now()));

    // Connections are labelled in the order the page's requests first
    // used them.

    const labels = new Map();

    for (const row of ordered) {
      const known = row.requestId && summaries.get(row.requestId);

      if (known && known.connection && !labels.has(known.connection)) {
        labels.set(known.connection, `c${labels.size + 1}`);
      }
    }

    const table = element("table", "page");
    const head = table.createTHead().insertRow();

    for (const [name, className] of COLUMNS) {
      head.append(element("th", className, name));
    }

    const rowsBody = table.createTBody();

    for (const row of ordered) {
      rowsBody.append(drawRow(row, total, labels));
    }

    content.replaceChildren(table);

    if (bufferFull) {
      content.append(element("p", "warning", "The browser's list of this page's requests was full before the toolbar started, so some are missing."));
    }

    if (!window.isSecureContext) {
      content.append(element("p", "warning", "This page is not a secure context, so the browser hides Server-Timing from it and only the page's own request has its record. Open it as localhost, a loopback address or over HTTPS."));
    }

    const limits = element("details");
    const list = element("ul");

    for (const text of LIMITS) {
      list.append(element("li", "", text));
    }

    limits.append(element("summary", "", "What is listed"), list);
    content.append(limits);
  };

  // Entries from before the script ran come from the browser's buffer,
  // and later ones as their responses end.

  if (typeof PerformanceObserver === "function") {
    bufferFull = performance.getEntriesByType("resource").length >= BUFFER;

    try {
      new PerformanceObserver((list) => {
        for (const entry of list.getEntries()) {
          add(entry);
        }

        drawPage();
        refresh();
      }).observe({ type: "resource", buffered: true });
    } catch {
      // A browser without Resource Timing lists only the page itself.
    }
  }

  view(own);
})();
