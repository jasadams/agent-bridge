"use strict";
// Renders the server's parsed Markdown node tree (see `markdown_nodes` in src/monitor_board.rs) as
// DOM. Agent-authored text is untrusted: nothing here parses HTML or builds markup from strings.
// Text is always a text node, tags come from a fixed allow-list, and links are re-validated client-side.
// Throws on anything unexpected so callers can fall back to showing the plain text.
window.renderMarkdown = nodes => {
  const tags = new Set("p h1 h2 h3 h4 h5 h6 blockquote ul ol li pre code em strong del table thead tbody tr th td hr br a span".split(" "));
  let work = 0;
  function build(value, depth) {
    if (++work > 20000 || depth > 32 || !value || typeof value !== "object") throw new Error("Complex document");
    if (value.kind === "text" && typeof value.text === "string") return document.createTextNode(value.text);
    if (value.kind !== "element" || !tags.has(value.tag) || !Array.isArray(value.children)) throw new Error("Unsupported document node");
    const element = document.createElement(value.tag);
    if (value.tag === "ol" && Number.isSafeInteger(value.start) && value.start >= 0) element.start = value.start;
    if (value.tag === "a" && typeof value.href === "string") {
      try {
        const url = new URL(value.href);
        if (["http:", "https:"].includes(url.protocol) && !url.username && !url.password) {
          element.href = url.href; element.target = "_blank"; element.rel = "noopener noreferrer"; element.referrerPolicy = "no-referrer";
        }
      } catch { /* An unsafe or malformed link stays inert text. */ }
    }
    for (const child of value.children) element.append(build(child, depth + 1));
    if (value.tag === "pre") return codeBlock(element);
    return element;
  }
  // A code block keeps its text selectable and gains a copy button. The button is a sibling of
  // the <pre>, so copying never changes the displayed code.
  function codeBlock(pre) {
    const wrapper = document.createElement("div");
    wrapper.className = "md-code";
    const bar = document.createElement("div");
    bar.className = "md-code__bar";
    const label = document.createElement("span");
    label.textContent = "code";
    const button = document.createElement("button");
    button.type = "button";
    button.className = "md-code__copy";
    button.textContent = "Copy";
    button.setAttribute("aria-label", "Copy code block");
    button.addEventListener("click", async () => {
      try { await navigator.clipboard.writeText(pre.textContent); button.textContent = "Copied"; }
      catch { button.textContent = "Copy failed"; }
      setTimeout(() => { button.textContent = "Copy"; }, 1400);
    });
    bar.append(label, button);
    pre.tabIndex = 0;
    wrapper.append(bar, pre);
    return wrapper;
  }
  if (!Array.isArray(nodes)) throw new Error("Missing document");
  const fragment = document.createDocumentFragment();
  for (const value of nodes) fragment.append(build(value, 0));
  return fragment;
};
