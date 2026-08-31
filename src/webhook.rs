//! Outbound webhooks: the controller telling somebody else what happened.
//!
//! Not the picklecast `--webhook` returning. That was glue between two
//! processes and is dead because both sides live in one binary now; this tells
//! a *third party*, and nothing on the display path waits for it.

use http_body_util::{BodyExt, Limited};
use hyper_util::rt::TokioIo;
use minijinja::{AutoEscape, Environment, UndefinedBehavior};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Semaphore};
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tracing::{debug, error, warn};

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
    // `Url::host_str` returns an IPv6 literal wrapped in brackets (`[::1]`),
    // which is exactly what the `Host` header wants but neither `IpAddr`
    // parsing, DNS, nor `ServerName` accepts -- each of those needs the
    // address bare. Keep `host` (bracketed) for the header and use this for
    // everything that resolves or verifies a name.
    let connect_host = host.trim_start_matches('[').trim_end_matches(']');
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
        tokio::net::TcpStream::connect((connect_host, port)),
    )
    .await
    .map_err(|_| format!("connecting to {connect_host}:{port} timed out"))?
    .map_err(|e| format!("connecting to {connect_host}:{port}: {e}"))?;

    // Merged into one map before anything is built, because
    // `Request::builder().header()` *appends* rather than replaces: a target
    // whose headers name `content-length` would otherwise put two conflicting
    // ones on the wire, which is the request-smuggling shape if any proxy sits
    // between us and the receiver. The target's own value wins for everything
    // else, so a vhosted receiver can still be given the `host` it needs.
    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    headers.insert("host".into(), format!("{host}:{port}"));
    headers.insert(
        "user-agent".into(),
        concat!("miniclientcontrol/", env!("CARGO_PKG_VERSION")).to_string(),
    );
    for (name, value) in &rendered.headers {
        headers.insert(name.clone(), value.clone());
    }
    // Last, and not overridable: it must describe the body we are actually
    // sending, whatever the target asked for.
    headers.insert("content-length".into(), rendered.body.len().to_string());

    let mut request = hyper::Request::builder()
        .method(target.method.as_str())
        .uri(&path);
    for (name, value) in &headers {
        request = request.header(name.as_str(), value.as_str());
    }
    let request = request
        .body(rendered.body.clone())
        .map_err(|e| format!("building the request: {e}"))?;

    let response = if https {
        let config = if target.insecure_tls {
            // Opt-in per target, never global: internal receivers on
            // self-signed certificates are this project's normal world, but a
            // target holding an API token must not skip verification silently.
            ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(NoVerification))
                .with_no_client_auth()
        } else {
            // Built here, not above the branch: cloning the whole root set on
            // every delivery just to discard it when `insecure_tls` is set
            // would be wasted work on the far more common path.
            let roots = RootCertStore {
                roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
            };
            ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth()
        };
        let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
        let server_name = connect_host
            .to_string()
            .try_into()
            .map_err(|_| format!("'{connect_host}' is not a valid server name"))?;
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
        .map(str::to_string);

    // Read and discard a bounded amount, so the socket is not left open for a
    // receiver that answers with an endless stream.
    let _ = Limited::new(response.into_body(), MAX_RESPONSE_BYTES)
        .collect()
        .await;

    // A `3xx` with no `Location` has nowhere to send anyone, so it is not a
    // redirect this code refused -- it is just a status, and reporting it as
    // an empty-location "redirect" would read as a bug in the log.
    if let (true, Some(location)) = ((300..400).contains(&status), location) {
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

/// How many deliveries may be in flight at once, across all targets.
///
/// Past this an event is dropped rather than queued. The alternative is
/// unbounded `spawn` on a Pi, and `playback.item_changed` against a receiver
/// that has begun to hang is exactly the shape that produces thousands of
/// parked tasks holding sockets. Dropping is honest: the contract is already
/// best-effort.
const MAX_INFLIGHT: usize = 8;

/// What the admin page shows beside a target.
///
/// In memory only. Persisting it would put an SD-card write on the path of
/// every event, on a device where the card is the component that dies.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LastResult {
    pub at: String,
    pub event: String,
    pub outcome: String,
    pub ok: bool,
}

pub struct Dispatcher {
    pool: sqlx::SqlitePool,
    device: String,
    inflight: Arc<Semaphore>,
    last: Arc<Mutex<HashMap<i64, LastResult>>>,
}

impl Dispatcher {
    pub fn new(pool: sqlx::SqlitePool) -> Self {
        Self {
            pool,
            device: device_name(),
            inflight: Arc::new(Semaphore::new(MAX_INFLIGHT)),
            last: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Tell every interested target about an event.
    ///
    /// **Never blocks, never fails, never awaits the network.** This is called
    /// from inside the control loop in `browser.rs`; a version that could wait
    /// would put a stranger's HTTP server in the path of what is on the screen.
    pub fn fire(&self, event: Event) {
        let pool = self.pool.clone();
        let device = self.device.clone();
        let inflight = self.inflight.clone();
        let last = self.last.clone();

        tokio::spawn(async move {
            let name = event.name();
            let targets: Vec<Target> = load_enabled(&pool)
                .await
                .into_iter()
                .filter(|t| t.wants(name))
                .collect();
            if targets.is_empty() {
                return;
            }

            let context = envelope(&event, &device, false);
            for target in targets {
                let permit = match inflight.clone().try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        warn!(
                            "Webhook '{}': {} dropped, {} deliveries already in flight",
                            target.name, name, MAX_INFLIGHT
                        );
                        continue;
                    }
                };
                let context = context.clone();
                let last = last.clone();
                tokio::spawn(async move {
                    // Bound to the delivery, and the FIRST statement so that
                    // nothing appended later can shorten its life. `let _ =
                    // permit;` at the end would not do: a wildcard `let` is not
                    // a read, so under 2021 disjoint capture the block never
                    // captures the permit at all -- it would drop at the end of
                    // this loop iteration, before the future is first polled,
                    // and the bound would be silently inert.
                    let _permit = permit;
                    let outcome = run(&target, &context).await;
                    record(&last, &target, name, &outcome).await;
                });
            }
        });
    }

    /// Render and deliver one target synchronously, for the test-send endpoint
    /// and for the unit tests. Records the result like a real delivery.
    pub async fn deliver_one(&self, target: &Target, event: &Event, test: bool) -> Outcome {
        let context = envelope(event, &self.device, test);
        let outcome = run(target, &context).await;
        record(&self.last, target, event.name(), &outcome).await;
        outcome
    }

    pub async fn last_results(&self) -> HashMap<i64, LastResult> {
        self.last.lock().await.clone()
    }
}

/// Render then deliver. A render error never reaches the network.
async fn run(target: &Target, context: &Value) -> Outcome {
    match render(target, context) {
        Ok(rendered) => deliver(target, &rendered).await,
        Err(message) => Outcome::Error(message),
    }
}

async fn record(
    last: &Arc<Mutex<HashMap<i64, LastResult>>>,
    target: &Target,
    event: &str,
    outcome: &Outcome,
) {
    // Named by `name`, never by URL: a URL may carry a token in its query.
    if outcome.ok() {
        debug!("Webhook '{}': {} -> {}", target.name, event, outcome.describe());
    } else {
        error!("Webhook '{}': {} -> {}", target.name, event, outcome.describe());
    }
    last.lock().await.insert(
        target.id,
        LastResult {
            at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            event: event.to_string(),
            outcome: outcome.describe(),
            ok: outcome.ok(),
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bare `sqlite::memory:` gives every pool connection its own anonymous
    /// database; a second connection -- which a `fire` delivery opens from its
    /// spawned task -- sees no `webhooks` table, `load_enabled` comes back
    /// empty, and the delivery is silently dropped. That surfaces as a
    /// `wait_for` timeout, not as an error naming the real cause. Capping the
    /// pool at one connection is a simpler guarantee than `?cache=shared`, and
    /// this is the one place every such test should get it from, so a test
    /// copied from a neighbour inherits the safe form automatically.
    async fn memory_pool() -> sqlx::SqlitePool {
        sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap()
    }

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
        let pool = memory_pool().await;
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
        let pool = memory_pool().await;
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
    ///
    /// Takes the response by value rather than `&'static str` so a caller with
    /// an owned, formatted string (say, one embedding another listener's URL)
    /// does not have to `Box::leak` it just to satisfy the spawned task's
    /// lifetime.
    async fn one_shot(response: impl Into<String>) -> (String, tokio::task::JoinHandle<String>) {
        one_shot_on("127.0.0.1:0", response)
            .await
            .expect("binding to IPv4 loopback must not fail")
    }

    /// The general form behind `one_shot`, parameterised on the bind address
    /// so the IPv6-literal test can ask for `[::1]:0` and get `None` back
    /// instead of a panic on a machine with no IPv6 loopback configured.
    async fn one_shot_on(
        bind_addr: &str,
        response: impl Into<String>,
    ) -> Option<(String, tokio::task::JoinHandle<String>)> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let response = response.into();
        let listener = tokio::net::TcpListener::bind(bind_addr).await.ok()?;
        let local = listener.local_addr().unwrap();
        let port = local.port();
        let url_host = if local.is_ipv6() {
            format!("[{}]", local.ip())
        } else {
            local.ip().to_string()
        };
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            // Read until the header block is complete, then keep reading until
            // the declared body length is satisfied. hyper happens to coalesce
            // a small request into one write today, so a single `read` passes
            // now, but nothing guarantees that stays true and a request that
            // outgrows one segment would silently truncate what the caller
            // asserts against.
            let header_end = loop {
                let n = socket.read(&mut chunk).await.unwrap();
                assert!(n > 0, "connection closed before the headers arrived");
                buf.extend_from_slice(&chunk[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break pos + 4;
                }
            };
            let content_length: usize = String::from_utf8_lossy(&buf[..header_end])
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().to_string())
                })
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            while buf.len() < header_end + content_length {
                let n = socket.read(&mut chunk).await.unwrap();
                assert!(n > 0, "connection closed before the body arrived");
                buf.extend_from_slice(&chunk[..n]);
            }
            socket.write_all(response.as_bytes()).await.unwrap();
            let _ = socket.flush().await;
            String::from_utf8_lossy(&buf).to_string()
        });
        Some((format!("http://{url_host}:{port}/hook"), handle))
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
        // Pins the reported status and the verbatim location string; it does
        // not prove the body never reached that location -- the address does
        // not resolve, so a follow attempt would surface as an error here
        // rather than a second request. See the test below for that proof.
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
        let response = format!(
            "HTTP/1.1 307 Temporary Redirect\r\nlocation: {elsewhere}\r\ncontent-length: 0\r\n\r\n"
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
        // Exactly one request was made: the body never went to the new
        // location. Still waiting to accept, so nothing ever connected to it.
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
    async fn a_3xx_with_no_location_is_a_plain_status_not_an_empty_redirect() {
        // A 304 legitimately carries no Location. Reporting it as
        // `Redirect { location: "" }` reads as a bug in the log -- there is
        // nowhere this could have redirected to.
        let (url, handle) = one_shot("HTTP/1.1 304 Not Modified\r\ncontent-length: 0\r\n\r\n").await;
        let mut target = target_wanting(&["cast.started"]);
        target.url = url;
        let rendered = render(&target, &ctx()).unwrap();
        let outcome = deliver(&target, &rendered).await;
        assert!(matches!(outcome, Outcome::Status(304)), "{outcome:?}");
        assert!(!outcome.describe().contains("redirect"), "{}", outcome.describe());
        let _ = handle.await.unwrap();
    }

    #[tokio::test]
    async fn a_target_header_cannot_duplicate_or_override_the_real_content_length() {
        let (url, handle) = one_shot("HTTP/1.1 204 No Content\r\ncontent-length: 0\r\n\r\n").await;
        let mut target = target_wanting(&["cast.started"]);
        target.url = url;
        // Both names collide with a header the builder already sets. The
        // target's own value must win for `user-agent`; the real body length
        // must win for `content-length`, whatever the target claims it to be.
        target.headers.insert("user-agent".into(), "CustomAgent/1".into());
        target.headers.insert("content-length".into(), "999999".into());

        let rendered = render(&target, &ctx()).unwrap();
        let body_len = rendered.body.len();
        let outcome = deliver(&target, &rendered).await;
        assert!(outcome.ok(), "{outcome:?}");

        let request = handle.await.unwrap();
        let lower = request.to_lowercase();
        assert_eq!(
            lower.matches("user-agent:").count(),
            1,
            "the target's header duplicated the builder's own instead of replacing it: {request}"
        );
        assert!(
            lower.contains("user-agent: customagent/1"),
            "the target's user-agent did not win: {request}"
        );
        assert_eq!(
            lower.matches("content-length:").count(),
            1,
            "the target's header duplicated content-length: {request}"
        );
        assert!(
            lower.contains(&format!("content-length: {body_len}")),
            "the real body length did not win over the target's bogus one: {request}"
        );
    }

    #[tokio::test]
    async fn an_ipv6_literal_target_is_delivered_to() {
        // `Url::host_str` hands back the literal with brackets ("[::1]"),
        // which neither `TcpStream::connect` nor `ServerName` accepts bare.
        // Not every sandbox has an IPv6 loopback configured, so a bind
        // failure here is "this machine can't run the test", not "the fix is
        // wrong" -- skip cleanly rather than fail.
        let Some((url, handle)) =
            one_shot_on("[::1]:0", "HTTP/1.1 204 No Content\r\ncontent-length: 0\r\n\r\n").await
        else {
            eprintln!("skipping an_ipv6_literal_target_is_delivered_to: no IPv6 loopback here");
            return;
        };
        let mut target = target_wanting(&["cast.started"]);
        target.url = url;

        let rendered = render(&target, &ctx()).unwrap();
        let outcome = deliver(&target, &rendered).await;
        assert!(matches!(outcome, Outcome::Status(204)), "{outcome:?}");

        let request = handle.await.unwrap();
        assert!(
            request.to_lowercase().contains("host: [::1]:"),
            "the Host header must keep the brackets for an IPv6 literal: {request}"
        );
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

    #[tokio::test]
    async fn fire_returns_immediately_even_when_the_receiver_hangs() {
        // The single property the whole design rests on: `fire` is called from
        // inside the control loop, so it must not wait for anybody's server.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // Accept and then never answer.
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(60)).await;
            drop(socket);
        });

        let pool = memory_pool().await;
        crate::db::run_migrations(&pool).await.unwrap();
        sqlx::query("INSERT INTO webhooks (name, url, events) VALUES (?, ?, ?)")
            .bind("Hangs")
            .bind(format!("http://127.0.0.1:{port}/hook"))
            .bind(r#"["playback.playlist_empty"]"#)
            .execute(&pool)
            .await
            .unwrap();

        let dispatcher = Dispatcher::new(pool);
        let before = std::time::Instant::now();
        dispatcher.fire(Event::PlaylistEmpty);
        assert!(
            before.elapsed() < Duration::from_millis(50),
            "fire blocked for {:?}",
            before.elapsed()
        );
    }

    #[tokio::test]
    async fn a_delivery_records_its_result_for_the_admin_page() {
        let (url, handle) = one_shot("HTTP/1.1 204 No Content\r\ncontent-length: 0\r\n\r\n").await;
        let pool = memory_pool().await;
        crate::db::run_migrations(&pool).await.unwrap();

        let mut target = target_wanting(&["playback.playlist_empty"]);
        target.url = url;

        let dispatcher = Dispatcher::new(pool);
        let outcome = dispatcher
            .deliver_one(&target, &Event::PlaylistEmpty, false)
            .await;
        assert!(outcome.ok());

        let last = dispatcher.last_results().await;
        let entry = last.get(&target.id).expect("no result was recorded");
        assert!(entry.ok);
        assert_eq!(entry.event, "playback.playlist_empty");
        assert!(entry.outcome.contains("204"));
        let _ = handle.await.unwrap();
    }

    #[tokio::test]
    async fn a_render_error_is_recorded_and_never_reaches_the_network() {
        let pool = memory_pool().await;
        crate::db::run_migrations(&pool).await.unwrap();

        let mut target = target_wanting(&["playback.playlist_empty"]);
        // Port 1 refuses instantly, so if this were sent the outcome would be a
        // connection error rather than a template one.
        target.url = "http://127.0.0.1:1/hook".into();
        target.body = Some("{{ unclosed ".into());

        let dispatcher = Dispatcher::new(pool);
        let outcome = dispatcher
            .deliver_one(&target, &Event::PlaylistEmpty, false)
            .await;

        match outcome {
            Outcome::Error(message) => assert!(
                message.contains("body template"),
                "the failure should name the template, got: {message}"
            ),
            other => panic!("expected a render error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_delivery_in_flight_holds_its_semaphore_permit() {
        // `MAX_INFLIGHT` bounds nothing unless a permit outlives the loop
        // iteration that acquired it. Binding it at the *end* of the delivery
        // block does not: a wildcard `let` is not a read, so under 2021
        // disjoint capture the block never captures the permit and it drops one
        // line after `spawn` returns, before the future is first polled. Then
        // `try_acquire_owned` can never fail, the drop path is unreachable, and
        // the source reads as though a ceiling exists while `fire` spawns
        // without one. So this asserts the permits are actually checked out.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // Accept every connection and answer none, so a delivery that has
        // started is still running when the assertion looks at the semaphore.
        tokio::spawn(async move {
            let mut held = Vec::new();
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                held.push(socket);
            }
        });

        let pool = memory_pool().await;
        crate::db::run_migrations(&pool).await.unwrap();
        for name in ["One", "Two", "Three"] {
            sqlx::query("INSERT INTO webhooks (name, url, events) VALUES (?, ?, ?)")
                .bind(name)
                .bind(format!("http://127.0.0.1:{port}/hook"))
                .bind(r#"["playback.playlist_empty"]"#)
                .execute(&pool)
                .await
                .unwrap();
        }

        let dispatcher = Dispatcher::new(pool);
        dispatcher.fire(Event::PlaylistEmpty);

        // The deliveries start on the runtime's own schedule -- a DB read stands
        // between `fire` and the first `try_acquire_owned` -- so wait for them
        // rather than guess a number of yields.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while dispatcher.inflight.available_permits() > MAX_INFLIGHT - 3
            && std::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            dispatcher.inflight.available_permits(),
            MAX_INFLIGHT - 3,
            "three deliveries are hanging, so three permits must still be checked out"
        );
    }

    #[tokio::test]
    async fn fire_reaches_every_subscribed_target_and_nobody_else() {
        // The timing test above says only that `fire` returns; a body that did
        // nothing at all would pass it. This one says it fans out, and that
        // `wants` filters inside `fire` and not only in isolation.
        let (url_a, handle_a) =
            one_shot("HTTP/1.1 204 No Content\r\ncontent-length: 0\r\n\r\n").await;
        let (url_b, handle_b) =
            one_shot("HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n").await;
        // Bound and listening like the others, so its absence from the results
        // is the subscription filter and not a URL nobody could have reached.
        let (url_c, handle_c) =
            one_shot("HTTP/1.1 204 No Content\r\ncontent-length: 0\r\n\r\n").await;

        let pool = memory_pool().await;
        crate::db::run_migrations(&pool).await.unwrap();
        for (name, url, events) in [
            ("Subscribed A", url_a, r#"["playback.playlist_empty"]"#),
            ("Subscribed B", url_b, r#"["cast.started","playback.playlist_empty"]"#),
            ("Elsewhere", url_c, r#"["cast.started"]"#),
        ] {
            sqlx::query("INSERT INTO webhooks (name, url, events) VALUES (?, ?, ?)")
                .bind(name)
                .bind(url)
                .bind(events)
                .execute(&pool)
                .await
                .unwrap();
        }

        let dispatcher = Dispatcher::new(pool);
        dispatcher.fire(Event::PlaylistEmpty);

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let last = loop {
            let last = dispatcher.last_results().await;
            if last.len() >= 2 || std::time::Instant::now() >= deadline {
                break last;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };

        let a = last.get(&1).expect("the first subscribed target recorded nothing");
        assert!(a.ok, "{a:?}");
        assert_eq!(a.event, "playback.playlist_empty");
        let b = last.get(&2).expect("the second subscribed target recorded nothing");
        assert!(b.ok, "{b:?}");
        assert!(
            !last.contains_key(&3),
            "a target subscribed only to cast.started was delivered to"
        );
        assert!(
            !handle_c.is_finished(),
            "the unsubscribed receiver was contacted"
        );

        assert!(handle_a.await.unwrap().contains("playback.playlist_empty"));
        assert!(handle_b.await.unwrap().contains("playback.playlist_empty"));
        handle_c.abort();
    }
}
