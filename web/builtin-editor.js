// A small editor for a built-in (clock, banner, QR, countdown). Dependency-free;
// the `el` helper is passed in so it reuses the host page's.
//
// BuiltinEditor.create({ builtin, el }) -> { root, read() }
// `builtin` is a { kind, ... } object or null.
(() => {
  'use strict';

  const KINDS = [
    ['clock', 'Uhr'],
    ['banner', 'Banner / Nachricht'],
    ['qr', 'QR-Code'],
    ['countdown', 'Countdown'],
  ];

  function create({ builtin, el }) {
    const cfg = builtin && builtin.kind ? builtin : { kind: 'clock' };

    const field = (label, node) => el('label', { class: 'f' }, el('span', { text: label }), node);
    const checkbox = (checked) => { const c = el('input', { type: 'checkbox' }); c.checked = !!checked; return c; };
    // Text size in vmin (the overlay's unit); an "Automatisch" toggle fills the
    // cell instead. Returns the control node, an `isAuto()` and a `read()` that
    // gives the vmin number or null.
    const sizeControl = (value) => {
      const auto = checkbox(value === null || value === undefined);
      const num = el('input', { type: 'number', min: '0.5', max: '40', step: '0.5',
        value: typeof value === 'number' ? value : 8, style: 'width:70px' });
      const unit = el('span', { text: 'vmin' });
      const sync = () => { num.disabled = unit.hidden = num.hidden = auto.checked; };
      auto.addEventListener('change', sync);
      sync();
      const node = el('span', { class: 'inline' }, auto, el('span', { text: 'Automatisch' }), num, unit);
      return { node, auto, num, isAuto: () => auto.checked, read: () => (auto.checked ? null : Number(num.value) || null) };
    };

    const kind = el('select', {}, ...KINDS.map(([v, t]) => el('option', { value: v, text: t })));
    kind.value = cfg.kind;

    // Shared style: colours and a font the device has offline.
    const bg = el('input', { type: 'color', value: cfg.background_color || '#000000' });
    const fg = el('input', { type: 'color', value: cfg.text_color || '#ffffff' });
    const font = el('select', {},
      el('option', { value: 'sans', text: 'Sans' }),
      el('option', { value: 'serif', text: 'Serif' }),
      el('option', { value: 'mono', text: 'Monospace' }));
    font.value = cfg.font || 'sans';
    const colours = el('div', { class: 'grid' }, field('Hintergrund', bg), field('Textfarbe', fg), field('Schriftart', font));

    // Clock.
    const c24 = checkbox(cfg.format_24h !== false);
    const cSec = checkbox(cfg.show_seconds);
    const cDate = checkbox(cfg.show_date);
    const cTz = el('input', { value: cfg.timezone || '', placeholder: 'z. B. Europe/Berlin (leer = Gerät)', style: 'width:min(280px,100%)' });
    const cSize = sizeControl(cfg.text_size);
    const clockBox = el('div', {},
      el('div', { class: 'grid' },
        el('label', { class: 'inline' }, c24, el('span', { text: '24-Stunden' })),
        el('label', { class: 'inline' }, cSec, el('span', { text: 'Sekunden' })),
        el('label', { class: 'inline' }, cDate, el('span', { text: 'Datum' }))),
      el('div', { class: 'grid' }, field('Zeitzone', cTz), field('Textgröße', cSize.node)));

    // Banner. Auto = a headline that fills the cell; a fixed size = wrapped body
    // text, which is when the alignment choice matters.
    const bText = el('textarea', { rows: '2', style: 'width:100%' }); bText.value = cfg.text || '';
    const bSize = sizeControl(cfg.text_size);
    const bAlign = el('select', {}, el('option', { value: 'center', text: 'zentriert' }), el('option', { value: 'left', text: 'linksbündig' }), el('option', { value: 'right', text: 'rechtsbündig' }));
    bAlign.value = cfg.align || 'center';
    const bOverflow = el('select', {}, el('option', { value: 'clip', text: 'Abschneiden' }), el('option', { value: 'marquee', text: 'Lauftext' }));
    bOverflow.value = cfg.overflow === 'marquee' ? 'marquee' : 'clip';
    const bAlignRow = el('div', { class: 'grid' }, field('Ausrichtung', bAlign));
    const bannerBox = el('div', {},
      el('div', { class: 'grid' }, field('Text', bText)),
      el('div', { class: 'grid' }, field('Textgröße', bSize.node), field('Überlauf', bOverflow)),
      bAlignRow);
    // Alignment only matters for wrapped body text; a headline fills, a ticker moves.
    const syncBanner = () => { bAlignRow.hidden = bSize.isAuto() || bOverflow.value === 'marquee'; };
    bSize.auto.addEventListener('change', syncBanner);
    bOverflow.addEventListener('change', syncBanner);

    // QR.
    const qSource = el('select', {}, el('option', { value: 'text', text: 'Text / URL' }), el('option', { value: 'cast', text: 'Gast-Adresse (Übertragung)' }));
    qSource.value = cfg.source === 'cast' ? 'cast' : 'text';
    const qText = el('input', { value: cfg.qr_text || '', placeholder: 'https://…', style: 'width:min(320px,100%)' });
    const qLabel = el('input', { value: cfg.label || '', style: 'width:160px' });
    const qTextRow = el('div', { class: 'grid' }, field('Text / URL', qText));
    const qrBox = el('div', {},
      el('div', { class: 'grid' }, field('Quelle', qSource), field('Beschriftung', qLabel)),
      qTextRow);
    const syncQr = () => { qTextRow.hidden = qSource.value !== 'text'; };
    qSource.addEventListener('change', syncQr);

    // Countdown.
    const dTarget = el('input', { type: 'datetime-local' });
    if (cfg.target) { try { dTarget.value = new Date(cfg.target).toISOString().slice(0, 16); } catch (_) { /* leave empty */ } }
    const dLabel = el('input', { value: cfg.label || '', style: 'width:160px' });
    const dDone = el('input', { value: cfg.done_text || '', placeholder: 'z. B. Jetzt!', style: 'width:160px' });
    const dSec = checkbox(cfg.show_seconds);
    const dMs = checkbox(cfg.show_ms);
    const dFormat = el('select', {}, el('option', { value: 'words', text: 'Worte (3 T 6 Std)' }), el('option', { value: 'digital', text: 'Digital (00:00:00)' }));
    dFormat.value = cfg.digital ? 'digital' : 'words';
    const dSize = sizeControl(cfg.text_size);
    const countdownBox = el('div', {},
      el('div', { class: 'grid' }, field('Ziel', dTarget)),
      el('div', { class: 'grid' }, field('Label', dLabel), field('Text danach', dDone)),
      el('div', { class: 'grid' }, field('Format', dFormat),
        el('label', { class: 'inline' }, dSec, el('span', { text: 'Sekunden' })),
        el('label', { class: 'inline' }, dMs, el('span', { text: 'Millisekunden' }))),
      el('div', { class: 'grid' }, field('Textgröße', dSize.node)));

    const boxes = { clock: clockBox, banner: bannerBox, qr: qrBox, countdown: countdownBox };
    const syncKind = () => {
      for (const [k, node] of Object.entries(boxes)) node.hidden = k !== kind.value;
    };
    kind.addEventListener('change', syncKind);

    const root = el('div', { class: 'builtin-editor' },
      el('div', { class: 'grid' }, field('Typ', kind)),
      clockBox, bannerBox, qrBox, countdownBox,
      colours);

    syncKind(); syncBanner(); syncQr();

    return {
      root,
      read() {
        const style = { background_color: bg.value, text_color: fg.value, font: font.value };
        switch (kind.value) {
          case 'banner':
            return { kind: 'banner', text: bText.value, text_size: bSize.read(), align: bAlign.value, overflow: bOverflow.value, ...style };
          case 'qr':
            return { kind: 'qr', source: qSource.value, qr_text: qText.value.trim(), label: qLabel.value, ...style };
          case 'countdown':
            return { kind: 'countdown', target: dTarget.value ? new Date(dTarget.value).toISOString() : '', label: dLabel.value, done_text: dDone.value, show_seconds: dSec.checked, show_ms: dMs.checked, digital: dFormat.value === 'digital', text_size: dSize.read(), ...style };
          default:
            return { kind: 'clock', format_24h: c24.checked, show_seconds: cSec.checked, show_date: cDate.checked, timezone: cTz.value.trim(), text_size: cSize.read(), ...style };
        }
      },
    };
  }

  globalThis.BuiltinEditor = { create };
})();
