"use strict";
// Agent Bridge web interface shell: access, routing, sidebar, agent conversations, traffic,
// the details panel, the quick switcher and the theme picker. Boards and topics have their own
// modules (boards.js, topics.js). Everything is built with createElement/textContent: agent
// text is untrusted, and the page's Content-Security-Policy forbids inline scripts and styles.
(() => {
  const { el, colorClass, initials, hhmm, dayKey, dayLabel, fullTime, svg, agentNames, setAgentNames, agentName } = window.AB;

  // Consume the private link before making requests or displaying the workspace.
  // Fragments never travel in HTTP requests; replace this history entry as well.
  function takeAccessLink() {
    if (!window.location.hash) return null;
    const fragment = new URLSearchParams(window.location.hash.slice(1));
    const candidate = fragment.get("access_token");
    if (candidate === null) return null; // an ordinary route such as #/topic/x
    window.history.replaceState(null, "", window.location.pathname + window.location.search);
    return /^[A-Za-z0-9_-]{43}$/.test(candidate) ? candidate : null;
  }
  let accessFromLink = takeAccessLink();
  const $ = id => document.getElementById(id);
  const login = $("login"), workspace = $("workspace"), body = $("body");
  let token = null, boards = null, topics = null;
  let generation = 0, sessionAbort = null, streamAbort = null, reconnectTimer = null, refreshTimer = null, reconnectDelay = 1000;
  let route = null, navigating = false, queuedRoute = null;
  const catalog = { boardNames: [], currentBoard: "main", topics: [], boardPoll: null };

  // ---------- small UI helpers ----------
  function toast(text, kind = "") {
    const node = el("div", `toast${kind ? ` toast--${kind}` : ""}`, text);
    $("toasts").append(node);
    setTimeout(() => node.remove(), 3500);
  }
  function showError(id, text) {
    if ($(id).textContent !== (text || "")) $(id).textContent = text || "";
    $(id).hidden = !text;
  }
  function connection(label, state = "") {
    $("connection-label").textContent = label;
    $("connection-dot").className = `dot ${state}`;
  }
  // Popover menus: one open at a time, closed by outside click or Escape.
  const menus = [];
  function menu(buttonId, menuId) {
    const button = $(buttonId), popup = $(menuId);
    const close = () => { popup.hidden = true; button.setAttribute("aria-expanded", "false"); };
    button.addEventListener("click", event => {
      event.stopPropagation();
      const open = popup.hidden;
      for (const other of menus) other.close();
      if (open) { popup.hidden = false; button.setAttribute("aria-expanded", "true"); popup.querySelector("button:not(:disabled)")?.focus(); }
    });
    popup.addEventListener("click", () => close());
    popup.addEventListener("keydown", event => { if (event.key === "Escape") { close(); button.focus(); } });
    menus.push({ close });
  }
  document.addEventListener("click", () => { for (const m of menus) m.close(); });

  // ---------- details panel (message delivery, topic threads) ----------
  let panelOwner = null, panelReturnFocus = null;
  function openPanel(title, content, owner = "thread") {
    const wasOpen = !$("panel").hidden;
    panelOwner = owner;
    $("panel-title").textContent = title;
    $("panel-body").replaceChildren(content);
    $("panel").hidden = false; body.classList.add("has-panel");
    // Keyboard and screen-reader users land in the panel, and return to where they were when it closes.
    if (!wasOpen) { panelReturnFocus = document.activeElement; $("panel").focus({ preventScroll: true }); }
  }
  function closePanel() {
    const wasOpen = !$("panel").hidden;
    panelOwner = null;
    $("panel").hidden = true; body.classList.remove("has-panel");
    $("panel-body").replaceChildren();
    if (wasOpen && panelReturnFocus?.isConnected) panelReturnFocus.focus({ preventScroll: true });
    panelReturnFocus = null;
    if (traffic.selectedId || dm.selectedId) { traffic.selectedId = dm.selectedId = null; detailSignature = null; renderRows(); renderDm(); }
  }
  $("panel-close").addEventListener("click", closePanel);

  // ---------- session, transport ----------
  function lock(message = "") {
    generation += 1;
    token = null;
    boards?.clear(); topics?.clear();
    sessionAbort?.abort(); streamAbort?.abort();
    clearTimeout(reconnectTimer); clearTimeout(refreshTimer); clearTimeout(catalog.boardPoll); clearTimeout(scanTimer);
    reconnectTimer = refreshTimer = catalog.boardPoll = scanTimer = null;
    route = null; navigating = false; queuedRoute = null; reconnectDelay = 1000; lastScan = 0;
    Object.assign(catalog, { boardNames: [], currentBoard: "main", topics: [] });
    traffic.reset(); dm.reset(); localPeer = null; clearInterval(agentTimer); agentTimer = null; setAgentNames([]);
    closePanel();
    document.body.dataset.view = "locked";
    $("sidebar").replaceChildren(); $("sidebar").hidden = true; body.classList.remove("has-sidebar");
    for (const id of ["sidebar-toggle", "open-switcher", "lock-button"]) $(id).hidden = true;
    for (const id of ["boards-view", "topics-view", "dm-view", "traffic-view"]) $(id).hidden = true;
    $("access-token").value = "";
    $("login-form").querySelector("button[type=submit]").disabled = false;
    workspace.hidden = true; login.hidden = false;
    connection("Locked");
    showError("login-error", message);
  }
  class ApiError extends Error {
    constructor(status, data = null) {
      super(status === 404 ? "This message is no longer available." : status === 400 ? "The filter values could not be accepted. Check the fields and try again." : "The local bridge could not complete this request. Try again.");
      this.status = status;
      this.data = data;
    }
  }
  async function transport(path, current = generation, options = {}) {
    const response = await fetch(path, {
      method: options.method || "GET",
      body: options.body,
      headers: { Authorization: `Bearer ${token}`, Accept: options.accept || "application/json", ...(options.body !== undefined ? { "Content-Type": "application/json" } : {}) },
      credentials: "omit", cache: "no-store", referrerPolicy: "no-referrer",
      signal: options.signal ? AbortSignal.any([sessionAbort.signal, options.signal]) : sessionAbort.signal,
    });
    if (current !== generation) throw new DOMException("Session changed", "AbortError");
    if (response.status === 401 || response.status === 403) {
      lock("Access was refused. Use the current token from `agent-bridge web access`.");
      throw new DOMException("Web interface locked", "AbortError");
    }
    return response;
  }
  async function api(path, current = generation, options = {}) {
    const response = await transport(path, current, options);
    const data = await response.json();
    if (current !== generation) throw new DOMException("Session changed", "AbortError");
    if (!response.ok) throw new ApiError(response.status, data);
    return data;
  }

  // ---------- agents and endpoints ----------
  // The name "user" is reserved for the human's inbox. Other inbox-provider endpoints are ordinary
  // agents (for example generic MCP clients), so only the name identifies the human.
  const isHuman = endpoint => !endpoint || endpoint.agent === "user";
  const endpointKey = endpoint => `${endpoint.peer_id || "local"}~${endpoint.agent}`;
  // The server fills in a peer id for every endpoint, local ones included, so "remote" means a peer
  // id that differs from this machine's own.
  let localPeer = null;
  const isRemote = endpoint => !!endpoint?.peer_id && !!localPeer && endpoint.peer_id !== localPeer;
  // Another machine's agent names are not known here, so remote agents keep the name they were sent as.
  const nameOf = endpoint => isRemote(endpoint) ? endpoint.agent : agentName(endpoint.agent);
  const endpointLabel = endpoint => endpoint?.agent ? (isRemote(endpoint) ? `${endpoint.machine_name || String(endpoint.peer_id).slice(0, 8)}/${nameOf(endpoint)}` : nameOf(endpoint)) : "unknown";
  function endpointName(endpoint) {
    if (!endpoint) return "Unknown agent";
    const machine = endpoint.machine_name || (endpoint.peer_id ? String(endpoint.peer_id).slice(0, 12) + "…" : "Local");
    return `${machine} / ${nameOf(endpoint)}`;
  }
  // An agent-to-agent conversation is a pair of agent identities, independent of direction.
  function pairOf(row) {
    const a = row.source, b = row.destination;
    if (!a || !b || isHuman(a) || isHuman(b) || !a.agent || !b.agent) return null;
    const ka = endpointKey(a), kb = endpointKey(b);
    if (ka === kb) return null;
    return ka < kb ? { key: `${ka}|${kb}`, a, b } : { key: `${kb}|${ka}`, a: b, b: a };
  }
  const states = new Set(["pending", "delivered", "failed", "unknown"]);
  function badge(state) {
    const outcome = states.has(state) ? state : "unknown";
    return el("span", `state-badge ${outcome}`, outcome);
  }
  const timeText = value => {
    const date = new Date(Number(value));
    if (!Number.isFinite(date.getTime())) return "Unknown time";
    const today = new Date().toDateString() === date.toDateString();
    return new Intl.DateTimeFormat(undefined, today ? { hour: "2-digit", minute: "2-digit", second: "2-digit" } : { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" }).format(date);
  };

  // ---------- message details (delivery inspector) ----------
  let detailSignature = null, detailRequest = 0;
  const detailBox = el("div", "detail");
  function patchNode(current, next) {
    const changedKey = current.nodeType === Node.ELEMENT_NODE && next.nodeType === Node.ELEMENT_NODE && current.dataset.messageId !== next.dataset.messageId;
    if (current.nodeType !== next.nodeType || current.nodeName !== next.nodeName || changedKey) { current.replaceWith(next); return; }
    if (current.nodeType === Node.TEXT_NODE) { if (current.nodeValue !== next.nodeValue) current.nodeValue = next.nodeValue; return; }
    if (current.nodeType === Node.ELEMENT_NODE) {
      for (const attr of [...current.attributes]) if (!next.hasAttribute(attr.name)) current.removeAttribute(attr.name);
      for (const attr of next.attributes) if (current.getAttribute(attr.name) !== attr.value) current.setAttribute(attr.name, attr.value);
    }
    const incoming = [...next.childNodes];
    for (let i = 0; i < incoming.length; i++) { if (current.childNodes[i]) patchNode(current.childNodes[i], incoming[i]); else current.append(incoming[i]); }
    while (current.childNodes.length > incoming.length) current.lastChild.remove();
  }
  function infoGrid(entries) {
    const grid = el("dl", "detail-grid");
    for (const [label, value, mono] of entries) grid.append(el("dt", "", label), el("dd", mono ? "mono" : "", value ?? "Unknown"));
    return grid;
  }
  function detailSection(title) { const section = el("section", "detail-section"); section.append(el("h3", "", title)); return section; }
  function endpointSection(title, endpoint = {}) {
    const section = detailSection(title);
    section.append(infoGrid([["Machine", endpoint.machine_name || (endpoint.peer_id ? "Unknown remote machine" : "Local bridge")], ["Agent", nameOf(endpoint)], ["Agent ID", endpoint.agent, true], ["Provider", endpoint.provider], ["Session", endpoint.session, true], ["Peer ID", endpoint.peer_id || "Local", true]]));
    return section;
  }
  function relationButton(label, id) {
    const button = el("button", "relation", label); button.type = "button"; button.dataset.messageId = id;
    button.addEventListener("click", () => openDetail(id));
    return button;
  }
  function renderDetail(data) {
    const signature = JSON.stringify(data);
    if (signature === detailSignature) return;
    const row = data.message || data;
    const container = el("div", "detail");
    const heading = el("div", "detail-header");
    heading.append(badge(row.state), el("h3", "mono", row.id));
    container.append(heading, endpointSection("Source", row.source), endpointSection("Destination", row.destination));
    const timing = detailSection("Delivery");
    timing.append(infoGrid([["Created", fullTime(row.created)], ["Updated", fullTime(row.updated)], ["Direction", row.direction], ["Detail", row.detail || "No additional delivery detail"]]));
    container.append(timing);
    const message = detailSection("Message");
    message.append(el("pre", "body-text", row.body || "(Empty message)"));
    container.append(message);
    const related = Array.isArray(data.related) ? data.related : [];
    if (row.parent || related.length) {
      const chain = detailSection("Conversation");
      if (row.parent) {
        if (row.parent.local_id) chain.append(relationButton(`↰ Parent · ${row.parent.id}`, row.parent.local_id));
        else chain.append(el("p", "help mono", `Parent ${row.parent.id} · ${row.parent.peer_id || "local"} · not present in this history`));
      }
      for (const entry of related) {
        if (entry.id === row.id || entry.id === row.parent?.local_id) continue;
        chain.append(relationButton(`${entry.parent?.local_id === row.id ? "↳ Reply" : "Related"} · ${endpointName(entry.source)} · ${entry.id}`, entry.id));
      }
      container.append(chain);
    }
    if (!detailBox.isConnected) openPanel("Delivery details", detailBox, "detail");
    patchNode(detailBox, container);
    detailSignature = signature;
  }
  async function openDetail(id) {
    if (!token || !id) return;
    const current = generation, request = ++detailRequest;
    const same = (traffic.selectedId === id || dm.selectedId === id) && detailSignature !== null;
    if (route?.kind === "dm") dm.selectedId = id; else traffic.selectedId = id;
    if (!same) { detailSignature = null; openPanel("Delivery details", detailBox, "detail"); detailBox.replaceChildren(el("p", "notice help", "Loading message…")); }
    renderRows(); renderDm();
    try {
      const data = await api(`/api/messages/${encodeURIComponent(id)}`, current);
      if (current !== generation || request !== detailRequest) return;
      renderDetail(data);
    } catch (error) {
      if (error.name !== "AbortError" && current === generation && request === detailRequest) {
        const message = error instanceof ApiError ? error.message : "The message could not be loaded. Try again when the bridge reconnects.";
        if (same) toast(message, "danger"); else detailBox.replaceChildren(el("p", "notice error", message));
      }
    }
  }

  // ---------- All traffic (the debug feed) ----------
  const traffic = {
    rows: [], nextCursor: null, query: new URLSearchParams(), selectedId: null, request: 0, dirty: true, nodes: new Map(),
    reset() { this.rows = []; this.nextCursor = null; this.query = new URLSearchParams(); this.selectedId = null; this.request = 0; this.dirty = true; this.nodes.clear(); $("message-list").replaceChildren(); $("filters").reset(); },
  };
  function renderRows() {
    const list = $("message-list");
    const present = new Set(traffic.rows.map(row => row.id));
    for (const [id, cached] of traffic.nodes) if (!present.has(id)) { cached.item.remove(); traffic.nodes.delete(id); }
    const filtered = traffic.query.size > 0;
    $("empty-state").hidden = traffic.rows.length !== 0;
    $("empty-state").querySelector("strong").textContent = filtered ? "No matching messages" : "No messages yet";
    $("empty-state").querySelector("span").textContent = filtered ? "Try a broader time range or reset the filters to see all message activity." : "Messages will appear here as agents communicate. The web interface does not start a conversation.";
    $("older-button").hidden = !traffic.nextCursor || traffic.rows.length >= 500;
    $("page-label").textContent = traffic.rows.length >= 500 ? "Showing 500 messages. Narrow the filters for older history." : `${traffic.rows.length} shown · newest first · live`;
    let position = list.firstElementChild;
    for (const row of traffic.rows) {
      const signature = JSON.stringify(row);
      const cached = traffic.nodes.get(row.id);
      let item = cached?.item;
      if (!cached || cached.signature !== signature) {
        const next = el("li");
        const button = el("button", `message-row${row.id === traffic.selectedId ? " selected" : ""}`);
        button.type = "button"; button.dataset.id = row.id; button.setAttribute("aria-pressed", row.id === traffic.selectedId ? "true" : "false");
        const top = el("div", "row-top");
        const routeBox = el("span", "route");
        routeBox.append(el("span", "endpoint-name", endpointName(row.source)), el("span", "arrow", "→"), el("span", "endpoint-name", endpointName(row.destination)));
        const time = el("time", "time", timeText(row.created));
        const date = new Date(Number(row.created));
        if (Number.isFinite(date.getTime())) time.dateTime = date.toISOString();
        time.title = fullTime(row.created);
        top.append(routeBox, time);
        const preview = el("p", "preview", row.body || "(Empty message)");
        const meta = el("div", "row-meta");
        meta.append(badge(row.state), el("span", "direction", row.direction || "local"));
        if (row.parent) meta.append(el("span", "reply-hint", "↳ Reply"));
        if (row.body_truncated) meta.append(el("span", "reply-hint", "Open for full message"));
        button.append(top, preview, meta);
        button.addEventListener("click", () => openDetail(row.id));
        next.append(button);
        if (item) patchNode(item, next); else item = next;
        traffic.nodes.set(row.id, { item, signature });
      }
      const button = item.querySelector(".message-row");
      button.classList.toggle("selected", row.id === traffic.selectedId);
      const pressed = String(row.id === traffic.selectedId);
      if (button.getAttribute("aria-pressed") !== pressed) button.setAttribute("aria-pressed", pressed);
      if (item !== position) list.insertBefore(item, position);
      position = item.nextElementSibling;
    }
  }
  async function loadTraffic(older = false, background = false) {
    if (!token) return;
    const current = generation, request = ++traffic.request;
    const params = new URLSearchParams(traffic.query);
    params.set("limit", "50");
    if (older && traffic.nextCursor) params.set("before", traffic.nextCursor);
    if (!background) $("older-button").disabled = true;
    try {
      const data = await api(`/api/messages?${params}`, current);
      if (current !== generation || request !== traffic.request) return;
      const messages = Array.isArray(data.messages) ? data.messages : [];
      if (older) {
        const seen = new Set(traffic.rows.map(row => row.id));
        traffic.rows = traffic.rows.concat(messages.filter(row => !seen.has(row.id))).slice(0, 500);
      } else traffic.rows = messages.slice(0, 500);
      traffic.nextCursor = typeof data.next_cursor === "string" ? data.next_cursor : null;
      renderRows();
      if (route?.kind === "traffic") traffic.dirty = false;
      showError("activity-error", "");
    } catch (error) {
      if (error.name !== "AbortError" && current === generation) showError("activity-error", error instanceof ApiError ? error.message : "The local bridge is unavailable. Waiting to reconnect.");
    } finally {
      if (current === generation && request === traffic.request) $("older-button").disabled = false;
    }
  }

  // ---------- Direct messages between agents (read only) ----------
  const dm = {
    pairs: [], seen: new Map(), selectedId: null, current: null, rows: [], nextCursor: null, request: 0, scanRequest: 0,
    reset() { this.pairs = []; this.seen.clear(); this.selectedId = null; this.current = null; this.rows = []; this.nextCursor = null; this.request++; this.scanRequest++; $("dm-messages").replaceChildren(); },
  };
  // Conversations are derived from recent messages. A bounded scan keeps this cheap and needs no
  // server change: messages already carry both endpoints.
  let agentTimer = null;
  async function loadAgentNames() {
    if (!token) return;
    const current = generation;
    try {
      const data = await api("/api/agents", current);
      if (current !== generation) return;
      const next = Array.isArray(data.agents) ? data.agents.filter(a => typeof a.name === "string" && typeof a.display_name === "string").map(a => [a.name, a.display_name]) : [];
      const same = next.length === agentNames.size && next.every(([key, value]) => agentNames.get(key) === value);
      if (same) return;
      setAgentNames(next);
      traffic.nodes.clear(); // cached rows were built with the old names
      renderSidebar();
      if (route?.kind === "dm" && dm.current) {
        const { a, b } = dm.current;
        $("dm-heading").textContent = `${endpointLabel(a)} ↔ ${endpointLabel(b)}`;
        renderDm(true);
      } else if (route?.kind === "traffic") renderRows();
    } catch (error) { /* names are cosmetic: keep showing routing names */ }
  }
  async function scanPairs() {
    if (!token) return;
    const current = generation, request = ++dm.scanRequest;
    try {
      const found = new Map();
      let cursor = null;
      for (let page = 0; page < 3; page++) {
        const params = new URLSearchParams({ limit: "100" });
        if (cursor) params.set("before", cursor);
        const data = await api(`/api/messages?${params}`, current);
        if (current !== generation || request !== dm.scanRequest) return;
        for (const row of data.messages || []) {
          const pair = pairOf(row);
          if (!pair) continue;
          const entry = found.get(pair.key) || { key: pair.key, a: pair.a, b: pair.b, latest: 0, count: 0, times: [] };
          entry.latest = Math.max(entry.latest, Number(row.created) || 0); entry.count++; entry.times.push(Number(row.created) || 0);
          found.set(pair.key, entry);
        }
        cursor = typeof data.next_cursor === "string" ? data.next_cursor : null;
        if (!cursor) break;
      }
      dm.pairs = [...found.values()].sort((x, y) => y.latest - x.latest).slice(0, 30);
      for (const pair of dm.pairs) if (!dm.seen.has(pair.key)) dm.seen.set(pair.key, pair.latest);
      if (route?.kind === "dm") { const open = dm.pairs.find(p => p.key === route.id); if (open) dm.seen.set(open.key, open.latest); }
      renderSidebar();
    } catch (error) { /* the next event or poll retries; the sidebar keeps its last list */ }
  }
  const dmUnread = pair => (route?.kind === "dm" && route.id === pair.key) ? 0 : pair.times.filter(time => time > (dm.seen.get(pair.key) ?? pair.latest)).length;
  async function loadDm(older = false) {
    if (!token || route?.kind !== "dm") return;
    const current = generation, request = ++dm.request, key = route.id;
    const pair = dm.current;
    if (!pair) return;
    $("dm-status").hidden = false; $("dm-status").textContent = older ? "Loading older messages…" : "Loading messages…";
    try {
      let cursor = older ? dm.nextCursor : null, matched = older ? dm.rows : [];
      for (let page = 0; page < 5; page++) {
        const params = new URLSearchParams({ agent: pair.a.agent, limit: "100" });
        if (cursor) params.set("before", cursor);
        const data = await api(`/api/messages?${params}`, current);
        if (current !== generation || request !== dm.request || route?.id !== key) return;
        const seenIds = new Set(matched.map(row => row.id));
        for (const row of data.messages || []) if (pairOf(row)?.key === key && !seenIds.has(row.id)) matched.push(row);
        cursor = typeof data.next_cursor === "string" ? data.next_cursor : null;
        if (!cursor || matched.length >= 30) break;
      }
      dm.rows = matched; dm.nextCursor = cursor;
      $("dm-status").hidden = true;
      renderDm(older);
    } catch (error) {
      if (error.name !== "AbortError" && current === generation) { $("dm-status").hidden = false; $("dm-status").textContent = "Unable to load this conversation. Retrying when the bridge reconnects."; }
    }
  }
  function renderDm(keepPosition = false) {
    const list = $("dm-messages"), scroller = $("dm-scroll");
    if (route?.kind !== "dm" || !dm.current) return;
    const atBottom = scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 60;
    const heightBefore = scroller.scrollHeight, topBefore = scroller.scrollTop;
    const ordered = [...dm.rows].sort((a, b) => Number(a.created) - Number(b.created));
    const rows = [];
    ordered.forEach((row, index) => {
      const before = ordered[index - 1];
      if (!before || dayKey(before.created) !== dayKey(row.created)) rows.push(el("li", "day", dayLabel(row.created)));
      const endpoint = row.source, name = endpointLabel(endpoint), tone = colorClass(endpointKey(endpoint));
      const grouped = !!before && before.source && endpointKey(before.source) === endpointKey(endpoint) && dayKey(before.created) === dayKey(row.created) && Number(row.created) - Number(before.created) < 5 * 60000;
      const item = el("li", `msg${grouped ? " is-grouped" : ""}${row.id === dm.selectedId ? " is-selected" : ""}`);
      const avatar = el("div", `avatar ${tone}`, initials(nameOf(endpoint))); avatar.setAttribute("aria-hidden", "true");
      const main = el("div", "msg__main");
      const meta = el("div", "msg__meta");
      meta.append(el("strong", `author ${tone}`, name), el("time", "time", hhmm(row.created)));
      if (row.state !== "delivered") meta.append(badge(row.state));
      main.append(meta, el("div", "md md--plain", row.body || "(Empty message)"));
      if (row.body_truncated) main.append(el("span", "reply-hint", "Truncated. Open the details for the full message."));
      const actions = el("div", "msg__actions"); actions.setAttribute("role", "toolbar"); actions.setAttribute("aria-label", "Message actions");
      const inspect = el("button", "btn btn--icon btn--quiet btn--sm", "ⓘ"); inspect.type = "button"; inspect.title = "Delivery details"; inspect.setAttribute("aria-label", "Delivery details");
      inspect.addEventListener("click", () => openDetail(row.id));
      actions.append(inspect);
      item.append(grouped ? el("div", "gutter-time", hhmm(row.created)) : avatar, main, actions);
      rows.push(item);
    });
    list.replaceChildren(...rows);
    $("dm-older").hidden = !dm.nextCursor;
    if (keepPosition) scroller.scrollTop = topBefore + (scroller.scrollHeight - heightBefore);
    else if (atBottom || !list.dataset.ready) { scroller.scrollTop = scroller.scrollHeight; list.dataset.ready = "1"; }
  }
  function showDm(key) {
    const pair = dm.pairs.find(p => p.key === key);
    // Opened from a link before the first scan: rebuild the pair from the key itself.
    const parts = key.split("|").map(part => { const [peer, ...agent] = part.split("~"); return { peer_id: peer === "local" ? null : peer, agent: agent.join("~"), machine_name: null }; });
    dm.current = pair ? { key, a: pair.a, b: pair.b } : { key, a: parts[0], b: parts[1] || parts[0] };
    dm.rows = []; dm.nextCursor = null; dm.selectedId = null;
    $("dm-messages").replaceChildren(); delete $("dm-messages").dataset.ready;
    const [a, b] = [dm.current.a, dm.current.b];
    $("dm-heading").textContent = `${endpointLabel(a)} ↔ ${endpointLabel(b)}`;
    $("dm-sub").textContent = `${a.provider ? a.provider + " · " : ""}${b.provider || ""}`.replace(/ · $/, "") || "agent conversation";
    $("dm-avatars").replaceChildren(...[a, b].map(endpoint => { const tone = colorClass(endpointKey(endpoint)); const av = el("div", `avatar ${tone}`, initials(nameOf(endpoint))); return av; }));
    if (pair) dm.seen.set(key, pair.latest);
    void loadDm();
    renderSidebar();
  }
  $("dm-older").addEventListener("click", () => loadDm(true));

  // ---------- routing ----------
  const sameRoute = (a, b) => !!a && !!b && a.kind === b.kind && a.id === b.id;
  const routeHash = r => r.kind === "traffic" ? "#/traffic" : `#/${r.kind}/${encodeURIComponent(r.id)}`;
  function parseRoute(hash) {
    if (hash === "#/traffic") return { kind: "traffic" };
    const match = /^#\/(board|topic|dm)\/(.+)$/.exec(hash);
    if (!match) return null;
    try { return { kind: match[1], id: decodeURIComponent(match[2]) }; } catch { return null; }
  }
  function defaultRoute() {
    const legacy = new URL(location.href).searchParams.get("topic");
    return legacy ? { kind: "topic", id: legacy } : { kind: "board", id: "main" };
  }
  function setHash(r, replace) {
    const hash = routeHash(r);
    const search = new URLSearchParams(location.search); search.delete("topic");
    const url = location.pathname + (search.size ? `?${search}` : "") + hash;
    if (location.hash === hash && !new URLSearchParams(location.search).has("topic")) return;
    if (replace) history.replaceState(null, "", url); else history.pushState(null, "", url);
  }
  // Switching views honours the board draft protocol: unsaved responses and message drafts get the
  // Save / Discard / Cancel prompt, and Cancel keeps both the view and the address in place.
  async function navigate(next, replace = false) {
    if (!token || !next) return;
    // A navigation that arrives while another is in flight (for example Back during a Save prompt)
    // is queued, not dropped, so the address and the view cannot disagree.
    if (navigating) { queuedRoute = { next, replace }; return; }
    if (sameRoute(route, next)) return;
    navigating = true;
    const session = generation;
    try {
      const leaving = route;
      if (leaving?.kind === "board" && next.kind === "board") {
        const before = boards.current();
        await boards.select(next.id);
        if (session !== generation) return;
        if (boards.current() !== next.id && boards.current() === before && before !== next.id) { queuedRoute = null; restoreHash(); return; }
      } else if (leaving?.kind === "board") {
        let allowed = false;
        await boards.leave(() => { allowed = true; });
        if (session !== generation) return;
        if (!allowed) { queuedRoute = null; restoreHash(); return; } // Cancel is the person's latest choice
      } else if (leaving?.kind === "topic") topics.hide();
      route = next;
      setHash(next, replace);
      await activate(next, leaving);
    } finally {
      // A run that outlived a lock must not touch the flags of the session that replaced it.
      if (session === generation) {
        navigating = false;
        const queued = queuedRoute; queuedRoute = null;
        if (queued) void navigate(queued.next, queued.replace);
      }
    }
  }
  function restoreHash() { if (route) history.replaceState(null, "", location.pathname + location.search + routeHash(route)); }
  async function activate(next, leaving) {
    const views = { board: "boards-view", topic: "topics-view", dm: "dm-view", traffic: "traffic-view" };
    for (const [kind, id] of Object.entries(views)) $(id).hidden = kind !== next.kind;
    document.body.dataset.view = next.kind === "board" ? "boards" : next.kind;
    if (leaving && leaving.kind !== next.kind && panelOwner) closePanel();
    if (next.kind === "board") {
      if (leaving?.kind !== "board") { await boards.open(); if (boards.current() !== next.id) await boards.select(next.id); }
    } else if (next.kind === "topic") topics.show(next.id);
    else if (next.kind === "dm") showDm(next.id);
    else if (next.kind === "traffic") { if (traffic.dirty) void loadTraffic(); }
    renderSidebar();
    document.title = next.kind === "traffic" ? "All traffic · Agent Bridge" : `${next.id} · Agent Bridge`;
  }
  window.addEventListener("hashchange", () => {
    // A pasted access link is always consumed first so the token never lingers in the address bar.
    if (new URLSearchParams(location.hash.slice(1)).has("access_token")) { unlockFromLink(takeAccessLink()); if (token && route) restoreHash(); return; }
    if (!token) return;
    void navigate(parseRoute(location.hash) || defaultRoute(), true);
  });

  // ---------- sidebar ----------
  function navItem({ glyph, label, short, target, unread, title, extra }) {
    const button = el("button", `nav-item${unread ? " is-unread" : ""}`);
    button.type = "button"; button.title = title || label;
    button.dataset.route = routeHash(target);
    if (sameRoute(route, target)) button.setAttribute("aria-current", "page");
    button.append(el("span", "glyph", glyph));
    button.append(short && short.nodeType ? short : el("span", "short", short || glyph));
    button.append(el("span", "grow", label));
    if (extra) button.append(extra);
    if (unread) { const count = el("span", "count", unread > 99 ? "99+" : unread); count.setAttribute("aria-label", `${unread} unread`); button.append(count); }
    button.addEventListener("click", () => { void navigate(target); $("sidebar").classList.remove("is-open"); });
    return button;
  }
  function renderSidebar() {
    if (!token) return;
    const sidebar = $("sidebar");
    const focused = document.activeElement?.closest?.("#sidebar .nav-item")?.dataset.route;
    const section = (title, items, action, empty) => {
      const box = el("div", "nav-section");
      const head = el("div", "nav-section__title"); head.append(el("span", "", title)); if (action) head.append(action);
      box.append(head);
      if (items.length) box.append(...items); else if (empty) box.append(el("div", "nav-empty", empty));
      return box;
    };
    const boardItems = catalog.boardNames.map(name => navItem({ glyph: "▤", label: name, short: Array.from(name)[0].toUpperCase(), target: { kind: "board", id: name } }));
    const topicItems = catalog.topics.map(topic => navItem({ glyph: "#", label: topic.name + (topic.archived ? " (archived)" : ""), short: `#${Array.from(topic.name)[0] ?? ""}`, target: { kind: "topic", id: topic.id }, unread: topic.unread, title: topic.name }));
    const create = el("button", "btn btn--quiet btn--sm", "+ New"); create.type = "button"; create.title = "Create topic";
    create.addEventListener("click", async () => { const id = await topics.create(); if (id) void navigate({ kind: "topic", id }); });
    const pairItems = dm.pairs.map(pair => {
      const tile = el("span", "short short--pair");
      for (const endpoint of [pair.a, pair.b]) { const dot = el("i", colorClass(endpointKey(endpoint)), initials(nameOf(endpoint))); tile.append(dot); }
      return navItem({ glyph: "⇄", label: `${endpointLabel(pair.a)} · ${endpointLabel(pair.b)}`, short: tile, target: { kind: "dm", id: pair.key }, unread: dmUnread(pair), title: `${endpointName(pair.a)} ⇄ ${endpointName(pair.b)}` });
    });
    const scroll = el("div", "sidebar__scroll");
    scroll.append(section("Boards", boardItems, null, "No boards yet"), section("Topics", topicItems, create, "No topics yet"), section("Direct messages", pairItems, null, "No agent conversations yet"));
    const tag = el("span", "tag", "debug");
    const foot = el("div", "sidebar__foot");
    foot.append(navItem({ glyph: "≋", label: "All traffic", short: "≋", target: { kind: "traffic" }, extra: tag }));
    sidebar.replaceChildren(scroll, foot);
    if (focused) [...sidebar.querySelectorAll(".nav-item")].find(item => item.dataset.route === focused)?.focus();
  }
  topics = window.createTopicMonitor({
    request: (path, options) => api(path, generation, options),
    onCatalog: list => { catalog.topics = list; renderSidebar(); },
    openPanel: (title, content) => openPanel(title, content, "thread"),
    closePanel,
  });
  // The sidebar's board list comes from boards.js while Boards is open (it owns that polling and
  // the unsaved-draft rules). Elsewhere a light poll keeps it fresh.
  boards = window.createBoardMonitor({
    request: (path, options) => api(path, generation, options),
    connect: async (path, options) => {
      const current = generation;
      const response = await transport(path, current, { ...options, accept: "text/event-stream" });
      if (!response.ok || !response.body) throw new Error("Live connection unavailable");
      return response;
    },
    authenticated: () => !!token,
    onViewChange: () => {},
    onCatalog: (names, current) => { catalog.boardNames = names; catalog.currentBoard = current; renderSidebar(); },
  });
  async function pollBoardNames() {
    clearTimeout(catalog.boardPoll);
    if (!token) return;
    const current = generation;
    try {
      if (route?.kind !== "board" && !document.hidden) {
        const data = await api("/api/boards", current);
        if (current === generation && Array.isArray(data.boards)) {
          const names = data.boards.map(row => row.name).filter(name => typeof name === "string").sort();
          if (names.join("\n") !== catalog.boardNames.join("\n")) { catalog.boardNames = names; renderSidebar(); }
        }
      }
    } catch (error) { /* keep the previous list */ }
    if (current === generation && token) catalog.boardPoll = setTimeout(pollBoardNames, 10000);
  }

  // ---------- live updates ----------
  // The conversation scan reads up to three pages, so live events may trigger it at most every 3s.
  let lastScan = 0, scanTimer = null;
  function throttledScan() {
    const wait = lastScan + 3000 - Date.now();
    if (wait <= 0) { lastScan = Date.now(); void scanPairs(); return; }
    if (!scanTimer) scanTimer = setTimeout(() => { scanTimer = null; lastScan = Date.now(); void scanPairs(); }, wait);
  }
  function scheduleRefresh() {
    clearTimeout(refreshTimer);
    traffic.dirty = true;
    refreshTimer = setTimeout(() => {
      if (route?.kind === "traffic") void loadTraffic(false, true);
      throttledScan();
      if (route?.kind === "dm") void loadDm();
      if (panelOwner === "detail" && (traffic.selectedId || dm.selectedId)) void openDetail(traffic.selectedId || dm.selectedId);
    }, 180);
  }
  function handleEvent(frame) {
    // Server events contain revision metadata only; content is fetched through the same
    // authenticated read-only API and current filters.
    if (frame.split("\n").some(line => line.startsWith("data:"))) scheduleRefresh();
  }
  async function stream(current) {
    if (!token || current !== generation) return;
    streamAbort?.abort();
    streamAbort = new AbortController();
    try {
      const response = await fetch("/api/events", { headers: { Authorization: `Bearer ${token}`, Accept: "text/event-stream" }, credentials: "omit", cache: "no-store", referrerPolicy: "no-referrer", signal: streamAbort.signal });
      if (current !== generation) return;
      if (response.status === 401 || response.status === 403) { lock("The web session ended. Use the current token to unlock it again."); return; }
      if (!response.ok || !response.body) throw new Error("Stream unavailable");
      reconnectDelay = 1000;
      connection("Connected", "live");
      const reader = response.body.getReader(), decoder = new TextDecoder();
      let buffer = "";
      for (;;) {
        const { value, done } = await reader.read();
        if (current !== generation) { await reader.cancel(); return; }
        if (done) throw new Error("Stream ended");
        buffer += decoder.decode(value, { stream: true });
        buffer = buffer.replace(/\r\n/g, "\n");
        if (buffer.length > 65536) throw new Error("Invalid event stream");
        let end;
        while ((end = buffer.indexOf("\n\n")) !== -1) { handleEvent(buffer.slice(0, end)); buffer = buffer.slice(end + 2); }
      }
    } catch (error) {
      if (error.name === "AbortError" || current !== generation || !token) return;
      streamAbort.abort();
      connection("Reconnecting", "offline");
      reconnectTimer = setTimeout(() => { scheduleRefresh(); stream(current); }, reconnectDelay);
      reconnectDelay = Math.min(reconnectDelay * 2, 30000);
    }
  }

  // ---------- quick switcher (Ctrl/Cmd+K) ----------
  const dialog = $("switcher"), input = $("switcher-input"), listEl = $("switcher-list");
  let choice = 0, shown = [];
  const targets = () => [
    ...catalog.boardNames.map(name => ({ label: `▤ ${name}`, kind: "Board", target: { kind: "board", id: name } })),
    ...catalog.topics.map(topic => ({ label: `# ${topic.name}`, kind: "Topic", target: { kind: "topic", id: topic.id } })),
    ...dm.pairs.map(pair => ({ label: `⇄ ${endpointLabel(pair.a)} · ${endpointLabel(pair.b)}`, kind: "DM", target: { kind: "dm", id: pair.key } })),
    { label: "≋ All traffic", kind: "Debug", target: { kind: "traffic" } },
  ];
  function drawSwitcher() {
    const query = input.value.trim().toLowerCase();
    shown = targets().filter(item => item.label.toLowerCase().includes(query));
    choice = Math.min(choice, Math.max(shown.length - 1, 0));
    listEl.replaceChildren(...shown.map((item, index) => {
      const button = el("button", "", item.label); button.type = "button"; button.id = `switcher-option-${index}`; button.setAttribute("role", "option"); button.setAttribute("aria-selected", String(index === choice));
      button.append(el("span", "kind", item.kind));
      button.addEventListener("click", () => pick(item));
      const row = el("li"); row.setAttribute("role", "presentation"); row.append(button);
      return row;
    }));
    if (shown.length) input.setAttribute("aria-activedescendant", `switcher-option-${choice}`); else input.removeAttribute("aria-activedescendant");
    if (!shown.length) { const none = el("li", "empty", "Nothing matches"); none.setAttribute("role", "presentation"); listEl.append(none); }
  }
  function pick(item) { dialog.close(); void navigate(item.target); }
  function openSwitcher() { if (!token) return; input.value = ""; choice = 0; drawSwitcher(); dialog.showModal(); input.focus(); }
  $("open-switcher").addEventListener("click", openSwitcher);
  input.addEventListener("input", () => { choice = 0; drawSwitcher(); });
  input.addEventListener("keydown", event => {
    if (event.key === "ArrowDown") { event.preventDefault(); choice = Math.min(choice + 1, shown.length - 1); drawSwitcher(); }
    else if (event.key === "ArrowUp") { event.preventDefault(); choice = Math.max(choice - 1, 0); drawSwitcher(); }
    else if (event.key === "Enter" && shown[choice]) pick(shown[choice]);
  });
  dialog.addEventListener("click", event => { if (event.target === dialog) dialog.close(); });

  // ---------- collapsible sidebar ----------
  const SIDEBAR_KEY = "agent-bridge-sidebar", mobile = () => matchMedia("(max-width: 780px)").matches;
  function setCollapsed(on, save = true) {
    body.classList.toggle("is-collapsed", on);
    $("sidebar-toggle").setAttribute("aria-expanded", String(!on)); $("sidebar-toggle").setAttribute("aria-label", on ? "Expand sidebar" : "Collapse sidebar");
    if (save) try { localStorage.setItem(SIDEBAR_KEY, on ? "collapsed" : "open"); } catch { /* layout preference only */ }
  }
  try { setCollapsed(localStorage.getItem(SIDEBAR_KEY) === "collapsed", false); } catch { /* ignore */ }
  const flipSidebar = () => mobile() ? $("sidebar").classList.toggle("is-open") : setCollapsed(!body.classList.contains("is-collapsed"));
  $("sidebar-toggle").addEventListener("click", flipSidebar);
  addEventListener("keydown", event => {
    const mod = event.ctrlKey || event.metaKey;
    if (mod && !event.shiftKey && event.key.toLowerCase() === "k") { event.preventDefault(); if (token) (dialog.open ? dialog.close() : openSwitcher()); }
    else if (mod && !event.shiftKey && event.key.toLowerCase() === "b" && token) { event.preventDefault(); flipSidebar(); }
    else if (event.key === "Escape" && panelOwner && !dialog.open && !document.querySelector("dialog[open]")) closePanel();
  });

  // ---------- theme picker ----------
  menu("theme-button", "theme-menu");
  menu("board-menu-button", "board-menu");
  document.addEventListener("themechange", () => syncThemeMenu());
  function syncThemeMenu() {
    for (const button of $("theme-menu").querySelectorAll("[data-theme-choice]")) button.setAttribute("aria-checked", String(button.dataset.themeChoice === window.AgentBridgeTheme.get()));
  }
  for (const button of $("theme-menu").querySelectorAll("[data-theme-choice]")) button.addEventListener("click", () => { window.AgentBridgeTheme.set(button.dataset.themeChoice); syncThemeMenu(); });
  syncThemeMenu();

  // ---------- unlock, lock, filters ----------
  $("login-form").addEventListener("submit", async event => {
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
      localPeer = status.machine?.peer_id || null;
      login.hidden = true; workspace.hidden = false;
      $("sidebar").hidden = false; body.classList.add("has-sidebar");
      for (const id of ["sidebar-toggle", "open-switcher", "lock-button"]) $(id).hidden = false;
      topics.start();
      void loadAgentNames(); clearInterval(agentTimer); agentTimer = setInterval(() => { if (!document.hidden) void loadAgentNames(); }, 60000);
      void scanPairs(); void pollBoardNames();
      const first = parseRoute(location.hash) || defaultRoute();
      route = null;
      await navigate(first, true);
      if (current === generation && token) stream(current);
    } catch (error) {
      if (error.name !== "AbortError" && current === generation) lock("The local bridge could not be reached. Check that the web interface is enabled (`agent-bridge web enable`), then try again.");
    } finally { if (current === generation) submit.disabled = false; }
  });
  $("lock-button").addEventListener("click", () => {
    if (topics.hasDrafts() && !window.confirm("Locking discards topic drafts. An uncertain publication may already have been accepted. Discard drafts and lock?")) return;
    void boards.leave(() => { lock(); $("access-token").focus(); }, true);
  });
  $("filters").addEventListener("submit", event => {
    event.preventDefault();
    const next = new URLSearchParams();
    for (const key of ["machine", "agent", "state"]) { const value = $(`${key}-filter`).value.trim(); if (value) next.set(key, value); }
    const since = $("since-filter").value ? new Date($("since-filter").value).getTime() : null;
    const until = $("until-filter").value ? new Date($("until-filter").value).getTime() : null;
    if ((since !== null && !Number.isFinite(since)) || (until !== null && !Number.isFinite(until)) || (since !== null && until !== null && since > until)) { showError("activity-error", "Choose a valid time range with From earlier than Until."); return; }
    if (since !== null) next.set("since", String(since));
    if (until !== null) next.set("until", String(until));
    traffic.query = next; traffic.nextCursor = null;
    void loadTraffic();
  });
  $("reset-filters").addEventListener("click", () => { $("filters").reset(); traffic.query = new URLSearchParams(); traffic.nextCursor = null; void loadTraffic(); });
  $("older-button").addEventListener("click", () => loadTraffic(true));
  window.addEventListener("pagehide", () => lock());
  window.addEventListener("beforeunload", event => { if (topics.hasDrafts()) { event.preventDefault(); event.returnValue = ""; } });
  function unlockFromLink(value) {
    // A link must not replace an active session or discard its response draft.
    if (!value || token) return;
    $("access-token").value = value;
    $("login-form").requestSubmit();
  }
  unlockFromLink(accessFromLink);
  accessFromLink = null;
})();
