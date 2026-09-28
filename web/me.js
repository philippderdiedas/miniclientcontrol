// Who is signed in, for the operator pages: a bar at the top with the account,
// the links its role may use and "Abmelden", a banner while no account exists,
// and `Me.atLeast(role)` for hiding what a role cannot do. The server enforces
// every rule; this only keeps the pages from offering what would be refused.
(() => {
  'use strict';
  const RANK = { editor: 1, manager: 2, admin: 3 };
  let current = null;

  async function load() {
    if (current) return current;
    try {
      const res = await fetch('/api/me');
      current = res.ok ? await res.json() : null;
    } catch (_) {
      current = null;
    }
    return current;
  }

  const atLeast = (role) => !!current && (RANK[current.role] || 0) >= RANK[role];

  function link(href, text) {
    const a = document.createElement('a');
    a.href = href;
    a.textContent = text;
    return a;
  }

  // Inserted at the top of <body>. Built with createElement: the name is an
  // account's, typed by somebody.
  async function bar() {
    const me = await load();
    if (!me) return;
    const box = document.createElement('div');
    box.className = 'me-bar';
    box.style.cssText = 'font: 13px system-ui, sans-serif; padding: .35rem .6rem; margin: 0 0 .6rem;'
      + 'border-radius: 4px; display: flex; gap: .8rem; align-items: center; flex-wrap: wrap;';
    if (me.open) {
      box.style.background = '#fff5e0';
      box.style.color = '#8a5a00';
      box.append(
        document.createTextNode('Kein Konto angelegt – die Verwaltung ist im Netz offen. '),
        link('/users.html', 'Ein Admin-Konto anlegen'));
    } else {
      box.style.background = '#f2f2f2';
      const who = document.createElement('span');
      who.textContent = `Angemeldet als ${me.name} (${me.role})`;
      box.append(who);
      if (atLeast('manager')) {
        const approvals = link('/approvals.html', 'Freigaben');
        box.append(approvals);
        fetch('/api/changesets?state=submitted')
          .then((res) => (res.ok ? res.json() : []))
          .then((list) => { if (list.length) approvals.textContent = `Freigaben (${list.length})`; })
          .catch(() => {});
      }
      if (atLeast('admin')) box.append(link('/users.html', 'Benutzer'));
      // Tokens belong to an account; the command-line credential has none.
      if (me.account) box.append(link('/tokens.html', 'API-Tokens'));
      if (me.name !== undefined) {
        const out = document.createElement('a');
        out.href = '#';
        out.textContent = 'Abmelden';
        out.addEventListener('click', async (event) => {
          event.preventDefault();
          await fetch('/api/logout', { method: 'POST' });
          location.href = '/login.html';
        });
        box.append(out);
      }
    }
    document.body.prepend(box);
  }

  globalThis.Me = { load, atLeast, bar, RANK };
})();
