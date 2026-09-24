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

    // A key that names its algorithm is only good for that one: an RSA key
    // published for RS256 must not verify anything else.
    let pick = |kty: &str| {
        keys.iter()
            .filter(|k| k.kty == kty && k.alg.as_deref().is_none_or(|a| a == alg))
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
    }

    #[test]
    fn a_key_is_only_good_for_its_own_algorithm() {
        let mut ks = keys();
        for k in &mut ks { k.alg = Some("PS256".into()); }
        assert_eq!(verify(RS.trim(), &ks, &expect()), Err(JwtError::UnknownKey));
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
