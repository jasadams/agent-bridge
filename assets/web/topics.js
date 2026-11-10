"use strict";
// Topics: a Slack-style channel timeline. The sidebar list lives in app.js; this module keeps the
// topic catalog fresh (for unread badges) and, while a topic is open, its posts and composer.
// Publication, retry and discard semantics are unchanged from the previous interface.
window.createTopicMonitor = ({ request, onCatalog, openPanel, closePanel }) => {
  const $ = id => document.getElementById(id);
  const { el, colorClass, initials, hhmm, dayKey, dayLabel, fullTime } = window.AB;
  const drafts = new Map();
  const seen = new Map();
  let running = false, viewing = false, epoch = 0, timer = null, selected = null, topics = [], agents = [];
  let next = null, previous = null, hasOlder = false, posts = new Map(), busy = null, loadedOnce = false;
  let refreshing = null, olderRequest = null, retryDelay = 5000, catalogSignature = "", renderedSignature = "", renderedIds = [];
  let mentionIndex = 0, mentionMatches = [];
  const draft = () => { if (!drafts.has(selected)) drafts.set(selected, { body: "", parent: null, pending: null, feedback: "" }); return drafts.get(selected); };
  const action = payload => request("/api/topic-actions", { method: "POST", body: JSON.stringify(payload) });
  const status = text => { $("topic-status").textContent = text; $("topic-status").hidden = !text; };
  const topicOf = id => topics.find(topic => topic.reference.topic_id === id);
  const escapeRegExp = value => value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  // Topics are local; the human publishes as the reserved inbox identity "inbox-user".
  const isHuman = post => post.author.agent_id === "inbox-user";
  const plainId = id => id.replace(/^inbox-/, "");
  const authorName = post => isHuman(post) ? "You" : (post.author_display_name && post.author_display_name !== post.author.agent_id ? post.author_display_name : plainId(post.author.agent_id));

  function unread(topic) {
    const id = topic.reference.topic_id;
    if (!seen.has(id)) seen.set(id, topic.high_water);
    if (viewing && id === selected) return 0;
    return Math.max(0, topic.high_water - seen.get(id));
  }
  function publishCatalog() {
    onCatalog(topics.map(topic => ({ id: topic.reference.topic_id, name: topic.name, archived: topic.archived, unread: unread(topic) })));
  }

  // ---------- header and composer state ----------
  function renderHeader() {
    const topic = topicOf(selected);
    $("topic-heading").textContent = topic ? `# ${topic.name}` : (selected ? "Topic unavailable" : "Choose a topic");
    $("topic-id").textContent = selected || "";
    $("topic-rename").disabled = !topic;
    $("topic-archive").disabled = !topic;
    $("topic-archive").textContent = topic?.archived ? "Restore" : "Archive";
    const d = selected ? draft() : null;
    $("topic-send").disabled = !topic || (topic.archived && !d?.pending) || !!busy;
    $("topic-body").disabled = !topic || topic.archived || !!busy || !!d?.pending;
    $("topic-clear-reply").disabled = !!d?.pending || !!busy;
    for (const button of $("topic-posts").querySelectorAll("button.js-reply")) button.disabled = !topic || topic.archived || !!busy || !!d?.pending;
    $("topic-older").hidden = !hasOlder;
    if (topic?.archived) status("Archived · read only");
  }
  function renderDraft() {
    if (!selected) return;
    const d = draft();
    if ($("topic-body").value !== d.body) { $("topic-body").value = d.body; autosize(); }
    const parent = d.parent ? posts.get(d.parent) : null;
    $("topic-reply").textContent = d.parent ? (parent ? authorName(parent) : d.parent) : "";
    $("topic-reply-chip").hidden = !d.parent;
    $("topic-feedback").textContent = d.pending ? "Acceptance uncertain. Retry uses the same message ID and exact payload." : d.feedback;
    $("topic-send").textContent = d.pending ? "Retry same message" : "Publish";
    $("topic-discard-uncertain").hidden = !d.pending;
    $("topic-discard-uncertain").disabled = !!busy;
    renderHeader();
  }
  function autosize() {
    const box = $("topic-body");
    box.style.height = "auto";
    box.style.height = `${Math.min(box.scrollHeight, 200)}px`;
  }

  // ---------- posts ----------
  function bodyNode(post) {
    const box = el("div", "md");
    if (Array.isArray(post.body_nodes)) {
      try { box.append(window.renderMarkdown(post.body_nodes)); return box; } catch { box.replaceChildren(); }
    }
    box.classList.add("md--plain");
    box.textContent = post.body;
    return box;
  }
  function postRow(post, before, replies) {
    const human = isHuman(post), name = authorName(post), tone = human ? "is-you" : colorClass(post.author.agent_id);
    const grouped = !!before && before.author.agent_id === post.author.agent_id && !post.parent && dayKey(before.created) === dayKey(post.created) && post.created - before.created < 5 * 60000;
    const row = el("li", `msg${grouped ? " is-grouped" : ""}`);
    row.dataset.id = post.id; row.dataset.sequence = String(post.sequence);
    const avatar = el("div", `avatar ${tone}`, initials(name)); avatar.setAttribute("aria-hidden", "true");
    const gutter = el("div", "gutter-time", hhmm(post.created));
    const main = el("div", "msg__main");
    const meta = el("div", "msg__meta");
    meta.append(el("strong", `author ${tone}`, name), (() => { const t = el("time", "time", hhmm(post.created)); t.dateTime = new Date(post.created).toISOString(); t.title = fullTime(post.created); return t; })());
    if (!human && post.author.agent_id !== name && plainId(post.author.agent_id) !== name) meta.append(el("span", "via", post.author.agent_id));
    main.append(meta);
    if (post.parent) {
      const parent = posts.get(post.parent);
      const link = el("button", "reply-ref", `↳ in reply to ${parent ? authorName(parent) : post.parent}`);
      link.type = "button";
      link.addEventListener("click", () => { const target = $("topic-posts").querySelector(`[data-id="${CSS.escape(post.parent)}"]`); target?.scrollIntoView({ block: "center" }); });
      main.append(link);
    }
    let body = bodyNode(post);
    body.dataset.collapsible = "true";
    main.append(body);
    const more = el("button", "show-more btn btn--sm", "Show more"); more.type = "button"; more.hidden = true; more.setAttribute("aria-expanded", "false");
    main.append(more);
    if (replies.length) {
      const thread = el("button", "thread-link", `${replies.length} ${replies.length === 1 ? "reply" : "replies"}`);
      thread.type = "button";
      thread.addEventListener("click", () => showThread(post));
      const foot = el("div", "msg__foot"); foot.append(thread); main.append(foot);
    }
    const actions = el("div", "msg__actions"); actions.setAttribute("role", "toolbar"); actions.setAttribute("aria-label", "Message actions");
    const reply = el("button", "btn btn--icon btn--quiet btn--sm js-reply", "↩"); reply.type = "button"; reply.title = "Reply"; reply.setAttribute("aria-label", "Reply");
    reply.addEventListener("click", () => { if (!busy && selected && !draft().pending) { draft().parent = post.id; renderDraft(); $("topic-body").focus(); } });
    const source = el("button", "btn btn--icon btn--quiet btn--sm", "‹›"); source.type = "button"; source.title = "View source"; source.setAttribute("aria-label", "View Markdown source"); source.setAttribute("aria-pressed", "false");
    source.addEventListener("click", () => {
      const showing = source.getAttribute("aria-pressed") === "true";
      source.setAttribute("aria-pressed", String(!showing));
      const replacement = showing ? bodyNode(post) : el("pre", "md-source", post.body);
      replacement.dataset.collapsible = "true";
      body.replaceWith(replacement); body = replacement; collapse(body, more);
    });
    actions.append(reply, source);
    row.append(grouped ? gutter : avatar, main, actions);
    return row;
  }
  function collapse(body, more) {
    // Long posts collapse behind "Show more" so a long thread stays navigable.
    body.classList.remove("is-collapsed"); more.hidden = true;
    if (body.scrollHeight <= 380) return;
    body.classList.add("is-collapsed"); more.hidden = false; more.textContent = "Show more"; more.setAttribute("aria-expanded", "false");
    more.onclick = () => { const collapsed = body.classList.toggle("is-collapsed"); more.textContent = collapsed ? "Show more" : "Show less"; more.setAttribute("aria-expanded", String(!collapsed)); };
  }
  function renderPosts(options = {}) {
    const list = $("topic-posts"), scroller = $("topic-scroll");
    const ordered = [...posts.values()].sort((a, b) => a.sequence - b.sequence);
    const ids = ordered.map(post => post.id);
    const signature = `${selected}|${ids.join(",")}`;
    if (signature === renderedSignature) return;
    const first = renderedSignature === "" || !renderedSignature.startsWith(`${selected}|`);
    const atBottom = scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 60;
    const heightBefore = scroller.scrollHeight, topBefore = scroller.scrollTop;
    const children = new Map();
    for (const post of ordered) if (post.parent) (children.get(post.parent) || children.set(post.parent, []).get(post.parent)).push(post);
    const rowsFor = from => {
      const rows = [];
      for (let index = from; index < ordered.length; index++) {
        const post = ordered[index], before = ordered[index - 1];
        if (!before || dayKey(before.created) !== dayKey(post.created)) rows.push(el("li", "day", dayLabel(post.created)));
        rows.push(postRow(post, before, children.get(post.id) || []));
      }
      return rows;
    };
    const fitRow = row => { const body = row.querySelector("[data-collapsible]"), more = row.querySelector(".show-more"); if (body && more) collapse(body, more); };
    // The common live case is new posts at the end. Appending keeps "Show more", "View source",
    // selection and focus on the posts already shown; only a reply refreshes its parent's thread link.
    const appended = !first && !options.keepPosition && renderedIds.length > 0 && ids.length > renderedIds.length && renderedIds.every((id, index) => ids[index] === id);
    if (appended) {
      const fresh = rowsFor(renderedIds.length), holder = document.createDocumentFragment();
      holder.append(...fresh); list.append(holder);
      for (const row of fresh) if (row.classList.contains("msg")) fitRow(row);
      for (let index = renderedIds.length; index < ordered.length; index++) {
        const parentId = ordered[index].parent, parentIndex = parentId ? ids.indexOf(parentId) : -1;
        const old = parentIndex >= 0 && parentIndex < renderedIds.length ? list.querySelector(`.msg[data-id="${CSS.escape(parentId)}"]`) : null;
        if (old) { const replacement = postRow(ordered[parentIndex], ordered[parentIndex - 1], children.get(parentId) || []); old.replaceWith(replacement); fitRow(replacement); }
      }
    } else {
      list.replaceChildren(...rowsFor(0));
      for (const row of list.querySelectorAll(".msg")) fitRow(row);
    }
    renderedIds = ids; renderedSignature = signature;
    if (options.keepPosition) scroller.scrollTop = topBefore + (scroller.scrollHeight - heightBefore);
    else if (first || atBottom) { scroller.scrollTop = scroller.scrollHeight; $("topic-new-pill").hidden = true; }
    else $("topic-new-pill").hidden = false;
    renderHeader();
  }
  function showThread(root) {
    const replies = [...posts.values()].filter(post => post.parent === root.id).sort((a, b) => a.sequence - b.sequence);
    const box = el("div", "thread");
    for (const post of [root, ...replies]) {
      const item = el("article", "thread__item");
      const name = authorName(post);
      const meta = el("div", "msg__meta"); meta.append(el("strong", `author ${isHuman(post) ? "is-you" : colorClass(post.author.agent_id)}`, name), el("span", "time", hhmm(post.created)));
      item.append(meta, bodyNode(post));
      box.append(item);
    }
    const reply = el("button", "btn btn--primary", "Reply in this thread"); reply.type = "button";
    reply.disabled = !!busy || !!topicOf(selected)?.archived;
    reply.addEventListener("click", () => { if (!busy && selected && !draft().pending) { draft().parent = root.id; renderDraft(); closePanel(); $("topic-body").focus(); } });
    box.append(reply);
    openPanel("Thread", box);
  }

  // ---------- data ----------
  function resetPosts() {
    posts = new Map(); next = previous = null; hasOlder = false; renderedSignature = ""; renderedIds = []; loadedOnce = false;
    $("topic-posts").replaceChildren(); $("topic-new-pill").hidden = true; olderRequest = null; $("topic-older").disabled = false; $("topic-older").hidden = true;
  }
  async function refresh() {
    if (!running || document.hidden || refreshing === epoch) return;
    const current = epoch, id = viewing ? selected : null;
    let catchUp = false;
    refreshing = current;
    try {
      const [catalog, registry] = await Promise.all([request("/api/topics"), request("/api/topic-agents")]);
      if (!running || current !== epoch) return;
      const nextCatalog = JSON.stringify(catalog.topics);
      topics = catalog.topics; agents = registry.agents;
      if (nextCatalog !== catalogSignature) catalogSignature = nextCatalog;
      publishCatalog(); renderHeader();
      if (id) {
        if (!topicOf(id)) { status("This topic is unavailable. Select another topic."); return; }
        const query = next === null ? "" : `?cursor=${encodeURIComponent(next)}`;
        const page = await request(`/api/topics/${encodeURIComponent(id)}/posts${query}`);
        if (!running || current !== epoch || selected !== id) return;
        for (const post of page.posts) posts.set(post.id, post);
        next = page.next_cursor;
        catchUp = page.has_more;
        if (previous === null) { previous = page.previous_cursor; hasOlder = page.has_older; }
        loadedOnce = true;
        seen.set(id, Math.max(page.high_water ?? 0, topicOf(id)?.high_water ?? 0));
        renderPosts(); publishCatalog();
        if (!topicOf(id)?.archived) status(posts.size ? "" : "No messages yet. Say something below.");
      }
      retryDelay = 5000;
    } catch (error) {
      if (running && current === epoch && error.name !== "AbortError") {
        status("Unable to refresh topics. Retrying.");
        catalogSignature = "";
        retryDelay = Math.min(retryDelay * 2, 30000);
      }
    } finally {
      if (refreshing === current) refreshing = null;
      if (running && current === epoch && !document.hidden) { clearTimeout(timer); timer = setTimeout(refresh, catchUp ? 100 : retryDelay); }
    }
  }

  // ---------- @mention picker ----------
  function mentionQuery() {
    const box = $("topic-body"), before = box.value.slice(0, box.selectionStart);
    const match = /(^|\s)@([^\s@]*)$/.exec(before);
    return match ? match[2].toLowerCase() : null;
  }
  function closeMentions() { $("topic-mention-pop").hidden = true; $("topic-mention-pop").replaceChildren(); mentionMatches = []; $("topic-body").removeAttribute("aria-activedescendant"); $("topic-body").setAttribute("aria-expanded", "false"); }
  function showMentions() {
    const query = mentionQuery();
    if (query === null) return closeMentions();
    mentionMatches = agents.filter(agent => `${agent.display_name} ${agent.reference.agent_id}`.toLowerCase().includes(query)).slice(0, 6);
    if (!mentionMatches.length) return closeMentions();
    mentionIndex = Math.min(mentionIndex, mentionMatches.length - 1);
    const pop = $("topic-mention-pop"); pop.replaceChildren();
    mentionMatches.forEach((agent, index) => {
      const option = el("button", "mention-option", undefined); option.type = "button"; option.id = `mention-option-${index}`; option.setAttribute("role", "option");
      option.setAttribute("aria-selected", String(index === mentionIndex));
      option.append(el("strong", "", `@${agent.reference.agent_id}`));
      if (agent.display_name && agent.display_name !== agent.reference.agent_id) option.append(el("small", "", agent.display_name));
      option.addEventListener("mousedown", event => { event.preventDefault(); pickMention(agent); });
      pop.append(option);
    });
    pop.hidden = false;
    $("topic-body").setAttribute("aria-expanded", "true");
    $("topic-body").setAttribute("aria-activedescendant", `mention-option-${mentionIndex}`);
  }
  function pickMention(agent) {
    const box = $("topic-body"), position = box.selectionStart;
    const before = box.value.slice(0, position).replace(/@[^\s@]*$/, `@${agent.reference.agent_id} `);
    box.value = before + box.value.slice(position);
    box.selectionStart = box.selectionEnd = before.length;
    if (selected) draft().body = box.value;
    closeMentions(); box.focus(); autosize();
  }
  function mentionsIn(body) {
    return agents.map(agent => agent.reference.agent_id).filter(id => new RegExp(`(?<![\\w@])@${escapeRegExp(id)}(?![\\w-])`).test(body));
  }

  // ---------- events ----------
  $("topic-body").addEventListener("input", event => { if (selected) draft().body = event.target.value; autosize(); showMentions(); });
  $("topic-body").addEventListener("keydown", event => {
    if (!$("topic-mention-pop").hidden && mentionMatches.length) {
      if (event.key === "ArrowDown" || event.key === "ArrowUp") { event.preventDefault(); mentionIndex = (mentionIndex + (event.key === "ArrowDown" ? 1 : mentionMatches.length - 1)) % mentionMatches.length; showMentions(); return; }
      if (event.key === "Enter" || event.key === "Tab") { event.preventDefault(); pickMention(mentionMatches[mentionIndex]); return; }
      if (event.key === "Escape") { event.preventDefault(); closeMentions(); return; }
    }
    if (event.key === "Enter" && !event.shiftKey && !event.isComposing) { event.preventDefault(); $("topic-composer").requestSubmit(); }
  });
  $("topic-body").addEventListener("blur", () => setTimeout(closeMentions, 120));
  document.addEventListener("visibilitychange", () => { clearTimeout(timer); if (running && !document.hidden) void refresh(); });
  $("topic-clear-reply").addEventListener("click", () => { if (selected) { draft().parent = null; renderDraft(); } });
  $("topic-discard-uncertain").addEventListener("click", () => {
    if (!selected || busy || !draft().pending) return;
    if (!confirm("This message may already have been accepted. Discard its retry key and draft?")) return;
    const d = draft(); d.pending = null; d.body = ""; d.parent = null;
    d.feedback = "Discarded locally. The prior publication may already exist in history."; renderDraft();
  });
  $("topic-new-pill").addEventListener("click", () => { const scroller = $("topic-scroll"); scroller.scrollTop = scroller.scrollHeight; $("topic-new-pill").hidden = true; });
  $("topic-scroll").addEventListener("scroll", () => { const scroller = $("topic-scroll"); if (scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 60) $("topic-new-pill").hidden = true; });
  $("topic-older").addEventListener("click", async () => {
    if (olderRequest) return;
    const id = selected, current = epoch, before = previous;
    const operation = {}; olderRequest = operation; $("topic-older").disabled = true;
    try {
      const page = await request(`/api/topics/${encodeURIComponent(id)}/posts?before=${encodeURIComponent(before)}`);
      if (!running || current !== epoch || id !== selected) return;
      for (const post of page.posts) posts.set(post.id, post);
      previous = page.previous_cursor; hasOlder = page.has_older; renderPosts({ keepPosition: true });
    } catch (_) { if (running && current === epoch && id === selected) status("Unable to load older messages."); }
    finally { if (olderRequest === operation) { olderRequest = null; $("topic-older").disabled = false; } }
  });
  $("topic-rename").addEventListener("click", async () => { const name = prompt("New topic name"); if (!name) return; const current = epoch, id = selected; try { await action({ action: "rename", topic_id: id, name }); if (running && current === epoch && id === selected) { clearTimeout(timer); await refresh(); } } catch (_) { if (running && current === epoch) status("Unable to rename topic."); } });
  $("topic-archive").addEventListener("click", async () => { const topic = topicOf(selected); if (!topic) return; const current = epoch, id = selected; try { await action({ action: topic.archived ? "restore" : "archive", topic_id: id }); if (running && current === epoch && id === selected) { clearTimeout(timer); await refresh(); } } catch (_) { if (running && current === epoch) status("Unable to update topic."); } });
  $("topic-composer").addEventListener("submit", async event => {
    event.preventDefault(); if (!selected || busy) return;
    const id = selected, d = draft();
    const payload = d.pending || { action: "publish", topic_id: id, id: crypto.randomUUID(), body: d.body, parent: d.parent, mentions: mentionsIn(d.body).map(agent_id => ({ agent_id })) };
    if (!payload.body.trim()) return;
    const operation = {}; busy = operation; closeMentions(); renderDraft();
    try { await action(payload); if (drafts.get(id) === d) { d.body = ""; d.parent = null; d.pending = null; d.feedback = ""; } }
    catch (error) { if (error.status >= 400 && error.status < 500 && error.status !== 408 && error.status !== 429 && !d.pending) { d.feedback = "Message rejected; your draft is kept."; } else { d.pending = payload; } }
    finally {
      if (busy !== operation) return;
      busy = null;
      if (running && selected) { renderDraft(); clearTimeout(timer); void refresh(); }
    }
  });

  return {
    // Keeps the catalog (and unread counts) fresh whenever the interface is unlocked.
    start() { running = true; epoch++; catalogSignature = ""; void refresh(); },
    // A topic timeline is open. Switching topics starts from a clean timeline.
    show(id) {
      viewing = true;
      if (selected !== id) { selected = id; resetPosts(); }
      epoch++; clearTimeout(timer);
      closeMentions(); renderHeader(); renderDraft(); renderPosts();
      status(posts.size ? "" : "Loading messages…");
      void refresh();
    },
    hide() { viewing = false; epoch++; clearTimeout(timer); olderRequest = null; $("topic-older").disabled = false; closeMentions(); if (running) void refresh(); },
    hasDrafts() { return [...drafts.values()].some(d => d.body || d.parent || d.pending); },
    async create() {
      const name = prompt("Topic name"); if (!name) return null;
      try { const topic = await action({ action: "create", name }); clearTimeout(timer); void refresh(); return topic.reference.topic_id; }
      catch (_) { status("Unable to create topic."); return null; }
    },
    selected: () => selected,
    clear() {
      running = viewing = false; epoch++; clearTimeout(timer); busy = olderRequest = null; selected = null; topics = []; agents = [];
      drafts.clear(); seen.clear(); catalogSignature = ""; resetPosts();
      $("topic-body").value = ""; autosize(); closeMentions(); closePanel();
      for (const id of ["topic-id", "topic-feedback", "topic-status", "topic-reply"]) $(id).textContent = "";
      $("topic-heading").textContent = "Choose a topic"; $("topic-reply-chip").hidden = true; $("topic-discard-uncertain").hidden = true;
      onCatalog([]); renderHeader();
    },
  };
};
