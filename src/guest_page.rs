//! Validating and displaying a URL a guest asked the kiosk to open.

use url::Url;

/// Long enough for any real link, short enough that a socket frame cannot be
/// used to push megabytes through the session.
pub const MAX_URL_LEN: usize = 2048;

/// Parse what a guest typed, or say why it will not do.
///
/// The error strings reach the guest's screen, so they are German and say what
/// to do rather than what went wrong internally.
pub fn parse_guest_url(raw: &str) -> Result<Url, &'static str> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("Bitte eine Adresse eingeben.");
    }
    if raw.len() > MAX_URL_LEN {
        return Err("Die Adresse ist zu lang.");
    }
    let parsed = Url::parse(raw).map_err(|_| "Das ist keine gültige Adresse.")?;
    // Scheme first. `file:` and `data:` would turn the display into a reader for
    // whatever the device can reach, and a kiosk has nobody standing there to
    // refuse it. Addresses on the local network stay allowed on purpose: a venue
    // may well want its own dashboard on the screen.
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("Nur http:// und https:// sind möglich.");
    }
    if parsed.host_str().map_or(true, str::is_empty) {
        return Err("Die Adresse hat keinen Host.");
    }
    Ok(parsed)
}

/// The URL as it may be logged or shown to the operator.
///
/// Credentials in an address are accepted -- refusing them would break showing
/// an internal dashboard on purpose, which is exactly what signage is for. The
/// real harm is that they reach the journal, a webhook receiver and the admin
/// card, so that is where it is solved: here, for every one of them.
///
/// Two places a secret sits are handled. The userinfo is dropped, which also
/// disposes of `http://google.com@evil.test`, a URL that reads like Google until
/// it is reserialised without it. And the *value* of every query or fragment
/// parameter whose name looks like a secret is replaced with `***` -- a
/// dashboard that logs in through `login.py?_password=…` put a real password in a
/// real journal before this. A secret in a path segment has no name to recognise
/// and is out of reach.
///
/// Everything else survives byte for byte: the query is rewritten piece by
/// piece rather than reparsed, because re-encoding a Grafana link would make the
/// log line stop matching what the operator typed.
pub fn redact(url: &Url) -> String {
    let mut shown = url.clone();
    let _ = shown.set_username("");
    let _ = shown.set_password(None);
    if let Some(query) = shown.query().map(mask_secret_params) {
        shown.set_query(Some(&query));
    }
    if let Some(fragment) = shown.fragment().map(mask_secret_params) {
        shown.set_fragment(Some(&fragment));
    }
    shown.to_string()
}

/// Parameter names that carry a secret, matched as substrings of the
/// lowercased, percent-decoded name -- `_password`, `access_token`, `api_key`
/// and `client_secret` included. Generous on purpose: masking a harmless value
/// costs a less readable log line, missing a password costs the password.
const SECRET_NAME_PARTS: &[&str] = &[
    "pass", "pwd", "token", "secret", "key", "auth", "sig", "session", "credential",
];

fn mask_secret_params(raw: &str) -> String {
    raw.split('&')
        .map(|piece| match piece.split_once('=') {
            Some((name, _)) if is_secret_name(name) => format!("{name}=***"),
            _ => piece.to_string(),
        })
        .collect::<Vec<_>>()
        .join("&")
}

fn is_secret_name(raw: &str) -> bool {
    let name = urlencoding::decode(raw)
        .map(|decoded| decoded.to_ascii_lowercase())
        .unwrap_or_else(|_| raw.to_ascii_lowercase());
    SECRET_NAME_PARTS.iter().any(|part| name.contains(part))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_http_and_https_are_accepted() {
        assert!(parse_guest_url("https://example.test/menu").is_ok());
        assert!(parse_guest_url("http://example.test/menu").is_ok());
        assert!(parse_guest_url("HTTPS://example.test/").is_ok());
        // The display is an output channel and the process has a filesystem.
        assert!(parse_guest_url("file:///etc/passwd").is_err());
        assert!(parse_guest_url("chrome://settings").is_err());
        assert!(parse_guest_url("javascript:alert(1)").is_err());
        assert!(parse_guest_url("data:text/html,hi").is_err());
    }

    #[test]
    fn a_url_needs_a_host_and_a_sane_length() {
        assert!(parse_guest_url("https://").is_err());
        assert!(parse_guest_url("not a url").is_err());
        assert!(parse_guest_url("").is_err());
        assert!(parse_guest_url("   ").is_err());
        let long = format!("https://example.test/{}", "a".repeat(MAX_URL_LEN));
        assert!(parse_guest_url(&long).is_err());
    }

    #[test]
    fn lan_targets_are_allowed_on_purpose() {
        // Decision on record: a venue may want an internal dashboard on screen.
        assert!(parse_guest_url("http://192.168.1.1/").is_ok());
        assert!(parse_guest_url("http://127.0.0.1:3000/").is_ok());
        assert!(parse_guest_url("http://wiki.intern/").is_ok());
    }

    #[test]
    fn credentials_survive_to_the_browser_but_never_to_a_log() {
        let parsed = parse_guest_url("https://admin:hunter2@wiki.intern/page").unwrap();
        // The browser gets them: refusing would break the internal-dashboard
        // case, and `?token=` would be equivalent and unfilterable anyway.
        assert!(parsed.as_str().contains("hunter2"));
        let shown = redact(&parsed);
        assert!(!shown.contains("hunter2"));
        assert!(!shown.contains("admin"));
        assert_eq!(shown, "https://wiki.intern/page");
    }

    #[test]
    fn redaction_reveals_a_deceptive_host() {
        // Reads like Google in a card that prints the raw string.
        let parsed = parse_guest_url("http://google.com@evil.test/").unwrap();
        assert_eq!(redact(&parsed), "http://evil.test/");
    }

    #[test]
    fn a_secret_in_the_query_is_masked_and_nothing_else_is_touched() {
        // The shape that put a real password in a real journal: checkmk's
        // kiosk login carries it as a query parameter.
        let parsed = Url::parse(
            "https://checkmk.test/igx/check_mk/login.py?_username=kiosk&_password=hunter2\
             &_login=1&_origtarget=dashboard.py%3Fname=problems",
        )
        .unwrap();
        let shown = redact(&parsed);
        assert!(!shown.contains("hunter2"), "{shown}");
        assert_eq!(
            shown,
            "https://checkmk.test/igx/check_mk/login.py?_username=kiosk&_password=***\
             &_login=1&_origtarget=dashboard.py%3Fname=problems"
        );
    }

    #[test]
    fn the_usual_names_for_a_secret_are_all_masked() {
        for name in ["password", "passwd", "pwd", "token", "access_token", "api_key",
                     "apikey", "key", "secret", "client_secret", "auth", "sig",
                     "signature", "session", "sessionid", "credential", "PASSWORD"] {
            let parsed = Url::parse(&format!("https://a.test/?{name}=hunter2&page=2")).unwrap();
            let shown = redact(&parsed);
            assert!(!shown.contains("hunter2"), "{name}: {shown}");
            assert!(shown.ends_with("&page=2"), "{name}: {shown}");
        }
    }

    #[test]
    fn a_harmless_query_survives_byte_for_byte() {
        // A Grafana link is mostly query, and re-encoding it would make the log
        // line stop matching what the operator typed.
        let raw = "https://grafana.test/d/x/kiosk?orgId=1&from=now%2Fd&to=now\
                   &var-employees=$__all&refresh=10s&kiosk=true";
        let parsed = Url::parse(raw).unwrap();
        assert_eq!(redact(&parsed), parsed.as_str());
    }

    #[test]
    fn a_secret_in_the_fragment_is_masked_too() {
        // Where an OAuth implicit flow leaves its token.
        let parsed = Url::parse("https://a.test/cb#access_token=hunter2&state=x").unwrap();
        let shown = redact(&parsed);
        assert_eq!(shown, "https://a.test/cb#access_token=***&state=x");
    }

    #[test]
    fn an_encoded_parameter_name_is_recognised() {
        let parsed = Url::parse("https://a.test/?pass%77ord=hunter2").unwrap();
        assert!(!redact(&parsed).contains("hunter2"));
    }
}
