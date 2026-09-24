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
openssl ec -in "$out/ec.pem" -pubout -outform DER 2>/dev/null | tail -c 64 > "$out/ec.point"
x=$(head -c 32 "$out/ec.point" | b64url)
y=$(tail -c 32 "$out/ec.point" | b64url)
rm "$out/ec.point"
printf '{"keys":[{"kty":"RSA","kid":"r1","alg":"RS256","n":"%s","e":"AQAB"},{"kty":"EC","kid":"e1","alg":"ES256","crv":"P-256","x":"%s","y":"%s"}]}' "$n" "$x" "$y" > "$out/jwks.json"
rm "$out/rsa.pem" "$out/ec.pem"
