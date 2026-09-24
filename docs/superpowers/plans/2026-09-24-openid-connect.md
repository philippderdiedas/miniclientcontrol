# OpenID Connect Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Sign in through any OpenID Connect provider: a mapped group yields an account with that role; a cast-only mapping yields a cast session with no account; local passwords can be switched off.

**Architecture:** A new `src/oidc/` module: `http.rs` (hyper/rustls GET and form POST), `jwt.rs` (ID-token verification on `ring`), `config.rs` (stored configuration, redirect URI), `flow.rs` (start/callback, outcomes). Cast sessions live in their own table under their own cookie and surface only in the middleware's cast branch, as a `Caster` extension distinct from `Identity`.

**Tech Stack:** Rust (axum 0.8, sqlx/SQLite, hyper 1, tokio-rustls, ring 0.17, base64 0.22), vanilla JS, stdlib Python tests with a fake provider.

Spec: `docs/superpowers/specs/2026-09-24-openid-connect-design.md`.

## Global Constraints

- **No `reqwest`**, no new crate that pulls `aws-lc-rs` or needs a C toolchain. Only crates already in `Cargo.toml`. If one is truly needed, verify with `cross build --target armv7-unknown-linux-gnueabihf` first.
- Issuer, token and JWKS URLs must be `https`, **except** a loopback host (`127.0.0.1`, `::1`, `localhost`), which may be `http` — that is how the test provider and local development work.
- Signature algorithms: `RS256`, `ES256`, `HS256` (key = client secret). Anything else, including `none`, is refused.
- Claims: `iss` == configured issuer (exact string), `aud` contains client ID (string or array), `azp` == client ID when `aud` has several entries, `exp` > now − 60 s, `nonce` == stored nonce.
- `state` entries: random, 10-minute TTL, single use, bound to an `mcc_oidc` cookie (`HttpOnly; SameSite=Lax; Path=/api/oidc`, `Secure` over TLS). Lax because the callback is a cross-site top-level navigation.
- The callback answers with a small HTML page that navigates to `next` itself (meta refresh + link), **not** a `302`: the session cookie is `SameSite=Strict`, and a redirect chain that started on the provider's site would not send it on the next request.
- Link SSO accounts by `(issuer, subject)` only (`user_identities`). Name collision → suffix `-sso`, `-sso2`, …
- SSO accounts store `password_hash = ''`; `verify_hash('')` is already false.
- Cast sessions: table `cast_sessions`, cookie `mcc_cast` (`HttpOnly; SameSite=Strict; Path=/`, `Secure` over TLS), 12 h sliding like account sessions. Read **only** in the middleware's cast branch.
- "Lokale Passwörter erlaubt" off: `POST /api/login` and HTTP Basic refuse local accounts; the command-line credential always works; switching it off requires SSO configured **and** an enabled admin linked in `user_identities`.
- Redirect URI: scheme + authority of `cast::sender_url(state, None)` + `/api/oidc/callback`. No second derivation of the device name.
- Client secret is write-only over the API (`secret_set: bool` on read).
- UI: `createElement`/`textContent`, no `innerHTML` interpolation. `web/` compiled in; new files need `touch src/web.rs`.
- Commits: no Claude co-author or session trailers.

---

### Task 1: ID-token verification (`src/oidc/jwt.rs`) and the HTTP client (`src/oidc/http.rs`)

**Files:**
- Create: `src/oidc/mod.rs`, `src/oidc/jwt.rs`, `src/oidc/http.rs`, `src/oidc/testdata/` (fixtures), `scripts/oidc-fixtures.sh`
- Modify: `src/main.rs` (`mod oidc;`)

**Interfaces:**
- Produces:
  - `pub struct Jwk { kty: String, kid: Option<String>, alg: Option<String>, n, e, crv, x, y: Option<String> }` (serde)
  - `pub struct Expect<'a> { issuer: &'a str, client_id: &'a str, nonce: &'a str, secret: &'a str, now: i64 }`
  - `pub fn verify(token: &str, keys: &[Jwk], expect: &Expect) -> Result<serde_json::Value, JwtError>` — returns the claims
  - `pub fn kid_of(token: &str) -> Option<String>`
  - `pub enum JwtError { Malformed, Algorithm, UnknownKey, Signature, Claim(&'static str) }` with `Display`
  - `pub async fn get_json(url: &str) -> anyhow::Result<serde_json::Value>`
  - `pub async fn post_form(url: &str, form: &[(&str, &str)], basic: Option<(&str, &str)>) -> anyhow::Result<serde_json::Value>`
  - `pub fn allowed_url(url: &str) -> bool` (https, or http on loopback)

- [ ] **Step 1: Fixtures**

`scripts/oidc-fixtures.sh` (run once; output committed under `src/oidc/testdata/`):

```bash
#!/bin/sh
# Test keys and pre-signed ID tokens for src/oidc/jwt.rs's tests. The Python
# suite is stdlib-only and cannot sign RS256/ES256, so these paths are covered
# in Rust against fixed fixtures. Regenerate only if the claims below change.
set -eu
out=src/oidc/testdata
mkdir -p "$out"
b64url() { openssl base64 -A | tr '+/' '-_' | tr -d '='; }
openssl genrsa -out "$out/rsa.pem" 2048 2>/dev/null
openssl ecparam -name prime256v1 -genkey -noout -out "$out/ec.pem"
claims='{"iss":"https://idp.test","aud":"mcc","sub":"u1","nonce":"n0nce","exp":4102444800,"preferred_username":"anna","groups":["staff"]}'
payload=$(printf '%s' "$claims" | b64url)
# RS256
h=$(printf '{"alg":"RS256","kid":"r1","typ":"JWT"}' | b64url)
sig=$(printf '%s.%s' "$h" "$payload" | openssl dgst -sha256 -sign "$out/rsa.pem" | b64url)
printf '%s.%s.%s' "$h" "$payload" "$sig" > "$out/rs256.jwt"
# ES256: openssl emits DER; JWS wants the raw 64-byte r||s.
h=$(printf '{"alg":"ES256","kid":"e1","typ":"JWT"}' | b64url)
printf '%s.%s' "$h" "$payload" | openssl dgst -sha256 -sign "$out/ec.pem" > "$out/es.der"
python3 - "$out/es.der" > "$out/es.raw" <<'PY'
import sys
d = open(sys.argv[1], 'rb').read()
# SEQUENCE { INTEGER r, INTEGER s }
i = 2 if d[1] < 0x80 else 3
def take(i):
    assert d[i] == 2; n = d[i + 1]; v = d[i + 2:i + 2 + n]; return v.lstrip(b'\0').rjust(32, b'\0'), i + 2 + n
r, i = take(i); s, _ = take(i)
sys.stdout.buffer.write(r + s)
PY
sig=$(b64url < "$out/es.raw")
printf '%s.%s.%s' "$h" "$payload" "$sig" > "$out/es256.jwt"
rm "$out/es.der" "$out/es.raw"
# Public keys as a JWKS.
n=$(openssl rsa -in "$out/rsa.pem" -noout -modulus 2>/dev/null | cut -d= -f2 | xxd -r -p | b64url)
pub=$(openssl ec -in "$out/ec.pem" -pubout -outform DER 2>/dev/null | tail -c 64)
x=$(printf '%s' "$pub" | head -c 32 | b64url)
y=$(printf '%s' "$pub" | tail -c 32 | b64url)
printf '{"keys":[{"kty":"RSA","kid":"r1","alg":"RS256","n":"%s","e":"AQAB"},{"kty":"EC","kid":"e1","alg":"ES256","crv":"P-256","x":"%s","y":"%s"}]}' "$n" "$x" "$y" > "$out/jwks.json"
rm "$out/rsa.pem" "$out/ec.pem"
```

(`tail -c 64` on the DER SubjectPublicKeyInfo is the uncompressed point minus its `04` prefix. `$pub` is binary — if the shell mangles it, write it to a temp file instead of a variable.) Run `sh scripts/oidc-fixtures.sh`; commit the three files in `src/oidc/testdata/`. The private keys are deleted: nothing needs them after signing.

- [ ] **Step 2: `jwt.rs` with tests**

```rust
//! Verifying an ID token: RS256 and ES256 against the provider's keys, HS256
//! against the client secret, then the claims OpenID Connect requires.
//! Written on `ring` directly -- a JWT crate would be one more dependency to
//! prove against the armv7 cross build for about a hundred lines of work.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ring::signature;
use serde::Deserialize;
use serde_json::Value;

const SKEW: i64 = 60;

#[derive(Debug, Clone, Deserialize)]
pub struct Jwk {
    pub kty: String,
    #[serde(default)]
    pub kid: Option<String>,
    #[serde(default)]
    pub alg: Option<String>,
    #[serde(default)]
    pub n: Option<String>,
    #[serde(default)]
    pub e: Option<String>,
    #[serde(default)]
    pub crv: Option<String>,
    #[serde(default)]
    pub x: Option<String>,
    #[serde(default)]
    pub y: Option<String>,
}

pub struct Expect<'a> {
    pub issuer: &'a str,
    pub client_id: &'a str,
    pub nonce: &'a str,
    pub secret: &'a str,
    pub now: i64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum JwtError {
    Malformed,
    Algorithm,
    UnknownKey,
    Signature,
    Claim(&'static str),
}

impl std::fmt::Display for JwtError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JwtError::Malformed => write!(f, "the token is malformed"),
            JwtError::Algorithm => write!(f, "the token's algorithm is not accepted"),
            JwtError::UnknownKey => write!(f, "no key of the provider matches the token"),
            JwtError::Signature => write!(f, "the signature does not verify"),
            JwtError::Claim(which) => write!(f, "the claim `{which}` does not match"),
        }
    }
}

fn part(raw: &str) -> Result<Vec<u8>, JwtError> {
    URL_SAFE_NO_PAD.decode(raw).map_err(|_| JwtError::Malformed)
}

fn header(token: &str) -> Result<Value, JwtError> {
    let head = token.split('.').next().ok_or(JwtError::Malformed)?;
    serde_json::from_slice(&part(head)?).map_err(|_| JwtError::Malformed)
}

pub fn kid_of(token: &str) -> Option<String> {
    header(token).ok()?.get("kid")?.as_str().map(str::to_string)
}

pub fn verify(token: &str, keys: &[Jwk], expect: &Expect) -> Result<Value, JwtError> {
    let mut parts = token.split('.');
    let (Some(h), Some(p), Some(s), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return Err(JwtError::Malformed);
    };
    let head = header(token)?;
    let alg = head.get("alg").and_then(Value::as_str).ok_or(JwtError::Malformed)?;
    let kid = head.get("kid").and_then(Value::as_str);
    let signed = format!("{h}.{p}");
    let sig = part(s)?;

    let pick = |kty: &str| {
        keys.iter()
            .filter(|k| k.kty == kty)
            .find(|k| kid.is_none() || k.kid.as_deref() == kid)
            .ok_or(JwtError::UnknownKey)
    };
    match alg {
        "RS256" => {
            let key = pick("RSA")?;
            let n = part(key.n.as_deref().ok_or(JwtError::UnknownKey)?)?;
            let e = part(key.e.as_deref().ok_or(JwtError::UnknownKey)?)?;
            signature::RsaPublicKeyComponents { n: &n, e: &e }
                .verify(&signature::RSA_PKCS1_2048_8192_SHA256, signed.as_bytes(), &sig)
                .map_err(|_| JwtError::Signature)?;
        }
        "ES256" => {
            let key = pick("EC")?;
            if key.crv.as_deref() != Some("P-256") {
                return Err(JwtError::UnknownKey);
            }
            let mut point = vec![4u8];
            point.extend(part(key.x.as_deref().ok_or(JwtError::UnknownKey)?)?);
            point.extend(part(key.y.as_deref().ok_or(JwtError::UnknownKey)?)?);
            signature::UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_FIXED, &point)
                .verify(signed.as_bytes(), &sig)
                .map_err(|_| JwtError::Signature)?;
        }
        "HS256" => {
            if expect.secret.is_empty() {
                return Err(JwtError::Algorithm);
            }
            let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, expect.secret.as_bytes());
            ring::hmac::verify(&key, signed.as_bytes(), &sig).map_err(|_| JwtError::Signature)?;
        }
        _ => return Err(JwtError::Algorithm),
    }

    let claims: Value = serde_json::from_slice(&part(p)?).map_err(|_| JwtError::Malformed)?;
    if claims.get("iss").and_then(Value::as_str) != Some(expect.issuer) {
        return Err(JwtError::Claim("iss"));
    }
    let audiences: Vec<&str> = match claims.get("aud") {
        Some(Value::String(one)) => vec![one.as_str()],
        Some(Value::Array(many)) => many.iter().filter_map(Value::as_str).collect(),
        _ => vec![],
    };
    if !audiences.contains(&expect.client_id) {
        return Err(JwtError::Claim("aud"));
    }
    if audiences.len() > 1 && claims.get("azp").and_then(Value::as_str) != Some(expect.client_id) {
        return Err(JwtError::Claim("azp"));
    }
    let exp = claims.get("exp").and_then(Value::as_i64).ok_or(JwtError::Claim("exp"))?;
    if exp < expect.now - SKEW {
        return Err(JwtError::Claim("exp"));
    }
    if claims.get("nonce").and_then(Value::as_str) != Some(expect.nonce) {
        return Err(JwtError::Claim("nonce"));
    }
    if claims.get("sub").and_then(Value::as_str).is_none_or(str::is_empty) {
        return Err(JwtError::Claim("sub"));
    }
    Ok(claims)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RS: &str = include_str!("testdata/rs256.jwt");
    const ES: &str = include_str!("testdata/es256.jwt");
    const JWKS: &str = include_str!("testdata/jwks.json");

    fn keys() -> Vec<Jwk> {
        let v: Value = serde_json::from_str(JWKS).unwrap();
        serde_json::from_value(v["keys"].clone()).unwrap()
    }

    fn expect() -> Expect<'static> {
        Expect { issuer: "https://idp.test", client_id: "mcc", nonce: "n0nce", secret: "s3cret", now: 1_700_000_000 }
    }

    fn hs256(claims: &Value, secret: &str) -> String {
        let h = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
        let p = URL_SAFE_NO_PAD.encode(claims.to_string());
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.as_bytes());
        let s = URL_SAFE_NO_PAD.encode(ring::hmac::sign(&key, format!("{h}.{p}").as_bytes()));
        format!("{h}.{p}.{s}")
    }

    fn claims() -> Value {
        serde_json::json!({"iss": "https://idp.test", "aud": "mcc", "sub": "u1", "nonce": "n0nce", "exp": 4102444800i64})
    }

    #[test]
    fn rs256_and_es256_verify() {
        assert_eq!(verify(RS.trim(), &keys(), &expect()).unwrap()["preferred_username"], "anna");
        assert_eq!(verify(ES.trim(), &keys(), &expect()).unwrap()["groups"][0], "staff");
    }

    #[test]
    fn a_tampered_payload_fails() {
        let mut parts: Vec<String> = RS.trim().split('.').map(str::to_string).collect();
        parts[1] = URL_SAFE_NO_PAD.encode(r#"{"iss":"https://idp.test","aud":"mcc","sub":"root","nonce":"n0nce","exp":4102444800}"#);
        assert_eq!(verify(&parts.join("."), &keys(), &expect()), Err(JwtError::Signature));
    }

    #[test]
    fn an_unknown_kid_is_an_unknown_key() {
        let mut ks = keys();
        for k in &mut ks { k.kid = Some("other".into()); }
        assert_eq!(verify(RS.trim(), &ks, &expect()), Err(JwtError::UnknownKey));
        assert_eq!(kid_of(RS.trim()).as_deref(), Some("r1"));
    }

    #[test]
    fn hs256_uses_the_client_secret() {
        assert!(verify(&hs256(&claims(), "s3cret"), &[], &expect()).is_ok());
        assert_eq!(verify(&hs256(&claims(), "wrong"), &[], &expect()), Err(JwtError::Signature));
    }

    #[test]
    fn none_and_unknown_algorithms_are_refused() {
        let h = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let p = URL_SAFE_NO_PAD.encode(claims().to_string());
        assert_eq!(verify(&format!("{h}.{p}."), &[], &expect()), Err(JwtError::Algorithm));
    }

    #[test]
    fn each_claim_is_checked() {
        let with = |k: &str, v: Value| { let mut c = claims(); c[k] = v; hs256(&c, "s3cret") };
        assert_eq!(verify(&with("iss", "https://evil.test".into()), &[], &expect()), Err(JwtError::Claim("iss")));
        assert_eq!(verify(&with("aud", "other".into()), &[], &expect()), Err(JwtError::Claim("aud")));
        assert_eq!(verify(&with("aud", serde_json::json!(["mcc", "x"])), &[], &expect()), Err(JwtError::Claim("azp")));
        assert_eq!(verify(&with("exp", 1_600_000_000i64.into()), &[], &expect()), Err(JwtError::Claim("exp")));
        assert_eq!(verify(&with("nonce", "replayed".into()), &[], &expect()), Err(JwtError::Claim("nonce")));
    }
}
```

- [ ] **Step 3: `http.rs`**

Model it on `webhook::send` (same connection code, no custom verifier): parse with `url::Url`, refuse unless `allowed_url`, connect TCP with a 10 s timeout, TLS with `webpki_roots` for `https`, `hyper::client::conn::http1::handshake`, 15 s request timeout, body capped at 256 KB with `http_body_util::Limited`, non-2xx → `Err` naming the status, JSON parse → `Err` on failure. `post_form` sends `content-type: application/x-www-form-urlencoded` with each pair `urlencoding::encode`d, and `authorization: Basic base64(urlencoded(id):urlencoded(secret))` when `basic` is given (RFC 6749 §2.3.1). Both send `accept: application/json`, `host`, and the user agent `managed_cert.rs` sends.

```rust
pub fn allowed_url(url: &str) -> bool {
    let Ok(url) = url::Url::parse(url) else { return false };
    match url.scheme() {
        "https" => true,
        // The test provider and local development; never across a network.
        "http" => matches!(url.host_str(), Some("127.0.0.1") | Some("::1") | Some("[::1]") | Some("localhost")),
        _ => false,
    }
}
```

Unit test `allowed_url` (https yes, http loopback yes, http LAN no, ftp no).

- [ ] **Step 4: `mod.rs`, build, test**

`src/oidc/mod.rs`: `pub mod http; pub mod jwt;` plus a module doc. `src/main.rs`: `mod oidc;`.

Run: `cargo test oidc` — Expected: all jwt and http tests pass.

- [ ] **Step 5: Commit**

```bash
git add scripts/oidc-fixtures.sh src/oidc src/main.rs
git commit -m "Verify OpenID Connect ID tokens on ring, and fetch over hyper"
```

---

### Task 2: Configuration, storage and the admin card

**Files:**
- Create: `src/oidc/config.rs`
- Modify: `src/db.rs` (tables), `src/models.rs` (`AppState::oidc`), `src/main.rs` (load, routes), `src/accounts/roles.rs` (rows + matrix), `web/admin.html` (card)
- Test: `tests/cast/test_oidc.py` (create; config half)

**Interfaces:**
- Produces:
  - `pub struct OidcConfig { issuer: String, client_id: String, client_secret: String, groups_claim: String, label: String, mapping: Vec<GroupMapping>, everyone_casts: bool, local_passwords: bool }` (serde, `Default` with `groups_claim = "groups"`, `local_passwords = true`)
  - `pub struct GroupMapping { group: String, target: Target }`, `pub enum Target { Admin, Manager, Editor, Cast }` (snake_case)
  - `impl OidcConfig { pub fn configured(&self) -> bool }` (issuer, client id non-empty)
  - `pub fn redirect_uri(state: &AppState) -> String`
  - `pub async fn load(pool) -> OidcConfig`, `pub async fn save(pool, &OidcConfig) -> anyhow::Result<()>` (setting key `oidc`)
  - `AppState::oidc: Arc<tokio::sync::RwLock<OidcConfig>>`
  - Routes: `GET /api/oidc/config` (admin), `PUT /api/oidc/config` (admin), `GET /api/oidc/info` (open: `{configured, label, local_passwords}`)
  - Tables: `user_identities(issuer TEXT, subject TEXT, user_id INTEGER REFERENCES users(id) ON DELETE CASCADE, UNIQUE(issuer, subject))`, `cast_sessions(token_hash TEXT PRIMARY KEY, name TEXT NOT NULL, issuer TEXT NOT NULL, subject TEXT NOT NULL, expires_at DATETIME NOT NULL)`

- [ ] **Step 1: Failing test**

`tests/cast/test_oidc.py`, starting with the helpers the later tasks reuse:

```python
"""Single sign-on with OpenID Connect, against a fake provider.

The provider below is stdlib-only, so it signs ID tokens with HS256 (the client
secret). RS256/ES256 are covered in Rust against fixed fixtures (src/oidc/jwt.rs).
"""
import base64, hashlib, hmac, json, os, sys, threading, time, urllib.error, urllib.parse, urllib.request
from http.cookiejar import CookieJar
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from test_cast import Server, check, failures, http, HTTP
from test_users import Browser, BASE, basic

IDP_PORT = 3071
ISSUER = f"http://127.0.0.1:{IDP_PORT}"
SECRET = "s3cret-for-tests"


def b64url(raw):
    return base64.urlsafe_b64encode(raw).rstrip(b"=").decode()


class Provider:
    """Discovery, an authorize endpoint that signs whoever `next_user` says,
    and a token endpoint. `tamper` lets a case break one claim."""
    def __init__(self):
        self.next_user = {"sub": "u1", "preferred_username": "anna", "groups": ["staff"]}
        self.tamper = {}
        self.codes = {}
        provider = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass

            def reply(self, status, body, headers=()):
                data = json.dumps(body).encode() if not isinstance(body, bytes) else body
                self.send_response(status)
                for k, v in headers:
                    self.send_header(k, v)
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def do_GET(self):
                url = urllib.parse.urlparse(self.path)
                q = dict(urllib.parse.parse_qsl(url.query))
                if url.path == "/.well-known/openid-configuration":
                    self.reply(200, {"issuer": ISSUER, "authorization_endpoint": f"{ISSUER}/authorize",
                                     "token_endpoint": f"{ISSUER}/token", "jwks_uri": f"{ISSUER}/jwks",
                                     "id_token_signing_alg_values_supported": ["HS256"]})
                elif url.path == "/jwks":
                    self.reply(200, {"keys": []})
                elif url.path == "/authorize":
                    code = b64url(os.urandom(12))
                    provider.codes[code] = {"nonce": q.get("nonce"), "challenge": q.get("code_challenge"),
                                            "client_id": q.get("client_id")}
                    back = f"{q['redirect_uri']}?code={code}&state={urllib.parse.quote(q['state'])}"
                    self.reply(302, b"", [("Location", back)])
                else:
                    self.reply(404, {})

            def do_POST(self):
                form = dict(urllib.parse.parse_qsl(self.rfile.read(int(self.headers["Content-Length"])).decode()))
                grant = provider.codes.pop(form.get("code"), None)
                verifier = form.get("code_verifier", "")
                challenge = b64url(hashlib.sha256(verifier.encode()).digest())
                if not grant or grant["challenge"] != challenge:
                    self.reply(400, {"error": "invalid_grant"})
                    return
                claims = {"iss": ISSUER, "aud": grant["client_id"], "exp": int(time.time()) + 300,
                          "nonce": grant["nonce"], **provider.next_user, **provider.tamper}
                head = b64url(json.dumps({"alg": "HS256", "typ": "JWT"}).encode())
                body = b64url(json.dumps(claims).encode())
                sig = b64url(hmac.new(SECRET.encode(), f"{head}.{body}".encode(), hashlib.sha256).digest())
                self.reply(200, {"access_token": "x", "token_type": "Bearer", "id_token": f"{head}.{body}.{sig}"})

        self.server = ThreadingHTTPServer(("127.0.0.1", IDP_PORT), Handler)

    def __enter__(self):
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        return self

    def __exit__(self, *a):
        self.server.shutdown()


def admin_put(path, body):
    return basic("PUT", path, "root", "longenough", body)


def configure(**extra):
    body = {"issuer": ISSUER, "client_id": "mcc", "client_secret": SECRET, "label": "Fablab",
            "mapping": [{"group": "staff", "target": "editor"}, {"group": "boss", "target": "admin"},
                        {"group": "members", "target": "cast"}]}
    body.update(extra)
    return admin_put("/api/oidc/config", body)


def config_flow():
    print("\n[160] configuring the provider")
    with Server():
        http("POST", "/api/users", {"name": "root", "password": "longenough", "role": "admin"})
        status, info = http("GET", "/api/oidc/info")
        check("unconfigured, the login page is told so", status == 200 and info["configured"] is False, info)
        status, _ = configure()
        check("the admin configures it", status == 200, status)
        status, conf = basic("GET", "/api/oidc/config", "root", "longenough")
        check("the secret never comes back", "client_secret" not in conf and conf["secret_set"] is True, conf)
        check("the redirect URI is shown", conf["redirect_uri"].endswith("/api/oidc/callback"), conf)
        status, info = http("GET", "/api/oidc/info")
        check("the login page learns the label", info == {"configured": True, "label": "Fablab",
                                                          "local_passwords": True}, info)
        status, _ = configure(client_secret=None)
        status, conf = basic("GET", "/api/oidc/config", "root", "longenough")
        check("leaving the secret out keeps it", conf["secret_set"] is True, conf)
        status, _ = configure(local_passwords=False)
        check("passwords cannot be switched off with no SSO admin yet", status == 400, status)
        status, _ = configure(issuer="http://192.168.1.9/")
        check("a plain-http provider on the network is refused", status == 400, status)


if __name__ == "__main__":
    config_flow()
    print("\n" + ("ALL PASSED" if not failures else f"{len(failures)} FAILED: {failures}"))
    sys.exit(1 if failures else 0)
```

Run: `cargo build && python3 tests/cast/test_oidc.py` — Expected: FAIL (404 on `/api/oidc/info`).

- [ ] **Step 2: Tables**

In `src/db.rs`, after `sessions`:

```rust
    // An account's identity at an OpenID Connect provider. Linked by issuer and
    // subject, never by name: a provider account called "admin" must not
    // become the local one.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS user_identities (
            issuer   TEXT NOT NULL,
            subject  TEXT NOT NULL,
            user_id  INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            UNIQUE(issuer, subject)
        );",
    )
    .execute(pool)
    .await?;
    // A signed-in person with no account, who may only cast. Its own table and
    // its own cookie, so the ordinary session lookup cannot see it at all.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS cast_sessions (
            token_hash TEXT PRIMARY KEY,
            name       TEXT NOT NULL,
            issuer     TEXT NOT NULL,
            subject    TEXT NOT NULL,
            expires_at DATETIME NOT NULL
        );",
    )
    .execute(pool)
    .await?;
```

- [ ] **Step 3: `config.rs`**

```rust
//! The provider this device signs in with, and what its groups mean here.

use serde::{Deserialize, Serialize};

use crate::models::AppState;

const KEY: &str = "oidc";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    Admin,
    Manager,
    Editor,
    /// A cast session and no account.
    Cast,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupMapping {
    pub group: String,
    pub target: Target,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct OidcConfig {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    pub groups_claim: String,
    pub label: String,
    pub mapping: Vec<GroupMapping>,
    pub everyone_casts: bool,
    pub local_passwords: bool,
}

impl Default for OidcConfig {
    fn default() -> Self {
        Self {
            issuer: String::new(),
            client_id: String::new(),
            client_secret: String::new(),
            groups_claim: "groups".into(),
            label: String::new(),
            mapping: Vec::new(),
            everyone_casts: false,
            local_passwords: true,
        }
    }
}

impl OidcConfig {
    pub fn configured(&self) -> bool {
        !self.issuer.is_empty() && !self.client_id.is_empty()
    }

    /// What the login button says: the configured label, else the issuer's host.
    pub fn button_label(&self) -> String {
        if !self.label.trim().is_empty() {
            return self.label.trim().to_string();
        }
        url::Url::parse(&self.issuer)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_else(|| "SSO".into())
    }
}

/// The callback this device registers with the provider, built from the one
/// place that answers what the device is called (`cast::sender_url`).
pub fn redirect_uri(state: &AppState) -> String {
    let sender = crate::cast::sender_url(state, None);
    match url::Url::parse(&sender) {
        Ok(url) => format!("{}/api/oidc/callback", url.origin().ascii_serialization()),
        Err(_) => format!("{}/api/oidc/callback", sender.trim_end_matches('/')),
    }
}

pub async fn load(pool: &sqlx::SqlitePool) -> OidcConfig {
    crate::db::load_setting(pool, KEY)
        .await
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

pub async fn save(pool: &sqlx::SqlitePool, config: &OidcConfig) -> anyhow::Result<()> {
    crate::db::save_setting(pool, KEY, &serde_json::to_string(config)?).await
}
```

(`cast::sender_url` must be reachable from `oidc`: it is `pub fn` in `cast/url.rs`; if `url` is a private module, re-export `pub use url::sender_url;` in `cast/mod.rs` — check how `main.rs` calls it today and use the same path.)

- [ ] **Step 4: State and the config API**

`AppState` gains `pub oidc: Arc<tokio::sync::RwLock<crate::oidc::config::OidcConfig>>`; in `main.rs`, after migrations: `oidc: Arc::new(RwLock::new(crate::oidc::config::load(&pool).await)),`. Any test code building `AppState` by hand gets `oidc: Default::default()`.

Create `src/oidc/api.rs` with `pub fn routes() -> Router<AppState>` holding `/api/oidc/info`, `/api/oidc/config` (get + put) now, and `start`/`callback` in Task 3. Merge it in `main.rs` like `accounts::api::routes()`.

```rust
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigUpdate {
    issuer: String,
    client_id: String,
    /// Absent or `null`: keep the stored secret. Write-only.
    #[serde(default)]
    client_secret: Option<String>,
    #[serde(default)]
    groups_claim: Option<String>,
    #[serde(default)]
    label: String,
    #[serde(default)]
    mapping: Vec<GroupMapping>,
    #[serde(default)]
    everyone_casts: bool,
    #[serde(default = "yes")]
    local_passwords: bool,
}
fn yes() -> bool { true }

async fn info(State(state): State<AppState>) -> Response {
    let config = state.oidc.read().await;
    Json(json!({
        "configured": config.configured(),
        "label": config.button_label(),
        "local_passwords": config.local_passwords,
    }))
    .into_response()
}

async fn get_config(State(state): State<AppState>) -> Response {
    let config = state.oidc.read().await.clone();
    Json(json!({
        "issuer": config.issuer,
        "client_id": config.client_id,
        "secret_set": !config.client_secret.is_empty(),
        "groups_claim": config.groups_claim,
        "label": config.label,
        "mapping": config.mapping,
        "everyone_casts": config.everyone_casts,
        "local_passwords": config.local_passwords,
        "redirect_uri": super::config::redirect_uri(&state),
    }))
    .into_response()
}

async fn put_config(State(state): State<AppState>, Json(body): Json<ConfigUpdate>) -> Response {
    let issuer = body.issuer.trim().trim_end_matches('/').to_string();
    if !issuer.is_empty() && !super::http::allowed_url(&issuer) {
        return error(StatusCode::BAD_REQUEST, "Der Anbieter muss über https erreichbar sein.");
    }
    let mut next = state.oidc.read().await.clone();
    next.issuer = issuer;
    next.client_id = body.client_id.trim().to_string();
    if let Some(secret) = body.client_secret {
        next.client_secret = secret;
    }
    next.groups_claim = body.groups_claim.filter(|c| !c.trim().is_empty()).unwrap_or_else(|| "groups".into());
    next.label = body.label.trim().to_string();
    next.mapping = body.mapping.into_iter().filter(|m| !m.group.trim().is_empty()).collect();
    next.everyone_casts = body.everyone_casts;
    if !body.local_passwords && !super::flow::sso_admin_exists(&state.pool).await {
        return error(StatusCode::BAD_REQUEST,
            "Lokale Passwörter lassen sich erst abschalten, wenn ein Admin per SSO angemeldet war.");
    }
    if !body.local_passwords && !next.configured() {
        return error(StatusCode::BAD_REQUEST, "Ohne eingerichteten Anbieter bleiben lokale Passwörter an.");
    }
    next.local_passwords = body.local_passwords;
    if let Err(e) = super::config::save(&state.pool, &next).await {
        tracing::error!("Failed to store the OpenID Connect configuration: {}", e);
        return error(StatusCode::INTERNAL_SERVER_ERROR, "Speichern fehlgeschlagen.");
    }
    *state.oidc.write().await = next;
    // A Basic credential that verified under the old rule must not outlive it.
    state.basic_cache.lock().await.clear();
    super::flow::forget_discovery().await;
    Json(json!({ "ok": true })).into_response()
}
```

`error()` is the same `{error}` helper the other API modules use. `flow::sso_admin_exists` and `flow::forget_discovery` are Task 3; for this task add them to a stub `src/oidc/flow.rs`:

```rust
/// Whether an enabled admin is linked to the provider -- the condition for
/// switching local passwords off without locking the venue out.
pub async fn sso_admin_exists(pool: &sqlx::SqlitePool) -> bool {
    sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM user_identities i JOIN users u ON u.id = i.user_id
         WHERE u.role = 'admin' AND u.disabled = 0",
    )
    .fetch_one(pool)
    .await
    .map(|n| n > 0)
    .unwrap_or(false)
}

pub async fn forget_discovery() {}
```

- [ ] **Step 5: Roles**

`src/accounts/roles.rs`:

```rust
        (true, ["api", "oidc", "info"]) | (true, ["api", "oidc", "start"]) | (true, ["api", "oidc", "callback"]) => Need::Open,
        (_, ["api", "oidc", "config"]) => Need::Admin,
```

placed **above** `(true, _) => Need::Read`. Matrix entries: `GET /api/oidc/info` Open, `GET /api/oidc/start` Open, `GET /api/oidc/callback` Open, `GET /api/oidc/config` Admin, `PUT /api/oidc/config` Admin.

- [ ] **Step 6: The admin card**

In `web/admin.html`, a card "Single Sign-on" among the admin-only cards (hidden for non-admins the way the others are): inputs for issuer, client ID, client secret (placeholder "gesetzt" when `secret_set`, empty input means keep), groups claim, label; a mapping table (group input + select admin/manager/editor/Nur Casten + remove button, "+ Zeile"); checkboxes "Alle vom Anbieter dürfen casten" and "Lokale Passwörter erlaubt"; a read-only line "Redirect-URI beim Anbieter eintragen: …" (textContent from `redirect_uri`); Speichern → `PUT /api/oidc/config`, showing the `{error}` inline. Built with `createElement`/`textContent`.

- [ ] **Step 7: Run and commit**

Run: `cargo test roles && cargo build && python3 tests/cast/test_oidc.py` — Expected: pass.

```bash
git add src tests/cast/test_oidc.py web/admin.html
git commit -m "Configure an OpenID Connect provider and what its groups mean"
```

---

### Task 3: The flow — start, callback, accounts and cast sessions

**Files:**
- Modify: `src/oidc/flow.rs`, `src/oidc/api.rs`, `src/models.rs` (`AppState::oidc_pending`)
- Test: `tests/cast/test_oidc.py`

**Interfaces:**
- Consumes: `http::{get_json, post_form}`, `jwt::{verify, kid_of, Expect, Jwk}`, `config::*`
- Produces:
  - `pub struct Pending { nonce: String, verifier: String, next: String, binding: String, created: Instant }`; `AppState::oidc_pending: Arc<Mutex<HashMap<String, Pending>>>` (key: state)
  - `pub enum Outcome { Account { user_id: i64, name: String }, Cast { name: String, issuer: String, subject: String }, Refused(&'static str) }`
  - `pub fn decide(groups: &[String], config: &OidcConfig) -> Option<Target>` (highest role wins; `Cast` only if no role; `everyone_casts` → `Cast` when nothing matched)
  - `pub async fn account_for(pool, issuer, subject, preferred_name, role) -> anyhow::Result<(i64, String)>`
  - `pub async fn disable_linked(pool, issuer, subject)`
  - `pub async fn create_cast_session(pool, name, issuer, subject) -> String`
  - `pub async fn cast_session_name(pool, token) -> Option<String>`
  - `pub const CAST_COOKIE: &str = "mcc_cast"`

- [ ] **Step 1: Failing tests**

Add to `tests/cast/test_oidc.py` (and call from `__main__`):

```python
def signin(browser, next_path="/admin.html"):
    """Walk the flow the way a browser does: start, provider, callback. Returns
    (status of the callback page, its text, the URL it sends the browser on to)."""
    opener = browser.opener
    req = urllib.request.Request(f"{BASE}/api/oidc/start?next={urllib.parse.quote(next_path)}")
    try:
        with opener.open(req, timeout=10) as res:  # follows start -> provider -> callback
            page = res.read().decode()
            status = res.status
    except urllib.error.HTTPError as e:
        page, status = e.read().decode(), e.code
    target = None
    marker = 'url='
    if marker in page:
        target = page.split(marker, 1)[1].split('"', 1)[0]
    return status, page, target


def flow_flow():
    print("\n[161] a mapped group signs in as an account with that role")
    with Server(), Provider() as idp:
        http("POST", "/api/users", {"name": "root", "password": "longenough", "role": "admin"})
        configure()
        b = Browser()
        status, page, target = signin(b)
        check("the callback answers with a page, not a redirect", status == 200, (status, page[:200]))
        check("which sends the browser on to next", target == "/admin.html", target)
        status, me, _ = b.call("GET", "/api/me")
        check("the session is the provider's user, as an editor",
              status == 200 and me["name"] == "anna" and me["role"] == "editor", me)
        users = basic("GET", "/api/users", "root", "longenough")[1]
        check("an account was created", any(u["name"] == "anna" for u in users), users)

        print("\n[162] the role follows the groups at every sign-in")
        idp.next_user = {"sub": "u1", "preferred_username": "anna", "groups": ["staff", "boss"]}
        b2 = Browser(); signin(b2)
        check("now an admin", b2.call("GET", "/api/me")[1]["role"] == "admin", None)
        idp.next_user = {"sub": "u1", "preferred_username": "anna", "groups": []}
        b3 = Browser(); status, page, _ = signin(b3)
        check("with no group left the sign-in is refused", "keine passende Gruppe" in page, page[:300])
        check("the account is disabled", next(u for u in basic("GET", "/api/users", "root", "longenough")[1]
                                             if u["name"] == "anna")["disabled"] is True, None)
        check("and its earlier session ended", b2.call("GET", "/api/me")[0] == 401, None)

        print("\n[163] a provider name never takes over a local account")
        idp.next_user = {"sub": "u9", "preferred_username": "root", "groups": ["staff"]}
        b4 = Browser(); signin(b4)
        me = b4.call("GET", "/api/me")[1]
        check("the SSO account is suffixed", me["name"] == "root-sso" and me["role"] == "editor", me)
        check("the local root is untouched", basic("GET", "/api/me", "root", "longenough")[1]["role"] == "admin", None)

        print("\n[164] a cast-only sign-in creates no account")
        idp.next_user = {"sub": "u5", "preferred_username": "mia", "groups": ["members"]}
        b5 = Browser(); status, page, target = signin(b5, "/")
        check("it succeeds and goes back to the guest page", status == 200 and target == "/", (status, target))
        check("no account", all(u["name"] != "mia" for u in basic("GET", "/api/users", "root", "longenough")[1]), None)
        status, _, url = b5.call("GET", "/admin.html")
        check("the admin page still asks to sign in", url and "/login.html" in url, url)
        check("and the API is a 401", b5.call("GET", "/api/playlist")[0] == 401, None)

        print("\n[165] everyone may cast, or nobody without a group")
        idp.next_user = {"sub": "u6", "preferred_username": "tom", "groups": []}
        b6 = Browser(); status, page, _ = signin(b6, "/")
        check("no group, not everyone: refused", "keine passende Gruppe" in page, page[:300])
        configure(everyone_casts=True)
        b7 = Browser(); status, page, target = signin(b7, "/")
        check("everyone may cast: a cast session", status == 200 and target == "/", (status, target))

        print("\n[166] tampered tokens and states are refused")
        for claim, value in (("aud", "someone-else"), ("iss", "http://evil.test"), ("exp", 1000), ("nonce", "x")):
            idp.tamper = {claim: value}
            bx = Browser(); status, page, _ = signin(bx)
            check(f"a wrong {claim} is refused", bx.call("GET", "/api/me")[0] == 401, (claim, page[:200]))
        idp.tamper = {}
        status, _, _ = Browser().call("GET", "/api/oidc/callback?code=nope&state=made-up", origin=None)
        check("an unknown state is refused", status in (400, 200), status)

        print("\n[167] next survives, a foreign next is dropped")
        idp.next_user = {"sub": "u1", "preferred_username": "anna", "groups": ["staff"]}
        _, _, target = signin(Browser(), "//evil.test/x")
        check("a foreign next becomes the admin page", target == "/admin.html", target)
```

(`Browser` from `test_users` follows redirects with a cookie jar, which is what a browser does here. `[166]`'s unknown-state case asserts on the account not being signed in rather than on a particular status; tighten to the status the implementation chooses.)

Run: expected FAIL at `[161]` (404 on `/api/oidc/start`).

- [ ] **Step 2: Discovery and keys (cached)**

In `flow.rs`:

```rust
struct Discovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
    basic_auth: bool,
}

static DISCOVERY: tokio::sync::Mutex<Option<(Discovery, Vec<Jwk>)>> = tokio::sync::Mutex::const_new(None);

pub async fn forget_discovery() {
    *DISCOVERY.lock().await = None;
}

async fn discover(config: &OidcConfig) -> anyhow::Result<(Discovery, Vec<Jwk>)> { /* ... */ }
```

`discover`: `GET {issuer}/.well-known/openid-configuration`; the document's `issuer` must equal the configured one (else error — a provider that disagrees about its own name is misconfigured or not the provider); every endpoint URL must pass `allowed_url`; `basic_auth` is true unless `token_endpoint_auth_methods_supported` exists and lacks `client_secret_basic`; then `GET jwks_uri` → `keys`. Cached in `DISCOVERY`; `keys_refreshed()` refetches the JWKS once when `jwt::kid_of(token)` names a key not in the cache.

- [ ] **Step 3: Start**

```rust
#[derive(Deserialize)]
struct StartQuery {
    #[serde(default)]
    next: Option<String>,
}

/// Only a path on this host: the rule the login page applies.
fn safe_next(next: Option<String>) -> String {
    match next {
        Some(n) if n.starts_with('/') && !n.starts_with("//") && !n.contains('\\') => n,
        _ => "/admin.html".into(),
    }
}

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rng(), &mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}
```

`start` handler: config must be `configured()` (else redirect to `/login.html?sso=off`); `discover` (error → `/login.html?sso=unreachable`); create `state`, `nonce`, `verifier`, `binding` (all `random_token()`); challenge = base64url(SHA-256(verifier)); prune `oidc_pending` entries older than 10 minutes, insert `Pending`; redirect (`303`) to `authorization_endpoint?response_type=code&client_id=…&redirect_uri=…&scope=openid%20profile%20groups&state=…&nonce=…&code_challenge=…&code_challenge_method=S256`, with `Set-Cookie: mcc_oidc=<binding>; HttpOnly; SameSite=Lax; Path=/api/oidc; Max-Age=600` (+ `Secure` when the request came via TLS — the `ViaTls` extension). `redirect_uri` is `config::redirect_uri(&state)` in both the authorize and the token request.

(`scope` includes `groups` because several providers only send the claim when asked; providers that do not know the scope ignore it.)

- [ ] **Step 4: Callback**

Order, each failure rendering the result page with a message and creating nothing:

1. Look up and **remove** `oidc_pending[state]` (single use); must exist, be < 10 min old, and its `binding` must equal the `mcc_oidc` cookie.
2. `post_form(token_endpoint, [grant_type=authorization_code, code, redirect_uri, code_verifier, (client_id, client_secret when not basic)], basic.then(|| (client_id, client_secret)))` → `id_token`.
3. `jwt::verify(id_token, keys, Expect { issuer, client_id, nonce, secret: client_secret, now })`; on `UnknownKey` refresh the JWKS once and retry.
4. `groups` = the configured claim as an array of strings (absent → empty); `decide(&groups, &config)`.
5. `Some(role)` → `account_for` + `accounts::create_session` → `mcc_session` cookie (same attributes as `accounts::api::login`); `Some(Cast)` → `create_cast_session` → `mcc_cast` cookie; `None` → `disable_linked(issuer, sub)` and the message "Anmeldung abgelehnt: keine passende Gruppe."
6. Answer `200` with the result page and `Set-Cookie: mcc_oidc=; Max-Age=0; Path=/api/oidc` plus the session cookie.

The result page (no templating of untrusted input into HTML — the message is one of a fixed set, and `next` is `safe_next`-checked and HTML-escaped):

```rust
fn page(message: Option<&str>, next: &str) -> Response {
    let escape = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('"', "&quot;");
    let body = match message {
        // A client-side navigation, not a 302: the session cookie is
        // SameSite=Strict, and a redirect chain that started on the provider's
        // site would not send it on the next request.
        None => format!(
            "<!doctype html><meta charset=utf-8><meta http-equiv=\"refresh\" content=\"0;url={0}\">\
             <p>Angemeldet. <a href=\"{0}\">Weiter</a></p>", escape(next)),
        Some(m) => format!(
            "<!doctype html><meta charset=utf-8><p>{}</p><p><a href=\"/login.html\">Zurück zur Anmeldung</a></p>",
            escape(m)),
    };
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], body).into_response()
}
```

- [ ] **Step 5: `decide`, accounts, cast sessions**

```rust
pub fn decide(groups: &[String], config: &OidcConfig) -> Option<Target> {
    let matched: Vec<Target> = config.mapping.iter()
        .filter(|m| groups.iter().any(|g| g == &m.group))
        .map(|m| m.target)
        .collect();
    for wanted in [Target::Admin, Target::Manager, Target::Editor, Target::Cast] {
        if matched.contains(&wanted) {
            return Some(wanted);
        }
    }
    config.everyone_casts.then_some(Target::Cast)
}
```

(unit-test it: highest wins, cast only without a role, everyone_casts fallback, empty → None.)

`account_for(pool, issuer, subject, preferred, role)` in one transaction: find `user_identities(issuer, subject)`; if linked → `UPDATE users SET role = ?, disabled = 0 WHERE id = ?`; else pick a free name (`preferred` trimmed, falling back to `subject`; then `-sso`, `-sso2`, … while taken), `INSERT INTO users (name, password_hash, role) VALUES (?, '', ?)`, `INSERT INTO user_identities`. Return `(id, name)`. Clear `state.basic_cache` after (a role changed).

`disable_linked`: `UPDATE users SET disabled = 1 WHERE id = (SELECT user_id FROM user_identities WHERE issuer = ? AND subject = ?)` and `DELETE FROM sessions WHERE user_id = (…)`.

`create_cast_session` / `cast_session_name`: as `accounts::create_session` / `session_identity` (SHA-256 of the token stored, 12 h sliding), on `cast_sessions`, returning the name.

The name for a cast session: `preferred_username`, else `name`, else `sub`.

- [ ] **Step 6: Run and commit**

Run: `cargo test oidc && cargo build && python3 tests/cast/test_oidc.py` — Expected: pass (Task 4 is what makes a cast session count on the guest page; `[164]` here only checks it is *not* an operator session).

```bash
git add src tests/cast/test_oidc.py
git commit -m "Sign in through the provider: an account for a role, a cast session otherwise"
```

---

### Task 4: The cast branch recognises a cast session (`Caster`)

**Files:**
- Modify: `src/accounts/middleware.rs`, `src/cast/api.rs` (`claim_session`, `authorize_sender`, `cast_info`), `src/accounts/mod.rs` (the `Caster` type)
- Test: `tests/cast/test_oidc.py`, `tests/cast/test_castaccess.py` (unchanged behaviour)

**Interfaces:**
- Produces: `#[derive(Clone)] pub struct Caster { pub name: String }` in `accounts`; the cast branch attaches `Caster` (from an account session **or** a cast session), never `Identity`. `authorize_sender(…, who: Option<&Caster>)`; `cast_info` takes `Option<Extension<Caster>>`.

- [ ] **Step 1: Failing test**

```python
def cast_flow():
    print("\n[168] a cast session counts on a screen kept for signed-in people")
    from test_display import Alone
    with Alone(), Provider() as idp:
        http("POST", "/api/users", {"name": "root", "password": "longenough", "role": "admin"})
        configure()
        admin_put("/api/displays/werkstatt", {"cast_access": "account"})
        idp.next_user = {"sub": "u5", "preferred_username": "mia", "groups": ["members"]}
        b = Browser(); signin(b, "/")
        status, body, _ = b.call("POST", "/api/cast/claim", {"display": "werkstatt"})
        check("the member with a cast session may claim", status == 200, (status, body))
        _, info, _ = b.call("GET", "/api/cast/info", origin=None)
        check("the guest page names them", info.get("account") == "mia", info)
        status, _, _ = Browser().call("POST", "/api/cast/claim", {"display": "werkstatt"})
        check("a guest still may not", status == 401, status)
```

Expected FAIL.

- [ ] **Step 2: Implement**

In `accounts/mod.rs`:

```rust
/// Who is casting, as the cast routes see it: a signed-in account or a cast
/// session. Deliberately not an `Identity` -- it carries no role, so nothing
/// that checks roles can be handed one.
#[derive(Debug, Clone)]
pub struct Caster {
    pub name: String,
}
```

In the middleware's cast branch, replace the identity attachment:

```rust
        let writing = method != Method::GET && method != Method::HEAD;
        if !writing || same_origin(request.headers(), request.uri()) {
            let caster = match session_token(request.headers()) {
                Some(token) => super::session_identity(&state.pool, &token).await.map(|who| who.name),
                None => None,
            };
            let caster = match caster {
                Some(name) => Some(name),
                None => match cookie(request.headers(), crate::oidc::flow::CAST_COOKIE) {
                    Some(token) => crate::oidc::flow::cast_session_name(&state.pool, &token).await,
                    None => None,
                },
            };
            if let Some(name) = caster {
                request.extensions_mut().insert(super::Caster { name });
            }
        }
```

Generalise `session_token` into `fn cookie(headers, name) -> Option<String>` and keep `session_token(headers)` as `cookie(headers, SESSION_COOKIE)`.

In `cast/api.rs`, change `Identity` to `Caster` in `authorize_sender`, `claim_session` and `cast_info` (`w.name` stays the same field). Search for any other `Extension<Identity>` in `src/cast/` and switch it.

- [ ] **Step 3: Run and commit**

Run: `cargo build && python3 tests/cast/test_oidc.py && python3 tests/cast/test_castaccess.py` — pass.

```bash
git add src tests/cast/test_oidc.py
git commit -m "Let a cast session cast where signed-in people may"
```

---

### Task 5: Local passwords off, and the login page

**Files:**
- Modify: `src/accounts/api.rs` (`login`), `src/accounts/middleware.rs` (`resolve`, Basic), `web/login.html`
- Test: `tests/cast/test_oidc.py`

- [ ] **Step 1: Failing test**

```python
def local_flow():
    print("\n[169] local passwords off")
    with Server(basic_auth_user="rescue", basic_auth_password="rescuepass1"), Provider() as idp:
        http("POST", "/api/users", {"name": "root", "password": "longenough", "role": "admin"},)
        configure()
        idp.next_user = {"sub": "u2", "preferred_username": "chef", "groups": ["boss"]}
        signin(Browser())
        status, _ = configure(local_passwords=False)
        check("with an SSO admin, passwords can be switched off", status == 200, status)
        status, _, _ = Browser().call("POST", "/api/login", {"name": "root", "password": "longenough"})
        check("the login form refuses a local account", status == 403, status)
        check("so does Basic", basic("GET", "/api/me", "root", "longenough")[0] == 401, None)
        check("the command-line credential still works",
              basic("GET", "/api/me", "rescue", "rescuepass1")[0] == 200, None)
        info = http("GET", "/api/oidc/info")[1]
        check("the login page is told", info["local_passwords"] is False, info)
```

(Check the flag names for the rescue credential in `models::Args` — `--basic-auth-user`/`--basic-auth-password` — and adjust the kwargs.)

- [ ] **Step 2: Implement**

`accounts::verify_password` stays as it is. In `login`: after the lockout check, if `!state.oidc.read().await.local_passwords`, answer `403 {"error": "Lokale Anmeldung ist abgeschaltet – bitte per SSO anmelden."}`. (The command-line credential has no account row and has never signed in through the form; it works through HTTP Basic, which the next sentence leaves alone.) In `resolve`'s Basic branch: when local passwords are off, skip `verify_password` (the rescue comparison above it still runs).

`web/login.html`: fetch `/api/oidc/info`; when `configured`, show a button "Mit ‹label› anmelden" linking to `/api/oidc/start?next=<the same safe next the page computes>`; when `local_passwords` is false, hide the name/password form. Show `?sso=off` / `?sso=unreachable` as a message ("SSO ist nicht eingerichtet." / "Der Anbieter ist nicht erreichbar."). `textContent` only.

- [ ] **Step 3: Run and commit**

Run: `cargo build && python3 tests/cast/test_oidc.py && python3 tests/cast/test_users.py && python3 tests/cast/test_auth.py && python3 tests/cast/test_basicauth.py`

```bash
git add src web/login.html tests/cast/test_oidc.py
git commit -m "Switch local passwords off, and offer the provider on the login page"
```

---

### Task 6: "nur angemeldet", documentation

**Files:**
- Modify: `web/displays.html` (label), `CLAUDE.md`, `README.md`, `docs/features.md`, `docs/deployment.md`, `docs/roadmap.md`, the spec

- [ ] **Step 1: Label** — in `displays.html`'s `ACCESS`, `['account', 'nur angemeldet']`.

- [ ] **Step 2: CLAUDE.md** — a section "Single sign-on (`src/oidc/`)" with the rules: no `reqwest` (and why), `(issuer, subject)` linking only, the Strict-cookie reason for the callback page, `mcc_oidc` Lax and single-use state, cast sessions in their own table and cookie read only in the cast branch as `Caster` (never `Identity`), redirect URI from `sender_url` only, local passwords off never touches the command-line credential, HS256 in the Python harness because it is stdlib-only. In the HTTP section, update the cast-branch bullet to say it attaches a `Caster`.

- [ ] **Step 3: README / docs** — README: the three `/api/oidc/*` routes and their shapes; `docs/deployment.md`: registering the client at a provider (redirect URI, scopes `openid profile groups`, confidential client, the groups claim), and the stable-name requirement; `docs/features.md`: the operator's view. Roadmap: remove the OpenID Connect entry. Spec: `Status: implemented`.

- [ ] **Step 4: Commit**

```bash
git add web/displays.html CLAUDE.md README.md docs
git commit -m "Document single sign-on"
```

---

## Final verification

- [ ] `cargo test`
- [ ] `cross build --target armv7-unknown-linux-gnueabihf` (or the target `docs/deployment.md` names) succeeds — the dependency rule is only real if this passes.
- [ ] Local instance on 3000 and its Chrome stopped; every suite in `tests/cast/` one at a time.
- [ ] Restart the user's local instance.
