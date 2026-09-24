# Who may cast

Status: implemented (designed and approved in conversation on 2026-09-24).

## What and why

Casting asks today *whether the guest is in the room* (`cast_auth`: none, code,
pairing) and never *who they are*. Now that there are accounts, a venue can keep
a screen open to every visitor while another is for members only — or switch
showing a web page off on the foyer screen and leave casting on.

Each screen says, per mode, who may use it: **anyone**, **signed-in accounts
only**, or **nobody**. Any account qualifies, whatever its role. The presence
check stays independent: a signed-in member in the next room should still not
take over the foyer, so an account never replaces the code.

## The switches, and how they combine

The venue's switches stay and keep their meaning: `cast_enabled`,
`guest_pages_enabled`, `--disable-cast`, `--guest-pages`. They are the one click
that stops a mode everywhere, and the flags that pin them keep working.

Below them, each screen has two values:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access { Anyone, Account, Off }
```

- `displays.cast_access`, `displays.page_access`, `TEXT NOT NULL DEFAULT
  'anyone'` — so after an upgrade every screen behaves exactly as before.

A claim for mode *m* on screen *s* is allowed when the venue switch for *m* is
on, *s*'s access for *m* is not `off`, and either it is `anyone` or the request
carries a signed-in account. Then, unchanged, the presence check (code or
pairing) runs.

## Where it is decided

**One place: `authorize_sender`**, which every claim already goes through, with
the resolved `Arc<Display>`. It gains an `Option<&Identity>`.

- `off` → `403` with the same wording as the venue switch being off, so the
  answer does not tell a guest more than "not here".
- `account` without an account → `401` with
  `{"error": "Zum Casten bitte anmelden.", "login": "/login.html?next=/"}`.
- Setting a screen to `off` for a mode **ends a running session of that mode on
  that screen**, the way the venue switch ends every session. Setting it to
  `account` does not end a running guest's session — it was allowed when it
  started, and the switch is about who may start one.

## Recognising a session without requiring one

The cast routes are exempt from authentication on purpose, and must stay so.
The middleware's cast branch gains one step before `next.run`: **if** the request
carries a valid session cookie — and, for anything but `GET`/`HEAD`, an `Origin`
of this host — the account is attached as the `Identity` extension; otherwise nothing is attached and the
request proceeds exactly as today. Never a `401` from this branch.

- The `Origin` check on writes is what stops another site from making a
  member's browser claim a screen with the member's cookie (`SameSite=Strict`
  stops most of it; this covers the rest, as it does for operator writes). A
  read needs none: browsers send no `Origin` on a same-origin `GET`, and a read
  only tells the page who is signed in.
- HTTP Basic is not considered here: guests are browsers, and a cached Basic
  credential should not silently turn a guest into an account.
- A replayed proposal never reaches a cast route, so `ReplayIdentity` is not
  involved.

`claim_session` takes `Option<Extension<Identity>>` and passes it down.

**The socket checks the page mode again.** A guest's code check claims in cast
mode and the page mode reuses that ticket, so the `present` frame — which
already re-checks the venue's `guest_pages_enabled` — also applies the screen's
`page_access`, against the account the session recorded at claim time.

## The guest page

`GET /api/cast/info` gives each screen its `cast_access` and `page_access`
(already folded with the venue switches, so `off` means off for whatever
reason), and **omits a screen that is `off` for both modes** — a guest cannot
use it, and listing it is the enumeration `info` already avoids when the venue
switches are off. The guest page has one screen list for both modes, which is
why a screen off for one mode only stays listed and is refused at the claim. For
`account` screens the page shows "Anmelden zum Casten", linking to
`/login.html?next=/` on the same HTTPS host (the login page already honours a
same-host `next`). `info` also says whether the caller is signed in and as
whom, so the page can show "Angemeldet als …" and skip the prompt.

`/login.html` and `POST /api/login` are already open (`Need::Open`) and served on
both listeners; the cookie is `Secure` over TLS.

## Who cast

The session records the account that claimed it (`None` for a guest). It shows in:

- the webhooks `cast.started` and `guest_page.shown`: a new `user` field (the
  account name, or `null`), added to `api::catalogue()`;
- the log line for a claim;
- `GET /api/displays` (operator), in the screen's cast state, so the displays
  page can say "gecastet von …".

## The operator page

`displays.html`, per card: two selects, "Casten" and "Webseite", each *jeder* /
*nur Konto* / *aus*, saved with `PUT /api/displays/{name}`, which gains
`cast_access` and `page_access` beside `label` (still `deny_unknown_fields`,
admin-only as before). When the venue switch for a mode is off the select is
shown disabled with a note that casting is off everywhere.

## Tests

Python, on a **non-primary** screen as CLAUDE.md requires:

- the matrix: mode × access × signed-in or not → allowed, `401` with `login`,
  or `403`;
- `account` still asks for the code when `cast_auth` is `code`;
- a valid cookie with a foreign `Origin` is treated as a guest;
- a Basic header on a claim is treated as a guest;
- setting a screen to `off` ends its running session of that mode and leaves
  the other screen's alone;
- `info` omits `off` screens and reports `access` and the signed-in account;
- `cast.started` carries `user`;
- after an upgrade (no values stored) every screen is `anyone`.

Rust: `Access` serde shape; the allow decision as a pure function over
(venue switch, access, signed in).

## Not in this

A casting-only role, per-role thresholds, an account replacing the presence
check, and OpenID Connect (the roadmap's next entry, which this makes more
useful).
