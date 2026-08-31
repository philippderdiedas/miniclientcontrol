//! Outbound webhooks: the controller telling somebody else what happened.
//!
//! Not the picklecast `--webhook` returning. That was glue between two
//! processes and is dead because both sides live in one binary now; this tells
//! a *third party*, and nothing on the display path waits for it.

use http_body_util::{BodyExt, Limited};
use hyper_util::rt::TokioIo;
use minijinja::{AutoEscape, Environment, UndefinedBehavior};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tracing::error;

/// Every event name the catalogue offers, in the order the admin page shows them.
///
/// The UI reads this through `GET /api/webhooks/events` and never hard-codes a
/// copy: a page offering placeholders the server does not send is the
/// overlay-preview mistake in a new place.
pub const ALL_EVENTS: [&str; 10] = [
    "playback.item_changed",
    "playback.playlist_empty",
    "override.set",
    "override.cleared",
    "cast.started",
    "cast.ended",
    "guest_page.shown",
    "guest_page.ended",
    "display.disconnected",
    "display.connected",
];

/// Something worth telling somebody about.
///
/// Every URL in here is **already redacted** by the emit site
/// (`guest_page::redact`). The type takes `String`, not `Url`, so a caller
/// cannot accidentally hand over one carrying credentials.
#[derive(Debug, Clone)]
pub enum Event {
    ItemChanged {
        item_id: i64,
        kind: &'static str,
        title: String,
        url: String,
        duration: u64,
    },
    PlaylistEmpty,
    OverrideSet {
        url: String,
        source: &'static str,
    },
    OverrideCleared {
        source: &'static str,
    },
    CastStarted {
        sender_ip: String,
        mode: String,
    },
    CastEnded {
        reason: &'static str,
        duration_secs: i64,
    },
    GuestPageShown {
        url: String,
        sender_ip: String,
    },
    GuestPageEnded {
        reason: &'static str,
        duration_secs: i64,
    },
    DisplayDisconnected {
        error: String,
    },
    DisplayConnected {
        reconnect: bool,
    },
}

impl Event {
    pub fn name(&self) -> &'static str {
        match self {
            Event::ItemChanged { .. } => "playback.item_changed",
            Event::PlaylistEmpty => "playback.playlist_empty",
            Event::OverrideSet { .. } => "override.set",
            Event::OverrideCleared { .. } => "override.cleared",
            Event::CastStarted { .. } => "cast.started",
            Event::CastEnded { .. } => "cast.ended",
            Event::GuestPageShown { .. } => "guest_page.shown",
            Event::GuestPageEnded { .. } => "guest_page.ended",
            Event::DisplayDisconnected { .. } => "display.disconnected",
            Event::DisplayConnected { .. } => "display.connected",
        }
    }

    pub fn data(&self) -> Value {
        match self {
            Event::ItemChanged { item_id, kind, title, url, duration } => json!({
                "item_id": item_id,
                "kind": kind,
                "title": title,
                "url": url,
                "duration": duration,
            }),
            Event::PlaylistEmpty => json!({}),
            Event::OverrideSet { url, source } => json!({ "url": url, "source": source }),
            Event::OverrideCleared { source } => json!({ "source": source }),
            Event::CastStarted { sender_ip, mode } => {
                json!({ "sender_ip": sender_ip, "mode": mode })
            }
            Event::CastEnded { reason, duration_secs } => {
                json!({ "reason": reason, "duration_secs": duration_secs })
            }
            Event::GuestPageShown { url, sender_ip } => {
                json!({ "url": url, "sender_ip": sender_ip })
            }
            Event::GuestPageEnded { reason, duration_secs } => {
                json!({ "reason": reason, "duration_secs": duration_secs })
            }
            Event::DisplayDisconnected { error } => json!({ "error": error }),
            Event::DisplayConnected { reconnect } => json!({ "reconnect": reconnect }),
        }
    }
}

/// The object a target receives, and the context a template renders against.
///
/// One shape for both, so a target with a template and a target without one see
/// identical data.
pub fn envelope(event: &Event, device: &str, test: bool) -> Value {
    let mut value = json!({
        "event": event.name(),
        "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "device": device,
        "data": event.data(),
    });
    if test {
        value["test"] = json!(true);
    }
    value
}

/// The machine's hostname, read once at startup.
///
/// Deliberately not a setting: a receiver needs to tell two displays apart and
/// the hostname already does that. Empty when it cannot be read — a webhook
/// must not be the reason a device fails to start.
pub fn device_name() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// One configured receiver.
///
/// `headers` is a `BTreeMap` so the order a target sends its headers in is
/// stable, which makes a test assertion possible and a log readable.
#[derive(Debug, Clone)]
pub struct Target {
    pub id: i64,
    pub name: String,
    pub url: String,
    pub method: String,
    pub events: Vec<String>,
    pub headers: BTreeMap<String, String>,
    pub body: Option<String>,
    pub insecure_tls: bool,
}

impl Target {
    pub fn wants(&self, event_name: &str) -> bool {
        self.events.iter().any(|e| e == event_name)
    }
}

/// The enabled targets, read fresh on every event.
///
/// Not cached: an operator who disables a target expects the *next* event to
/// respect it, and this is a table with single-digit rows.
///
/// The JSON columns are `COALESCE`d because a real SQL `NULL` fails to decode
/// and would take the whole query with it -- one hand-written row would
/// otherwise silence every webhook.
pub async fn load_enabled(pool: &sqlx::SqlitePool) -> Vec<Target> {
    let rows = sqlx::query_as::<_, (i64, String, String, String, String, String, Option<String>, bool)>(
        "SELECT id, name, url,
                COALESCE(method, 'POST'),
                COALESCE(events, '[]'),
                COALESCE(headers, '{}'),
                body,
                COALESCE(insecure_tls, 0)
         FROM webhooks
         WHERE is_enabled = 1
         ORDER BY id ASC",
    )
    .fetch_all(pool)
    .await;

    let rows = match rows {
        Ok(rows) => rows,
        Err(e) => {
            error!("Failed to read webhook targets: {}", e);
            return Vec::new();
        }
    };

    rows.into_iter()
        .map(|(id, name, url, method, events, headers, body, insecure_tls)| Target {
            id,
            name,
            url,
            method,
            events: serde_json::from_str(&events).unwrap_or_default(),
            headers: serde_json::from_str(&headers).unwrap_or_default(),
            body,
            insecure_tls,
        })
        .collect()
}

/// A target's payload, ready to send.
#[derive(Debug)]
pub struct Rendered {
    pub body: String,
    /// Lower-cased header names, so the content-type default cannot end up
    /// duplicated by a target that spelled it differently.
    pub headers: BTreeMap<String, String>,
}

/// A minijinja environment with this project's two non-default decisions.
fn environment() -> Environment<'static> {
    let mut env = Environment::new();
    // minijinja escapes for HTML by default, which turns `&` into `&amp;`
    // inside a JSON string. Nothing here is ever HTML.
    env.set_auto_escape_callback(|_| AutoEscape::None);
    // A field missing from *this* event must not fail the delivery -- a
    // template written for one event is routinely subscribed to another, and a
    // missing title cannot be allowed to silence a display-disconnected notice.
    env.set_undefined_behavior(UndefinedBehavior::Lenient);
    env
}

/// Compile a template without rendering it, for validation on save.
///
/// The only check possible up front: a template valid for one event and
/// nonsense for another is what the test send is for.
pub fn compile_check(template: &str) -> Result<(), String> {
    environment()
        .template_from_str(template)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Render a target's body and headers against one event.
pub fn render(target: &Target, context: &Value) -> Result<Rendered, String> {
    let env = environment();

    let body = match &target.body {
        Some(template) => env
            .render_str(template, context)
            .map_err(|e| format!("body template: {e}"))?,
        None => context.to_string(),
    };

    let mut headers = BTreeMap::new();
    for (name, template) in &target.headers {
        let value = env
            .render_str(template, context)
            .map_err(|e| format!("header {name}: {e}"))?;
        headers.insert(name.to_ascii_lowercase(), value);
    }
    headers
        .entry("content-type".to_string())
        .or_insert_with(|| "application/json".to_string());

    Ok(Rendered { body, headers })
}

/// Connecting is given less than the whole budget so a black-holed address
/// cannot use it all up before a byte is sent.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const TOTAL_TIMEOUT: Duration = Duration::from_secs(10);
/// A receiver answering with a stream and never closing otherwise ties up the
/// socket for the whole timeout. Read a bounded amount and drop the rest.
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub enum Outcome {
    Status(u16),
    /// Refused, not followed: following one would send this target's
    /// `Authorization` header to a host the operator never configured.
    Redirect { status: u16, location: String },
    Error(String),
}

impl Outcome {
    pub fn ok(&self) -> bool {
        matches!(self, Outcome::Status(code) if (200..300).contains(code))
    }

    pub fn describe(&self) -> String {
        match self {
            Outcome::Status(code) => format!("HTTP {code}"),
            Outcome::Redirect { status, location } => {
                format!("HTTP {status} redirect to {location}, not followed")
            }
            Outcome::Error(message) => message.clone(),
        }
    }
}

/// Send one rendered payload to one target. One attempt, no retry.
pub async fn deliver(target: &Target, rendered: &Rendered) -> Outcome {
    match tokio::time::timeout(TOTAL_TIMEOUT, send(target, rendered)).await {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(message)) => Outcome::Error(message),
        Err(_) => Outcome::Error(format!("no answer within {}s", TOTAL_TIMEOUT.as_secs())),
    }
}

async fn send(target: &Target, rendered: &Rendered) -> Result<Outcome, String> {
    let url = url::Url::parse(&target.url).map_err(|e| format!("bad URL: {e}"))?;
    let host = url.host_str().ok_or("the URL has no host")?.to_string();
    let https = match url.scheme() {
        "https" => true,
        "http" => false,
        other => return Err(format!("unsupported scheme {other}")),
    };
    let port = url.port_or_known_default().unwrap_or(if https { 443 } else { 80 });
    let path = match url.query() {
        Some(query) => format!("{}?{}", url.path(), query),
        None => url.path().to_string(),
    };

    let tcp = tokio::time::timeout(
        CONNECT_TIMEOUT,
        tokio::net::TcpStream::connect((host.as_str(), port)),
    )
    .await
    .map_err(|_| format!("connecting to {host}:{port} timed out"))?
    .map_err(|e| format!("connecting to {host}:{port}: {e}"))?;

    let mut request = hyper::Request::builder()
        .method(target.method.as_str())
        .uri(&path)
        .header("host", format!("{host}:{port}"))
        .header(
            "user-agent",
            concat!("miniclientcontrol/", env!("CARGO_PKG_VERSION")),
        )
        .header("content-length", rendered.body.len().to_string());
    for (name, value) in &rendered.headers {
        request = request.header(name.as_str(), value.as_str());
    }
    let request = request
        .body(rendered.body.clone())
        .map_err(|e| format!("building the request: {e}"))?;

    let response = if https {
        let roots = RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let config = if target.insecure_tls {
            // Opt-in per target, never global: internal receivers on
            // self-signed certificates are this project's normal world, but a
            // target holding an API token must not skip verification silently.
            ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(NoVerification))
                .with_no_client_auth()
        } else {
            ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth()
        };
        let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
        let server_name = host
            .clone()
            .try_into()
            .map_err(|_| format!("'{host}' is not a valid server name"))?;
        let tls = connector
            .connect(server_name, tcp)
            .await
            .map_err(|e| format!("the TLS handshake failed: {e}"))?;
        exchange(TokioIo::new(tls), request).await?
    } else {
        exchange(TokioIo::new(tcp), request).await?
    };

    let status = response.status().as_u16();
    let location = response
        .headers()
        .get(hyper::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();

    // Read and discard a bounded amount, so the socket is not left open for a
    // receiver that answers with an endless stream.
    let _ = Limited::new(response.into_body(), MAX_RESPONSE_BYTES)
        .collect()
        .await;

    if (300..400).contains(&status) {
        return Ok(Outcome::Redirect { status, location });
    }
    Ok(Outcome::Status(status))
}

async fn exchange<S>(
    io: TokioIo<S>,
    request: hyper::Request<String>,
) -> Result<hyper::Response<hyper::body::Incoming>, String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, connection) = hyper::client::conn::http1::handshake(io)
        .await
        .map_err(|e| format!("the HTTP handshake failed: {e}"))?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    sender
        .send_request(request)
        .await
        .map_err(|e| format!("sending the request: {e}"))
}

/// Accepts any certificate. Reachable only through a target's `insecure_tls`.
#[derive(Debug)]
struct NoVerification;

impl tokio_rustls::rustls::client::danger::ServerCertVerifier for NoVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[tokio_rustls::rustls::pki_types::CertificateDer<'_>],
        _server_name: &tokio_rustls::rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: tokio_rustls::rustls::pki_types::UnixTime,
    ) -> Result<tokio_rustls::rustls::client::danger::ServerCertVerified, tokio_rustls::rustls::Error>
    {
        Ok(tokio_rustls::rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
        _dss: &tokio_rustls::rustls::DigitallySignedStruct,
    ) -> Result<tokio_rustls::rustls::client::danger::HandshakeSignatureValid, tokio_rustls::rustls::Error>
    {
        Ok(tokio_rustls::rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
        _dss: &tokio_rustls::rustls::DigitallySignedStruct,
    ) -> Result<tokio_rustls::rustls::client::danger::HandshakeSignatureValid, tokio_rustls::rustls::Error>
    {
        Ok(tokio_rustls::rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<tokio_rustls::rustls::SignatureScheme> {
        tokio_rustls::rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_event_names_itself() {
        assert_eq!(Event::PlaylistEmpty.name(), "playback.playlist_empty");
        assert_eq!(
            Event::CastStarted { sender_ip: "192.168.1.44".into(), mode: "cast".into() }.name(),
            "cast.started"
        );
    }

    #[test]
    fn the_envelope_carries_event_timestamp_device_and_data() {
        let value = envelope(
            &Event::CastStarted { sender_ip: "192.168.1.44".into(), mode: "cast".into() },
            "foyer-pi",
            false,
        );
        assert_eq!(value["event"], "cast.started");
        assert_eq!(value["device"], "foyer-pi");
        assert_eq!(value["data"]["sender_ip"], "192.168.1.44");
        assert_eq!(value["data"]["mode"], "cast");
        assert!(value["timestamp"].as_str().unwrap().ends_with('Z'));
        assert!(value.get("test").is_none(), "a real delivery is not flagged as a test");
    }

    #[test]
    fn a_test_delivery_says_so() {
        let value = envelope(&Event::PlaylistEmpty, "foyer-pi", true);
        assert_eq!(value["test"], true);
    }

    #[test]
    fn a_url_with_credentials_is_redacted_in_the_payload() {
        let url = url::Url::parse("https://bob:hunter2@dash.example.test/panel").unwrap();
        let event = Event::GuestPageShown {
            url: crate::guest_page::redact(&url),
            sender_ip: "192.168.1.44".into(),
        };
        let text = event.data().to_string();
        assert!(!text.contains("hunter2"), "the password reached the payload: {text}");
        assert!(!text.contains("bob"), "the username reached the payload: {text}");
        assert!(text.contains("dash.example.test"));
    }

    #[test]
    fn the_catalogue_lists_every_variant_exactly_once() {
        let mut seen: Vec<&str> = ALL_EVENTS.to_vec();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), ALL_EVENTS.len(), "ALL_EVENTS has a duplicate");
        assert_eq!(ALL_EVENTS.len(), 10);
    }

    fn target_wanting(events: &[&str]) -> Target {
        Target {
            id: 1,
            name: "Test".into(),
            url: "https://example.test/hook".into(),
            method: "POST".into(),
            events: events.iter().map(|e| e.to_string()).collect(),
            headers: Default::default(),
            body: None,
            insecure_tls: false,
        }
    }

    #[test]
    fn a_target_wants_only_the_events_it_subscribed_to() {
        let target = target_wanting(&["cast.started", "cast.ended"]);
        assert!(target.wants("cast.started"));
        assert!(!target.wants("playback.item_changed"));
        assert!(!target.wants("cast.startedX"), "matching must be exact, not a prefix");
    }

    #[test]
    fn a_target_subscribed_to_nothing_wants_nothing() {
        let target = target_wanting(&[]);
        for name in ALL_EVENTS {
            assert!(!target.wants(name));
        }
    }

    #[tokio::test]
    async fn load_enabled_skips_disabled_rows_and_decodes_json_columns() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::db::run_migrations(&pool).await.unwrap();

        sqlx::query(
            "INSERT INTO webhooks (name, url, events, headers, body, is_enabled)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind("Discord")
        .bind("https://example.test/a")
        .bind(r#"["cast.started"]"#)
        .bind(r#"{"X-Token":"abc"}"#)
        .bind("{{ event }}")
        .bind(true)
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query("INSERT INTO webhooks (name, url, is_enabled) VALUES (?, ?, ?)")
            .bind("Switched off")
            .bind("https://example.test/b")
            .bind(false)
            .execute(&pool)
            .await
            .unwrap();

        let targets = load_enabled(&pool).await;
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].name, "Discord");
        assert_eq!(targets[0].events, vec!["cast.started".to_string()]);
        assert_eq!(targets[0].headers.get("X-Token").map(String::as_str), Some("abc"));
        assert_eq!(targets[0].body.as_deref(), Some("{{ event }}"));
        assert_eq!(targets[0].method, "POST", "the column default is POST");
    }

    #[tokio::test]
    async fn a_null_json_column_does_not_take_the_whole_query_down() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::db::run_migrations(&pool).await.unwrap();

        // A row written by hand, or by an older version, can hold a real NULL.
        // One such row must not blank the whole list.
        sqlx::query("INSERT INTO webhooks (name, url, events, headers) VALUES (?, ?, NULL, NULL)")
            .bind("Hand-written")
            .bind("https://example.test/c")
            .execute(&pool)
            .await
            .unwrap();

        let targets = load_enabled(&pool).await;
        assert_eq!(targets.len(), 1, "the COALESCE on the read path is missing");
        assert!(targets[0].events.is_empty());
        assert!(targets[0].headers.is_empty());
    }

    fn ctx() -> serde_json::Value {
        envelope(
            &Event::GuestPageShown {
                url: "https://dash.example.test/a?x=1&y=2".into(),
                sender_ip: "192.168.1.44".into(),
            },
            "foyer-pi",
            false,
        )
    }

    #[test]
    fn no_template_sends_the_envelope_as_json() {
        let target = target_wanting(&["guest_page.shown"]);
        let rendered = render(&target, &ctx()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&rendered.body).unwrap();
        assert_eq!(parsed["event"], "guest_page.shown");
        assert_eq!(
            rendered.headers.get("content-type").map(String::as_str),
            Some("application/json")
        );
    }

    #[test]
    fn tojson_escapes_a_value_into_valid_json() {
        let mut target = target_wanting(&["guest_page.shown"]);
        // A title with a quote in it is what breaks the naive "{{ x }}" form.
        target.body = Some(r#"{"text": {{ data.url | tojson }}}"#.into());
        let rendered = render(&target, &ctx()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&rendered.body)
            .expect("tojson must produce parseable JSON");
        assert_eq!(parsed["text"], "https://dash.example.test/a?x=1&y=2");
    }

    #[test]
    fn autoescape_is_off_so_an_ampersand_survives() {
        let mut target = target_wanting(&["guest_page.shown"]);
        target.body = Some("{{ data.url }}".into());
        let rendered = render(&target, &ctx()).unwrap();
        assert!(
            rendered.body.contains("x=1&y=2"),
            "HTML escaping turned & into &amp;: {}",
            rendered.body
        );
    }

    #[test]
    fn an_undefined_field_renders_empty_instead_of_failing() {
        let mut target = target_wanting(&["guest_page.shown"]);
        target.body = Some("title=[{{ data.title }}]".into());
        let rendered = render(&target, &ctx()).expect("a missing field must not fail the delivery");
        assert_eq!(rendered.body, "title=[]");
    }

    #[test]
    fn header_values_are_templates_too() {
        let mut target = target_wanting(&["guest_page.shown"]);
        target.headers.insert("X-Event".into(), "{{ event }}".into());
        target.headers.insert("Authorization".into(), "Bearer static-token".into());
        let rendered = render(&target, &ctx()).unwrap();
        assert_eq!(rendered.headers.get("x-event").map(String::as_str), Some("guest_page.shown"));
        assert_eq!(
            rendered.headers.get("authorization").map(String::as_str),
            Some("Bearer static-token")
        );
    }

    #[test]
    fn a_target_setting_its_own_content_type_keeps_it() {
        let mut target = target_wanting(&["guest_page.shown"]);
        target.body = Some("plain words".into());
        target.headers.insert("Content-Type".into(), "text/plain".into());
        let rendered = render(&target, &ctx()).unwrap();
        assert_eq!(rendered.headers.get("content-type").map(String::as_str), Some("text/plain"));
    }

    #[test]
    fn a_broken_template_is_an_error_with_a_position_not_a_panic() {
        let mut target = target_wanting(&["guest_page.shown"]);
        target.body = Some("{{ unclosed ".into());
        let error = render(&target, &ctx()).unwrap_err();
        assert!(!error.is_empty());
        assert!(compile_check("{{ unclosed ").is_err());
        assert!(compile_check("{{ data.url | tojson }}").is_ok());
    }

    /// A one-shot HTTP server on an ephemeral port. Returns its URL, a handle
    /// that yields the request it received, and nothing else -- the tests that
    /// need a real listener over many requests live in the Python suite.
    async fn one_shot(response: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = socket.read(&mut buf).await.unwrap();
            socket.write_all(response.as_bytes()).await.unwrap();
            let _ = socket.flush().await;
            String::from_utf8_lossy(&buf[..n]).to_string()
        });
        (format!("http://127.0.0.1:{port}/hook"), handle)
    }

    #[tokio::test]
    async fn a_delivery_sends_the_body_and_the_headers() {
        let (url, handle) = one_shot("HTTP/1.1 204 No Content\r\ncontent-length: 0\r\n\r\n").await;
        let mut target = target_wanting(&["cast.started"]);
        target.url = url;
        target.headers.insert("X-Token".into(), "abc".into());

        let rendered = render(&target, &ctx()).unwrap();
        let outcome = deliver(&target, &rendered).await;

        assert!(matches!(outcome, Outcome::Status(204)), "{outcome:?}");
        assert!(outcome.ok());

        let request = handle.await.unwrap();
        assert!(request.starts_with("POST /hook HTTP/1.1"), "{request}");
        assert!(request.to_lowercase().contains("x-token: abc"), "{request}");
        assert!(request.contains("\"event\""), "the body did not arrive: {request}");
    }

    #[tokio::test]
    async fn a_redirect_is_refused_and_reported_with_its_location() {
        let (url, handle) = one_shot(
            "HTTP/1.1 301 Moved Permanently\r\nlocation: https://elsewhere.test/h\r\ncontent-length: 0\r\n\r\n",
        )
        .await;
        let mut target = target_wanting(&["cast.started"]);
        target.url = url;

        let rendered = render(&target, &ctx()).unwrap();
        let outcome = deliver(&target, &rendered).await;

        match &outcome {
            Outcome::Redirect { status, location } => {
                assert_eq!(*status, 301);
                assert_eq!(location, "https://elsewhere.test/h");
            }
            other => panic!("a redirect must not be followed: {other:?}"),
        }
        assert!(!outcome.ok());
        // Exactly one request was made: the body never went to the new location.
        let _ = handle.await.unwrap();
    }

    #[tokio::test]
    async fn the_body_never_reaches_the_redirect_location() {
        // The test above proves the *outcome*, but its location is a name that
        // does not resolve -- so it cannot tell "did not follow" from "could
        // not follow". Here the location is a second real listener, and the
        // refusal is proven by its silence.
        let (elsewhere, elsewhere_handle) =
            one_shot("HTTP/1.1 204 No Content\r\ncontent-length: 0\r\n\r\n").await;
        let response: &'static str = Box::leak(
            format!(
                "HTTP/1.1 307 Temporary Redirect\r\nlocation: {elsewhere}\r\ncontent-length: 0\r\n\r\n"
            )
            .into_boxed_str(),
        );
        let (url, handle) = one_shot(response).await;
        let mut target = target_wanting(&["cast.started"]);
        target.url = url;
        // The header a followed redirect would leak to a host the operator
        // never configured.
        target.headers.insert("Authorization".into(), "Bearer secret".into());

        let rendered = render(&target, &ctx()).unwrap();
        let outcome = deliver(&target, &rendered).await;
        assert!(matches!(outcome, Outcome::Redirect { .. }), "{outcome:?}");

        let first = handle.await.unwrap();
        assert!(
            first.to_lowercase().contains("authorization: bearer secret"),
            "the first hop should have had the header: {first}"
        );
        // Still waiting to accept, so nothing ever connected to it.
        assert!(
            tokio::time::timeout(Duration::from_millis(250), elsewhere_handle)
                .await
                .is_err(),
            "the request was forwarded to the redirect location"
        );
    }

    #[tokio::test]
    async fn a_500_is_not_ok_but_is_still_a_status() {
        let (url, handle) = one_shot("HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\n\r\n").await;
        let mut target = target_wanting(&["cast.started"]);
        target.url = url;
        let rendered = render(&target, &ctx()).unwrap();
        let outcome = deliver(&target, &rendered).await;
        assert!(matches!(outcome, Outcome::Status(500)), "{outcome:?}");
        assert!(!outcome.ok());
        let _ = handle.await.unwrap();
    }

    #[tokio::test]
    async fn an_unreachable_target_is_an_error_not_a_panic() {
        let mut target = target_wanting(&["cast.started"]);
        // Port 1 on loopback: nothing listens, and the refusal is immediate.
        target.url = "http://127.0.0.1:1/hook".into();
        let rendered = render(&target, &ctx()).unwrap();
        let outcome = deliver(&target, &rendered).await;
        assert!(matches!(outcome, Outcome::Error(_)), "{outcome:?}");
        assert!(!outcome.describe().is_empty());
    }
}
