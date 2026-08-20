// The overlay runtime: a badge the operator can put on top of whatever the
// playlist is showing.
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
//
// Like `autoscroll.js` this file is both served over HTTP and `include_str!`-ed
// into the binary, and it must stay idempotent: it is evaluated again after
// every navigation because a strict CSP can stop the pre-navigation injection
// from running.

(function () {
  'use strict';

  if (globalThis.__ov) return;

  const HOST_ID = '__mcc_overlay';
  const CORNERS = {
    'top-left': 'top: var(--m); left: var(--m);',
    'top-right': 'top: var(--m); right: var(--m);',
    'bottom-left': 'bottom: var(--m); left: var(--m);',
    'bottom-right': 'bottom: var(--m); right: var(--m);',
    'top-center': 'top: var(--m); left: 50%; transform: translateX(-50%);',
    'bottom-center': 'bottom: var(--m); left: 50%; transform: translateX(-50%);',
  };

  let config = null;
  let host = null;
  let shadow = null;
  let ticker = null;
  let observer = null;

  function ensureHost() {
    if (host && host.isConnected) return host;

    host = document.getElementById(HOST_ID);
    if (!host) {
      host = document.createElement('div');
      host.id = HOST_ID;
      // "manual" and not "auto": an auto popover closes on the next click or
      // Escape anywhere on the page, and this one is not the guest's to dismiss.
      host.setAttribute('popover', 'manual');
      shadow = null;
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

    if (!shadow) {
      shadow = host.shadowRoot || host.attachShadow({ mode: 'open' });
    }
    return host;
  }

  // Watch for the page throwing the overlay away. Cheap: one observer for the
  // whole document, and it only ever does work when the host is really gone.
  function watch() {
    if (observer) return;
    observer = new MutationObserver(() => {
      if (!config || !config.enabled) return;
      if (host && host.isConnected) return;
      render();
    });
    observer.observe(document.documentElement, { childList: true, subtree: true });
  }

  // Every URL the overlay fetches goes through here.
  function url(path) {
    const base = config && config.base ? config.base : '';
    if (!base) return path;
    try {
      return new URL(path, base).href;
    } catch (_) {
      return base.replace(/\/$/, '') + path;
    }
  }

  function formatTime(now, withSeconds) {
    const parts = { hour: '2-digit', minute: '2-digit' };
    if (withSeconds) parts.second = '2-digit';
    return now.toLocaleTimeString(config.locale || undefined, parts);
  }

  function formatDate(now) {
    return now.toLocaleDateString(config.locale || undefined,
      { weekday: 'long', day: 'numeric', month: 'long', year: 'numeric' });
  }

  function styles() {
    const corner = CORNERS[config.position] || CORNERS['bottom-right'];
    // Sizes are in vmin so one configuration looks the same on a 1080p panel and
    // on a portrait 4K one -- signage is looked at from across a room, and a
    // pixel size that reads well on one screen is invisible on the other.
    return `
      :host { all: initial; }
      .box {
        position: fixed;
        ${corner}
        --m: ${config.margin}vmin;
        box-sizing: border-box;
        display: flex;
        flex-direction: column;
        gap: 0.4em;
        max-width: ${config.max_width}vw;
        padding: 0.7em 1em;
        border-radius: 0.5em;
        background: ${config.background};
        color: ${config.color};
        opacity: ${config.opacity};
        font-family: system-ui, sans-serif;
        font-size: ${config.size}vmin;
        line-height: 1.25;
        font-weight: 600;
        text-align: ${config.position.endsWith('right') ? 'right' : 'left'};
        white-space: pre-wrap;
        overflow-wrap: anywhere;
      }
      .box.plain { background: transparent; padding: 0; border-radius: 0; }
      .row { display: flex; align-items: center; gap: 0.6em; justify-content: inherit; }
      .clock { font-variant-numeric: tabular-nums; font-size: 1.6em; font-weight: 700; }
      .date { opacity: 0.85; font-size: 0.85em; font-weight: 500; }
      img.logo { max-width: 100%; max-height: 6em; object-fit: contain; }
      img.qr { width: ${config.qr_size}vmin; height: ${config.qr_size}vmin; background: #fff; padding: 0.3em; border-radius: 0.3em; }
      .qrwrap { display: flex; align-items: center; gap: 0.6em; }
      .qrlabel { font-size: 0.8em; font-weight: 500; }
    `;
  }

  function render() {
    if (!config || !config.enabled) {
      remove();
      return;
    }

    ensureHost();
    watch();

    const style = document.createElement('style');
    style.textContent = styles();

    const box = document.createElement('div');
    box.className = 'box' + (config.background === 'transparent' ? ' plain' : '');

    if (config.image_path) {
      const img = document.createElement('img');
      img.className = 'logo';
      // Joined onto the controller's own origin, never left relative: this DOM
      // lives in the displayed page's document, so a relative path would be
      // fetched from that dashboard's host and 404.
      img.src = url(config.image_path);
      img.alt = '';
      box.appendChild(img);
    }

    if (config.text) {
      const text = document.createElement('div');
      // textContent, never innerHTML: this string is typed in the admin UI and
      // lands inside pages we do not own.
      text.textContent = config.text;
      box.appendChild(text);
    }

    if (config.show_clock) {
      const clock = document.createElement('div');
      clock.className = 'clock';
      clock.dataset.clock = '1';
      box.appendChild(clock);
    }

    if (config.show_date) {
      const date = document.createElement('div');
      date.className = 'date';
      date.dataset.date = '1';
      box.appendChild(date);
    }

    if (config.qr_text) {
      const wrap = document.createElement('div');
      wrap.className = 'qrwrap';
      const img = document.createElement('img');
      img.className = 'qr';
      // Served by the controller: an offline device cannot fetch a QR library,
      // and an SVG scales to whatever the panel is.
      img.src = url('/api/qr.svg?text=' + encodeURIComponent(config.qr_text));
      img.alt = '';
      if (config.qr_label) {
        const label = document.createElement('div');
        label.className = 'qrlabel';
        label.textContent = config.qr_label;
        wrap.appendChild(label);
      }
      wrap.appendChild(img);
      box.appendChild(wrap);
    }

    shadow.replaceChildren(style, box);
    tick();

    if (ticker) clearInterval(ticker);
    if (config.show_clock || config.show_date) {
      ticker = setInterval(tick, config.show_seconds ? 1000 : 15000);
    }
  }

  function tick() {
    if (!shadow) return;
    const now = new Date();
    const clock = shadow.querySelector('[data-clock]');
    if (clock) clock.textContent = formatTime(now, !!config.show_seconds);
    const date = shadow.querySelector('[data-date]');
    if (date) date.textContent = formatDate(now);
  }

  function remove() {
    if (ticker) { clearInterval(ticker); ticker = null; }
    if (host) {
      if (host.hidePopover) { try { host.hidePopover(); } catch (_) { /* not shown */ } }
      host.remove();
    }
    host = null;
    shadow = null;
  }

  globalThis.__ov = {
    // One entry point, called with the whole configuration: the controller has
    // no way to know what the page currently shows, so every apply is a full
    // replacement rather than a diff.
    apply(next) {
      config = next && typeof next === 'object' ? next : null;
      if (!config || !config.enabled) {
        remove();
        return false;
      }
      config.margin = Number(config.margin) || 3;
      config.size = Number(config.size) || 2.4;
      config.max_width = Number(config.max_width) || 40;
      config.qr_size = Number(config.qr_size) || 14;
      config.opacity = config.opacity === undefined ? 1 : Number(config.opacity);
      config.background = config.background || 'rgba(0,0,0,0.65)';
      config.color = config.color || '#ffffff';
      config.position = CORNERS[config.position] ? config.position : 'bottom-right';
      render();
      return true;
    },
    clear: remove,
    state() {
      return {
        installed: true,
        enabled: !!(config && config.enabled),
        attached: !!(host && host.isConnected),
        inTopLayer: !!(host && host.matches && (() => {
          try { return host.matches(':popover-open'); } catch (_) { return false; }
        })()),
      };
    },
  };
})();
