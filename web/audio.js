// The room-audio panel, shared by the guest page and the admin page.
//
// Two callers, two endpoints, same controls:
//
//  * `/api/cast/audio` — the guest's, guarded by `caster_only` and exempt from
//    basic auth, because somebody sharing a screen has to reach it.
//  * `/api/audio` — the operator's, behind the usual credentials, and usable
//    whether or not anybody is casting.
//
// One implementation on purpose: the two panels are the same knobs, and a second
// copy would drift the moment one of them gains a feature.

(function (global) {
  'use strict';

  const POLL_MS = 3000;

  // Built with DOM calls, never innerHTML: sink descriptions and stream labels
  // come from whatever pactl reports, i.e. from application names.
  function el(tag, props, ...children) {
    const node = document.createElement(tag);
    Object.assign(node, props || {});
    for (const child of children) if (child) node.append(child);
    return node;
  }

  /**
   * @param {object} options
   *   root      - element to fill; hidden while no audio backend exists
   *   endpoint  - '/api/audio' or '/api/cast/audio'
   *   heading   - optional headline text
   */
  function mount(options) {
    const root = options.root;
    const endpoint = options.endpoint;

    const sinkSelect = el('select');
    const sinkMute = el('button', { type: 'button', textContent: '🔊',
                                    title: 'Ausgang stumm schalten' });
    const sinkVolume = el('input', { type: 'range', min: '0', max: '100' });
    const sinkRow = el('div', { className: 'audiorow' },
      el('label', { textContent: 'Ausgang' }), sinkSelect, sinkMute, sinkVolume);
    const streams = el('div');
    const hint = el('p', { className: 'muted' });

    root.replaceChildren(
      ...(options.heading ? [el('h2', { textContent: options.heading })] : []),
      sinkRow, streams, hint);
    root.hidden = true;

    let timer = null;
    // While a slider is under the thumb the poll must not redraw it, or the
    // value jumps back mid-drag.
    let busy = false;

    async function call(body) {
      const res = await fetch(endpoint, body ? {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
      } : {});
      if (!res.ok) return null;
      return res.json();
    }

    function render(state) {
      if (!state || !state.available) {
        root.hidden = true;
        return;
      }
      root.hidden = false;
      if (busy) return;

      const current = state.sinks.find((s) => s.is_default);
      sinkSelect.replaceChildren();
      for (const sink of state.sinks) {
        sinkSelect.append(el('option', { value: sink.name, textContent: sink.description }));
      }
      if (current) {
        sinkSelect.value = current.name;
        sinkVolume.value = current.volume;
        sinkMute.textContent = current.muted ? '🔇' : '🔊';
      }
      sinkRow.hidden = state.sinks.length === 0;

      streams.replaceChildren();
      for (const stream of state.streams) {
        const name = el('span', {
          className: 'name' + (stream.is_cast ? ' cast' : ''),
          textContent: stream.is_cast ? (options.castLabel || 'Übertragung') : stream.label,
          title: stream.label,
        });

        const mute = el('button', { type: 'button', textContent: stream.muted ? '🔇' : '🔊' });
        mute.addEventListener('click', async () => {
          render(await call({ target: 'stream', index: stream.index,
                              action: 'mute', value: !stream.muted }));
        });

        const slider = el('input', { type: 'range', min: '0', max: '100',
                                     value: String(stream.volume) });
        slider.addEventListener('pointerdown', () => { busy = true; });
        slider.addEventListener('pointerup', () => { busy = false; });
        slider.addEventListener('change', async () => {
          busy = false;
          render(await call({ target: 'stream', index: stream.index,
                              action: 'volume', value: Number(slider.value) }));
        });

        streams.append(el('div', { className: 'audiorow' }, name, mute, slider));
      }

      hint.textContent = state.streams.length ? '' : 'Zurzeit gibt niemand Ton aus.';
    }

    sinkVolume.addEventListener('pointerdown', () => { busy = true; });
    sinkVolume.addEventListener('pointerup', () => { busy = false; });
    sinkVolume.addEventListener('change', async () => {
      busy = false;
      render(await call({ target: 'sink', action: 'volume', value: Number(sinkVolume.value) }));
    });
    sinkMute.addEventListener('click', async () => {
      const muted = sinkMute.textContent === '🔇';
      render(await call({ target: 'sink', action: 'mute', value: !muted }));
    });
    sinkSelect.addEventListener('change', async () => {
      render(await call({ target: 'sink', action: 'select', name: sinkSelect.value }));
    });

    async function tick() {
      render(await call(null));
    }

    return {
      start() {
        this.stop();
        tick();
        timer = setInterval(tick, POLL_MS);
      },
      stop() {
        if (timer) { clearInterval(timer); timer = null; }
        root.hidden = true;
      },
      refresh: tick,
    };
  }

  global.AudioPanel = { mount };
})(globalThis);
