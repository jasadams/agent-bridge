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
  let copySequence = 0;
  const node = (tag, text, className) => { const n = document.createElement(tag); if (text !== undefined) n.textContent = String(text); if (className) n.className = className; return n; };
  const text = (element, value) => { if (element.textContent !== value) element.textContent = value; };
  const status = value => { text($("board-status"), value); };
  const error = value => { text($("board-error"), value || ""); $("board-error").hidden = !value; };
  const capture = () => ({ auth: authEpoch, epoch: boardEpoch, name: board });
  const current = c => authenticated() && c.auth === authEpoch && c.epoch === boardEpoch && c.name === board;
  const validName = name => typeof name === "string" && /^[a-z0-9][a-z0-9_-]{0,63}$/.test(name);
  const validView = (view, name) => view && view.name === name && typeof view.markdown === "string" && typeof view.revision === "string" && Array.isArray(view.nodes) && Array.isArray(view.controls);
  const dirty = () => !!draft && (draft.kind === "checkbox" || draft.value !== draft.original || draft.state !== "editing");
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
          backoff = 1000;
          if (data.revision !== (latest?.revision || shown?.revision)) {
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
      if (existing.isEqualNode(desired)) continue;
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
    text($("board-raw"), view.markdown);
    let document;
    try { document = renderNodes(view.nodes, !view.render_notice); }
    catch { document = window.document.createDocumentFragment(); document.append(node("pre", typeof view.display_markdown === "string" ? view.display_markdown : "This document exceeds the rendering limit. Use Raw Markdown to inspect its exact source.", "board-render-fallback")); }
    if (!view.markdown) document.append(node("p", view.exists ? "This board is empty." : "main has not been written yet.", "help"));
    if (view.render_notice) document.prepend(node("p", view.render_notice, "help notice"));
    reconcile($("board-document"), document);
    $("board-document").hidden = raw; $("board-raw").hidden = !raw;
  }
  const messageDrafts = new Map();
  const messageDirty = () => Array.from(messageDrafts.values()).some(m => m.value || m.submission || m.pending);
  const messagePending = () => Array.from(messageDrafts.values()).some(m => m.pending);
  const messageIdentity = control => JSON.stringify([board, control.field_id, control.rebase_key]);
  function renderMessage(m) {
    m.row.hidden = m.name !== board;
    m.input.readOnly = m.pending || m.state === "uncertain";
    m.send.disabled = m.pending || (!m.submission && (!m.value.trim() || m.stale || !m.present));
    text(m.send, m.submission ? "Retry same message" : "Send");
    m.review.hidden = !m.stale || m.state === "uncertain";
    m.review.disabled = m.pending;
    m.discard.disabled = m.pending;
    m.discard.hidden = !m.value && !m.submission;
    const target = m.owner;
    text(m.target, `To ${target.name} · ${target.provider} · session ${target.session}${target.endpoint ? ` · ${target.endpoint}` : ""}`);
    text(m.feedback, m.pending ? "Sending to broker…" : m.feedbackText || (m.stale ? "The board or input changed. Your draft is kept. Review before sending." : "Enter sends · Shift+Enter adds a newline."));
    $("board-messages").hidden = !Array.from(messageDrafts.values()).some(d => d.name === board);
  }
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
    if (m.pending || m.state === "uncertain") return;
    const c = capture(), view = await readSnapshot(true);
    if (!current(c) || !active || !view || messageDrafts.get(m.key) !== m) return;
    const found = view.controls.filter(control => control.kind === "message" && messageIdentity(control) === m.key);
    if (found.length !== 1) { m.feedbackText = "This exact input is unavailable or changed. Your draft is kept; discard it only after inspecting the current target."; renderMessage(m); return; }
    m.submission = null; m.state = "editing"; m.revision = view.revision; m.controlId = found[0].id; m.control = found[0]; m.present = true; m.stale = false;
    m.feedbackText = "Current input reviewed. Check the target and press Enter to send."; renderMessage(m); m.input.focus();
  }
  async function sendMessage(m) {
    if (m.pending || !active || !authenticated() || m.name !== board || messageDrafts.get(m.key) !== m) return;
    if (!m.submission && (!m.value.trim() || m.stale || !m.present)) return;
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
      if (m.present && m.control) { m.revision = m.currentRevision; m.controlId = m.control.id; m.stale = false; }
    } catch (e) {
      if (!current(c) || !active || messageDrafts.get(m.key) !== m || m.submission !== submission) return;
      if (e.data?.accepted === false && e.data?.code !== "uncertain") {
        m.state = "rejected";
        if (e.status === 409) m.submission = null;
        m.feedbackText = e.data.error || "The broker rejected this message. Your draft is kept.";
        if (e.status === 409) m.stale = true;
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
        const row = node("section", undefined, "message-composer"), label = node("label", control.label), input = node("textarea"), target = node("p", "", "message-target help"), feedback = node("p", "", "message-feedback help"), actions = node("div", undefined, "response-actions"), send = node("button", "Send"), review = node("button", "Review current input"), discard = node("button", "Discard draft");
        input.id = "message-" + crypto.randomUUID(); input.rows = 3; input.maxLength = 65536; label.htmlFor = input.id;
        feedback.id = input.id + "-feedback"; feedback.setAttribute("role", "status"); feedback.setAttribute("aria-live", "polite"); input.setAttribute("aria-describedby", feedback.id);
        for (const button of [send, review, discard]) button.type = "button";
        actions.append(send, review, discard); row.append(label, target, input, feedback, actions); row.dataset.fieldId = control.field_id;
        m = { key, name: board, owner: control.owner, value: "", version: 0, revision: view.revision, controlId: control.id, row, input, target, feedback, send, review, discard, state: "editing", pending: false, submission: null, composing: false };
        const state = m;
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
      m.present = true; m.control = control; m.currentRevision = view.revision;
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
      renderMessage(m);
    }
    $("board-messages").hidden = !Array.from(messageDrafts.values()).some(m => m.name === board);
  }
  function controls(view) {
    syncMessages(view);
    const responseControls = view.controls.filter(control => control.kind !== "message");
    document.querySelector(".board-responses").hidden = !responseControls.length && !draft;
    if (view.controls.length > 4096) { controlRows.clear(); $("board-controls").replaceChildren(node("p", "Too many response controls to display safely.", "error")); return; }
    const retained = new Set();
    let index = 0;
    for (const control of responseControls) {
      if (!control || typeof control.id !== "string" || typeof control.label !== "string" || !["text", "checkbox"].includes(control.kind)) continue;
      if (control.kind === "checkbox" && typeof control.checked !== "boolean") continue;
      if (control.kind === "text" && (typeof control.value !== "string" || typeof control.rebase_key !== "string" || typeof control.field_id !== "string")) continue;
      const key = JSON.stringify([control.kind, control.field_id || control.line, control.label]);
      let row = controlRows.get(key);
      if (!row) {
        row = node("div", undefined, "board-control");
        if (control.kind === "checkbox") {
          const wrapper = node("label"), input = node("input"), caption = node("span"); input.type = "checkbox";
          input.addEventListener("change", () => { const target = row.control, checked = input.checked; draft = makeDraft(target, checked); renderEditor(); void saveDraft(); });
          wrapper.append(input, caption); row.append(wrapper);
        } else {
          const edit = node("button"); edit.type = "button";
          edit.addEventListener("click", () => { draft = makeDraft(row.control, row.control.value); renderEditor(); controls(shown); $("response-text").focus(); });
          row.append(node("p", "", "response-name"), node("p", "", "response-value"), edit);
        }
        controlRows.set(key, row);
      }
      row.control = control; row.dataset.controlId = control.id;
      const label = `${control.label || "Response"} (line ${Number(control.line)})`;
      if (control.kind === "checkbox") {
        const input = row.querySelector("input"); input.checked = draft?.kind === "checkbox" && draft.id === control.id ? draft.value : control.checked; input.disabled = saving || !!draft; input.setAttribute("aria-label", label);
        text(row.querySelector("span"), label);
      } else {
        text(row.querySelector(".response-name"), label); text(row.querySelector(".response-value"), control.value || "(No response)");
        const edit = row.querySelector("button"); text(edit, `Respond: ${control.label || "Text field"}`); edit.disabled = saving || !!draft;
      }
      const existing = $("board-controls").childNodes[index++];
      if (existing !== row) $("board-controls").insertBefore(row, existing || null);
      retained.add(key);
    }
    for (const [key, row] of controlRows) if (!retained.has(key)) { row.remove(); controlRows.delete(key); }
    if (!retained.size) {
      if (!$("board-controls").querySelector(".help")) $("board-controls").replaceChildren(node("p", "This board has no editable response fields.", "help"));
    } else {
      for (const child of Array.from($("board-controls").children)) if (!child.classList.contains("board-control")) child.remove();
    }
  }
  function show(view) { shown = view; renderDocument(view); controls(view); $("board-newer").hidden = !latest; }
  function makeDraft(control, value) {
    return { name: board, id: control.id, kind: control.kind, fieldId: control.field_id, key: control.rebase_key, revision: shown.revision, original: control.kind === "text" ? control.value : control.checked, value, state: "editing", reviewed: false, label: control.label };
  }
  function rebaseCandidate() {
    if (!draft || draft.kind !== "text" || !latest || draft.reviewed !== latest.revision || shown?.revision !== latest.revision) return null;
    const found = latest.controls.filter(c => c.kind === "text" && c.field_id === draft.fieldId && c.rebase_key === draft.key);
    return found.length === 1 ? found[0] : null;
  }
  function renderEditor() {
    document.querySelector(".board-responses").hidden = !draft && !shown?.controls.some(control => control.kind !== "message");
    $("response-editor").hidden = !draft;
    if (!draft) { $("response-text").value = ""; $("response-feedback").textContent = ""; return; }
    $("response-heading").textContent = draft.label || "Edit response";
    $("response-text").hidden = draft.kind !== "text"; $("response-label").hidden = draft.kind !== "text";
    if (draft.kind === "text" && $("response-text").value !== draft.value) $("response-text").value = draft.value;
    $("response-text").disabled = saving;
    $("response-intent").hidden = draft.kind !== "checkbox"; $("response-intent").textContent = draft.value ? "Requested state: checked" : "Requested state: unchecked";
    $("response-save").disabled = saving || draft.state !== "editing";
    $("response-cancel").disabled = saving;
    $("response-review").hidden = draft.state === "editing"; $("response-review").disabled = saving;
    $("response-rebase").hidden = !rebaseCandidate(); $("response-rebase").disabled = saving;
    let feedback = saving ? "Saving response…" : "Edit the response, then Save or Cancel.";
    if (!saving && draft.state !== "editing") feedback = draft.state === "uncertain" ? "The save may have applied. Do not repeat it yet. Review the current board first." : "The board changed. Your response is kept. Review the current board before continuing.";
    if (!saving && draft.reviewed) feedback += rebaseCandidate() ? " This exact text field is unchanged; you may explicitly rebase, review and save." : " The field changed, moved or is no longer editable. Cancel this response, then reopen the intended control after review.";
    $("response-feedback").textContent = feedback;
  }
  async function readSnapshot(force = false) {
    if (!active || !authenticated() || saving) return null;
    if (reading) { if (!force) { refreshRequested = true; return null; } stopRead(); }
    const c = capture(), id = ++sequence; readAbort = new AbortController(); reading = true;
    const revision = latest?.revision || shown?.revision;
    const conditional = !force && revision ? `?if_revision=${encodeURIComponent(revision)}` : "";
    try {
      const result = await request(`/api/boards/${encodeURIComponent(c.name)}${conditional}`, { signal: readAbort.signal });
      if (!current(c) || id !== sequence || !active) return null;
      if (result.name !== c.name) throw new Error("Mismatched board snapshot");
      backoff = 1000; error("");
      if (result.unchanged) { status(latest && latest.revision !== shown?.revision ? "Live · newer content is available" : draft || selectionHeld() ? "Live · keeping your response or text selection" : "Live · up to date"); return latest || shown; }
      if (!validView(result, c.name)) throw new Error("Invalid board snapshot");
      if (result.revision === shown?.revision) {
        // An external edit may have returned byte-for-byte to the visible version.
        // Do not apply a superseded intermediate snapshot after selection ends.
        if (latest && latest.revision !== result.revision) {
          latest = null; $("board-newer").hidden = true;
          if (draft && draft.reviewed && draft.reviewed !== result.revision) { draft.reviewed = false; renderEditor(); }
        }
        status(draft || selectionHeld() ? "Live · keeping your response or text selection" : "Live · up to date"); return shown;
      }
      if (result.revision === latest?.revision) { status("Live · newer content is available"); return latest; }
      if (shown && (draft || selectionHeld())) {
        latest = result;
        if (draft && draft.reviewed !== result.revision) { draft.reviewed = false; renderEditor(); }
        $("board-newer").hidden = false; status("Live · newer content is available");
      }
      else { latest = null; show(result); status("Live · up to date"); }
      return result;
    } catch (e) {
      if (current(c) && id === sequence && e.name !== "AbortError") { error("The board could not be refreshed. Your last content is kept; reconnecting."); status("Stale · reconnecting"); reconnect(); }
      return null;
    } finally { if (current(c) && id === sequence) { reading = false; readAbort = null; flushRefresh(); } }
  }
  async function refresh() {
    if (!active || !authenticated()) return;
    if (saving) { refreshRequested = true; return; }
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
    if (!draft || saving || draft.state !== "editing" || !shown || !active) return false;
    const d = draft, c = capture(); stopRead(); saving = true; controls(shown); renderEditor(); error("");
    try {
      const result = await request(`/api/boards/${encodeURIComponent(d.name)}/responses`, { method: "POST", body: JSON.stringify({ board: d.name, expected_revision: d.revision, control_id: d.id, value: d.kind === "text" ? { type: "text", text: d.value } : { type: "checkbox", checked: d.value } }) });
      if (!current(c) || draft !== d || !active) return false;
      if (!result.applied || !validView(result.board, d.name)) throw new Error("Unconfirmed save result");
      draft = null; latest = null; show(result.board); renderEditor(); status("Saved response"); return true;
    } catch (e) {
      if (!current(c) || draft !== d || !active) return false;
      if (e.status === 409 && validView(e.data?.board, d.name)) { latest = e.data.board; d.state = "conflict"; }
      else if (e.status && e.status < 500 && e.status !== 429 && !e.data?.uncertain) { error(e.data?.error || "This response could not be saved. Review it before trying again."); }
      else { d.state = "uncertain"; latest = null; }
      d.reviewed = false; renderEditor(); return false;
    } finally { if (current(c) && draft === d || current(c) && !draft) { saving = false; if (shown) controls(shown); renderEditor(); flushRefresh(); } }
  }
  function discard() { draft = null; saving = false; renderEditor(); if (latest) { const view = latest; latest = null; show(view); } else if (shown) controls(shown); }
  async function allowLeave(clearsPrivate = false) {
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
    stop(); copySequence++; text($("board-copy-feedback"), ""); boardEpoch++; board = name; shown = null; latest = null; draft = null; backoff = 1000;
    for (const m of messageDrafts.values()) m.row.hidden = true;
    $("board-messages").hidden = true;
    $("selected-board-name").textContent = board; $("board-selector").value = board;
    $("board-document").replaceChildren(); $("board-raw").textContent = ""; $("board-controls").replaceChildren(); controlRows.clear(); renderEditor(); error(""); status("Loading board…");
    const selected = capture();
    await refresh();
    if (active && current(selected)) void startLive();
  }
  async function open() {
    if (active) return;
    const c = capture(); if (active && !await allowLeave() || !current(c)) return;
    active = true; onViewChange("boards"); status("Loading boards…");
    try {
      const data = await request("/api/boards");
      if (!current(c) || !active) return;
      if (!Array.isArray(data.boards)) throw new Error("Invalid board list");
      const names = data.boards.filter(row => validName(row.name)).map(row => row.name).sort();
      $("board-selector").replaceChildren(...names.map(name => { const option = node("option", name); option.value = name; return option; }));
      $("board-selector").value = board;
      await refresh();
      if (current(c) && active) void startLive();
    } catch (e) { if (current(c) && active && e.name !== "AbortError") { error("The board list could not be loaded. Refresh or return to Messages and try again."); status("Stale · reconnecting"); } }
  }
  async function leave(action, clearsPrivate = false) { const c = capture(); if (await allowLeave(clearsPrivate) && current(c)) { active = false; stop(); boardEpoch++; action(); } }
  function clear() {
    active = false; authEpoch++; boardEpoch++; stop(); copySequence++; text($("board-copy-feedback"), ""); finishChoice(false);
    messageDrafts.clear(); $("message-controls").replaceChildren(); $("board-messages").hidden = true;
    board = "main"; shown = null; latest = null; draft = null; saving = false; raw = false; backoff = 1000;
    $("board-document").replaceChildren(); $("board-raw").textContent = ""; $("board-controls").replaceChildren(); controlRows.clear(); $("board-selector").replaceChildren();
    $("selected-board-name").textContent = "main"; $("response-heading").textContent = "Edit response"; $("response-intent").textContent = "";
    $("board-newer").hidden = true; $("board-raw-toggle").setAttribute("aria-pressed", "false"); renderEditor(); error(""); status("Choose Boards to load main.");
  }
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
    const live = () => current(c) && active && shown?.revision === revision && id === copySequence && button.isConnected;
    const feedback = value => { if (live()) text($("board-copy-feedback"), value); };
    if (new TextEncoder().encode(payload).length > 65536) {
      feedback("Code exceeds the 64 KiB copy limit. Select a smaller portion manually."); return;
    }
    if (typeof navigator.clipboard?.writeText !== "function") {
      feedback("Clipboard unavailable. Select and copy the code manually."); return;
    }
    try {
      await navigator.clipboard.writeText(payload);
      feedback("Code copied.");
    } catch {
      feedback("Clipboard permission denied or copy failed. Select and copy the code manually.");
    }
  });
  $("board-selector").addEventListener("change", e => { const name = e.target.value; e.target.value = board; void choose(name); });
  $("board-refresh").addEventListener("click", () => { if (draft || selectionHeld()) void review(); else void readSnapshot(true); });
  $("board-review-refresh").addEventListener("click", () => { void review(); });
  $("board-raw-toggle").addEventListener("click", () => { raw = !raw; $("board-raw-toggle").setAttribute("aria-pressed", String(raw)); $("board-document").hidden = raw; $("board-raw").hidden = !raw; });
  $("response-text").addEventListener("input", e => { if (draft && !saving && draft.kind === "text") draft.value = e.target.value; });
  $("response-save").addEventListener("click", () => { void saveDraft(); });
  $("response-cancel").addEventListener("click", () => { if (!saving) { discard(); status("Response cancelled; nothing was saved by Cancel."); } });
  $("response-review").addEventListener("click", () => { void review(); });
  $("response-rebase").addEventListener("click", () => { const candidate = rebaseCandidate(); if (!candidate || !draft) return; draft.id = candidate.id; draft.revision = latest.revision; draft.original = candidate.value; draft.state = "editing"; draft.reviewed = false; latest = null; $("board-newer").hidden = true; renderEditor(); status("Rebased unchanged field. Review your response, then Save explicitly."); });
  $("unsaved-save").addEventListener("click", async () => { const resolve = leaveChoice; if (!resolve || $("unsaved-save").disabled) return; if (!dirty() && messageDirty()) { finishChoice(true); return; } if ($("unsaved-dialog").open) $("unsaved-dialog").close(); const saved = await saveDraft(); if (leaveChoice === resolve) finishChoice(saved); });
  $("unsaved-discard").addEventListener("click", () => { for (const m of Array.from(messageDrafts.values())) discardMessage(m); discard(); finishChoice(true); });
  $("unsaved-cancel").addEventListener("click", () => finishChoice(false));
  $("unsaved-dialog").addEventListener("cancel", e => { e.preventDefault(); finishChoice(false); });
  document.addEventListener("selectionchange", () => {
    if (active && latest && !draft && !saving && !selectionHeld()) { const view = latest; latest = null; show(view); status("Live · up to date"); }
  });
  window.addEventListener("beforeunload", e => { if (dirty() || saving || messageDirty() || messagePending()) { e.preventDefault(); e.returnValue = ""; } });
  return { open, leave, clear, hasDraft: () => dirty() || messageDirty() };
};
