"use strict";
// Small DOM and formatting helpers shared by the web interface scripts. Everything builds nodes
// with createElement and textContent. Colour is chosen by class, never by an inline style, because
// the page's Content-Security-Policy forbids inline styles.
window.AB = (() => {
  const el = (tag, className, text) => {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined && text !== null) node.textContent = String(text);
    return node;
  };
  // FNV-1a: a stable identity-to-colour mapping, so an agent keeps its colour across reloads.
  const hash = value => {
    let h = 2166136261;
    for (const ch of String(value)) { h ^= ch.codePointAt(0); h = Math.imul(h, 16777619) >>> 0; }
    return h >>> 0;
  };
  const colorClass = name => `agent-c${hash(name) % 6}`;
  const initials = name => {
    const parts = String(name).split(/[^A-Za-z0-9]+/).filter(Boolean);
    const text = parts.length > 1 ? parts[0][0] + parts[1][0] : (parts[0] || "?").slice(0, 2);
    return text.toUpperCase();
  };
  const valid = ms => Number.isFinite(new Date(Number(ms)).getTime());
  const hhmm = ms => valid(ms) ? new Date(Number(ms)).toLocaleTimeString([], { hour: "numeric", minute: "2-digit" }) : "Unknown";
  const dayKey = ms => valid(ms) ? new Date(Number(ms)).toDateString() : "unknown";
  const dayLabel = ms => {
    if (!valid(ms)) return "Unknown date";
    const date = new Date(Number(ms)), now = new Date();
    if (date.toDateString() === now.toDateString()) return "Today";
    const yesterday = new Date(now); yesterday.setDate(now.getDate() - 1);
    if (date.toDateString() === yesterday.toDateString()) return "Yesterday";
    return date.toLocaleDateString([], { weekday: "long", month: "short", day: "numeric" });
  };
  const fullTime = ms => valid(ms) ? new Date(Number(ms)).toLocaleString() : "Unknown";
  const svg = (paths, size = 18) => {
    const icon = document.createElementNS("http://www.w3.org/2000/svg", "svg");
    icon.setAttribute("viewBox", "0 0 24 24"); icon.setAttribute("width", String(size)); icon.setAttribute("height", String(size));
    icon.setAttribute("fill", "none"); icon.setAttribute("stroke", "currentColor"); icon.setAttribute("stroke-width", "2");
    icon.setAttribute("stroke-linecap", "round"); icon.setAttribute("stroke-linejoin", "round");
    icon.setAttribute("aria-hidden", "true"); icon.setAttribute("focusable", "false");
    for (const d of paths) { const path = document.createElementNS("http://www.w3.org/2000/svg", "path"); path.setAttribute("d", d); icon.append(path); }
    return icon;
  };
  return { el, hash, colorClass, initials, hhmm, dayKey, dayLabel, fullTime, svg };
})();
