// An editor's draft on the content pages: a bar with what is waiting, and a
// "vorgeschlagen" mark on the cards and rows it touches. The server records the
// editor's writes instead of executing them; this only makes that visible.
(() => {
  'use strict';

  // One readable line per recorded request, as DOM: what it touches by name
  // and content rather than by id, and a preview on hover for an asset.
  // `bundle` supplies `refs` (names and assets as they read now) and the
  // sibling requests, which is where a placeholder like `new:2` is resolved.
  function render(request, bundle) {
    const refs = (bundle && bundle.refs) || { assets: {}, playlists: {}, items: {} };
    const siblings = (bundle && bundle.requests) || [];
    const body = request.body && typeof request.body === 'object' ? request.body : {};
    const before = request.before && typeof request.before === 'object' && !Array.isArray(request.before)
      ? request.before : null;
    const created = (placeholder) => siblings.find((r) => r.placeholder === placeholder && r.method === 'POST');
    const out = document.createElement('span');
    const text = (t) => out.append(document.createTextNode(t));

    const playlistName = (id) => {
      if (id === null || id === undefined || id === '') return '–';
      if (refs.playlists[String(id)]) return `„${refs.playlists[String(id)]}“`;
      const made = created(String(id));
      if (made && made.body && made.body.name) return `„${made.body.name}“ (neu)`;
      return `#${id}`;
    };
    // An asset or item named by what it shows, hoverable when there is a file.
    const assetNode = (id, fallback) => {
      const asset = refs.assets[String(id)] || (fallback && fallback.local_path ? fallback : null);
      const node = document.createElement('span');
      node.textContent = `„${(asset && asset.filename) || (fallback && fallback.filename) || `Asset #${id}`}“`;
      if (asset && globalThis.Preview && Preview.kindOf(asset)) {
        node.style.textDecoration = 'underline dotted';
        node.style.cursor = 'help';
        Preview.hover(node, asset);
      }
      return node;
    };
    const urlNode = (url) => {
      const node = document.createElement('span');
      let short = url;
      try { const u = new URL(url); short = u.host + u.pathname; } catch (_) { /* as given */ }
      node.textContent = short.length > 60 ? short.slice(0, 57) + '…' : short;
      node.title = url;
      return node;
    };
    const content = (source) => {
      if (!source) return null;
      if (source.asset_id !== null && source.asset_id !== undefined) return assetNode(source.asset_id, source);
      if (source.url) return urlNode(source.url);
      return null;
    };
    const itemSource = (id) => refs.items[String(id)] || before || (created(String(id)) || {}).body || null;
    const item = (id) => {
      const node = content(itemSource(id));
      if (node) out.append(node); else text(`Item #${id}`);
    };
    const inPlaylist = (source) => {
      if (source && source.playlist_id !== undefined && source.playlist_id !== null) {
        text(` in ${playlistName(source.playlist_id)}`);
      }
    };

    // Only what differs from the object as it read when proposed: a card saves
    // its whole form, so the body alone would list every field on it.
    const shown = (k, v) => {
      if (v === null || v === undefined || v === '') return '–';
      if (k === 'playlist_id' || k === 'default_playlist_id') return playlistName(v);
      if (k === 'asset_id') return (refs.assets[String(v)] || {}).filename || `#${v}`;
      if (k === 'duration') return `${v} s`;
      if (typeof v === 'boolean') return v ? 'ja' : 'nein';
      return typeof v === 'object' ? JSON.stringify(v) : String(v);
    };
    // The read and the write name an item's overlay differently, and an overlay
    // that draws nothing is stored as null -- so an untouched overlay section
    // must not read as a change.
    const readKey = (k) => (k === 'overlay' && before && 'overlay_config' in before ? 'overlay_config' : k);
    const normal = (k, v) => {
      if (k !== 'overlay' || !v || typeof v !== 'object') return v;
      const draws = v.text || v.image_asset_id || v.qr_text || v.color;
      return draws ? v : null;
    };
    const LABELS = {
      duration: 'Dauer', enabled: 'aktiv', start_date: 'ab', end_date: 'bis', keep_loaded: 'geladen halten',
      play_order: 'Position', playlist_id: 'Playlist', asset_id: 'Datei', url: 'URL', name: 'Name',
      fit_mode: 'Einpassen', fit_background: 'Hintergrund', overlay: 'Overlay', scroll_config: 'Scrollen',
    };
    const detail = () => {
      const changed = Object.keys(body).filter((k) => k !== 'filename'
        && (!before || !(readKey(k) in before)
            || JSON.stringify(normal(k, before[readKey(k)])) !== JSON.stringify(normal(k, body[k]))));
      if (!changed.length) return;
      text(': ' + changed.map((k) => {
        const label = LABELS[k] || k;
        return before && readKey(k) in before
          ? `${label} ${shown(k, before[readKey(k)])} → ${shown(k, body[k])}`
          : `${label} ${shown(k, body[k])}`;
      }).join(', '));
    };

    const segments = request.path.split('/');
    const kind = segments[2];
    const id = segments[3] || '';
    if (request.method === 'UPLOAD') {
      text('Upload ');
      out.append(assetNode(id, { filename: body.filename }));
    } else if (request.method === 'POST' && request.path === '/api/playlists') {
      text(`Neue Playlist „${body.name || ''}“`);
    } else if (request.method === 'POST' && request.path === '/api/playlist') {
      text(`Neues Item in ${playlistName(body.playlist_id)}: `);
      const node = content(body);
      if (node) out.append(node); else text('(ohne Inhalt)');
      if (body.duration) text(`, ${body.duration} s`);
    } else if (request.path.endsWith('/move')) {
      item(id);
      inPlaylist(itemSource(id));
      text(body.direction === 'up' ? ' nach oben' : ' nach unten');
    } else if (request.path.endsWith('/schedule')) {
      text(`Zeitplan von „${segments[3]}“: Standard ${playlistName(body.default_playlist_id)}`);
      const windows = Array.isArray(body.windows) ? body.windows : [];
      text(windows.length ? `, ${windows.length} Zeitfenster` : ', keine Zeitfenster');
    } else if (kind === 'playlist') {
      item(id);
      inPlaylist(itemSource(id));
      if (request.method === 'DELETE') text(' löschen'); else { text(' ändern'); detail(); }
    } else if (kind === 'playlists') {
      text(`Playlist ${before && before.name ? `„${before.name}“` : playlistName(id)}`);
      if (request.method === 'DELETE') text(' löschen'); else { text(' ändern'); detail(); }
    } else if (kind === 'assets') {
      text('Asset ');
      out.append(assetNode(id, before));
      if (request.method === 'DELETE') text(' löschen'); else { text(' ändern'); detail(); }
    } else {
      text(`${request.method} ${request.path}`);
      detail();
    }
    return out;
  }

  // The same line as plain text, where no DOM is wanted.
  function describe(request, bundle) {
    return render(request, bundle).textContent;
  }

  async function draft() {
    try {
      const res = await fetch('/api/changesets/draft');
      return res.ok ? await res.json() : null;
    } catch (_) {
      return null;
    }
  }

  function mark(requests) {
    const touched = new Set();
    for (const r of requests) {
      const m = r.path.match(/^\/api\/(playlist|assets)\/(\d+)/);
      if (m) touched.add(`${m[1]}:${m[2]}`);
    }
    for (const card of document.querySelectorAll('.card[data-id]')) {
      const on = touched.has(`playlist:${card.dataset.id}`);
      let badge = card.querySelector('.proposed-badge');
      if (on && !badge) {
        badge = document.createElement('span');
        badge.className = 'badge proposed-badge';
        badge.textContent = 'vorgeschlagen';
        (card.querySelector('.card-head') || card).append(badge);
      } else if (!on && badge) {
        badge.remove();
      }
    }
    for (const row of document.querySelectorAll('#assetsBody tr')) {
      const id = row.firstChild ? row.firstChild.textContent : '';
      row.style.background = touched.has(`assets:${id}`) ? '#fff5e0' : '';
    }
  }

  let box = null;
  let list = null;
  let mineLink = null;

  // Only the count lives in the bar; the list is its own page, proposals.html.
  async function refreshMine() {
    if (!mineLink) return;
    let data = { unseen: 0 };
    try {
      const res = await fetch('/api/changesets/mine');
      if (res.ok) data = await res.json();
    } catch (_) { /* next poll */ }
    mineLink.textContent = data.unseen ? `Meine Vorschläge (${data.unseen} neu)` : 'Meine Vorschläge';
    mineLink.style.fontWeight = data.unseen ? 'bold' : '';
  }

  async function refresh() {
    const current = await draft();
    const requests = (current && current.requests) || [];
    mark(requests);
    if (!box) return;
    box.querySelector('.count').textContent = requests.length
      ? `${requests.length} Änderung(en) im Entwurf`
      : 'Kein Entwurf – Änderungen werden als Vorschlag gesammelt.';
    // Only the draft's own buttons.
    for (const button of box.querySelectorAll('button.draft-action')) button.disabled = !requests.length;
    list.replaceChildren(...requests.map((r) => {
      const li = document.createElement('li');
      li.append(render(r, current), ' ');
      const drop = document.createElement('button');
      drop.textContent = '×';
      drop.title = 'Aus dem Entwurf entfernen';
      drop.addEventListener('click', async () => {
        await fetch(`/api/changesets/draft/requests/${r.id}`, { method: 'DELETE' });
        refresh();
      });
      li.append(drop);
      return li;
    }));
  }

  async function start() {
    const me = await Me.load();
    if (!me || me.role !== 'editor') return;
    box = document.createElement('div');
    box.className = 'proposal-bar';
    box.style.cssText = 'font: 13px system-ui, sans-serif; background: #fff5e0; color: #5a3d00;'
      + 'padding: .45rem .6rem; border-radius: 4px; margin: 0 0 .6rem;';
    const count = document.createElement('span');
    count.className = 'count';
    const toggle = document.createElement('button');
    toggle.className = 'draft-action';
    toggle.textContent = 'Ansehen';
    const submit = document.createElement('button');
    submit.className = 'draft-action';
    submit.textContent = 'Einreichen';
    const discard = document.createElement('button');
    discard.className = 'draft-action';
    discard.textContent = 'Verwerfen';
    list = document.createElement('ul');
    list.hidden = true;
    mineLink = document.createElement('a');
    mineLink.href = '/proposals.html';
    mineLink.textContent = 'Meine Vorschläge';
    toggle.addEventListener('click', () => { list.hidden = !list.hidden; });
    submit.addEventListener('click', async () => {
      const res = await fetch('/api/changesets/draft/submit', { method: 'POST' });
      if (res.ok) alert('Eingereicht – ein Manager gibt die Änderungen frei.');
      refresh();
    });
    discard.addEventListener('click', async () => {
      if (!confirm('Alle Änderungen im Entwurf verwerfen?')) return;
      await fetch('/api/changesets/draft', { method: 'DELETE' });
      refresh();
    });
    box.append(count, ' ', toggle, ' ', submit, ' ', discard, ' ', mineLink, list);
    const anchor = document.querySelector('.me-bar');
    if (anchor) anchor.after(box); else document.body.prepend(box);
    refresh();
    refreshMine();
    setInterval(refresh, 3000);
    setInterval(refreshMine, 10000);
  }

  globalThis.Proposals = { start, refresh, refreshMine, render, describe };
})();
