// What a screen shows, on the operator pages: one small picture per screen,
// refreshed every ten seconds while the page is visible. One node per screen,
// reused, so a page that rebuilds its status line every poll does not reload
// the picture every poll.
(() => {
  'use strict';

  const nodes = new Map();
  const REFRESH = 10000;

  // Style, not the `hidden` attribute: the box's own inline display would win.
  function show(entry, visible) {
    entry.box.style.display = visible ? 'inline-block' : 'none';
  }

  async function refresh(name, entry) {
    if (document.visibilityState !== 'visible') return;
    try {
      const res = await fetch(`/api/displays/${encodeURIComponent(name)}/screenshot`, { cache: 'no-store' });
      if (!res.ok) { show(entry, false); return; }
      const blob = await res.blob();
      if (entry.url) URL.revokeObjectURL(entry.url);
      entry.url = URL.createObjectURL(blob);
      entry.img.src = entry.url;
      show(entry, true);
      const since = res.headers.get('x-screen-frozen-since');
      const age = Number(res.headers.get('x-screenshot-age') || 0);
      entry.mark.style.display = since ? 'block' : 'none';
      if (since) {
        entry.mark.textContent = `eingefroren seit ${new Date(since).toLocaleTimeString()}`;
      }
      entry.box.title = age > 15 ? `Bild ${age} s alt` : 'aktuell';
    } catch (_) {
      show(entry, false);
    }
  }

  // The picture for one screen: the same element every time, so it can be
  // appended into a freshly built row without starting over.
  function node(name) {
    let entry = nodes.get(name);
    if (!entry) {
      const box = document.createElement('span');
      box.className = 'screenshot';
      box.style.cssText = 'display:none; position:relative; vertical-align:middle; margin:0 .5rem;';
      const img = document.createElement('img');
      img.alt = '';
      img.style.cssText = 'width:160px; border:1px solid #ccc; border-radius:3px; display:block;';
      const mark = document.createElement('span');
      mark.style.cssText = 'display:none; position:absolute; left:0; right:0; bottom:0; background:#a11; color:#fff;'
        + 'font:12px system-ui, sans-serif; padding:1px 4px; text-align:center;';
      box.append(img, mark);
      entry = { box, img, mark, url: null };
      nodes.set(name, entry);
      refresh(name, entry);
      setInterval(() => refresh(name, entry), REFRESH);
    }
    return entry.box;
  }

  document.addEventListener('visibilitychange', () => {
    if (document.visibilityState === 'visible') for (const [name, entry] of nodes) refresh(name, entry);
  });

  globalThis.Screenshot = { node };
})();
