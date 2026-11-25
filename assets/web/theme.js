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

  // A theme named in the address fragment (`#theme=orca`, or `#theme=orca&access_token=…`) wins over
  // the saved one and is remembered like a menu choice. The fragment is never sent in a request.
  // Exactly one known value counts; an unknown, empty or repeated `theme` is ignored and left alone.
  // Once applied it is removed from the address (a route and the access token stay for app.js to
  // consume), so a reload keeps the user's later menu choice instead of re-applying the link's.
  // Each `&`-separated part is read by the same rule for finding and for removing it (the leading `&`
  // keeps URLSearchParams from treating a `?` as a query marker).
  const parts = location.hash.slice(1).split("&"), keyOf = part => [...new URLSearchParams(`&${part}`)][0]?.[0];
  const requested = parts.filter(part => keyOf(part) === "theme").map(part => new URLSearchParams(`&${part}`).get("theme"));
  if (requested.length === 1 && known(requested[0])) {
    current = requested[0];
    try { localStorage.setItem(KEY, current); } catch { /* the choice still applies for this page */ }
    // Drop only the `theme=…` part, as text: every other character (a route, the access token for
    // app.js) stays exactly as it was, which re-serialising the parameters would not guarantee.
    // If that leaves a `?…` part first where it was not first before, keep a separating `&` in front:
    // URLSearchParams would otherwise strip the `?` and read the part differently from how it was read.
    const kept = parts.map((part, index) => ({ part, index })).filter(({ part }) => keyOf(part) !== "theme");
    const rest = (kept[0] && kept[0].index > 0 && kept[0].part.startsWith("?") ? "&" : "") + kept.map(({ part }) => part).join("&");
    history.replaceState(null, "", location.pathname + location.search + (rest ? `#${rest}` : ""));
  }

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
