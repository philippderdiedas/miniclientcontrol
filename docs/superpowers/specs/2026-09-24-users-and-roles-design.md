# Users, roles and proposed changes

**Status:** implemented
**Date:** 2026-09-24

## What and why

One set of basic-auth credentials covers everything today: whoever may change a
playlist may also change the webhooks, the casting switch and the password itself.
A venue wants three kinds of people instead:

- **Admin** — the system side: settings, webhooks, displays' labels, casting,
  audio, users.
- **Manager** — content: assets, playlists, items, timetables; overrides and "Play
  now"; approves what an editor proposes.
- **Editor** (Redakteur) — proposes content changes, which take effect only when a
  manager approves them.

## Decisions taken

1. **Accounts with sessions for browsers, HTTP Basic for scripts.** Both check the
   same accounts and carry the account's role. Existing scripts keep working,
   because today's credential becomes the first admin account.
2. **Open until the first account exists**, exactly as a device without
   credentials is today — an upgrade must not lock a venue out of its own screen.
   The admin pages say so in a banner.
3. **Proposals are recorded requests, not a new write path.** An editor's content
   write is not executed but stored — method, path, body, and a snapshot of the
   object *before* — in a second table. A manager applies a proposal by replaying
   its requests through the same router, as themself. The handlers stay as they
   are; only a middleware in front of the content write routes is new. (A shared
   transactional write layer was the alternative; it would have made a proposal
   strictly all-or-nothing at the price of rebuilding every content handler.)
4. **A proposal is a bundle** (Sammelvorschlag): an editor collects changes in a
   draft and submits them together; a manager approves or rejects the bundle.
5. **A stale proposal is never applied.** Before the first request is replayed,
   every object the bundle touches is read again and compared with its snapshot;
   one difference and the bundle is *veraltet* and can only be rejected. Nothing
   is silently overwritten.

## Accounts and signing in

Tables, created in `db::run_migrations`:

```sql
CREATE TABLE IF NOT EXISTS users (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    name          TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,          -- the PBKDF2 format settings.rs already uses
    role          TEXT NOT NULL CHECK (role IN ('admin', 'manager', 'editor')),
    disabled      BOOLEAN NOT NULL DEFAULT 0,
    created_at    DATETIME DEFAULT CURRENT_TIMESTAMP
);
CREATE TABLE IF NOT EXISTS sessions (
    token_hash TEXT PRIMARY KEY,          -- SHA-256 of the cookie value; the value is never stored
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    expires_at DATETIME NOT NULL
);
```

- **`POST /api/login`** `{ name, password }` sets `mcc_session` (`HttpOnly`,
  `SameSite=Strict`, `Path=/`, `Secure` when served over TLS) and answers
  `{ name, role }`. A session expires after **12 hours without a request**; each
  authenticated request moves the expiry. Sessions live in SQLite so a restart
  signs nobody out. **`POST /api/logout`** deletes it. **`GET /api/me`** answers
  `{ name, role, open }` — `open: true` in open mode — and is what the pages use to
  hide what a role cannot do.
- **Failed sign-ins are limited per address**: 10 failures in 5 minutes answer
  `429` for the rest of the window. PBKDF2 is slow on purpose, which is also why a
  flood of guesses must not reach it.
- **`web/login.html`** is the page an unauthenticated HTML request is redirected
  to, with `?next=` back to where it came from. `/login.html` and `POST
  /api/login` are reachable without an account — a third, small exemption beside
  the display and cast lists, kept separate from both for the reason those two are
  kept apart.
- **Cross-site requests:** a write authenticated by the cookie must carry an
  `Origin` naming this host; otherwise `403`. `SameSite=Strict` already stops most
  of it; the check covers the rest. Basic-authenticated requests are not
  cookie-driven and need no check.

## The middleware, in order

`basic_auth_middleware` becomes `auth_middleware`, deciding per request:

1. `is_display_path` from loopback, and `cast::is_cast_public_path` — exempt,
   unchanged.
2. `/login.html`, `POST /api/login` — exempt.
3. A valid session cookie → that account.
4. `Authorization: Basic` → that account, verified against its hash (a verified
   header is cached per account, as `auth_cache` does today, so a page polling
   every two seconds does not run PBKDF2 every two seconds; the cache is cleared
   whenever that account's password or role changes).
5. The CLI recovery credential (below) → a built-in admin.
6. **No account exists at all** → open mode: the request proceeds as an admin.
7. Otherwise `401` — for a `GET` of an HTML page, a redirect to
   `/login.html?next=…` instead.

The resolved identity is attached to the request (an axum extension) for the
role check and the proposal middleware.

## Roles

A single table of route → minimum role, checked after the identity is known.
`403 { "error": "…" }` when the role is too low.

| Routes | Admin | Manager | Editor |
|---|---|---|---|
| `GET` of everything below | ✓ | ✓ | ✓ |
| `/api/settings`, `/api/webhooks*`, `/api/users*`, `/api/audio`, `DELETE /api/displays/{name}/cast/session`, `PUT /api/displays/{name}` (label) | ✓ | – | – |
| Content writes: `POST /api/assets`, `PUT`/`DELETE /api/assets/{id}`, `POST /api/playlists`, `PUT`/`DELETE /api/playlists/{id}`, `POST /api/playlist`, `PUT`/`DELETE /api/playlist/{id}`, `POST /api/playlist/{id}/move`, `PUT /api/displays/{name}/schedule` | direct | direct | **recorded as a proposal** |
| Overrides, `POST …/control/current` | ✓ | ✓ | – |
| `/api/changesets*` (review, approve, reject) | ✓ | ✓ | own draft only |

Overrides and "Play now" are live by nature and are not proposable.

## Upgrade and recovery

- **Today's stored credential becomes the first admin.** On the first start with
  the `users` table empty and `basic_auth_user` plus its hash in `settings`, one
  transaction inserts that admin (the hash is taken as it is — same PBKDF2 format)
  and deletes the two settings keys.
- **`--basic-auth-user` / `--basic-auth-password` stay the way back in**: a
  built-in admin that is never stored and always accepted, as the flags are today.
  A forgotten admin password is repaired by starting with them and changing it.
- The settings API's `auth_enabled`, `auth_user`, `auth_password` are **removed**;
  the users API replaces them. A request still sending them is refused (unknown
  field), not silently ignored.
- **The last enabled admin cannot be deleted, disabled or demoted** — that would
  leave a device only the CLI can open. Creating the first account forces it to be
  an admin.

## Users API and page (admin)

- `GET /api/users` — `[{ id, name, role, disabled }]`, never a hash.
- `POST /api/users` `{ name, password, role }`; `PUT /api/users/{id}` `{ role?,
  password?, disabled? }`; `DELETE /api/users/{id}`. Changing a password or role,
  or disabling, ends that account's sessions.
- `PUT /api/me/password` `{ current, new }` — anyone, for themself.
- `web/users.html` lists the accounts with role, a "deaktiviert" switch, a new
  password field, and a form to add one. The auth block in `admin.html` becomes a
  link to it.

## Proposals

```sql
CREATE TABLE IF NOT EXISTS changesets (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    author_id    INTEGER NOT NULL REFERENCES users(id),
    state        TEXT NOT NULL CHECK (state IN ('draft','submitted','applied','rejected','stale','failed')),
    note         TEXT,                      -- the reviewer's, on reject/fail
    created_at   DATETIME DEFAULT CURRENT_TIMESTAMP,
    submitted_at DATETIME,
    decided_by   INTEGER REFERENCES users(id),
    decided_at   DATETIME
);
CREATE TABLE IF NOT EXISTS change_requests (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    changeset_id INTEGER NOT NULL REFERENCES changesets(id) ON DELETE CASCADE,
    position     INTEGER NOT NULL,
    method       TEXT NOT NULL,
    path         TEXT NOT NULL,
    body         TEXT,                      -- JSON as the editor sent it
    placeholder  TEXT,                      -- 'new:N' for a create, else NULL
    before       TEXT,                      -- the object as it read when proposed; NULL for a create
    applied      BOOLEAN NOT NULL DEFAULT 0,
    result       TEXT                       -- status and body of the replay
);
```

and on `assets` one column, `pending_changeset INTEGER REFERENCES changesets(id)`.

**Recording** (`src/proposals/`, a middleware on the content write routes for the
editor role): the request is not passed on. The editor's `draft` changeset is
created if they have none; the request is appended with its `before` snapshot;
the answer is `202 { "proposed": true, "change": id, "placeholder": "new:N"? }`.

- **`before`** is the object as its read route returns it: an item as it appears
  in `GET /api/playlist`, a playlist in `GET /api/playlists`, an asset in `GET
  /api/assets`, a timetable as `GET /api/displays/{name}/schedule`; for a move,
  the ordered ids of the item's playlist. One function maps a write path to the
  object it touches and reads it.
- **A create gets a placeholder**, `new:1`, `new:2`, … within the bundle. Later
  requests in the same bundle may use it wherever an id goes (`playlist_id`,
  `asset_id`, a path segment). This is what lets one bundle propose a playlist,
  an upload and items in that playlist.
- **An upload is stored at once** — a file cannot sensibly wait as JSON — as an
  asset with `pending_changeset` set. Such an asset is left out of every list and
  picker except its author's, and cannot be referenced by anyone else. Rejecting
  or discarding the bundle deletes it and its file; applying clears the column.

**The editor's draft:** `GET /api/changesets/draft` (the bundle with its requests),
`DELETE /api/changesets/draft/requests/{id}` (drop one), `DELETE
/api/changesets/draft` (discard all), `POST /api/changesets/draft/submit`. An
editor has at most one draft; submitting starts the next one fresh.

**Review** (manager and admin): `GET /api/changesets?state=submitted`, each with
its requests and, per request, `before` and the proposed body; `POST
/api/changesets/{id}/approve`; `POST /api/changesets/{id}/reject` `{ note }`.

**Applying**, in `approve`:

1. **Stale check first**, for every request with a `before`: read the object now
   and compare. Any difference → state `stale`, nothing applied, `409` naming the
   request. A stale bundle can only be rejected.
2. **Replay in order** through the application's own router (`tower::ServiceExt::
   oneshot` on a clone), with the approving manager as identity so the role check
   passes and webhooks and signals fire exactly as for a direct write. Before each
   request, every `new:N` in its path and body is replaced by the id the earlier
   create returned. The status and body are stored in `result`, `applied` is set.
3. **Stop at the first failure** (any non-2xx): state `failed`, the reviewer's
   note says which request, the requests before it stay applied and are shown as
   such. Handlers are not transactional, so the bundle cannot be all-or-nothing;
   the stale check in step 1 catches the common reason a replay would fail, and a
   failure that remains is a real validation error, shown rather than hidden.
4. Otherwise state `applied`; pending assets of the bundle lose their mark.

For step 2, `POST /api/playlist` returns `{ "id": … }` with its `201` — today it
answers with no body, and a create whose id cannot be learnt cannot be referred
to.

**UI:**

- A shared `web/proposals.js` shows an editor a bar on the content pages
  (`playlist.html`, `assets.html`, `displays.html`): "N Änderungen im Entwurf —
  Einreichen · Verwerfen · Ansehen". A card or row with a pending change carries
  a "vorgeschlagen" badge. An editor's pickers list their own proposed playlists
  and uploads, marked as proposed, with the placeholder as value.
- `web/approvals.html` (manager, admin) lists submitted bundles: author, time, and
  per request a readable line ("Item #12 in „Foyer“: Dauer 10 → 20, Fit contain →
  cover") with before/after, and Freigeben / Ablehnen with a note. A stale bundle
  shows why and offers only Ablehnen. The nav links to it with the number waiting.

## Testing

**Rust (`#[cfg(test)]`):** the role table (every listed route with each role, as a
matrix, so a route added later without a row fails a test); session expiry and
sliding; the `Origin` check; the login lockout bound under concurrent attempts (N
concurrent callers against the bound, on a multi-worker runtime — the shape that
caught the cast lockout race); the upgrade from a stored credential; the
last-admin guard; placeholder substitution in paths and nested JSON; the
write-path → object mapping for `before`.

**Python (`tests/cast/test_users.py`, new; plain HTTP):**

- open mode with no accounts; the first account must be an admin; after it
  exists, an unauthenticated request is `401` and an HTML page redirects;
- login, `GET /api/me`, logout; Basic auth for a script; a disabled account and a
  changed password end sessions; the CLI credential still gets in;
- each role against a representative route of each row;
- an editor's bundle — an upload, a new playlist, an item in it using both
  placeholders, and an edit to an existing item — is invisible until a manager
  approves, then all of it is live; the pending asset is hidden from another
  account meanwhile;
- a bundle whose item a manager changed after it was proposed is `stale` and
  cannot be approved; rejecting a bundle deletes its pending upload's file;
- a replay failure stops the bundle as `failed` with the applied requests marked.

The existing suites run without accounts — open mode — and must stay green
unchanged; `test_basicauth.py` moves to the new model (its credential is the
first admin).

## Not in scope

- Accounts shared across devices, password reset by e-mail (the CLI credential
  is the recovery path).
- OpenID Connect sign-in — the next step, with its own spec. The account model is
  built for it: an SSO account is the same `users` row with an issuer and subject
  in place of a password hash, and local accounts stay as the fallback for a device
  that cannot reach its identity provider.
- Proposing overrides, "Play now", settings or webhooks.
- Webhook events for proposals.
- Per-playlist or per-display permissions.

## Amendments made while implementing

- **Open mode needs no command-line credential either.** Open only when there is
  no account *and* no `--basic-auth-*` flag: a device run with only the flags had
  a protected admin before, and would have come up open.
- A bundle is claimed with a state `applying` in one statement before it is
  replayed, so two managers approving at once cannot both replay it.
- Webhook targets are admin-only even to read (their headers hold other people's
  tokens), which the role table's first draft had as `Read`.
- The editor's pickers do **not** yet offer a playlist proposed in the same draft:
  the API accepts `new:N` as `playlist_id`, but the playlist page loads a list by
  id and has no draft objects to show. An editor proposes the playlist, and adds
  items to it once it is approved — or through the API with the placeholder.
- New pages need `touch src/web.rs` to be compiled in (see `CLAUDE.md`).
