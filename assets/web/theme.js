"use strict";
// Runs synchronously from <head>, after style.css, so the saved theme is applied before first
// paint. It must be an external script: the page's Content-Security-Policy forbids inline scripts.
// The theme name is the only preference this interface stores in the browser. It is neither a
// credential nor a draft; access tokens and unsent messages are never persisted.
(() => {
  const THEMES = [
    { id: "classic", label: "Classic" },
    { id: "sticker", label: "Sticker Bomb" },
    { id: "orca", label: "Orca" },
  ];
  const KEY = "agent-bridge-theme", DEFAULT = "classic";
  const known = id => THEMES.some(theme => theme.id === id);
  let current = DEFAULT;
  try { const saved = localStorage.getItem(KEY); if (known(saved)) current = saved; } catch { /* storage can be unavailable */ }

  function sheet(id) {
    const link = document.createElement("link");
    link.rel = "stylesheet"; link.id = "theme-sheet"; link.href = `/theme-${id}.css`;
    return link;
  }
  document.documentElement.dataset.theme = current;
  document.head.append(sheet(current));

  // The one sheet that is loading, if any. A new choice cancels it, so at most one sheet is ever
  // pending and the loaded one is the last choice's.
  let pending = null;

  window.AgentBridgeTheme = {
    themes: THEMES,
    get: () => current,
    set(id) {
      if (!known(id) || id === current) return false;
      pending?.remove();
      const next = pending = sheet(id);
      next.id = "theme-sheet-next";
      // Load the new sheet before dropping the old one, so switching never flashes unstyled content.
      next.addEventListener("load", () => {
        if (pending !== next) return; // a later choice already replaced this one
        pending = null;
        document.getElementById("theme-sheet")?.remove();
        next.id = "theme-sheet";
        document.documentElement.dataset.theme = id;
      }, { once: true });
      next.addEventListener("error", () => {
        if (pending !== next) return; // a later choice already replaced this one
        pending = null;
        next.remove();
        // Back to the theme that is actually applied, which is not always the previous choice.
        current = document.documentElement.dataset.theme;
        try { localStorage.setItem(KEY, current); } catch { /* ignore */ }
        document.dispatchEvent(new CustomEvent("themechange", { detail: current }));
      }, { once: true });
      document.head.append(next);
      current = id;
      try { localStorage.setItem(KEY, id); } catch { /* the choice still applies for this page */ }
      document.dispatchEvent(new CustomEvent("themechange", { detail: id }));
      return true;
    },
  };
})();
