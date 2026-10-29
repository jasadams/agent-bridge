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
  function renderNodes(nodes) {
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
    text($("selected-board-name"), view.name);
    text($("board-raw"), view.markdown);
    let document;
    try { document = renderNodes(view.nodes); }
    catch { document = window.document.createDocumentFragment(); document.append(node("pre", view.markdown, "board-render-fallback")); }
    if (!view.markdown) document.append(node("p", view.exists ? "This board is empty." : "main has not been written yet.", "help"));
    if (view.render_notice) document.prepend(node("p", view.render_notice, "help notice"));
    reconcile($("board-document"), document);
    $("board-document").hidden = raw; $("board-raw").hidden = !raw;
  }
  function controls(view) {
    document.querySelector(".board-responses").hidden = !view.controls.length && !draft;
    if (view.controls.length > 4096) { controlRows.clear(); $("board-controls").replaceChildren(node("p", "Too many response controls to display safely.", "error")); return; }
    const retained = new Set();
    let index = 0;
    for (const control of view.controls) {
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
    document.querySelector(".board-responses").hidden = !draft && !shown?.controls.length;
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
  async function allowLeave() {
    if (saving) { status("Saving response. Wait for its outcome before leaving."); return false; }
    if (!dirty()) { discard(); return true; }
    if (leaveChoice) return false;
    $("unsaved-save").disabled = draft.state !== "editing";
    const dialog = $("unsaved-dialog"); dialog.showModal(); $("unsaved-cancel").focus();
    return new Promise(resolve => { leaveChoice = resolve; });
  }
  function finishChoice(allowed) { const resolve = leaveChoice; leaveChoice = null; if ($("unsaved-dialog").open) $("unsaved-dialog").close(); resolve?.(allowed); }
  async function choose(name) {
    if (!validName(name) || name === board) return;
    const c = capture();
    if (!await allowLeave() || !current(c)) return;
    stop(); boardEpoch++; board = name; shown = null; latest = null; draft = null; backoff = 1000;
    $("selected-board-name").textContent = board; $("board-selector").value = board;
    $("board-document").replaceChildren(); $("board-raw").textContent = ""; $("board-controls").replaceChildren(); controlRows.clear(); renderEditor(); error(""); status("Loading board…");
    const selected = capture();
    await refresh();
    if (active && current(selected)) void startLive();
  }
  async function open() {
    if (active) return;
    const c = capture(); if (!await allowLeave() || !current(c)) return;
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
  async function leave(action) { const c = capture(); if (await allowLeave() && current(c)) { active = false; stop(); boardEpoch++; action(); } }
  function clear() {
    active = false; authEpoch++; boardEpoch++; stop(); finishChoice(false);
    board = "main"; shown = null; latest = null; draft = null; saving = false; raw = false; backoff = 1000;
    $("board-document").replaceChildren(); $("board-raw").textContent = ""; $("board-controls").replaceChildren(); controlRows.clear(); $("board-selector").replaceChildren();
    $("selected-board-name").textContent = "main"; $("response-heading").textContent = "Edit response"; $("response-intent").textContent = "";
    $("board-newer").hidden = true; $("board-raw-toggle").setAttribute("aria-pressed", "false"); renderEditor(); error(""); status("Choose Boards to load main.");
  }
  $("board-selector").addEventListener("change", e => { const name = e.target.value; e.target.value = board; void choose(name); });
  $("board-refresh").addEventListener("click", () => { if (draft || selectionHeld()) void review(); else void readSnapshot(true); });
  $("board-review-refresh").addEventListener("click", () => { void review(); });
  $("board-raw-toggle").addEventListener("click", () => { raw = !raw; $("board-raw-toggle").setAttribute("aria-pressed", String(raw)); $("board-document").hidden = raw; $("board-raw").hidden = !raw; });
  $("response-text").addEventListener("input", e => { if (draft && !saving && draft.kind === "text") draft.value = e.target.value; });
  $("response-save").addEventListener("click", () => { void saveDraft(); });
  $("response-cancel").addEventListener("click", () => { if (!saving) { discard(); status("Response cancelled; nothing was saved by Cancel."); } });
  $("response-review").addEventListener("click", () => { void review(); });
  $("response-rebase").addEventListener("click", () => { const candidate = rebaseCandidate(); if (!candidate || !draft) return; draft.id = candidate.id; draft.revision = latest.revision; draft.original = candidate.value; draft.state = "editing"; draft.reviewed = false; latest = null; $("board-newer").hidden = true; renderEditor(); status("Rebased unchanged field. Review your response, then Save explicitly."); });
  $("unsaved-save").addEventListener("click", async () => { const resolve = leaveChoice; if (!resolve) return; if ($("unsaved-dialog").open) $("unsaved-dialog").close(); const saved = await saveDraft(); if (leaveChoice === resolve) finishChoice(saved); });
  $("unsaved-discard").addEventListener("click", () => { discard(); finishChoice(true); });
  $("unsaved-cancel").addEventListener("click", () => finishChoice(false));
  $("unsaved-dialog").addEventListener("cancel", e => { e.preventDefault(); finishChoice(false); });
  document.addEventListener("selectionchange", () => {
    if (active && latest && !draft && !saving && !selectionHeld()) { const view = latest; latest = null; show(view); status("Live · up to date"); }
  });
  window.addEventListener("beforeunload", e => { if (dirty() || saving) { e.preventDefault(); e.returnValue = ""; } });
  return { open, leave, clear, hasDraft: dirty };
};
