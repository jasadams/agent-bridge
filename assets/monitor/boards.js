"use strict";
// No credentials, private drafts or board snapshots leave this tab's memory.
window.createBoardMonitor = ({ request, connect, authenticated, onViewChange }) => {
  const $ = id => document.getElementById(id);
  const tags = new Set("p h1 h2 h3 h4 h5 h6 blockquote ul ol li pre code em strong del table thead tbody tr th td hr br a span".split(" "));
  let active = false, authEpoch = 0, boardEpoch = 0, sequence = 0;
  let board = "main", shown = null, latest = null, draft = null, saving = false;
  let timer = null, readAbort = null, reading = false, backoff = 1000, raw = false, leaveChoice = null;
  let liveAbort = null, liveSequence = 0, refreshRequested = false;
  const controlRows = new Map();
  let copySequence = 0, savedControl = null;
  const copyAttempts = new WeakMap();
  const copyIconPath = "M9 9h12v12H9z M15 9V3H3v12h6";
  const copyIcon = (button, copied) => {
    button.toggleAttribute("data-copied", copied);
    button.querySelector("svg path").setAttribute("d", copied ? "M5 12l4 4L19 6" : copyIconPath);
  };
  const node = (tag, text, className) => { const n = document.createElement(tag); if (text !== undefined) n.textContent = String(text); if (className) n.className = className; return n; };
  const text = (element, value) => { if (element.textContent !== value) element.textContent = value; };
  const status = value => { text($("board-status"), value); };
  const error = value => { text($("board-error"), value || ""); $("board-error").hidden = !value; };
  const capture = () => ({ auth: authEpoch, epoch: boardEpoch, name: board, authority: authorityEpoch });
  const current = c => authenticated() && c.auth === authEpoch && c.epoch === boardEpoch && c.name === board;
  const validName = name => typeof name === "string" && /^[a-z0-9][a-z0-9_-]{0,63}$/.test(name);
  const validView = (view, name) => view && view.name === name && typeof view.markdown === "string" && typeof view.revision === "string" && Array.isArray(view.nodes) && Array.isArray(view.controls);
  const dirty = () => !!draft && (draft.kind === "checkbox" || draft.value !== draft.original || draft.state !== "editing");
  // The authoritative lifecycle advances independently of the frozen document.
  let authority = null, authorityEpoch = 0, lifecycleOperation = null, archiveIntent = null;
  let archivedEpoch = 0, archivedAbort = null, archivedActionFeedback = "";
  const scope = view => JSON.stringify([view?.incarnation || null, view?.lifecycle_generation || null]);
  const phase = view => view?.state || "active";
  const writable = view => !!authority && !lifecycleOperation && phase(authority) === "active" && !authority.read_only && scope(view) === scope(authority);
  const authorityCurrent = c => current(c) && c.authority === authorityEpoch;
  function lifecycleUI() {
    const state = authority ? phase(authority) : "unavailable";
    let message = state === "active" ? "" : state === "archived" ? "Archived · read only. Restore before permanent deletion." : state === "purging" ? "Expired · permanent cleanup is pending. Restore is unavailable." : state === "restoring" ? "Restoring · read only until recovery completes." : "Board unavailable · its displayed content and local drafts are kept.";
    if (lifecycleOperation) message = lifecycleOperation.pending ? `${lifecycleOperation.action === "archive" ? "Archiving" : "Restoring"} ${lifecycleOperation.name}… Wait for the outcome.` : lifecycleOperation.feedback;
    text($("board-lifecycle-status"), message);
    if ($("archived-dialog").open && lifecycleOperation) text($("archived-feedback"), message + (lifecycleOperation.pending ? "" : " Close this dialog, then Inspect current board state."));
    $("board-lifecycle-review").hidden = !lifecycleOperation && state === "active";
    $("board-lifecycle-review").disabled = !!lifecycleOperation?.pending;
    $("board-archive").disabled = board === "main" || !shown || !writable(shown) || saving || messagePending();
    $("board-archive").title = board === "main" ? "Main cannot be archived." : "Archive for 30 days, then permanently delete managed content.";
    $("board-selector").disabled = !!lifecycleOperation?.pending;
    $("archive-confirm-submit").disabled = !!lifecycleOperation;
    $("archived-close").disabled = !!lifecycleOperation?.pending;
    $("archived-refresh").disabled = !!lifecycleOperation?.pending;
    for (const button of $("archived-list").querySelectorAll("button")) button.disabled = !!lifecycleOperation || button.dataset.unavailable === "true";
  }
  function observeAuthority(view) {
    if (!view || view.name !== board) return;
    const changed = authority && (scope(authority) !== scope(view) || phase(authority) !== phase(view) || !!authority.read_only !== !!view.read_only);
    if (changed) {
      authorityEpoch++;
      if (draft) { draft.interrupted = true; draft.reviewed = false; if (draft.state === "editing") draft.state = "conflict"; }
    }
    authority = { name: view.name, revision: view.revision, incarnation: view.incarnation,
      lifecycle_generation: view.lifecycle_generation, state: phase(view), read_only: !!view.read_only,
      archived_at: view.archived_at, expires_at: view.expires_at };
    if (shown) controls(shown); else lifecycleUI();
  }
  function unavailable() {
    observeAuthority({ name: board, revision: authority?.revision, incarnation: authority?.incarnation,
      lifecycle_generation: authority?.lifecycle_generation, state: "unavailable", read_only: true });
  }
  function renderTimestamp(view) {
    const footer = $("board-last-updated"), value = view?.last_updated;
    let formatted = "Unknown";
    if (Number.isSafeInteger(value) && Math.abs(value) <= 8640000000000000) {
      formatted = new Intl.DateTimeFormat(undefined, {
        year: "numeric", month: "short", day: "numeric", hour: "2-digit",
        minute: "2-digit", second: "2-digit", timeZoneName: "short"
      }).format(new Date(value));
    }
    text(footer, view ? `Last Updated: ${formatted}` : "");
    footer.hidden = !view;
  }
  // Metadata only moves with the matching visible/deferred content revision.
  function receiveTimestamp(result) {
    if (shown?.revision === result.revision && typeof result.exists === "boolean" &&
        shown.exists !== result.exists && (draft || selectionHeld())) {
      latest = { ...shown, exists: result.exists, last_updated: result.last_updated,
        last_updated_source: result.last_updated_source };
      $("board-newer").hidden = false;
      return;
    }
    for (const view of [shown, latest]) {
      if (view && view.revision === result.revision) {
        if (typeof result.exists === "boolean") view.exists = result.exists;
        view.last_updated = result.last_updated;
        view.last_updated_source = result.last_updated_source;
      }
    }
    if (shown?.revision === result.revision) {
      renderTimestamp(shown);
      if (!shown.markdown) {
        const empty = $("board-document").querySelector("p.help");
        if (empty) text(empty, shown.exists ? "This board is empty." : "main has not been written yet.");
      }
    }
  }
  // Catalog discovery has its own lifecycle: document switches and frozen
  // response snapshots must neither restart polling nor invalidate its results.
  let catalogEpoch = 0, catalogTimer = null, catalogFlight = null;
  let catalogDelay = 2000, catalogAgain = false, catalogNames = null;
  const catalogCurrent = epoch => active && authenticated() && epoch === catalogEpoch;
  function catalogFeedback(value) {
    const element = $("board-catalog-feedback");
    text(element, value); element.hidden = !value;
  }
  function stopCatalog() {
    catalogEpoch++; clearTimeout(catalogTimer); catalogTimer = null;
    catalogFlight?.abort(); catalogFlight = null; catalogAgain = false;
  }
  function reconcileCatalog(names) {
    const selector = $("board-selector"), desired = Array.from(new Set([...names, board])).sort();
    const existing = new Map(Array.from(selector.options, option => [option.value, option]));
    for (let index = 0; index < desired.length; index++) {
      const name = desired[index];
      let option = existing.get(name);
      if (!option) { option = node("option", name); option.value = name; }
      if (selector.options[index] !== option) selector.insertBefore(option, selector.options[index] || null);
    }
    while (selector.options.length > desired.length) selector.lastElementChild.remove();
    // Setting the same value needlessly can close native selector popups.
    if (selector.value !== board) selector.value = board;
  }
  function validateCatalog(data) {
    if (!data || !Array.isArray(data.boards) || data.boards.length > 256) throw new Error("Invalid board list");
    const rows = new Map();
    for (const row of data.boards) {
      if (!row || !validName(row.name) || typeof row.exists !== "boolean" || typeof row.revision !== "string" || !/^[a-f0-9]{64}$/.test(row.revision)) throw new Error("Invalid board list");
      const prior = rows.get(row.name);
      if (prior && (prior.exists !== row.exists || prior.revision !== row.revision)) throw new Error("Inconsistent board list");
      rows.set(row.name, row);
    }
    if (!rows.has("main")) throw new Error("Board list is missing main");
    return Array.from(rows.keys()).sort();
  }
  function scheduleCatalog(epoch, delay) {
    if (!catalogCurrent(epoch)) return;
    clearTimeout(catalogTimer);
    catalogTimer = setTimeout(() => { catalogTimer = null; void refreshCatalog(); }, delay);
  }
  async function refreshCatalog(catchUp = false) {
    if (!active || !authenticated()) return;
    if (catchUp) { catalogDelay = 2000; clearTimeout(catalogTimer); catalogTimer = null; }
    if (catalogFlight) { if (catchUp) catalogAgain = true; return; }
    const epoch = catalogEpoch, controller = new AbortController(), started = Date.now();
    catalogFlight = controller;
    let deadline, delay = 4000;
    try {
      const timeout = new Promise((_, reject) => { deadline = setTimeout(() => {
        controller.abort(); reject(new Error("Board list timed out"));
      }, 4000); });
      const data = await Promise.race([request("/api/boards", { signal: controller.signal }), timeout]);
      if (!catalogCurrent(epoch) || catalogFlight !== controller) return;
      const names = validateCatalog(data);
      reconcileCatalog(names); catalogNames = names; catalogDelay = 2000;
      catalogFeedback(names.includes(board) ? "" : "The selected board is no longer listed. Its identity is kept; refresh it to check availability.");
      if (!names.includes(board) && !lifecycleOperation?.pending) void readSnapshot();
      delay = Math.max(0, 4000 - (Date.now() - started));
    } catch (e) {
      if (!catalogCurrent(epoch) || catalogFlight !== controller) return;
      catalogFeedback("Board list unavailable. Known boards are kept; retrying automatically.");
      delay = catalogDelay; catalogDelay = Math.min(catalogDelay * 2, 30000);
    } finally {
      clearTimeout(deadline);
      if (catalogCurrent(epoch) && catalogFlight === controller) {
        catalogFlight = null;
        const again = catalogAgain; catalogAgain = false;
        scheduleCatalog(epoch, again ? 0 : delay);
      }
    }
  }
  function catchUpCatalog() { if (active && authenticated()) void refreshCatalog(true); }
  function selectionHeld() {
    const selection = window.getSelection();
    return !!selection && !selection.isCollapsed && ["board-document", "board-raw", "board-controls"].some(id => $(id).contains(selection.anchorNode) || $(id).contains(selection.focusNode));
  }
  function stopRead() { readAbort?.abort(); readAbort = null; sequence++; reading = false; refreshRequested = false; }
  function stopLive() { clearTimeout(timer); timer = null; liveSequence++; liveAbort?.abort(); liveAbort = null; }
  function stop() { stopLive(); stopRead(); }
  function reconnect() {
    stopLive(); refreshRequested = false;
    if (!active || !authenticated()) return;
    backoff = Math.min(backoff * 2, 30000);
    timer = setTimeout(() => { timer = null; void startLive(); }, backoff);
  }
  function flushRefresh() {
    if (refreshRequested && active && authenticated() && !reading && !saving) {
      refreshRequested = false;
      void refresh();
    }
  }
  async function startLive() {
    if (!active || !authenticated()) return;
    stopLive();
    const c = capture(), id = liveSequence, controller = new AbortController();
    liveAbort = controller;
    const live = () => current(c) && active && id === liveSequence && !controller.signal.aborted;
    try {
      const response = await connect(`/api/boards/${encodeURIComponent(c.name)}/events`, { signal: controller.signal });
      if (!live()) { await response.body?.cancel(); return; }
      if (!response.ok || !response.body) throw new Error("Board live connection unavailable");
      catchUpCatalog();
      // Catch changes between the initial snapshot and connecting, including reconnect gaps.
      await refresh();
      if (!live()) { await response.body.cancel(); return; }
      const reader = response.body.getReader(), decoder = new TextDecoder();
      let buffer = "";
      for (;;) {
        const { value, done } = await reader.read();
        if (!live()) { await reader.cancel(); return; }
        if (done) throw new Error("Board live connection ended");
        buffer += decoder.decode(value, { stream: true });
        buffer = buffer.replace(/\r\n/g, "\n");
        if (buffer.length > 65536) throw new Error("Invalid board event stream");
        let end;
        while ((end = buffer.indexOf("\n\n")) !== -1) {
          const frame = buffer.slice(0, end); buffer = buffer.slice(end + 2);
          const lines = frame.split("\n");
          if (!lines.includes("event: board-revision")) continue;
          const data = JSON.parse(lines.filter(line => line.startsWith("data:")).map(line => line.slice(5).trimStart()).join("\n"));
          if (data.name !== c.name || typeof data.revision !== "string" || !/^[a-f0-9]{64}$/.test(data.revision)) throw new Error("Invalid board revision event");
          const metadata = ["exists", "last_updated", "last_updated_source"].some(key => Object.hasOwn(data, key));
          if (metadata && (typeof data.exists !== "boolean" ||
              !(data.last_updated === null || Number.isSafeInteger(data.last_updated) && Math.abs(data.last_updated) <= 8640000000000000) ||
              !["file_mtime", "unknown"].includes(data.last_updated_source))) throw new Error("Invalid board metadata event");
          observeAuthority(data);
          receiveMessageLabels(data);
          backoff = 1000;
          const currentView = latest || shown;
          if (data.revision !== currentView?.revision || metadata &&
              (data.exists !== currentView?.exists || data.last_updated !== currentView?.last_updated || data.last_updated_source !== currentView?.last_updated_source) ||
              phase(data) !== phase(currentView) || scope(data) !== scope(currentView) || !!data.read_only !== !!currentView?.read_only) {
            refreshRequested = true;
            flushRefresh();
          }
        }
      }
    } catch (e) {
      if (live() && e.name !== "AbortError") {
        error("The live board connection was interrupted. Your last content is kept; reconnecting.");
        status("Stale · reconnecting"); reconnect();
      }
    }
  }
  function renderNodes(nodes, copyable = true) {
    let work = 0;
    function build(value, depth) {
      if (++work > 20000 || depth > 32 || !value || typeof value !== "object") throw new Error("Complex document");
      if (value.kind === "control" && typeof value.id === "string") return inlineControl(value.id);
      if (value.kind === "text" && typeof value.text === "string") return document.createTextNode(value.text);
      if (value.kind !== "element" || !tags.has(value.tag) || !Array.isArray(value.children)) throw new Error("Unsupported document node");
      const element = node(value.tag);
      if (value.tag === "ol" && Number.isSafeInteger(value.start) && value.start >= 0) element.start = value.start;
      if (value.tag === "a" && typeof value.href === "string") {
        try {
          const url = new URL(value.href);
          if (["http:", "https:"].includes(url.protocol) && !url.username && !url.password) {
            element.href = url.href; element.target = "_blank"; element.rel = "noopener noreferrer"; element.referrerPolicy = "no-referrer";
          }
        } catch { /* Unsafe links remain inert text. */ }
      }
      for (const child of value.children) element.append(build(child, depth + 1));
      if (value.tag === "a" && element.querySelector(".board-code-copy")) {
        // Keep native buttons outside hyperlinks, including code nested in
        // emphasis. Text/code retains the original safe link destination.
        function linkChildren(child) {
          if (child.nodeType === Node.ELEMENT_NODE && child.classList.contains("board-inline-code")) {
            const linked = element.cloneNode(false), code = child.firstElementChild;
            linked.append(code); child.prepend(linked); return child;
          }
          if (child.nodeType === Node.ELEMENT_NODE) {
            for (const nested of Array.from(child.childNodes)) nested.replaceWith(linkChildren(nested));
            return child;
          }
          const linked = element.cloneNode(false); linked.append(child.cloneNode(true)); return linked;
        }
        const wrapper = node("span");
        for (const child of Array.from(element.childNodes)) wrapper.append(linkChildren(child));
        return wrapper;
      }
      if (copyable && (value.tag === "code" || value.tag === "pre")) {
        const wrapper = node(value.tag === "pre" ? "div" : "span", undefined,
          value.tag === "pre" ? "board-code-block" : "board-inline-code");
        const button = node("button", undefined, "board-code-copy");
        button.type = "button";
        button.setAttribute("aria-label", value.tag === "pre" ? "Copy code block" : "Copy inline code");
        button.title = button.getAttribute("aria-label");
        const icon = document.createElementNS("http://www.w3.org/2000/svg", "svg");
        icon.setAttribute("viewBox", "0 0 24 24");
        icon.setAttribute("aria-hidden", "true");
        icon.setAttribute("focusable", "false");
        icon.setAttribute("fill", "none");
        icon.setAttribute("stroke", "currentColor");
        icon.setAttribute("stroke-width", "2");
        icon.setAttribute("stroke-linejoin", "round");
        const sheets = document.createElementNS("http://www.w3.org/2000/svg", "path");
        sheets.setAttribute("d", copyIconPath);
        icon.append(sheets);
        button.append(icon);
        wrapper.append(element, button);
        return wrapper;
      }
      return element;
    }
    const fragment = document.createDocumentFragment();
    for (const value of nodes) fragment.append(build(value, 0));
    return fragment;
  }
  // Reconcile safe, locally constructed nodes so an updated table cell does not
  // replace its unchanged table, paragraphs or links.
  function reconcile(parent, next) {
    const children = Array.from(next.childNodes);
    for (let index = 0; index < children.length; index++) {
      const desired = children[index], existing = parent.childNodes[index];
      if (!existing) { parent.append(desired); continue; }
      if (existing === desired) continue;
      if (desired.nodeType === Node.ELEMENT_NODE && desired.classList.contains("board-control")) { existing.replaceWith(desired); continue; }
      if (existing.isEqualNode(desired) && !(desired.nodeType === Node.ELEMENT_NODE && desired.querySelector(".board-control"))) continue;
      if (existing.nodeType !== desired.nodeType || existing.nodeName !== desired.nodeName) { existing.replaceWith(desired); continue; }
      if (existing.nodeType === Node.TEXT_NODE) { existing.nodeValue = desired.nodeValue; continue; }
      for (const attribute of Array.from(existing.attributes)) if (!desired.hasAttribute(attribute.name)) existing.removeAttribute(attribute.name);
      for (const attribute of Array.from(desired.attributes)) if (existing.getAttribute(attribute.name) !== attribute.value) existing.setAttribute(attribute.name, attribute.value);
      reconcile(existing, desired);
    }
    while (parent.childNodes.length > children.length) parent.lastChild.remove();
  }
  function renderDocument(view) {
    copySequence++; text($("board-copy-feedback"), "");
    text($("selected-board-name"), view.name);
    renderTimestamp(view);
    text($("board-raw"), view.markdown);
    let document;
    try { document = renderNodes(view.nodes, !view.render_notice); }
    catch { document = window.document.createDocumentFragment(); document.append(node("pre", typeof view.display_markdown === "string" ? view.display_markdown : "This document exceeds the rendering limit. Use Raw Markdown to inspect its exact source.", "board-render-fallback")); }
    if (!view.markdown) document.append(node("p", view.exists ? "This board is empty." : "main has not been written yet.", "help"));
    if (view.control_notice) document.prepend(node("p", view.control_notice, "help notice"));
    if (view.render_notice) document.prepend(node("p", view.render_notice, "help notice"));
    reconcile($("board-document"), document);
    $("board-document").hidden = raw; $("board-raw").hidden = !raw;
  }
  const messageDrafts = new Map();
  const messageDirty = () => Array.from(messageDrafts.values()).some(m => m.value || m.submission || m.pending);
  const messagePending = () => Array.from(messageDrafts.values()).some(m => m.pending);
  const messageIdentity = control => JSON.stringify([board, control.field_id, control.rebase_key]);
  let messageLabels = {};
  const compactMessageLabel = value => {
    const clean = value.replace(/[\u0000-\u001f\u007f-\u009f\u061c\u200e\u200f\u2028-\u202e\u2066-\u2069]/g, "").trim();
    let label = "";
    for (const ch of clean) { if (new TextEncoder().encode(label + ch).length > 64) break; label += ch; }
    return label;
  };
  const messageLabel = control => {
    const known = messageLabels[control.rebase_key];
    if (typeof known === "string" && compactMessageLabel(known)) return compactMessageLabel(known);
    const owner = control.owner;
    const generated = value => /(claude|codex|opencode)-[0-9a-f-]{36}|opencode-ses_/i.test(value);
    if (!generated(owner.name) && owner.name !== `${owner.provider}-${owner.session}`) return compactMessageLabel(owner.name);
    if (control.label && !control.label.includes(owner.name) && !generated(control.label) && compactMessageLabel(control.label)) return compactMessageLabel(control.label);
    return ({claude:"Claude", codex:"Codex", opencode:"OpenCode", inbox:"Agent"})[owner.provider] || "Agent";
  };
  function receiveMessageLabels(value) {
    if (!value.message_labels || typeof value.message_labels !== "object" || Array.isArray(value.message_labels)) return;
    messageLabels = value.message_labels;
    for (const m of messageDrafts.values()) if (m.name === board) text(m.label, messageLabel(m.control));
  }
  function renderMessage(m) {
    m.row.hidden = m.name !== board;
    m.input.readOnly = m.pending || m.state === "uncertain";
    m.send.disabled = m.pending || !!lifecycleOperation || (!m.submission && (!writable(m) || (!m.value.trim() || m.stale || !m.present)));
    text(m.send, m.submission ? "Retry same message" : "Send");
    m.review.hidden = !m.stale || m.state === "uncertain";
    m.review.disabled = m.pending || !writable(m);
    m.discard.disabled = m.pending;
    m.discard.hidden = (!m.value && !m.submission) || (!m.stale && m.state === "editing");
    text(m.feedback, m.pending ? "Sending to broker…" : m.feedbackText || (m.stale ? "The board or input changed. Your draft is kept. Review before sending." : ""));
    $("board-messages").hidden = !Array.from(messageDrafts.values()).some(d => d.name === board);
  }
  // Resizing can clip a box that was already focused: focus events do not fire
  // again when the viewport or the bounded stack changes size.
  const messageResize = new ResizeObserver(() => {
    const input = document.activeElement;
    if (active && input?.matches("#message-controls textarea")) input.scrollIntoView({block:"nearest"});
  });
  messageResize.observe($("board-messages"));
  messageResize.observe($("message-controls"));
  function discardMessage(m) {
    if (m.pending) return;
    const uncertain = m.state === "uncertain";
    m.value = ""; m.version++; m.submission = null; m.state = "editing"; m.input.value = "";
    m.feedbackText = uncertain ? "Draft discarded. The prior submission may already have been accepted; inspect message history." : "Draft discarded.";
    if (!m.present) { m.row.remove(); messageDrafts.delete(m.key); }
    else { m.revision = m.currentRevision; m.controlId = m.control.id; m.stale = false; }
    renderMessage(m);
  }
  async function reviewMessage(m) {
    if (m.pending || m.state === "uncertain" || !writable(m)) return;
    const c = capture(), view = await readSnapshot(true);
    if (!authorityCurrent(c) || !writable(m) || !active || !view || messageDrafts.get(m.key) !== m) return;
    const found = view.controls.filter(control => control.kind === "message" && messageIdentity(control) === m.key);
    if (found.length !== 1) { m.feedbackText = "This exact input is unavailable or changed. Your draft is kept; discard it only after inspecting the current target."; renderMessage(m); return; }
    m.submission = null; m.state = "editing"; m.revision = view.revision; m.controlId = found[0].id; m.control = found[0]; m.present = true; m.stale = false;
    m.feedbackText = "Current input reviewed. Check the target and press Enter to send."; renderMessage(m); m.input.focus();
  }
  async function sendMessage(m) {
    if (m.pending || !active || !authenticated() || m.name !== board || messageDrafts.get(m.key) !== m) return;
    if (lifecycleOperation || (!m.submission && (!writable(m) || !m.value.trim() || m.stale || !m.present))) return;
    if (!m.submission) m.submission = Object.freeze({ board: m.name, expected_revision: m.revision, control_id: m.controlId, id: crypto.randomUUID(), text: m.value });
    const submission = m.submission, c = capture(), version = m.version;
    const focusedHere = m.row.contains(document.activeElement);
    m.pending = true; m.feedbackText = ""; renderMessage(m);
    if (focusedHere) m.input.focus();
    try {
      const result = await request(`/api/boards/${encodeURIComponent(submission.board)}/messages`, { method: "POST", body: JSON.stringify(submission) });
      if (!current(c) || !active || messageDrafts.get(m.key) !== m || m.submission !== submission) return;
      if (result.accepted !== true || result.id !== submission.id || result.message?.id !== submission.id || result.message?.body !== submission.text || result.message?.sender !== "user" || result.message?.recipient !== m.owner.name || result.message?.recipient_session !== m.owner.session || typeof result.state !== "string") throw new Error("Unconfirmed message acceptance");
      m.submission = null; m.state = "editing";
      if (m.version === version) { m.value = ""; m.input.value = ""; }
      m.feedbackText = `Broker accepted message (${result.state}). This does not mean the agent has read or completed it.`;
      if (m.present && m.control && writable(m)) { m.revision = m.currentRevision; m.controlId = m.control.id; m.stale = false; }
    } catch (e) {
      if (!current(c) || !active || messageDrafts.get(m.key) !== m || m.submission !== submission) return;
      if (e.data?.accepted === false && e.data?.code !== "uncertain") {
        m.state = "rejected";
        if (e.status === 409) m.submission = null;
        m.feedbackText = e.data.error || "The broker rejected this message. Your draft is kept.";
        if (e.status === 409) m.stale = true;
        if (authorityCurrent(c) && validView(e.data?.board, board)) observeAuthority(e.data.board);
      } else {
        m.state = "uncertain";
        m.feedbackText = "Acceptance is uncertain. Your exact draft and message ID are kept. Retry the same message explicitly, or inspect history before discarding.";
      }
    } finally {
      if (current(c) && active && messageDrafts.get(m.key) === m) { const focused = document.activeElement;
        const keepFocus = m.row.contains(focused);
        m.pending = false; renderMessage(m);
        if (keepFocus) m.input.focus(); }
    }
  }
  function syncMessages(view) {
    for (const m of messageDrafts.values()) if (m.name === board) m.present = false;
    for (const control of view.controls) {
      if (control.kind !== "message" || typeof control.field_id !== "string" || typeof control.rebase_key !== "string" || typeof control.id !== "string" || typeof control.label !== "string" || !control.owner || !["name", "provider", "session", "endpoint"].every(k => typeof control.owner[k] === "string")) continue;
      const key = messageIdentity(control);
      let m = messageDrafts.get(key);
      if (!m) {
        const row = node("section", undefined, "message-composer"), label = node("label", messageLabel(control)), input = node("textarea"), feedback = node("p", "", "message-feedback help"), actions = node("div", undefined, "response-actions"), send = node("button", "Send"), review = node("button", "Review current input"), discard = node("button", "Discard draft");
        input.id = "message-" + crypto.randomUUID(); input.rows = 1; input.maxLength = 65536; label.htmlFor = input.id;
        feedback.id = input.id + "-feedback"; feedback.setAttribute("role", "status"); feedback.setAttribute("aria-live", "polite"); input.setAttribute("aria-describedby", feedback.id);
        for (const button of [send, review, discard]) button.type = "button";
        actions.append(send, review, discard); row.append(label, input, actions, feedback); row.dataset.fieldId = control.field_id;
        m = { key, name: board, incarnation: view.incarnation, lifecycle_generation: view.lifecycle_generation, owner: control.owner, value: "", version: 0, revision: view.revision, controlId: control.id, row, label, input, feedback, send, review, discard, state: "editing", pending: false, submission: null, composing: false };
        const state = m;
        input.addEventListener("focus", () => { input.scrollIntoView({block:"nearest"}); });
        input.addEventListener("compositionstart", () => { state.composing = true; });
        input.addEventListener("compositionend", () => { state.composing = false; });
        input.addEventListener("input", () => { if (!state.pending && state.state !== "uncertain") { state.value = input.value; state.version++; state.submission = null; state.feedbackText = ""; renderMessage(state); } });
        input.addEventListener("keydown", e => {
          if (e.key !== "Enter" || e.shiftKey || e.ctrlKey || e.altKey || e.metaKey || e.isComposing || state.composing || e.keyCode === 229) return;
          e.preventDefault(); if (!e.repeat) void sendMessage(state);
        });
        send.addEventListener("click", () => { void sendMessage(state); });
        review.addEventListener("click", () => { void reviewMessage(state); });
        discard.addEventListener("click", () => discardMessage(state));
        messageDrafts.set(key, m); $("message-controls").append(row);
      }
      m.present = true; m.control = control; text(m.label, messageLabel(control)); m.currentRevision = view.revision;
      if (m.revision !== view.revision) {
        if (m.value || m.submission || m.pending) m.stale = true;
        else { m.revision = view.revision; m.controlId = control.id; m.stale = false; }
      }
    }
    for (const [key, m] of messageDrafts) {
      if (m.name === board && !m.present) {
        if (!m.value && !m.submission && !m.pending) { m.row.remove(); messageDrafts.delete(key); continue; }
        m.stale = true;
      }
      if (!writable(m)) m.stale = true;
      renderMessage(m);
    }
    $("board-messages").hidden = !Array.from(messageDrafts.values()).some(m => m.name === board);
  }
  function inlineControl(id) {
    const control = shown.controls.find(c => c.id === id && ["checkbox", "text"].includes(c.kind));
    if (!control) throw new Error("Invalid control reference");
    const row = node(control.kind === "checkbox" ? "span" : "div", undefined, "board-control board-inline-response");
    row.control = control; row.dataset.controlId = id;
    if (control.field_id) row.dataset.fieldId = control.field_id;
    const pinned = capture(), revision = shown.revision;
    const live = () => current(pinned) && writable(shown) && active && row.isConnected && shown?.revision === revision && shown.controls.includes(control);
    const input = node(control.kind === "checkbox" ? "input" : "textarea");
    input.setAttribute("aria-label", `${control.label || "Response"} (line ${control.line})`);
    if (control.kind === "checkbox") {
      input.type = "checkbox"; input.checked = control.checked;
      input.addEventListener("change", () => {
        if (!live() || draft || saving) { input.checked = control.checked; return; }
        draft = makeDraft(control, input.checked); renderEditor(); void saveDraft();
      });
    } else {
      input.className = "board-inline-text"; input.value = control.value; input.rows = 5; input.maxLength = 65536;
      input.addEventListener("focus", () => {
        if (!live() || saving || draft) return;
        draft = makeDraft(control, input.value); controls(shown); $("response-text").focus();
      });
      row.append(node("label", control.label || "Response", "response-name"));
    }
    row.append(input);
    const feedback = node("span", savedControl === id ? "Saved response" : "", "inline-response-feedback");
    feedback.id = `feedback-${id}`; feedback.setAttribute("role", "status");
    input.setAttribute("aria-describedby", feedback.id); row.append(feedback);
    if (control.kind === "text") {
      const actions = node("div", undefined, "response-actions passive-actions");
      for (const label of ["Save", "Cancel"]) {
        const button = node("button", label); button.type = "button";
        button.addEventListener("click", () => {
          if (!live() || draft || saving) return;
          if (label === "Cancel") { input.value = control.value; return; }
          draft = makeDraft(control, input.value); renderEditor(); void saveDraft();
        });
        actions.append(button);
      }
      row.append(actions);
    }
    controlRows.set(id, row);
    return row;
  }
  function controls(view) {
    syncMessages(view);
    for (const row of controlRows.values()) {
      const input = row.querySelector("input, textarea.board-inline-text");
      if (!input) continue;
      input.disabled = !writable(shown) || saving || (!!draft && draft.id !== row.control.id);
      for (const button of row.querySelectorAll(".passive-actions button")) button.disabled = !writable(shown) || saving || !!draft;
      if (row.control.kind === "checkbox") input.checked = draft?.id === row.control.id ? draft.value : row.control.checked;
    }
    renderEditor(); lifecycleUI();
  }
  function show(view) { receiveMessageLabels(view); shown = view; $("board-controls").append($("response-editor")); controlRows.clear(); renderDocument(view); controls(view); $("board-newer").hidden = !latest; }
  function makeDraft(control, value) {
    return { name: board, incarnation: shown.incarnation, lifecycle_generation: shown.lifecycle_generation, id: control.id, kind: control.kind, fieldId: control.field_id, key: control.rebase_key, revision: shown.revision, original: control.kind === "text" ? control.value : control.checked, value, state: "editing", reviewed: false, label: control.label };
  }
  function rebaseCandidate() {
    if (!draft || draft.interrupted || !writable(draft) || draft.kind !== "text" || !latest || draft.reviewed !== latest.revision || shown?.revision !== latest.revision) return null;
    const found = latest.controls.filter(c => c.kind === "text" && c.field_id === draft.fieldId && c.rebase_key === draft.key);
    return found.length === 1 ? found[0] : null;
  }
  function renderEditor() {
    const row = draft && controlRows.get(draft.id);
    const host = row?.isConnected && !raw ? row : $("board-controls");
    if ($("response-editor").parentNode !== host) host.append($("response-editor"));
    document.querySelector(".board-responses").hidden = !draft || host !== $("board-controls");
    for (const entry of controlRows.values()) {
      const passive = entry.querySelector("textarea.board-inline-text");
      if (passive) passive.hidden = !!draft && entry === row && !raw;
      const actions = entry.querySelector(".passive-actions");
      if (actions) actions.hidden = !!draft && entry === row && !raw;
    }
    $("response-editor").hidden = !draft;
    if (!draft) { $("board-controls").append($("response-editor")); $("response-text").removeAttribute("aria-label"); $("response-text").value = ""; $("response-feedback").textContent = ""; return; }
    $("response-heading").textContent = draft.label || "Edit response";
    $("response-text").setAttribute("aria-label", draft.label || "Response");
    $("response-text").setAttribute("aria-describedby", "response-feedback");
    $("response-text").hidden = draft.kind !== "text"; $("response-label").hidden = draft.kind !== "text";
    if (draft.kind === "text" && $("response-text").value !== draft.value) $("response-text").value = draft.value;
    $("response-text").disabled = saving;
    $("response-intent").hidden = draft.kind !== "checkbox"; $("response-intent").textContent = draft.value ? "Requested state: checked" : "Requested state: unchecked";
    $("response-save").disabled = !writable(draft) || draft.interrupted || saving || draft.state !== "editing";
    $("response-cancel").disabled = saving;
    $("response-review").hidden = draft.state === "editing"; $("response-review").disabled = saving;
    $("response-rebase").hidden = !rebaseCandidate(); $("response-rebase").disabled = saving;
    let feedback = saving ? "Saving response…" : "Edit the response, then Save or Cancel.";
    if (!saving && draft.state !== "editing") feedback = draft.state === "uncertain" ? "The save may have applied. Do not repeat it yet. Review the current board first." : "The board changed. Your response is kept. Review the current board before continuing.";
    if (!saving && draft.reviewed) feedback += rebaseCandidate() ? " This exact text field is unchanged; you may explicitly rebase, review and save." : " The field changed, moved or is no longer editable. Cancel this response, then reopen the intended control after review.";
    $("response-feedback").textContent = draft.error || feedback;
  }
  async function readSnapshot(force = false) {
    if (!active || !authenticated() || lifecycleOperation?.pending) return null;
    if (reading) { if (!force) { refreshRequested = true; return null; } stopRead(); }
    const c = capture(), id = ++sequence; readAbort = new AbortController(); reading = true;
    const revision = latest?.revision || shown?.revision;
    const conditional = !force && revision ? `?if_revision=${encodeURIComponent(revision)}` : "";
    try {
      const result = await request(`/api/boards/${encodeURIComponent(c.name)}${conditional}`, { signal: readAbort.signal });
      if (!authorityCurrent(c) || id !== sequence || !active) return null;
      if (result.name !== c.name) throw new Error("Mismatched board snapshot");
      observeAuthority(result);
      receiveMessageLabels(result);
      backoff = 1000; error("");
      if (result.unchanged) { receiveTimestamp(result); status(latest && latest.revision !== shown?.revision ? "Live · newer content is available" : draft || selectionHeld() ? "Live · keeping your response or text selection" : "Live · up to date"); return latest || shown; }
      if (!validView(result, c.name)) throw new Error("Invalid board snapshot");
      if (result.revision === shown?.revision) {
        receiveTimestamp(result);
        // An external edit may have returned byte-for-byte to the visible version.
        // Do not apply a superseded intermediate snapshot after selection ends.
        if (latest && latest.revision !== result.revision) {
          latest = null; $("board-newer").hidden = true;
          if (draft && draft.reviewed && draft.reviewed !== result.revision) { draft.reviewed = false; renderEditor(); }
        }
        status(draft || selectionHeld() ? "Live · keeping your response or text selection" : "Live · up to date"); return shown;
      }
      if (result.revision === latest?.revision) { receiveTimestamp(result); status("Live · newer content is available"); return latest; }
      if (shown && (draft || saving || selectionHeld())) {
        latest = result;
        if (draft && draft.reviewed !== result.revision) { draft.reviewed = false; renderEditor(); }
        $("board-newer").hidden = false; status("Live · newer content is available");
      }
      else { latest = null; show(result); status("Live · up to date"); }
      return result;
    } catch (e) {
      if (authorityCurrent(c) && id === sequence && e.name !== "AbortError") { if (e.status === 404) unavailable(); error("The board could not be refreshed. Your last content is kept; reconnecting."); status("Stale · reconnecting"); reconnect(); }
      return null;
    } finally { if (current(c) && id === sequence) { reading = false; readAbort = null; flushRefresh(); } }
  }
  async function refresh() {
    if (!active || !authenticated()) return;
    if (lifecycleOperation?.pending) return;
    if (latest && !draft && !selectionHeld()) { const next = latest; latest = null; show(next); }
    await readSnapshot();
  }
  async function review() {
    const c = capture(), originalDraft = draft;
    const view = await readSnapshot(true);
    if (!current(c) || draft !== originalDraft || !view) return;
    window.getSelection()?.removeAllRanges();
    if (draft) { latest = view; draft.reviewed = view.revision; show(view); renderEditor(); }
    else { latest = null; show(view); status("Live · latest content shown"); }
  }
  async function saveDraft() {
    if (!draft || !writable(draft) || draft.interrupted || saving || draft.state !== "editing" || !shown || !active) return false;
    const d = draft, c = capture(); d.error = ""; stopRead(); saving = true; controls(shown); renderEditor(); error("");
    try {
      const result = await request(`/api/boards/${encodeURIComponent(d.name)}/responses`, { method: "POST", body: JSON.stringify({ board: d.name, expected_revision: d.revision, control_id: d.id, value: d.kind === "text" ? { type: "text", text: d.value } : { type: "checkbox", checked: d.value } }) });
      if (!current(c) || draft !== d || !active) return false;
      if (!result.applied || !validView(result.board, d.name)) throw new Error("Unconfirmed save result");
      if (!authorityCurrent(c) || d.interrupted) { d.state = "conflict"; d.interrupted = true; d.error = "The save was accepted before the lifecycle changed. Your interrupted draft is kept for review; it cannot write to the new generation."; return false; }
      observeAuthority(result.board);
      savedControl = result.saved_control_id; draft = null; latest = null; show(result.board); renderEditor(); status("Saved response"); return true;
    } catch (e) {
      if (!current(c) || draft !== d || !active) return false;
      if (e.status === 409 && validView(e.data?.board, d.name)) { if (authorityCurrent(c)) { observeAuthority(e.data.board); latest = e.data.board; } d.state = "conflict"; }
      else if (e.status && e.status < 500 && e.status !== 429 && !e.data?.uncertain) { d.error = e.data?.error || "This response could not be saved. Review it before trying again."; error(d.error); }
      else { d.state = "uncertain"; latest = null; }
      d.reviewed = false; renderEditor(); return false;
    } finally { if (current(c) && draft === d || current(c) && !draft) { saving = false; if (shown) controls(shown); renderEditor(); flushRefresh(); } }
  }
  function discard() { draft = null; saving = false; renderEditor(); if (latest) { const view = latest; latest = null; show(view); } else if (shown) controls(shown); }
  async function allowLeave(clearsPrivate = false) {
    if (lifecycleOperation?.pending) { status("Wait for the board lifecycle operation to finish."); return false; }
    if (lifecycleOperation && !clearsPrivate) { status("Inspect the originating board to resolve the lifecycle outcome before leaving."); return false; }
    if (lifecycleOperation && clearsPrivate && !dirty() && !messageDirty()) {
      if (leaveChoice) return false;
      $("unsaved-save").disabled = true; text($("unsaved-save"), "Save and continue");
      text($("unsaved-title"), "Discard unresolved board action?");
      text($("unsaved-description"), "The action may already have applied. Locking forgets its local status and cannot undo it. Discard and continue, or cancel and inspect the board.");
      $("unsaved-dialog").showModal(); $("unsaved-cancel").focus();
      return new Promise(resolve => { leaveChoice = resolve; });
    }
    if (messagePending()) { status("Sending message. Wait for its outcome before leaving."); return false; }
    if (saving) { status("Saving response. Wait for its outcome before leaving."); return false; }
    if (!dirty() && !messageDirty()) { discard(); return true; }
    if (leaveChoice) return false;
    const messagesOnly = !dirty() && messageDirty();
    $("unsaved-save").disabled = (clearsPrivate && messageDirty()) || (!messagesOnly && draft.state !== "editing");
    text($("unsaved-save"), messagesOnly ? "Keep drafts and continue" : "Save and continue");
    text($("unsaved-title"), messagesOnly ? "Keep your message drafts?" : "Keep your response?");
    text($("unsaved-description"), messagesOnly ? (clearsPrivate ? "Locking clears private drafts. Discard drafts or cancel to stay. An uncertain submission may already have been accepted; inspect history." : "Keep drafts in this tab, discard them, or cancel. Continuing never sends a message. An uncertain submission may already have been accepted; inspect history.") : "Save the response before leaving, discard it, or cancel to keep editing.");
    const dialog = $("unsaved-dialog"); dialog.showModal(); $("unsaved-cancel").focus();
    return new Promise(resolve => { leaveChoice = resolve; });
  }
  function finishChoice(allowed) { const resolve = leaveChoice; leaveChoice = null; if ($("unsaved-dialog").open) $("unsaved-dialog").close(); resolve?.(allowed); }
  async function choose(name) {
    if (!validName(name) || name === board) return;
    const c = capture();
    if (!await allowLeave() || !current(c)) return;
    stop(); renderTimestamp(null); copySequence++; text($("board-copy-feedback"), ""); boardEpoch++; authorityEpoch++; authority = null; messageLabels = {}; board = name; shown = null; latest = null; draft = null; backoff = 1000;
    for (const m of messageDrafts.values()) m.row.hidden = true;
    $("board-messages").hidden = true;
    $("selected-board-name").textContent = board; $("board-selector").value = board;
    $("board-controls").append($("response-editor")); $("board-document").replaceChildren(); $("board-raw").textContent = ""; controlRows.clear(); renderEditor(); error(""); status("Loading board…");
    const selected = capture();
    await refresh();
    if (active && current(selected)) void startLive();
  }
  async function open() {
    if (active) return;
    const c = capture(); if (active && !await allowLeave() || !current(c)) return;
    active = true; onViewChange("boards"); status("Loading boards…");
    reconcileCatalog(catalogNames || []);
    catchUpCatalog();
    await refresh();
    if (current(c) && active) void startLive();
  }
  async function leave(action, clearsPrivate = false) { const c = capture(); if (await allowLeave(clearsPrivate) && current(c)) { active = false; stopCatalog(); stop(); boardEpoch++; action(); } }
  function clear() {
    active = false; authEpoch++; boardEpoch++; stopCatalog(); catalogNames = null; catalogDelay = 2000; catalogFeedback(""); stop(); renderTimestamp(null); copySequence++; text($("board-copy-feedback"), ""); finishChoice(false);
    messageLabels = {}; messageDrafts.clear(); $("message-controls").replaceChildren(); $("board-messages").hidden = true;
    authorityEpoch++; authority = null; lifecycleOperation?.controller?.abort(); lifecycleOperation = null; archiveIntent = null; closeArchives();
    if ($("archive-confirm-dialog").open) $("archive-confirm-dialog").close();
    $("archive-confirm-description").textContent = ""; text($("board-lifecycle-status"), "");
    board = "main"; shown = null; latest = null; draft = null; saving = false; raw = false; backoff = 1000;
    $("board-controls").append($("response-editor")); $("board-document").replaceChildren(); $("board-raw").textContent = ""; controlRows.clear(); $("board-selector").replaceChildren();
    $("selected-board-name").textContent = "main"; $("response-heading").textContent = "Edit response"; $("response-intent").textContent = "";
    $("board-newer").hidden = true; $("board-raw-toggle").setAttribute("aria-pressed", "false"); renderEditor(); lifecycleUI(); text($("board-lifecycle-status"), ""); error(""); status("Choose Boards to load main.");
  }
  function closeArchives() {
    archivedEpoch++; archivedAbort?.abort(); archivedAbort = null;
    if ($("archived-dialog").open) $("archived-dialog").close();
    $("archived-list").replaceChildren(); archivedActionFeedback = ""; text($("archived-feedback"), "");
  }
  function archiveDate(value, field) {
    const time = node("time"); time.dataset.archiveDate = field;
    if (Number.isSafeInteger(value) && Math.abs(value) <= 8640000000000000) {
      time.dateTime = new Date(value).toISOString();
      time.textContent = new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "long" }).format(new Date(value));
    } else time.textContent = "Unknown";
    return time;
  }
  async function loadArchives() {
    if (!active || !authenticated() || !$("archived-dialog").open || lifecycleOperation?.pending) return;
    archivedAbort?.abort(); const controller = new AbortController(), epoch = ++archivedEpoch, auth = authEpoch;
    archivedAbort = controller; text($("archived-feedback"), "Loading archived boards…");
    const live = () => active && authenticated() && authEpoch === auth && epoch === archivedEpoch && $("archived-dialog").open;
    let deadline;
    try {
      const timeout = new Promise((_, reject) => { deadline = setTimeout(() => { controller.abort(); reject(new Error("Archive list timed out")); }, 5000); });
      const data = await Promise.race([request("/api/archived-boards", { signal: controller.signal }), timeout]);
      if (!live()) return;
      if (!data || !Array.isArray(data.boards) || data.boards.length > 256) throw new Error("Invalid archived board list");
      const names = new Set(), rows = [];
      for (const entry of data.boards) {
        if (!entry || !validName(entry.name) || entry.name === "main" || names.has(entry.name) || !/^[a-f0-9]{64}$/.test(entry.revision) || !["archived", "restoring", "purging"].includes(entry.state)) throw new Error("Invalid archived record");
        names.add(entry.name);
        const row = node("section", undefined, "archived-row"); row.dataset.boardName = entry.name;
        row.append(node("h3", entry.name));
        const archived = node("p", "Archived: "), expires = node("p", "Permanent deletion: ");
        archived.append(archiveDate(entry.archived_at, "archived_at")); expires.append(archiveDate(entry.expires_at, "expires_at"));
        const feedback = node("p", entry.state === "purging" ? "Expired. Cleanup is pending; Restore is unavailable." : entry.state === "restoring" ? "Restore recovery is pending; inspect its current state." : "Read only until explicitly restored.", "help");
        const actions = node("div", undefined, "response-actions"), restore = node("button", "Restore"), view = node("button", "View archived board");
        restore.type = view.type = "button"; restore.dataset.archiveAction = "restore";
        restore.dataset.unavailable = String(entry.state !== "archived");
        const rowEpoch = epoch;
        restore.addEventListener("click", () => { if (rowEpoch === archivedEpoch && row.isConnected && entry.state === "archived") void lifecycleAction("restore", entry); });
        view.addEventListener("click", async () => { if (rowEpoch !== archivedEpoch || !row.isConnected || lifecycleOperation) return; closeArchives(); await choose(entry.name); });
        actions.append(restore, view); row.append(archived, expires, feedback, actions); rows.push(row);
      }
      $("archived-list").replaceChildren(...rows);
      text($("archived-feedback"), archivedActionFeedback || (rows.length ? "" : "No archived boards.")); lifecycleUI();
    } catch (e) { if (live() && e.name !== "AbortError") text($("archived-feedback"), "Archived boards unavailable. Refresh to try again; no operation was retried."); }
    finally { clearTimeout(deadline); if (live()) archivedAbort = null; }
  }
  async function prepareArchive() {
    if (board === "main" || !shown || !writable(shown) || lifecycleOperation || archiveIntent) return;
    const c = capture();
    if (!await allowLeave() || !current(c) || !shown || !writable(shown)) return;
    archiveIntent = { name: board, revision: shown.revision, incarnation: shown.incarnation, lifecycle_generation: shown.lifecycle_generation };
    text($("archive-confirm-description"), `Archive ${board}? It will disappear from active lists. You can restore it for 30 days; after that the broker permanently deletes its managed content and metadata. External backups are unaffected.`);
    $("archive-confirm-dialog").showModal(); $("archive-confirm-cancel").focus();
  }
  async function lifecycleAction(action, target) {
    if (lifecycleOperation || !active || !authenticated() || saving || messagePending() || !validName(target.name)) return;
    if (action === "archive" && (target.name !== board || board === "main" || !writable(target) || target.revision !== shown?.revision)) return;
    // Fence pre-action selected snapshots AND buffered SSE before admitting an
    // action. Catalog polling has its independent epoch and remains untouched.
    stop(); authorityEpoch++; archivedEpoch++; archivedAbort?.abort(); archivedAbort = null;
    const operation = { action, name: target.name, revision: target.revision, scope: scope(target), pending: true, code: "pending" };
    lifecycleOperation = operation; archivedActionFeedback = ""; const c = capture(), controller = new AbortController(); operation.controller = controller;
    let deadline;
    if (shown) controls(shown); else lifecycleUI();
    try {
      const timeout = new Promise((_, reject) => { deadline = setTimeout(() => { controller.abort(); reject(new Error("Lifecycle outcome timed out")); }, 15000); });
      const result = await Promise.race([request(`/api/boards/${encodeURIComponent(target.name)}/${action}`, { signal: controller.signal, method: "POST", body: JSON.stringify({ board: target.name, expected_revision: target.revision }) }), timeout]);
      if (!current(c) || lifecycleOperation !== operation || !active) return;
      // A decoded success must describe this exact board and a new lifecycle
      // generation. Otherwise preserve the operation for explicit inspection.
      if (!result.applied || !validView(result.board, target.name)
          || typeof target.incarnation !== "string" || result.board.incarnation !== target.incarnation
          || typeof target.lifecycle_generation !== "string" || typeof result.board.lifecycle_generation !== "string"
          || !result.board.lifecycle_generation || result.board.lifecycle_generation === target.lifecycle_generation
          || phase(result.board) !== (action === "archive" ? "archived" : "active")) throw new Error("Unconfirmed lifecycle result");
      lifecycleOperation = null;
      if (target.name === board) {
        observeAuthority(result.board);
        if (draft || saving || selectionHeld()) { latest = result.board; $("board-newer").hidden = false; }
        else { latest = null; show(result.board); }
      }
      archivedActionFeedback = `${target.name}: ${action === "archive" ? "archived for 30 days" : "restored"}.`;
      status(action === "archive" ? "Board archived for 30 days." : "Board restored. Old drafts remain bound to their prior generation.");
    } catch (e) {
      if (!current(c) || lifecycleOperation !== operation || !active) return;
      if (validView(e.data?.board, target.name) && target.name === board) {
        observeAuthority(e.data.board); latest = e.data.board;
      }
      if (e.status && e.status < 500 && e.status !== 429) {
        lifecycleOperation = null;
        archivedActionFeedback = e.data?.error || "The lifecycle action was rejected. Inspect the current board before trying again.";
        error(archivedActionFeedback);
      } else {
        operation.pending = false; operation.code = e.data?.code || "uncertain";
        operation.feedback = e.data?.code === "pending" ? "Restore recovery is pending. Inspect current state; do not repeat the operation." : "The lifecycle outcome is uncertain. Inspect the originating board before any further operation; nothing is retried automatically.";
      }
    } finally {
      clearTimeout(deadline);
      if (current(c) && active) {
        if (lifecycleOperation === operation) operation.pending = false;
        if (shown) controls(shown); else lifecycleUI();
        catchUpCatalog();
        if ($("archived-dialog").open) void loadArchives();
        void startLive();
      }
    }
  }
  async function inspectLifecycle() {
    if (!active || !authenticated() || lifecycleOperation?.pending) return;
    const operation = lifecycleOperation, name = operation?.name || board, c = capture();
    try {
      const result = await request(`/api/boards/${encodeURIComponent(name)}`);
      if (!authorityCurrent(c) || lifecycleOperation !== operation || !validView(result, name)) return;
      if (phase(result) !== "restoring") lifecycleOperation = null;
      if (name === board) {
        observeAuthority(result);
        if (draft || selectionHeld()) { latest = result; $("board-newer").hidden = false; }
        else { latest = null; show(result); }
      }
      status(`Inspected ${name}: ${phase(result)}. No mutation was repeated.`);
      if (shown) controls(shown); else lifecycleUI();
      catchUpCatalog(); if ($("archived-dialog").open) void loadArchives();
    } catch (e) {
      if (!authorityCurrent(c) || lifecycleOperation !== operation) return;
      if (e.status === 404) { lifecycleOperation = null; if (name === board) unavailable(); }
      error("The originating board could not be inspected. Its prior content and drafts are kept."); lifecycleUI();
    }
  }
  $("board-archive").addEventListener("click", () => { void prepareArchive(); });
  $("archive-confirm-cancel").addEventListener("click", () => { archiveIntent = null; $("archive-confirm-dialog").close(); text($("archive-confirm-description"), ""); });
  $("archive-confirm-dialog").addEventListener("cancel", () => { archiveIntent = null; text($("archive-confirm-description"), ""); });
  $("archive-confirm-submit").addEventListener("click", () => {
    const intent = archiveIntent; if (!intent || lifecycleOperation) return;
    archiveIntent = null; $("archive-confirm-dialog").close(); text($("archive-confirm-description"), ""); void lifecycleAction("archive", intent);
  });
  $("board-archived-open").addEventListener("click", () => { if (!active || lifecycleOperation?.pending) return; if (!$("archived-dialog").open) $("archived-dialog").showModal(); void loadArchives(); });
  $("archived-refresh").addEventListener("click", () => { void loadArchives(); });
  $("archived-close").addEventListener("click", () => { if (!lifecycleOperation?.pending) closeArchives(); });
  $("archived-dialog").addEventListener("cancel", event => { if (lifecycleOperation?.pending) event.preventDefault(); else closeArchives(); });
  $("board-lifecycle-review").addEventListener("click", () => { void inspectLifecycle(); });
  // Keep an existing manual selection (and its frozen snapshot) during a
  // pointer copy; keyboard users can still Tab to and activate each button.
  $("board-document").addEventListener("mousedown", event => {
    if (event.button === 0 && event.target.closest("button.board-code-copy")) event.preventDefault();
  });
  $("board-document").addEventListener("click", async event => {
    const button = event.target.closest("button.board-code-copy");
    if (!button || !$("board-document").contains(button)) return;
    event.preventDefault(); event.stopPropagation();
    if (!active || !authenticated() || !shown) return;
    const adjacent = button.previousElementSibling;
    const code = adjacent?.tagName === "A" ? adjacent.querySelector("code") : adjacent;
    if (!code || !["CODE", "PRE"].includes(code.tagName)) return;
    const payload = code.textContent, c = capture(), revision = shown.revision, id = ++copySequence;
    copyAttempts.set(button, id);
    copyIcon(button, false);
    const live = () => current(c) && active && shown?.revision === revision && id === copySequence && button.isConnected;
    const feedback = (value, success = false) => {
      if (!live()) return;
      $("board-copy-feedback").classList.toggle("board-copy-success", success);
      text($("board-copy-feedback"), value);
    };
    feedback("");
    if (new TextEncoder().encode(payload).length > 65536) {
      feedback("Code exceeds the 64 KiB copy limit. Select a smaller portion manually."); return;
    }
    if (typeof navigator.clipboard?.writeText !== "function") {
      feedback("Clipboard unavailable. Select and copy the code manually."); return;
    }
    try {
      await navigator.clipboard.writeText(payload);
      if (!live()) return;
      feedback("Code copied.", true);
      copyIcon(button, true);
      // Reset this button independently of later copies elsewhere. A repeated
      // attempt on this button owns its own timeout and cannot be reset early.
      setTimeout(() => {
        if (button.isConnected && copyAttempts.get(button) === id) copyIcon(button, false);
      }, 1200);
    } catch {
      feedback("Clipboard permission denied or copy failed. Select and copy the code manually.");
    }
  });
  $("board-selector").addEventListener("change", e => { const name = e.target.value; e.target.value = board; void choose(name); });
  $("board-refresh").addEventListener("click", () => { catchUpCatalog(); if (draft || selectionHeld()) void review(); else void readSnapshot(true); });
  $("board-review-refresh").addEventListener("click", () => { void review(); });
  $("board-raw-toggle").addEventListener("click", () => { raw = !raw; $("board-raw-toggle").setAttribute("aria-pressed", String(raw)); $("board-document").hidden = raw; $("board-raw").hidden = !raw; renderEditor(); });
  $("response-text").addEventListener("input", e => { if (draft && !saving && draft.kind === "text") draft.value = e.target.value; });
  $("response-save").addEventListener("click", () => { void saveDraft(); });
  $("response-cancel").addEventListener("click", () => { if (!saving) { discard(); status("Response cancelled; nothing was saved by Cancel."); } });
  $("response-review").addEventListener("click", () => { void review(); });
  $("response-rebase").addEventListener("click", () => { const candidate = rebaseCandidate(); if (!candidate || !draft) return; draft.id = candidate.id; draft.revision = latest.revision; draft.original = candidate.value; draft.state = "editing"; draft.error = ""; draft.reviewed = false; latest = null; $("board-newer").hidden = true; renderEditor(); status("Rebased unchanged field. Review your response, then Save explicitly."); });
  $("unsaved-save").addEventListener("click", async () => { const resolve = leaveChoice; if (!resolve || $("unsaved-save").disabled) return; if (!dirty() && messageDirty()) { finishChoice(true); return; } if ($("unsaved-dialog").open) $("unsaved-dialog").close(); const saved = await saveDraft(); if (leaveChoice === resolve) finishChoice(saved); });
  $("unsaved-discard").addEventListener("click", () => { for (const m of Array.from(messageDrafts.values())) discardMessage(m); discard(); finishChoice(true); });
  $("unsaved-cancel").addEventListener("click", () => finishChoice(false));
  $("unsaved-dialog").addEventListener("cancel", e => { e.preventDefault(); finishChoice(false); });
  document.addEventListener("selectionchange", () => {
    if (active && latest && !draft && !saving && !selectionHeld()) { const view = latest; latest = null; show(view); status("Live · up to date"); }
  });
  document.addEventListener("visibilitychange", () => { if (document.visibilityState === "visible") catchUpCatalog(); });
  window.addEventListener("online", catchUpCatalog);
  window.addEventListener("focus", catchUpCatalog);
  window.addEventListener("beforeunload", e => { if (dirty() || saving || messageDirty() || messagePending() || lifecycleOperation) { e.preventDefault(); e.returnValue = ""; } });
  return { open, leave, clear, hasDraft: () => dirty() || messageDirty() || !!lifecycleOperation };
};
