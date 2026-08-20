// The overlay runtime: badges the operator can put on top of whatever the
// playlist is showing.
//
// There are two sources -- a global overlay and the current playlist item's own
// -- and they are additive, so the runtime takes a *list* of layers. Layers that
// want the same corner share one box, stacked in order, and the first layer in a
// box decides how it looks; layers in different corners get their own box. That
// rule is what lets "clock, always" and "QR for this item" coexist without
// anybody computing offsets by hand.
//
// This is injected into *foreign* pages, so it defends itself on three fronts
// that a normal widget never has to think about:
//
//  * **The page's CSS.** A shadow root plus `all: initial` on the container, so
//    a dashboard's `div { display: none }` or its font stack cannot reach in.
//  * **The top layer.** An element in fullscreen (a video, some dashboards)
//    covers *every* z-index there is. A manual popover is in the top layer, so
//    it stays visible; the z-index below is only the fallback for browsers
//    without popover support.
//  * **Pages that rebuild themselves.** SPAs replace whole subtrees, taking the
//    overlay with them. A MutationObserver puts it back rather than leaving a
//    display that silently lost its notice.
//  * **The network, which is not ours.** Nothing here fetches anything. Chromium's
//    Local Network Access refuses a request from a public origin to 127.0.0.1
//    without a permission click, and a kiosk has nobody to click it (measured on
//    Chrome 151 with a fresh profile: `fetch` and `<img>` both fail). So the QR
//    arrives as a module matrix and is drawn as inline SVG -- which an `img-src`
//    CSP cannot refuse either -- and an image arrives as a `data:` URI.
//
// Like `autoscroll.js` this file is both served over HTTP and `include_str!`-ed
// into the binary, and it must stay idempotent: it is evaluated again after
// every navigation because a strict CSP can stop the pre-navigation injection
// from running.

(function () {
  'use strict';

  if (globalThis.__ov) return;

  const HOST_PREFIX = '__mcc_overlay';
  const CORNERS = {
    'top-left': 'top: var(--m); left: var(--m);',
    'top-right': 'top: var(--m); right: var(--m);',
    'bottom-left': 'bottom: var(--m); left: var(--m);',
    'bottom-right': 'bottom: var(--m); right: var(--m);',
    'top-center': 'top: var(--m); left: 50%; transform: translateX(-50%);',
    'bottom-center': 'bottom: var(--m); left: 50%; transform: translateX(-50%);',
  };

  let payload = null;
  // One entry per corner in use: { host, shadow }.
  const boxes = new Map();
  let ticker = null;
  let observer = null;

  function ensureHost(position) {
    const id = HOST_PREFIX + '_' + position;
    let entry = boxes.get(position);
    let host = entry && entry.host;

    if (!host || !host.isConnected) {
      host = document.getElementById(id);
    }
    if (!host) {
      host = document.createElement('div');
      host.id = id;
      // "manual" and not "auto": an auto popover closes on the next click or
      // Escape anywhere on the page, and this one is not the page's to dismiss.
      host.setAttribute('popover', 'manual');
      entry = null;
    }
    if (!host.isConnected) {
      (document.body || document.documentElement).appendChild(host);
    }

    // Reset every property the page could have inherited into the host.
    host.style.cssText = 'all: initial; position: fixed; inset: auto; '
      + 'margin: 0; padding: 0; border: 0; background: transparent; '
      + 'z-index: 2147483647; pointer-events: none;';

    if (host.showPopover) {
      try {
        host.showPopover();
      } catch (_) {
        // Already showing, or the element was re-attached: harmless either way.
      }
    }

    const shadow = host.shadowRoot || host.attachShadow({ mode: 'open' });
    entry = { host, shadow };
    boxes.set(position, entry);
    return entry;
  }

  // Watch for the page throwing the overlay away. Cheap: one observer for the
  // whole document, and it only ever does work when the host is really gone.
  function watch() {
    if (observer) return;
    observer = new MutationObserver(() => {
      if (!layers().length) return;
      for (const entry of boxes.values()) {
        if (!entry.host.isConnected) {
          render();
          return;
        }
      }
    });
    observer.observe(document.documentElement, { childList: true, subtree: true });
  }

  function formatTime(now, withSeconds) {
    const parts = { hour: '2-digit', minute: '2-digit' };
    if (withSeconds) parts.second = '2-digit';
    return now.toLocaleTimeString((payload && payload.locale) || undefined, parts);
  }

  function formatDate(now) {
    return now.toLocaleDateString((payload && payload.locale) || undefined,
      { weekday: 'long', day: 'numeric', month: 'long', year: 'numeric' });
  }

  function layers() {
    return payload && Array.isArray(payload.layers) ? payload.layers : [];
  }

  function hasContent(layer) {
    return !!(layer.text || layer.image_data || layer.show_clock || layer.show_date
              || (layer.qr_modules && layer.qr_modules.length));
  }

  // The box style comes from its *first* layer: when a global overlay and an
  // item overlay share a corner they are one box, and two competing background
  // colours in one box would look like a bug rather than a choice.
  // rgba() from a hex colour and an alpha, because the two are stored apart: a
  // CSS string in the settings failed silently on a typo, and there is nobody in
  // front of the screen to notice a box that lost its background.
  function rgba(hex, alpha) {
    const digits = String(hex || '').replace('#', '');
    const full = digits.length === 3
      ? digits.split('').map((c) => c + c).join('')
      : digits.slice(0, 6);
    const value = parseInt(full, 16);
    if (!Number.isFinite(value) || full.length !== 6) return `rgba(0,0,0,${alpha})`;
    return `rgba(${(value >> 16) & 255},${(value >> 8) & 255},${value & 255},${alpha})`;
  }

  function boxBackground(style) {
    if (style.plain) return 'transparent';
    // The escape hatch wins when it is set: it exists for the gradient nobody
    // wanted to express as one colour.
    if (style.background_css) return style.background_css;
    return rgba(style.background_color, style.background_alpha);
  }

  function styles(style, position) {
    const corner = CORNERS[position] || CORNERS['bottom-right'];
    // Sizes are in vmin so one configuration looks the same on a 1080p panel and
    // on a portrait 4K one -- signage is looked at from across a room, and a
    // pixel size that reads well on one screen is invisible on the other.
    return `
      :host { all: initial; }
      .box {
        position: fixed;
        ${corner}
        --m: ${style.margin}vmin;
        box-sizing: border-box;
        display: flex;
        flex-direction: column;
        gap: 0.5em;
        max-width: ${style.max_width}vw;
        padding: 0.7em 1em;
        border-radius: 0.5em;
        background: ${boxBackground(style)};
        color: ${rgba(style.color, style.color_alpha)};
        font-family: system-ui, sans-serif;
        font-size: ${style.size}vmin;
        line-height: 1.25;
        font-weight: 600;
        text-align: ${position.endsWith('right') ? 'right' : 'left'};
        white-space: pre-wrap;
        overflow-wrap: anywhere;
      }
      .box.plain { background: transparent; padding: 0; border-radius: 0; }
      .layer { display: flex; flex-direction: column; gap: 0.3em; }
      .clock { font-variant-numeric: tabular-nums; font-size: 1.6em; font-weight: 700; }
      .date { opacity: 0.85; font-size: 0.85em; font-weight: 500; }
      img.logo { max-width: 100%; max-height: 6em; object-fit: contain; }
      .qr { width: ${style.qr_size}vmin; height: ${style.qr_size}vmin; display: block;
             background: #fff; border-radius: 0.3em; }
      .qrwrap { display: flex; align-items: center; gap: 0.6em; }
      .qrlabel { font-size: 0.8em; font-weight: 500; }
    `;
  }

  // The QR code, drawn from the modules the controller sent.
  //
  // Inline SVG rather than an image: nothing is fetched (see the top of the file),
  // and inline DOM is not governed by `img-src`, so this survives a page whose CSP
  // would refuse even a `data:` URI. Runs of dark modules become one rect each,
  // which keeps a version-6 code at a few dozen nodes instead of a thousand.
  const QUIET = 2;

  function qrSvg(rows) {
    const size = rows.length;
    if (!size) return null;
    const span = size + QUIET * 2;
    const NS = 'http://www.w3.org/2000/svg';

    const svg = document.createElementNS(NS, 'svg');
    svg.setAttribute('class', 'qr');
    svg.setAttribute('viewBox', `0 0 ${span} ${span}`);
    // The quiet zone has to be light or a scanner may not find the code, and the
    // box behind it is usually dark.
    svg.setAttribute('shape-rendering', 'crispEdges');

    const paper = document.createElementNS(NS, 'rect');
    paper.setAttribute('width', String(span));
    paper.setAttribute('height', String(span));
    paper.setAttribute('fill', '#fff');
    svg.appendChild(paper);

    rows.forEach((row, y) => {
      let runStart = -1;
      for (let x = 0; x <= row.length; x++) {
        const dark = row[x] === '1';
        if (dark && runStart < 0) runStart = x;
        if (!dark && runStart >= 0) {
          const rect = document.createElementNS(NS, 'rect');
          rect.setAttribute('x', String(runStart + QUIET));
          rect.setAttribute('y', String(y + QUIET));
          rect.setAttribute('width', String(x - runStart));
          rect.setAttribute('height', '1');
          rect.setAttribute('fill', '#000');
          svg.appendChild(rect);
          runStart = -1;
        }
      }
    });

    return svg;
  }

  // One layer's content, without any of the box's own styling.
  function renderLayer(layer) {
    const group = document.createElement('div');
    group.className = 'layer';

    if (layer.image_data) {
      const img = document.createElement('img');
      img.className = 'logo';
      // A `data:` URI, not a URL: see the note at the top about Local Network
      // Access. This is the one element the overlay cannot draw itself.
      img.src = layer.image_data;
      img.alt = '';
      group.appendChild(img);
    }

    if (layer.text) {
      const text = document.createElement('div');
      // textContent, never innerHTML: this string is typed in the admin UI and
      // lands inside pages we do not own.
      text.textContent = layer.text;
      group.appendChild(text);
    }

    if (layer.show_clock) {
      const clock = document.createElement('div');
      clock.className = 'clock';
      clock.dataset.clock = layer.show_seconds ? 'seconds' : 'minutes';
      group.appendChild(clock);
    }

    if (layer.show_date) {
      const date = document.createElement('div');
      date.className = 'date';
      date.dataset.date = '1';
      group.appendChild(date);
    }

    const qr = layer.qr_modules && layer.qr_modules.length ? qrSvg(layer.qr_modules) : null;
    if (qr) {
      const wrap = document.createElement('div');
      wrap.className = 'qrwrap';
      if (layer.qr_label) {
        const label = document.createElement('div');
        label.className = 'qrlabel';
        label.textContent = layer.qr_label;
        wrap.appendChild(label);
      }
      wrap.appendChild(qr);
      group.appendChild(wrap);
    }

    return group;
  }

  function render() {
    const active = layers().filter(hasContent);
    if (!active.length) {
      remove();
      return;
    }

    watch();

    // Group by corner, keeping the given order: the global layer comes first, so
    // it sits at the top of a shared box and decides its style.
    const grouped = new Map();
    for (const layer of active) {
      const position = CORNERS[layer.position] ? layer.position : 'bottom-right';
      if (!grouped.has(position)) grouped.set(position, []);
      grouped.get(position).push(layer);
    }

    // Corners nobody wants any more lose their box, or an item's badge would
    // linger after the playlist moved on.
    for (const [position, entry] of [...boxes.entries()]) {
      if (!grouped.has(position)) {
        dropBox(position, entry);
      }
    }

    for (const [position, group] of grouped) {
      const { shadow } = ensureHost(position);
      const style = document.createElement('style');
      style.textContent = styles(group[0], position);

      const box = document.createElement('div');
      box.className = 'box' + (group[0].plain ? ' plain' : '');
      for (const layer of group) {
        box.appendChild(renderLayer(layer));
      }
      shadow.replaceChildren(style, box);
    }

    tick();

    if (ticker) clearInterval(ticker);
    const wantsClock = active.some((layer) => layer.show_clock || layer.show_date);
    const wantsSeconds = active.some((layer) => layer.show_clock && layer.show_seconds);
    if (wantsClock) {
      ticker = setInterval(tick, wantsSeconds ? 1000 : 15000);
    }
  }

  function tick() {
    const now = new Date();
    for (const { shadow } of boxes.values()) {
      for (const node of shadow.querySelectorAll('[data-clock]')) {
        node.textContent = formatTime(now, node.dataset.clock === 'seconds');
      }
      for (const node of shadow.querySelectorAll('[data-date]')) {
        node.textContent = formatDate(now);
      }
    }
  }

  function dropBox(position, entry) {
    if (entry.host.hidePopover) {
      try { entry.host.hidePopover(); } catch (_) { /* not shown */ }
    }
    entry.host.remove();
    boxes.delete(position);
  }

  function remove() {
    if (ticker) { clearInterval(ticker); ticker = null; }
    for (const [position, entry] of [...boxes.entries()]) {
      dropBox(position, entry);
    }
  }

  const DEFAULTS = {
    margin: 3, size: 2.4, max_width: 40, qr_size: 14,
    background_color: '#000000', background_alpha: 0.65, plain: false,
    background_css: '', color: '#ffffff', color_alpha: 1,
    position: 'bottom-right',
  };

  function normalize(layer) {
    const merged = { ...DEFAULTS, ...layer };
    for (const key of ['margin', 'size', 'max_width', 'qr_size',
                       'background_alpha', 'color_alpha']) {
      const value = Number(merged[key]);
      merged[key] = Number.isFinite(value) ? value : DEFAULTS[key];
    }
    if (!CORNERS[merged.position]) merged.position = DEFAULTS.position;
    return merged;
  }

  globalThis.__ov = {
    // One entry point, called with every layer that should be on screen: the
    // controller has no way to know what the page currently shows, so every
    // apply is a full replacement rather than a diff.
    apply(next) {
      const given = next && typeof next === 'object' ? next : {};
      payload = {
        locale: given.locale || undefined,
        layers: (Array.isArray(given.layers) ? given.layers : []).map(normalize),
      };
      render();
      return boxes.size > 0;
    },
    clear() {
      payload = null;
      remove();
    },
    state() {
      return {
        installed: true,
        boxes: boxes.size,
        positions: [...boxes.keys()],
        layers: layers().length,
        attached: [...boxes.values()].every((entry) => entry.host.isConnected)
          && boxes.size > 0,
        inTopLayer: [...boxes.values()].every((entry) => {
          try { return entry.host.matches(':popover-open'); } catch (_) { return false; }
        }) && boxes.size > 0,
      };
    },
  };
})();
