"use strict";
(() => {
  const $ = (id) => document.getElementById(id);
  const login = $("login");
  const workspace = $("workspace");
  const list = $("message-list");
  let token = null;
  let boards = null;
  function view(name) {
    $("messages-view").hidden = name !== "messages";
    $("boards-view").hidden = name !== "boards";
    $("messages-tab").setAttribute("aria-pressed", String(name === "messages"));
    $("boards-tab").setAttribute("aria-pressed", String(name === "boards"));
  }
  let generation = 0;
  let sessionAbort = null;
  let streamAbort = null;
  let reconnectTimer = null;
  let refreshTimer = null;
  let reconnectDelay = 1000;
  let rows = [];
  let nextCursor = null;
  let selectedId = null;
  let query = new URLSearchParams();
  let listRequest = 0;
  let detailRequest = 0;
  const states = new Set(["queued", "sending", "submitted", "delivered", "acknowledged", "failed", "uncertain", "cancelled", "blocked"]);

  function element(tag, className, text) {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined && text !== null) node.textContent = String(text);
    return node;
  }
  function showError(id, text) {
    $(id).textContent = text || "";
    $(id).hidden = !text;
  }
  function connection(label, state = "") {
    $("connection-label").textContent = label;
    $("connection-dot").className = `dot ${state}`;
  }
  function lock(message = "") {
    generation += 1;
    token = null;
    boards?.clear();
    view("messages");
    sessionAbort?.abort();
    streamAbort?.abort();
    clearTimeout(reconnectTimer);
    clearTimeout(refreshTimer);
    reconnectTimer = null;
    refreshTimer = null;
    rows = [];
    nextCursor = null;
    selectedId = null;
    list.replaceChildren();
    $("detail-content").replaceChildren();
    $("detail-content").hidden = true;
    $("detail-placeholder").hidden = false;
    $("close-detail").hidden = true;
    $("access-token").value = "";
    $("login-form").querySelector("button[type=submit]").disabled = false;
    workspace.hidden = true;
    login.hidden = false;
    $("lock-button").hidden = true;
    connection("Locked");
    showError("login-error", message);
  }
  class ApiError extends Error {
    constructor(status, data = null) {
      super(status === 404 ? "This message is no longer available." : status === 400 ? "The filter values could not be accepted. Check the fields and try again." : "The local bridge could not complete this request. Try refreshing.");
      this.status = status;
      this.data = data;
    }
  }
  async function api(path, current = generation, options = {}) {
    const response = await fetch(path, {
      method: options.method || "GET",
      body: options.body,
      headers: { Authorization: `Bearer ${token}`, Accept: "application/json", ...(options.body !== undefined ? { "Content-Type": "application/json" } : {}) },
      credentials: "omit",
      cache: "no-store",
      referrerPolicy: "no-referrer",
      signal: options.signal ? AbortSignal.any([sessionAbort.signal, options.signal]) : sessionAbort.signal,
    });
    if (current !== generation) throw new DOMException("Session changed", "AbortError");
    if (response.status === 401 || response.status === 403) {
      lock("Access was refused. Use the current token from the local monitor command.");
      throw new DOMException("Monitor locked", "AbortError");
    }
    const data = await response.json();
    if (current !== generation) throw new DOMException("Session changed", "AbortError");
    if (!response.ok) throw new ApiError(response.status, data);
    return data;
  }
  function endpointName(endpoint) {
    if (!endpoint) return "Unknown agent";
    const machine = endpoint.machine_name || (endpoint.peer_id ? String(endpoint.peer_id).slice(0, 12) + "…" : "Local");
    return `${machine} / ${endpoint.agent || "Unknown agent"}`;
  }
  function timeText(value) {
    const date = new Date(Number(value));
    if (!Number.isFinite(date.getTime())) return "Unknown time";
    const today = new Date().toDateString() === date.toDateString();
    return new Intl.DateTimeFormat(undefined, today ? { hour: "2-digit", minute: "2-digit", second: "2-digit" } : { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" }).format(date);
  }
  function timestamp(value) {
    const date = new Date(Number(value));
    return Number.isFinite(date.getTime()) ? date.toLocaleString() : "Unknown";
  }
  function badge(state) {
    return element("span", `state-badge${states.has(state) ? ` ${state}` : ""}`, state || "unknown");
  }
  function renderRows() {
    list.replaceChildren();
    $("message-count").textContent = rows.length;
    $("empty-state").hidden = rows.length !== 0;
    const filtered = query.size > 0;
    $("empty-state").querySelector("h3").textContent = filtered ? "No matching messages" : "No messages yet";
    $("empty-state").querySelector("p").textContent = filtered ? "Try a broader time range or reset the filters to see all message activity." : "Messages will appear here as agents communicate. The monitor does not start a conversation.";
    $("older-button").hidden = !nextCursor || rows.length >= 500;
    $("page-label").textContent = rows.length >= 500 ? "Showing 500 messages. Narrow the filters for older history." : "Newest first · live updates refresh this page";
    for (const row of rows) {
      const item = element("li");
      const button = element("button", `message-row${row.id === selectedId ? " selected" : ""}`);
      button.type = "button";
      button.dataset.id = row.id;
      button.setAttribute("aria-pressed", row.id === selectedId ? "true" : "false");
      const top = element("div", "row-top");
      const route = element("span", "route");
      route.append(element("span", "endpoint-name", endpointName(row.source)), element("span", "arrow", "→"), element("span", "endpoint-name", endpointName(row.destination)));
      const time = element("time", "time", timeText(row.created));
      const date = new Date(Number(row.created));
      if (Number.isFinite(date.getTime())) time.dateTime = date.toISOString();
      time.title = timestamp(row.created);
      top.append(route, time);
      const preview = element("p", "preview", row.body || "(Empty message)");
      const meta = element("div", "row-meta");
      meta.append(badge(row.state), element("span", "direction", row.direction || "local"));
      if (row.parent) meta.append(element("span", "reply-hint", "↳ Reply"));
      if (row.body_truncated) meta.append(element("span", "reply-hint", "Open for full message"));
      button.append(top, preview, meta);
      button.addEventListener("click", () => openDetail(row.id, true));
      item.append(button);
      list.append(item);
    }
  }
  async function loadMessages(older = false) {
    if (!token) return;
    const current = generation;
    const request = ++listRequest;
    const params = new URLSearchParams(query);
    params.set("limit", "50");
    if (older && nextCursor) params.set("before", nextCursor);
    $("refresh-button").disabled = true;
    $("older-button").disabled = true;
    try {
      const data = await api(`/api/messages?${params}`, current);
      if (current !== generation || request !== listRequest) return;
      const messages = Array.isArray(data.messages) ? data.messages : [];
      if (older) {
        const seen = new Set(rows.map((row) => row.id));
        rows = rows.concat(messages.filter((row) => !seen.has(row.id))).slice(0, 500);
      } else {
        rows = messages.slice(0, 500);
      }
      nextCursor = typeof data.next_cursor === "string" ? data.next_cursor : null;
      renderRows();
      showError("activity-error", "");
      $("last-updated").textContent = `Updated ${timeText(Date.now())}`;
    } catch (error) {
      if (error.name !== "AbortError" && current === generation) showError("activity-error", error instanceof ApiError ? error.message : "The local bridge is unavailable. Waiting to reconnect; you can also refresh.");
    } finally {
      if (current === generation && request === listRequest) {
        $("refresh-button").disabled = false;
        $("older-button").disabled = false;
      }
    }
  }
  function infoGrid(entries) {
    const grid = element("dl", "detail-grid");
    for (const [label, value, mono] of entries) {
      grid.append(element("dt", "", label), element("dd", mono ? "mono" : "", value ?? "Unknown"));
    }
    return grid;
  }
  function detailSection(title) {
    const section = element("section", "detail-section");
    section.append(element("h3", "", title));
    return section;
  }
  function endpointSection(title, endpoint = {}) {
    const section = detailSection(title);
    section.append(infoGrid([
      ["Machine", endpoint.machine_name || (endpoint.peer_id ? "Unknown remote machine" : "Local bridge")],
      ["Agent", endpoint.agent],
      ["Provider", endpoint.provider],
      ["Session", endpoint.session, true],
      ["Peer ID", endpoint.peer_id || "Local", true],
    ]));
    return section;
  }
  function relationButton(label, id) {
    const button = element("button", "relation", label);
    button.type = "button";
    button.addEventListener("click", () => openDetail(id, true));
    return button;
  }
  function renderDetail(data) {
    const row = data.message || data;
    const container = $("detail-content");
    container.replaceChildren();
    const heading = element("div", "detail-header");
    heading.append(badge(row.state), element("h3", "mono", row.id));
    container.append(heading, endpointSection("Source", row.source), endpointSection("Destination", row.destination));
    const timing = detailSection("Delivery");
    timing.append(infoGrid([["Created", timestamp(row.created)], ["Updated", timestamp(row.updated)], ["Direction", row.direction], ["Detail", row.detail || "No additional delivery detail"]]));
    container.append(timing);
    const body = detailSection("Message");
    body.append(element("pre", "body-text", row.body || "(Empty message)"));
    container.append(body);
    const related = Array.isArray(data.related) ? data.related : [];
    if (row.parent || related.length) {
      const chain = detailSection("Conversation");
      if (row.parent) {
        if (row.parent.local_id) chain.append(relationButton(`↰ Parent · ${row.parent.id}`, row.parent.local_id));
        else chain.append(element("p", "help mono", `Parent ${row.parent.id} · ${row.parent.peer_id || "local"} · not present in this history`));
      }
      for (const entry of related) {
        if (entry.id === row.id || entry.id === row.parent?.local_id) continue;
        chain.append(relationButton(`${entry.parent?.local_id === row.id ? "↳ Reply" : "Related"} · ${endpointName(entry.source)} · ${entry.id}`, entry.id));
      }
      container.append(chain);
    }
  }
  async function openDetail(id, focus = false) {
    if (!token || !id) return;
    const current = generation;
    const request = ++detailRequest;
    selectedId = id;
    renderRows();
    $("detail-placeholder").hidden = true;
    $("detail-content").hidden = false;
    $("close-detail").hidden = false;
    $("detail-content").replaceChildren(element("p", "notice help", "Loading message…"));
    try {
      const data = await api(`/api/messages/${encodeURIComponent(id)}`, current);
      if (current !== generation || request !== detailRequest) return;
      renderDetail(data);
      if (focus) {
        const panel = $("detail-panel");
        panel.setAttribute("tabindex", "-1");
        panel.focus({ preventScroll: true });
        if (window.matchMedia("(max-width: 780px)").matches) panel.scrollIntoView({ block: "start", behavior: "auto" });
      }
    } catch (error) {
      if (error.name !== "AbortError" && current === generation && request === detailRequest) $("detail-content").replaceChildren(element("p", "notice error", error instanceof ApiError ? error.message : "The message could not be loaded. Try again when the bridge reconnects."));
    }
  }
  function scheduleRefresh() {
    clearTimeout(refreshTimer);
    refreshTimer = setTimeout(() => {
      loadMessages();
      if (selectedId) openDetail(selectedId);
    }, 180);
  }
  function handleEvent(frame) {
    // Server events contain revision metadata only; content is fetched through
    // the same authenticated read-only API and current filters.
    if (frame.split("\n").some((line) => line.startsWith("data:"))) scheduleRefresh();
  }
  async function stream(current) {
    if (!token || current !== generation) return;
    streamAbort?.abort();
    streamAbort = new AbortController();
    try {
      const response = await fetch("/api/events", {
        headers: { Authorization: `Bearer ${token}`, Accept: "text/event-stream" },
        credentials: "omit", cache: "no-store", referrerPolicy: "no-referrer", signal: streamAbort.signal,
      });
      if (current !== generation) return;
      if (response.status === 401 || response.status === 403) {
        lock("The monitor session ended. Use the current token to unlock it again.");
        return;
      }
      if (!response.ok || !response.body) throw new Error("Stream unavailable");
      reconnectDelay = 1000;
      connection("Connected", "live");
      const reader = response.body.getReader();
      const decoder = new TextDecoder();
      let buffer = "";
      for (;;) {
        const { value, done } = await reader.read();
        if (current !== generation) { await reader.cancel(); return; }
        if (done) throw new Error("Stream ended");
        buffer += decoder.decode(value, { stream: true });
        buffer = buffer.replace(/\r\n/g, "\n");
        if (buffer.length > 65536) throw new Error("Invalid event stream");
        let end;
        while ((end = buffer.indexOf("\n\n")) !== -1) {
          handleEvent(buffer.slice(0, end));
          buffer = buffer.slice(end + 2);
        }
      }
    } catch (error) {
      if (error.name === "AbortError" || current !== generation || !token) return;
      streamAbort.abort();
      connection("Reconnecting", "offline");
      reconnectTimer = setTimeout(() => { loadMessages(); stream(current); }, reconnectDelay);
      reconnectDelay = Math.min(reconnectDelay * 2, 30000);
    }
  }
  $("login-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    const entered = $("access-token").value.trim();
    if (!entered) return;
    lock();
    token = entered;
    sessionAbort = new AbortController();
    const current = generation;
    const submit = $("login-form").querySelector("button[type=submit]");
    submit.disabled = true;
    connection("Authenticating");
    try {
      const status = await api("/api/status", current);
      if (current !== generation) return;
      $("local-machine").textContent = status.machine?.machine_name || "Local bridge";
      login.hidden = true;
      workspace.hidden = false;
      $("lock-button").hidden = false;
      showError("activity-error", "");
      await loadMessages();
      if (current === generation && token) stream(current);
    } catch (error) {
      if (error.name !== "AbortError" && current === generation) lock("The local bridge could not be reached. Check that the monitor is enabled, then try again.");
    } finally {
      if (current === generation) submit.disabled = false;
    }
  });
  boards = window.createBoardMonitor({
    request: (path, options) => api(path, generation, options),
    authenticated: () => !!token,
    onViewChange: view,
  });
  $("messages-tab").addEventListener("click", () => { void boards.leave(() => view("messages")); });
  $("boards-tab").addEventListener("click", () => { void boards.open(); });
  $("lock-button").addEventListener("click", () => { void boards.leave(() => { lock(); $("access-token").focus(); }); });
  $("filters").addEventListener("submit", (event) => {
    event.preventDefault();
    const next = new URLSearchParams();
    for (const key of ["machine", "agent", "state"]) {
      const value = $(`${key}-filter`).value.trim();
      if (value) next.set(key, value);
    }
    const since = $("since-filter").value ? new Date($("since-filter").value).getTime() : null;
    const until = $("until-filter").value ? new Date($("until-filter").value).getTime() : null;
    if ((since !== null && !Number.isFinite(since)) || (until !== null && !Number.isFinite(until)) || (since !== null && until !== null && since > until)) {
      showError("activity-error", "Choose a valid time range with From earlier than Until.");
      return;
    }
    if (since !== null) next.set("since", String(since));
    if (until !== null) next.set("until", String(until));
    query = next;
    nextCursor = null;
    loadMessages();
  });
  $("reset-filters").addEventListener("click", () => { $("filters").reset(); query = new URLSearchParams(); nextCursor = null; loadMessages(); });
  $("refresh-button").addEventListener("click", scheduleRefresh);
  $("older-button").addEventListener("click", () => loadMessages(true));
  $("close-detail").addEventListener("click", () => {
    selectedId = null;
    detailRequest += 1;
    $("detail-content").replaceChildren();
    $("detail-content").hidden = true;
    $("detail-placeholder").hidden = false;
    $("close-detail").hidden = true;
    renderRows();
  });
  window.addEventListener("pagehide", () => lock());
})();
