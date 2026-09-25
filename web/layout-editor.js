// A layout editor: widgets on a 24x24 grid, dragged and resized with the
// pointer, snapping to the grid. Dependency-free; the scroll editor and the
// asset picker are passed in so it reuses the playlist page's own.
//
// LayoutEditor.create({ layout, aspect, assets, scrollEditor, assetSelect, el })
//   -> { root, read() }
// `layout` is { widgets: [...] } or null. `aspect` is width/height of the screen.
//
// Box mode positions a *single* box with no source, for the overlay:
// LayoutEditor.create({ boxMode: true, region, allowEmpty, aspect, el })
//   -> { root, read() }  where read() returns {x,y,w,h} or null (when allowEmpty).
(() => {
  'use strict';

  const GRID = 24;

  const TEMPLATES = {
    'Vollbild': [[0, 0, 24, 24]],
    'L-Form': [[0, 0, 18, 20], [18, 0, 6, 20], [0, 20, 24, 4]],
    'Haupt + Leiste': [[0, 0, 24, 20], [0, 20, 24, 4]],
    'Haupt + Spalte': [[0, 0, 18, 24], [18, 0, 6, 24]],
    '50 / 50': [[0, 0, 12, 24], [12, 0, 12, 24]],
    '2 x 2': [[0, 0, 12, 12], [12, 0, 12, 12], [0, 12, 12, 12], [12, 12, 12, 12]],
  };

  // Mirrors OverlayRegion::PRESETS in src/settings.rs -- keep the two in step.
  // (The Rust table has no "Mitte"; no legacy corner maps to it.)
  const OVERLAY_PRESETS = {
    'Oben links': [0, 0, 6, 4], 'Oben mitte': [9, 0, 6, 4], 'Oben rechts': [18, 0, 6, 4],
    'Unten links': [0, 20, 6, 4], 'Unten mitte': [9, 20, 6, 4], 'Unten rechts': [18, 20, 6, 4],
    'Mitte': [9, 10, 6, 4],
  };

  function overlaps(a, b) {
    return !(a.x + a.w <= b.x || b.x + b.w <= a.x || a.y + a.h <= b.y || b.y + b.h <= a.y);
  }

  function fits(box, others) {
    if (box.x < 0 || box.y < 0 || box.x + box.w > GRID || box.y + box.h > GRID || box.w < 1 || box.h < 1) {
      return false;
    }
    return !others.some((o) => overlaps(box, o));
  }

  function create({ layout, aspect, assets, scrollEditor, assetSelect, el, boxMode, region, allowEmpty }) {
    const ratio = aspect && aspect > 0 ? aspect : 16 / 9;
    const MAX = boxMode ? 1 : 12;
    // Widgets carry their editing state; `box` is the grid rectangle. In box
    // mode there is one box and no source.
    const widgets = boxMode
      ? (region ? [{ box: { x: region.x, y: region.y, w: region.w, h: region.h }, source: null }] : [])
      : (layout && Array.isArray(layout.widgets) ? layout.widgets : []).map((w) => ({
          box: { x: w.x, y: w.y, w: w.w, h: w.h },
          source: w.source && w.source.kind ? w.source
            : (w.source && w.source.asset_id != null ? { asset_id: w.source.asset_id } : { url: (w.source && w.source.url) || '' }),
          scroll_config: w.scroll_config || { type: 'None' },
          fit_mode: w.fit_mode || 'contain',
        }));

    let selected = null;

    const canvas = el('div', { class: 'layout-canvas' });
    canvas.style.cssText = `position:relative; width:100%; max-width:640px; aspect-ratio:${ratio};`
      + 'border:1px solid #888; background:#eef;'
      + `background-image:linear-gradient(#0001 1px,transparent 1px),linear-gradient(90deg,#0001 1px,transparent 1px);`
      + `background-size:calc(100%/${GRID}) calc(100%/${GRID});`;

    const fields = el('div', { class: 'layout-fields' });
    const hint = el('p', { class: 'muted' });
    const editors = new Map(); // widget -> its field editors, built once

    function cell() {
      return { w: canvas.clientWidth / GRID, h: canvas.clientHeight / GRID };
    }

    function place(node, box) {
      node.style.left = `${(box.x / GRID) * 100}%`;
      node.style.top = `${(box.y / GRID) * 100}%`;
      node.style.width = `${(box.w / GRID) * 100}%`;
      node.style.height = `${(box.h / GRID) * 100}%`;
    }

    function label(widget) {
      const s = widget.source;
      if (!s) return 'Overlay';
      if (s.kind) return 'Built-in: ' + s.kind;
      return s.asset_id != null ? `Asset ${s.asset_id}` : (s.url ? s.url.replace(/^https?:\/\//, '') : 'leer');
    }

    function renderFields() {
      fields.replaceChildren();
      // Box mode has no source/scroll/fit: the box is only a place on the grid.
      if (boxMode) return;
      if (!selected) {
        fields.append(el('p', { class: 'muted', text: 'Ein Widget wählen, um seine Quelle zu setzen.' }));
        return;
      }
      let e = editors.get(selected);
      if (!e) {
        const kind = el('select', {},
          el('option', { value: 'url', text: 'Seite (URL)' }),
          el('option', { value: 'asset', text: 'Asset' }),
          el('option', { value: 'builtin', text: 'Built-in' }));
        kind.value = selected.source && selected.source.kind ? 'builtin'
          : (selected.source && selected.source.asset_id != null ? 'asset' : 'url');
        const url = el('input', { value: (selected.source && selected.source.url) || '', placeholder: 'https://...', style: 'width:min(360px,100%)' });
        const asset = assetSelect(selected.source && selected.source.asset_id);
        const scroll = scrollEditor(selected.scroll_config);
        const fit = el('select', {}, ...['contain', 'cover', 'fill', 'none', 'width', 'height'].map((v) =>
          el('option', { value: v, text: v })));
        fit.value = selected.fit_mode;
        // The built-in sub-editor is dependency-free and shares this page's `el`.
        const builtin = BuiltinEditor.create({ builtin: selected.source && selected.source.kind ? selected.source : null, el });
        builtin.root.addEventListener('input', commit);
        builtin.root.addEventListener('change', commit);
        const sync = () => {
          const k = kind.value;
          url.hidden = k !== 'url';
          asset.hidden = k !== 'asset';
          builtin.root.hidden = k !== 'builtin';
          // A built-in has no scroll or fit -- it renders itself into the cell.
          scroll.root.hidden = k === 'builtin';
        };
        kind.addEventListener('change', () => { sync(); commit(); });
        [url, asset, fit].forEach((n) => n.addEventListener('input', commit));
        scroll.onInput(commit);
        sync();
        e = { kind, url, asset, scroll, fit, builtin };
        editors.set(selected, e);
      }
      const field = (t, n) => el('label', { class: 'f' }, el('span', { text: t }), n);
      const source = e.kind.value === 'url' ? field('URL', e.url)
        : (e.kind.value === 'asset' ? field('Asset', e.asset) : el('span'));
      fields.append(el('div', { class: 'grid' },
        field('Quelle', e.kind),
        source,
        e.kind.value === 'builtin' ? el('span') : field('Einpassen', e.fit)),
        e.builtin.root,
        e.scroll.root);
    }

    function commit() {
      if (!selected) return;
      const e = editors.get(selected);
      if (!e) return;
      if (e.kind.value === 'builtin') {
        selected.source = e.builtin.read();
      } else if (e.kind.value === 'asset') {
        selected.source = { asset_id: e.asset.value ? Number(e.asset.value) : null };
      } else {
        selected.source = { url: e.url.value.trim() };
      }
      selected.scroll_config = e.scroll.read();
      selected.fit_mode = e.fit.value;
      const node = selected._node;
      if (node) node.querySelector('.wlabel').textContent = label(selected);
    }

    function drag(node, widget, handle, mode) {
      handle.addEventListener('pointerdown', (down) => {
        down.preventDefault();
        // The resize handle sits inside the node that is itself the move handle;
        // without this the pointerdown bubbles up and starts a move as well, and
        // the two drags fight -- which reads as "resize does nothing".
        down.stopPropagation();
        select(widget);
        const c = cell();
        const start = { ...widget.box };
        const move = (e) => {
          const dx = Math.round((e.clientX - down.clientX) / c.w);
          const dy = Math.round((e.clientY - down.clientY) / c.h);
          const next = { ...start };
          if (mode === 'move') { next.x = start.x + dx; next.y = start.y + dy; }
          else { next.w = start.w + dx; next.h = start.h + dy; }
          const others = widgets.filter((w) => w !== widget).map((w) => w.box);
          if (fits(next, others)) { widget.box = next; place(node, next); }
        };
        const up = () => {
          window.removeEventListener('pointermove', move);
          window.removeEventListener('pointerup', up);
        };
        window.addEventListener('pointermove', move);
        window.addEventListener('pointerup', up);
      });
    }

    function widgetNode(widget) {
      const node = el('div', { class: 'wbox' });
      node.style.cssText = 'position:absolute; box-sizing:border-box; border:2px solid #0b6b3a;'
        + 'background:#0b6b3a22; overflow:hidden; cursor:move; font:12px system-ui;';
      const text = el('div', { class: 'wlabel', text: label(widget) });
      text.style.cssText = 'padding:2px 4px; pointer-events:none;';
      const resize = el('div', {});
      resize.style.cssText = 'position:absolute; right:0; bottom:0; width:12px; height:12px; background:#0b6b3a; cursor:nwse-resize;';
      node.append(text, resize);
      // The global overlay's box cannot be removed (it must sit somewhere); a
      // widget or an item overlay (allowEmpty) can.
      if (!boxMode || allowEmpty) {
        const remove = el('button', { type: 'button', text: '×', title: boxMode ? 'Auf global zurück' : 'Widget entfernen' });
        remove.style.cssText = 'position:absolute; top:0; right:0; border:0; background:#a11; color:#fff; cursor:pointer;';
        remove.addEventListener('pointerdown', (e) => e.stopPropagation());
        remove.addEventListener('click', () => {
          const i = widgets.indexOf(widget);
          if (i >= 0) { widgets.splice(i, 1); editors.delete(widget); if (selected === widget) selected = null; render(); }
        });
        node.append(remove);
      }
      widget._node = node;
      place(node, widget.box);
      drag(node, widget, node, 'move');
      drag(node, widget, resize, 'resize');
      return node;
    }

    function select(widget) {
      selected = widget;
      for (const w of widgets) {
        if (w._node) w._node.style.outline = w === widget ? '2px solid #05f' : 'none';
      }
      renderFields();
    }

    function render() {
      canvas.replaceChildren();
      for (const w of widgets) canvas.append(widgetNode(w));
      if (selected && !widgets.includes(selected)) selected = null;
      select(selected || widgets[0] || null);
      updateHint();
    }

    function addWidget() {
      // First free 6x6 spot from the top-left, else stack.
      const others = widgets.map((w) => w.box);
      for (let y = 0; y + 6 <= GRID; y += 1) {
        for (let x = 0; x + 6 <= GRID; x += 1) {
          const box = { x, y, w: 6, h: 6 };
          if (fits(box, others)) {
            const widget = { box, source: { url: '' }, scroll_config: { type: 'None' }, fit_mode: 'contain' };
            widgets.push(widget);
            render();
            select(widget);
            return;
          }
        }
      }
    }

    function applyTemplate(name) {
      const boxes = TEMPLATES[name];
      if (!boxes) return;
      widgets.length = 0;
      editors.clear();
      selected = null;
      for (const [x, y, w, h] of boxes) {
        widgets.push({ box: { x, y, w, h }, source: { url: '' }, scroll_config: { type: 'None' }, fit_mode: 'contain' });
      }
      render();
    }

    function updateHint() {
      if (boxMode) {
        const b = widgets[0] && widgets[0].box;
        hint.textContent = b
          ? `Region ${b.x},${b.y} · ${b.w}×${b.h} — ziehen/skalieren oder eine Vorlage wählen.`
          : 'Zur globalen Box. Eine Vorlage wählen, um eine eigene Region zu setzen.';
        return;
      }
      const cross = widgets.filter((w) => w.source && w.source.url).length;
      hint.textContent = cross >= 4
        ? `${widgets.length} Widgets — ${cross} eigene Seiten; auf schwacher Hardware (Raspberry Pi) kann das knapp werden.`
        : `${widgets.length} Widget(s).`;
    }

    // Box mode: set (or replace) the single box from a preset rectangle.
    function setBox([x, y, w, h]) {
      widgets.length = 0;
      editors.clear();
      selected = null;
      widgets.push({ box: { x, y, w, h }, source: null });
      render();
    }

    // Layout background (behind and between the widgets). A checkbox, because a
    // colour input has no empty state; unchecked = the default black.
    const bgOwn = el('input', { type: 'checkbox' });
    bgOwn.checked = !!(layout && layout.background);
    const bgInput = el('input', { type: 'color', value: (layout && layout.background) || '#000000' });

    let toolbar;
    if (boxMode) {
      // A preset per named place, plus (when the box may be dropped) a way back
      // to the global box.
      const presetSelect = el('select', {}, el('option', { value: '', text: 'Vorlage …' }),
        ...Object.keys(OVERLAY_PRESETS).map((name) => el('option', { value: name, text: name })));
      presetSelect.addEventListener('change', () => {
        if (presetSelect.value) setBox(OVERLAY_PRESETS[presetSelect.value]);
        presetSelect.value = '';
      });
      const kids = [presetSelect, hint];
      if (allowEmpty) {
        const clearBtn = el('button', { type: 'button', text: 'Aus (global)' });
        clearBtn.addEventListener('click', () => { widgets.length = 0; editors.clear(); selected = null; render(); });
        kids.splice(1, 0, clearBtn);
      }
      toolbar = el('div', { class: 'row' }, ...kids);
    } else {
      const templateSelect = el('select', {}, el('option', { value: '', text: 'Vorlage …' }),
        ...Object.keys(TEMPLATES).map((name) => el('option', { value: name, text: name })));
      templateSelect.addEventListener('change', () => {
        if (!templateSelect.value) return;
        if (widgets.length && !confirm('Aktuelle Widgets durch die Vorlage ersetzen?')) { templateSelect.value = ''; return; }
        applyTemplate(templateSelect.value);
        templateSelect.value = '';
      });
      const addBtn = el('button', { type: 'button', text: '+ Widget' });
      addBtn.addEventListener('click', () => { if (widgets.length < MAX) addWidget(); });
      const bgLabel = el('label', { class: 'inline' }, bgOwn, el('span', { text: 'Hintergrund' }), bgInput);
      toolbar = el('div', { class: 'row' }, templateSelect, addBtn, bgLabel, hint);
    }

    const root = el('div', { class: 'layout-editor' }, toolbar, canvas, fields);

    // The canvas needs a size before the first placement; render after layout.
    requestAnimationFrame(render);

    return {
      root,
      read() {
        if (boxMode) {
          const b = widgets[0] && widgets[0].box;
          return b ? { x: b.x, y: b.y, w: b.w, h: b.h } : null;
        }
        return {
          widgets: widgets.map((w) => ({
            x: w.box.x, y: w.box.y, w: w.box.w, h: w.box.h,
            source: w.source.kind ? w.source
              : (w.source.asset_id != null ? { asset_id: w.source.asset_id } : { url: w.source.url }),
            scroll_config: w.scroll_config,
            fit_mode: w.fit_mode,
          })),
          background: bgOwn.checked ? bgInput.value : null,
        };
      },
    };
  }

  globalThis.LayoutEditor = { create, OVERLAY_PRESETS };
})();
