# Single sign-on with OpenID Connect

Status: implemented (designed and approved in conversation on 2026-09-24).

## What and why

Accounts exist; each has a local password. A venue that already runs an identity
provider — Nextcloud, Keycloak, Authentik, anything speaking OpenID Connect —
wants its people to sign in with what they already have, get the role their
groups say, and lose it when they leave the group. And members who only want to
cast should be able to prove who they are without anyone creating an account for
them.

Generic OpenID Connect: discovery, the authorization-code flow with PKCE, ID-token
verification. Nothing provider-specific.

## Two outcomes of an SSO sign-in

What a sign-in produces depends on the groups in the ID token and the venue's
mapping:

1. **A group maps to a role** (`admin`, `manager`, `editor`) → **an account**. It
   is created on the first sign-in and updated on every later one. It needs a row:
   proposals have an author, an admin sees and can disable it, and the role
   follows the groups at every sign-in. Several matching groups → the highest
   role.
2. **No role, but casting is allowed** — a group maps to *nur Casten*, or the
   venue allows every user of the provider to cast → **a cast session and no
   account**. It carries the provider's name for the person (`preferred_username`,
   else `name`, else `sub`), plus issuer and subject, and expires like any session.
   Nothing is stored beyond it.
3. Neither → refused, with a message naming why ("keine passende Gruppe").

### The line between them is structural

A cast session lives in **its own table** (`cast_sessions`) under **its own
cookie** (`mcc_cast`). The ordinary resolution (`accounts::middleware::resolve`)
never reads either, so a cast session cannot reach an operator page by any
mistake in a role check — there is nothing for it to be mistaken for. Only the
cast branch of the middleware, which already *recognises* an account session
without requiring one, also recognises a cast session, and attaches it as an
`Identity` with no `user_id` and a new marker that it is cast-only. `cast::access`
treats either as "signed in". Everywhere else a request with only `mcc_cast` is a
request with no credentials.

In the displays page and the guest page, *nur Konto* is relabelled **nur
angemeldet**; the stored value stays `account`.

## Configuration

A card "Single Sign-on" on the admin page (admin only), stored in `settings`:

- issuer URL, client ID, client secret — the secret is write-only in the API
  (never returned, shown as "gesetzt"), like the operator password was;
- the groups claim name (default `groups`) and the button label (default the
  issuer's host);
- the mapping: rows of *group → admin | manager | editor | nur Casten*;
- **alle vom Anbieter dürfen casten** (default off) — many providers have no
  "everyone" group;
- **lokale Passwörter erlaubt** (default on), below.

The card shows the **exact redirect URI** to register with the provider:
`https://<host>/api/oidc/callback`, where the host comes from `cast::sender_url`
— the one place that answers what this device is called. A second answer would
drift from the first. When that name is not stable (a DHCP address with neither a
managed certificate name nor `--public-url`), the card says so, because the
provider will refuse a redirect URI that changed.

## The flow

1. `GET /api/oidc/start?next=…` (open, like `/login.html`): discovery (cached),
   a random `state`, `nonce` and PKCE verifier, stored server-side for ten minutes
   keyed by `state` and bound to a short-lived cookie; a same-host `next` only,
   the rule the login page already applies. Redirect to the provider.
2. `GET /api/oidc/callback?code&state` (open): the `state` must match the stored
   one *and* the cookie; it is consumed on first use. Exchange the code at the
   token endpoint (`client_secret_basic`, falling back to `client_secret_post` if
   discovery says so) with the verifier.
3. Verify the ID token: signature (RS256 and ES256 against the provider's JWKS,
   fetched and cached, refetched once on an unknown `kid`; HS256 with the client
   secret), `iss` equal to the configured issuer, `aud` containing the client ID
   (and `azp` when there are several), `exp` in the future with a minute of skew,
   `nonce` equal to the stored one. Any failure → back to the login page with a
   message; nothing is created.
4. Read the groups claim (a list of strings; absent = no groups), decide the
   outcome above, set the session cookie, redirect to `next`.

The HTTP client is the combination `managed_cert.rs` already uses — hyper,
`tokio-rustls`, `webpki-roots`, `ring` — **not `reqwest`**, whose `rustls` feature
hard-wires `aws-lc-rs` and does not cross-compile for armv7 here. JWT and JWK
handling is written on `ring` directly or with a crate that needs nothing else;
**verify with `cross build`** before relying on any new dependency.

## Accounts from SSO

- **Linked by `(issuer, subject)`, never by name.** A new table
  `user_identities (issuer, subject, user_id)` with `UNIQUE(issuer, subject)`.
  Linking by name would let a provider account called `admin` take over the local
  one. When the provider's name collides with an existing account, the new account
  gets a suffix (`anna` → `anna-sso`, then `-sso2` …).
- An SSO account **has no password**. `users.password_hash` stays `NOT NULL` —
  SQLite cannot relax a column in place, and rebuilding `users` under the foreign
  keys of `sessions` and `changesets` is a risk this does not need — and is stored
  as the empty string, which `verify_password` treats as "no password": Basic auth
  and the login form refuse it. Scripts keep using local accounts.
- **The role is set from the groups at every sign-in.** When no group maps to a
  role any more, the account is **disabled** and its sessions end — the person
  left the group, and "still an editor until they happen to sign in again" is not
  acceptable. (Removal is only learned at a sign-in; an existing session lives
  until it expires. Documented, not solved: there is no back-channel logout here.)
- The last-admin guard still applies to local changes; a role coming from the
  provider may demote the last *SSO* admin, which is why "nur SSO" below requires
  the CLI credential to stay possible.

## "Nur SSO": local passwords off

With **lokale Passwörter erlaubt** off:

- the login page shows only the SSO button; `POST /api/login` refuses a local
  account; HTTP Basic refuses a local account;
- **the command-line credential always works**, form and Basic — it is the way back
  when the provider is gone or the device is offline;
- turning it off is **refused** while SSO is not configured, or while no account
  linked to the provider has the admin role — so nobody locks themselves out
  without being told.

## The login page and the guest page

`/login.html` shows "Mit ‹Label› anmelden" when SSO is configured, linking to
`/api/oidc/start?next=<the page's next>`. The guest page's "Anmelden zum Casten"
already goes to `/login.html?next=/`, so a member can sign in by SSO and come back
to the guest page with a cast session.

## Who cast

Unchanged mechanism: the name on the session (account name or the provider's name
for a cast session) is what "gecastet von …" and the `user` field of the webhooks
show.

## Tests

The Python suite is stdlib-only, which cannot sign RS256. A **fake provider** in
the harness (http.server) serves discovery, a token endpoint and a JWKS, and signs
ID tokens with **HS256** (the client secret). Against it:

- sign-in end to end: role from a group, account created, role updated on the next
  sign-in, disabled (and sessions ended) when the group is gone;
- a cast-only sign-in creates no account, is recognised by the claim on a
  `nur angemeldet` screen, and is **refused on every operator route** (a redirect
  to the login page, a `401` on the API);
- "alle vom Anbieter dürfen casten" on and off;
- wrong `state`, reused `state`, wrong `nonce`, wrong `aud`, expired token, bad
  signature → refused, nothing created;
- a provider name colliding with a local account → suffixed, the local account
  untouched;
- "nur SSO": the form and Basic refuse local accounts, the CLI credential still
  works, and turning it off without an SSO admin is refused;
- `next` survives the round trip and a foreign `next` is dropped.

Rust: RS256 and ES256 verification against embedded test keys and pre-signed
tokens (valid, wrong key, wrong `kid`, tampered payload); claim checks as pure
functions; the redirect URI built from `sender_url`.

## Not in this

Back-channel or front-channel logout, refresh tokens (the controller's own session
is what lasts), several providers at once, provider-specific group APIs, and API
tokens for scripts.

## Amendments made while implementing

- **A cast session is attached as a `Caster`, not an `Identity`.** A type with a
  name and no role cannot be handed to anything that checks roles; an `Identity`
  with a "cast-only" marker could. Account sessions reach the cast routes as a
  `Caster` too.
- **The callback answers with a page that navigates itself**, not a redirect: the
  session cookie is `SameSite=Strict`, and a redirect chain that began on the
  provider's site would not carry it. Verified in a real Chrome.
- **`start` first moves the browser to the canonical address** (the redirect
  URI's origin). Beginning under another name put the binding cookie and the
  session cookie on different hosts.
- **The command-line credential works over Basic**, not the form — it never had
  an account row and never signed in through the form; that is unchanged.
- **A key that names its `alg` verifies only that algorithm.**
