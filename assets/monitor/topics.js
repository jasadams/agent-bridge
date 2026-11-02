"use strict";
window.createTopicMonitor = ({ request }) => {
  const $ = id => document.getElementById(id);
  const drafts = new Map();
  let active = false, epoch = 0, timer = null, selected = null, topics = [], agents = [];
  let next = null, previous = null, hasOlder = false, posts = new Map(), busy = null;
  let refreshing = null, olderRequest = null, retryDelay = 5000, catalogSignature = "", agentSignature = "";
  const node = (tag, value) => { const element = document.createElement(tag); if (value != null) element.textContent = value; return element; };
  const draft = () => { if (!drafts.has(selected)) drafts.set(selected, { body: "", parent: null, mentions: [], pending: null, feedback: "" }); return drafts.get(selected); };
  const action = payload => request("/api/topic-actions", { method: "POST", body: JSON.stringify(payload) });
  const status = text => { $("topic-status").textContent = text; };
  function renderCatalog() {
    const list = $("topic-list"), existing = new Map([...list.children].map(item => [item.dataset.topicId, item]));
    for (const topic of topics) {
      const id = topic.reference.topic_id;
      let li = existing.get(id);
      if (!li) {
        li = node("li"); li.dataset.topicId = id;
        const button = node("button"); button.type = "button";
        button.append(node("span"), node("small", id));
        button.addEventListener("click", () => { void choose(id); });
        li.append(button); list.append(li);
      }
      const button = li.firstElementChild, label = button.firstElementChild;
      const name = topic.name + (topic.archived ? " (archived)" : "");
      if (label.textContent !== name) label.textContent = name;
      button.type = "button"; button.setAttribute("aria-pressed", String(topic.reference.topic_id === selected));
      existing.delete(id);
    }
    for (const li of existing.values()) li.remove();
    $("topic-list-status").textContent = topics.length ? "" : "No topics yet.";
  }
  function renderTopic() {
    const topic = topics.find(item => item.reference.topic_id === selected);
    $("topic-heading").textContent = topic?.name || (selected ? "Topic unavailable" : "Choose a topic");
    $("topic-id").textContent = selected || "";
    $("topic-rename").disabled = !topic;
    $("topic-archive").disabled = !topic;
    $("topic-archive").textContent = topic?.archived ? "Restore" : "Archive";
    $("topic-send").disabled = !topic || (topic.archived && !draft().pending) || busy;
    $("topic-body").disabled = !topic || topic.archived || busy || !!(selected && draft().pending);
    for (const button of $("topic-posts").querySelectorAll("button")) button.disabled = !topic || topic.archived || busy || !!draft().pending;
    $("topic-older").hidden = !hasOlder;
    if (topic?.archived) status("Archived · read only");
  }
  function renderPosts() {
    const list = $("topic-posts"), existing = new Map([...list.children].map(item => [item.dataset.id, item]));
    for (const post of [...posts.values()].sort((a,b) => a.sequence - b.sequence)) {
      if (existing.has(post.id)) continue;
      const li = node("li"), author = node("strong", post.author_display_name || post.author.agent_id);
      li.dataset.id = post.id; li.dataset.sequence = String(post.sequence);
      li.append(author, node("small", ` ${post.author.agent_id} · ${new Date(post.created).toLocaleString()}`));
      if (post.parent) li.append(node("p", `Reply to ${post.parent}`));
      if (post.mentions?.length) li.append(node("p", `Mentions: ${post.mentions.map(mention => mention.agent_id).join(", ")}`));
      li.append(node("p", post.body));
      const reply = node("button", "Reply"); reply.type = "button";
      reply.addEventListener("click", () => { if (!busy && selected && !draft().pending) { draft().parent = post.id; renderDraft(); } });
      li.append(reply);
      const following = [...list.children].find(child => Number(child.dataset.sequence) > post.sequence);
      list.insertBefore(li, following || null);
    }
  }
  function renderDraft() {
    if (!selected) return;
    const d = draft(); if ($("topic-body").value !== d.body) $("topic-body").value = d.body;
    $("topic-reply").textContent = d.parent ? `Reply to ${d.parent}` : "";
    $("topic-clear-reply").hidden = !d.parent;
    const select = $("topic-mentions"); select.replaceChildren();
    for (const agent of agents) {
      const option = node("option", `${agent.display_name || agent.reference.agent_id} · ${agent.reference.agent_id}`);
      option.value = agent.reference.agent_id; option.selected = d.mentions.includes(option.value); select.append(option);
    }
    $("topic-feedback").textContent = d.pending ? "Acceptance uncertain. Retry uses the same message ID and exact payload." : d.feedback;
    $("topic-send").textContent = d.pending ? "Retry same message" : "Publish";
    $("topic-discard-uncertain").hidden = !d.pending;
    $("topic-discard-uncertain").disabled = busy;
    $("topic-mentions").disabled = !!d.pending || busy;
    $("topic-clear-reply").disabled = !!d.pending || busy;
    renderTopic();
  }
  async function refresh() {
    if (!active || document.hidden || refreshing === epoch) return;
    const current = epoch, id = selected;
    let catchUp = false;
    refreshing = current;
    try {
      const [catalog, registry] = await Promise.all([request("/api/topics"), request("/api/topic-agents")]);
      if (!active || current !== epoch) return;
      const nextCatalog = JSON.stringify(catalog.topics), nextAgents = JSON.stringify(registry.agents);
      topics = catalog.topics; agents = registry.agents;
      if (nextCatalog !== catalogSignature) { catalogSignature = nextCatalog; renderCatalog(); }
      if (nextAgents !== agentSignature && document.activeElement !== $("topic-mentions")) {
        agentSignature = nextAgents; renderDraft();
      }
      renderTopic();
      if (id) {
        if (!topics.some(topic => topic.reference.topic_id === id)) {
          status("This topic is unavailable. Select another topic.");
          return;
        }
        const query = next === null ? "" : `?cursor=${encodeURIComponent(next)}`;
        const page = await request(`/api/topics/${encodeURIComponent(id)}/posts${query}`);
        if (!active || current !== epoch || selected !== id) return;
        for (const post of page.posts) posts.set(post.id, post);
        next = page.next_cursor;
        catchUp = page.has_more;
        if (previous === null) { previous = page.previous_cursor; hasOlder = page.has_older; }
        renderPosts(); renderTopic();
        if (!topics.find(topic => topic.reference.topic_id === id)?.archived) status(posts.size ? "" : "No messages yet.");
      }
      retryDelay = 5000;
    } catch (error) {
      if (active && current === epoch && error.name !== "AbortError") {
        status("Unable to refresh topics. Retrying.");
        $("topic-list-status").textContent = "Unable to refresh topics. Retrying.";
        catalogSignature = "";
        retryDelay = Math.min(retryDelay * 2, 30000);
      }
    } finally {
      if (refreshing === current) refreshing = null;
      if (active && current === epoch && !document.hidden) { clearTimeout(timer); timer = setTimeout(refresh, catchUp ? 100 : retryDelay); }
    }
  }
  async function choose(id) {
    selected = id; epoch++; clearTimeout(timer); olderRequest = null; $("topic-older").disabled = false; posts = new Map(); next = previous = null; hasOlder = false;
    $("topic-posts").replaceChildren();
    const url = new URL(location.href); url.searchParams.set("topic", id); history.replaceState(null, "", url.pathname + url.search);
    renderCatalog(); renderPosts(); renderDraft(); status("Loading messages…"); await refresh();
  }
  $("topic-body").addEventListener("input", e => { if (selected) draft().body = e.target.value; });
  $("topic-mentions").addEventListener("change", e => { if (selected) draft().mentions = [...e.target.selectedOptions].map(option => option.value); });
  $("topic-mentions").addEventListener("blur", () => { agentSignature = ""; if (active) { clearTimeout(timer); void refresh(); } });
  document.addEventListener("visibilitychange", () => {
    clearTimeout(timer);
    if (active && !document.hidden) void refresh();
  });
  $("topic-clear-reply").addEventListener("click", () => { if (selected) { draft().parent = null; renderDraft(); } });
  $("topic-discard-uncertain").addEventListener("click", () => {
    if (!selected || busy || !draft().pending) return;
    if (!confirm("This message may already have been accepted. Discard its retry key and draft?")) return;
    const d = draft(); d.pending = null; d.body = ""; d.parent = null; d.mentions = [];
    d.feedback = "Discarded locally. The prior publication may already exist in history."; renderDraft();
  });
  $("topic-older").addEventListener("click", async () => {
    if (olderRequest) return;
    const id = selected, current = epoch, before = previous;
    const operation = {}; olderRequest = operation; $("topic-older").disabled = true;
    try {
      const anchor = $("topic-posts").firstElementChild;
      const anchorTop = anchor?.getBoundingClientRect().top;
      const page = await request(`/api/topics/${encodeURIComponent(id)}/posts?before=${encodeURIComponent(before)}`);
      if (!active || current !== epoch || id !== selected) return;
      for (const post of page.posts) posts.set(post.id, post);
      previous = page.previous_cursor; hasOlder = page.has_older; renderPosts(); renderTopic();
      if (anchor && anchorTop != null) window.scrollBy(0, anchor.getBoundingClientRect().top - anchorTop);
    } catch (_) { if (active && current === epoch && id === selected) status("Unable to load older messages."); }
    finally { if (olderRequest === operation) { olderRequest = null; $("topic-older").disabled = false; } }
  });
  $("topic-create").addEventListener("click", async () => { const name = prompt("Topic name"); if (!name) return; const current = epoch; try { const topic = await action({action:"create", name}); if (active && current === epoch) await choose(topic.reference.topic_id); } catch (_) { if (active && current === epoch) status("Unable to create topic."); } });
  $("topic-rename").addEventListener("click", async () => { const name = prompt("New topic name"); if (!name) return; const current = epoch, id = selected; try { await action({action:"rename",topic_id:id,name}); if (active && current === epoch && id === selected) { clearTimeout(timer); await refresh(); } } catch (_) { if (active && current === epoch && id === selected) status("Unable to rename topic."); } });
  $("topic-archive").addEventListener("click", async () => { const topic = topics.find(item => item.reference.topic_id === selected); if (!topic) return; const current = epoch, id = selected; try { await action({action:topic.archived ? "restore" : "archive",topic_id:id}); if (active && current === epoch && id === selected) { clearTimeout(timer); await refresh(); } } catch (_) { if (active && current === epoch && id === selected) status("Unable to change archive state."); } });
  $("topic-composer").addEventListener("submit", async event => {
    event.preventDefault(); if (!selected || busy) return;
    const id = selected, d = draft();
    const payload = d.pending || { action:"publish", topic_id:id, id:crypto.randomUUID(), body:d.body, parent:d.parent, mentions:d.mentions.map(agent_id => ({agent_id})) };
    if (!payload.body.trim()) return;
    const operation = {}; busy = operation; renderDraft();
    try { await action(payload); if (drafts.get(id) === d) { d.body = ""; d.parent = null; d.mentions = []; d.pending = null; d.feedback = "Published."; } }
    catch (error) { if (error.status >= 400 && error.status < 500 && error.status !== 408 && error.status !== 429 && !d.pending) { d.feedback = "Message rejected; your draft is kept."; } else { d.pending = payload; } }
    finally {
      if (busy !== operation) return;
      busy = null;
      if (active && selected) {
        renderDraft();
        clearTimeout(timer); void refresh();
      }
    }
  });
  return {
    open() { active = true; selected = selected || new URL(location.href).searchParams.get("topic"); epoch++; catalogSignature = ""; $("topic-list-status").textContent = "Loading topics…"; renderDraft(); void refresh(); },
    stop() { active = false; epoch++; clearTimeout(timer); olderRequest = null; $("topic-older").disabled = false; },
    hasDrafts() { return [...drafts.values()].some(d => d.body || d.parent || d.mentions.length || d.pending); },
    clear() { active = false; epoch++; clearTimeout(timer); busy = olderRequest = null; selected = null; topics = []; agents = []; posts.clear(); drafts.clear(); next = previous = null; hasOlder = false; catalogSignature = agentSignature = ""; $("topic-body").value = ""; $("topic-posts").replaceChildren(); $("topic-list").replaceChildren(); $("topic-mentions").replaceChildren(); for (const id of ["topic-id", "topic-reply", "topic-feedback", "topic-status", "topic-list-status"]) $(id).textContent = ""; $("topic-heading").textContent = "Choose a topic"; $("topic-clear-reply").hidden = $("topic-discard-uncertain").hidden = $("topic-older").hidden = true; $("topic-older").disabled = false; renderTopic(); },
  };
};
