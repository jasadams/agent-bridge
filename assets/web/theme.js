"use strict";
// Runs synchronously from <head>, after style.css, so the saved theme is applied before first
// paint. It must be an external script: the page's Content-Security-Policy forbids inline scripts.
// The theme name is the only preference this interface stores in the browser. It is neither a
// credential nor a draft; access tokens and unsent messages are never persisted.
(() => {
  const THEMES = [
    { id: "classic", label: "Classic" },
    { id: "sticker", label: "Sticker Bomb" },
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

  window.AgentBridgeTheme = {
    themes: THEMES,
    get: () => current,
    set(id) {
      if (!known(id) || id === current) return false;
      const old = document.getElementById("theme-sheet"), next = sheet(id);
      next.id = "theme-sheet-next";
      // Load the new sheet before dropping the old one, so switching never flashes unstyled content.
      next.addEventListener("load", () => { old?.remove(); next.id = "theme-sheet"; document.documentElement.dataset.theme = id; }, { once: true });
      const previous = current;
      next.addEventListener("error", () => {
        next.remove();
        if (current !== id) return; // a later choice already replaced this one
        current = previous;
        try { localStorage.setItem(KEY, previous); } catch { /* ignore */ }
        document.dispatchEvent(new CustomEvent("themechange", { detail: previous }));
      }, { once: true });
      document.head.append(next);
      current = id;
      try { localStorage.setItem(KEY, id); } catch { /* the choice still applies for this page */ }
      document.dispatchEvent(new CustomEvent("themechange", { detail: id }));
      return true;
    },
  };
})();
