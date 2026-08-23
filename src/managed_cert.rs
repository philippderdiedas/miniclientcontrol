//! The managed wildcard certificate.
//!
//! A device on a private address cannot be given a certificate a browser
//! trusts: no CA will sign a name for `192.168.178.15`, and a self-signed one
//! costs every guest a warning page. The way around it is a public zone whose
//! DNS spells the address out in the name — `192-168-178-15.clientctrl.cc`
//! resolves to `192.168.178.15` — so a public wildcard certificate for
//! `*.clientctrl.cc` covers a name that points at the LAN.
//!
//! This module fetches that certificate, caches it, and renews it. It never
//! fails hard: a device that cannot reach the API falls back to the self-signed
//! path, because casting must not keep the signage from booting.
//!
//! **The private key is served without authentication**, which is inherent to
//! the arrangement — every device needs it, so it is not a secret. Anybody can
//! therefore impersonate a `*.clientctrl.cc` name. The trade is deliberate: the
//! alternative is a warning page in front of every guest, and the names only
//! ever point into somebody's LAN.

use anyhow::{anyhow, Context, Result};
use http_body_util::BodyExt;
use hyper_util::rt::TokioIo;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tracing::{info, warn};

/// Where the certificate comes from. Note this is `clientcontrol.cc` while the
/// zone it certifies is `clientctrl.cc` — two different names, not a typo.
const API_HOST: &str = "api.clientcontrol.cc";

/// Renew this far before expiry. Let's Encrypt issues for 90 days, so a device
/// that is switched on at least monthly never serves an expired certificate.
const RENEW_WITHIN: Duration = Duration::from_secs(30 * 24 * 3600);

/// How often the renewal task wakes up. The window above is measured in weeks,
/// so there is nothing to gain from looking more often.
const CHECK_EVERY: Duration = Duration::from_secs(12 * 3600);

/// Retry delay after a failed fetch. Short enough that a device which booted
/// before the network was up recovers on its own.
const RETRY_AFTER: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Debug)]
pub struct Bundle {
    /// Leaf plus intermediates, in that order.
    pub fullchain: String,
    pub key: String,
    /// Expiry as a unix timestamp.
    pub not_after: i64,
    /// The names the certificate actually carries, as the API reports them.
    pub domains: Vec<String>,
}

impl Bundle {
    fn expires_in(&self) -> Duration {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        Duration::from_secs((self.not_after - now).max(0) as u64)
    }

    fn needs_renewal(&self) -> bool {
        self.expires_in() < RENEW_WITHIN
    }

    /// Whether this certificate actually covers the name we intend to serve.
    ///
    /// Checked rather than assumed: the API is a moving part, and serving a
    /// certificate for the wrong name is a worse failure than falling back to a
    /// self-signed one — the browser warning is scarier and the cause is not
    /// visible anywhere.
    fn covers(&self, host: &str) -> bool {
        self.domains.iter().any(|pattern| match pattern.strip_prefix("*.") {
            // A wildcard covers exactly one label.
            Some(suffix) => host
                .strip_suffix(suffix)
                .and_then(|head| head.strip_suffix('.'))
                .is_some_and(|label| !label.is_empty() && !label.contains('.')),
            None => pattern == host,
        })
    }
}

pub fn cache_path(cert_path: &Path) -> PathBuf {
    let mut name = cert_path.as_os_str().to_os_string();
    name.push(".managed.json");
    PathBuf::from(name)
}

fn parse_bundle(raw: &str) -> Result<Bundle> {
    let value: serde_json::Value = serde_json::from_str(raw).context("parsing the response")?;
    let text = |key: &str| -> Result<String> {
        value
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| anyhow!("the response has no '{}'", key))
    };

    let not_after_raw = text("not_after")?;
    let not_after = chrono::DateTime::parse_from_rfc3339(&not_after_raw)
        .with_context(|| format!("parsing not_after '{}'", not_after_raw))?
        .timestamp();

    let domains = value
        .get("domains")
        .and_then(|v| v.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    Ok(Bundle {
        fullchain: text("fullchain")?,
        key: text("key")?,
        not_after,
        domains,
    })
}

/// One HTTPS GET, on hyper over rustls.
///
/// Deliberately not `reqwest`: its `rustls` feature hard-wires `aws-lc-rs`,
/// which needs a C toolchain the armv7 cross image does not have. hyper and
/// tokio-rustls are already in the tree via axum, so this costs one crate of
/// root certificates and no native code.
async fn fetch() -> Result<Bundle> {
    let roots = RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));

    let tcp = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::net::TcpStream::connect((API_HOST, 443)),
    )
    .await
    .context("connecting timed out")?
    .with_context(|| format!("connecting to {}", API_HOST))?;

    let server_name = API_HOST
        .try_into()
        .map_err(|_| anyhow!("'{}' is not a valid server name", API_HOST))?;
    let tls = connector
        .connect(server_name, tcp)
        .await
        .context("the TLS handshake failed")?;

    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
        .await
        .context("the HTTP handshake failed")?;
    tokio::spawn(async move {
        let _ = connection.await;
    });

    let request = hyper::Request::builder()
        .uri("/")
        .header("host", API_HOST)
        .header("user-agent", concat!("miniclientcontrol/", env!("CARGO_PKG_VERSION")))
        .body(String::new())
        .context("building the request")?;

    let response = tokio::time::timeout(Duration::from_secs(20), sender.send_request(request))
        .await
        .context("the request timed out")?
        .context("sending the request")?;

    let status = response.status();
    if !status.is_success() {
        return Err(anyhow!("{} answered {}", API_HOST, status));
    }

    let body = response
        .into_body()
        .collect()
        .await
        .context("reading the response")?
        .to_bytes();
    parse_bundle(&String::from_utf8_lossy(&body))
}

fn read_cache(path: &Path) -> Option<Bundle> {
    let raw = std::fs::read_to_string(path).ok()?;
    match parse_bundle(&raw) {
        Ok(bundle) => Some(bundle),
        Err(e) => {
            warn!("Ignoring the cached managed certificate at {}: {:#}", path.display(), e);
            None
        }
    }
}

fn write_cache(path: &Path, raw: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
    }
    std::fs::write(path, raw).with_context(|| format!("writing {}", path.display()))?;
    // The file carries the private key.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("locking down {}", path.display()))?;
    }
    Ok(())
}

async fn fetch_and_cache(path: &Path, host: &str) -> Result<Bundle> {
    let bundle = fetch().await?;
    if !bundle.covers(host) {
        return Err(anyhow!(
            "the certificate covers {:?}, which does not include {}",
            bundle.domains,
            host
        ));
    }
    // Re-serialise rather than storing the raw body: it keeps the cache in the
    // one shape `parse_bundle` accepts, whatever else the API grows.
    let raw = serde_json::json!({
        "fullchain": bundle.fullchain,
        "key": bundle.key,
        "not_after": chrono::DateTime::from_timestamp(bundle.not_after, 0)
            .map(|t| t.to_rfc3339())
            .unwrap_or_default(),
        "domains": bundle.domains,
    });
    write_cache(path, &raw.to_string())?;
    Ok(bundle)
}

/// The certificate to start with: the cache when it is still good, otherwise a
/// fresh one, otherwise whatever the cache holds even if it has expired.
///
/// That last step is the interesting one. Falling back to the self-signed
/// certificate would also change the *name* the device advertises, so every QR
/// code already printed or scanned would stop working. An expired certificate
/// keeps the name and costs a warning page — the same warning the self-signed
/// path would have shown anyway.
pub async fn obtain(cert_path: &Path, host: &str) -> Option<Bundle> {
    let path = cache_path(cert_path);
    let cached = read_cache(&path).filter(|bundle| bundle.covers(host));

    if let Some(bundle) = &cached {
        if !bundle.needs_renewal() {
            info!(
                "Managed certificate for {} is good for another {} days",
                host,
                bundle.expires_in().as_secs() / 86_400
            );
            return cached;
        }
    }

    match fetch_and_cache(&path, host).await {
        Ok(bundle) => {
            info!(
                "Fetched the managed certificate for {}, valid for {} days",
                host,
                bundle.expires_in().as_secs() / 86_400
            );
            Some(bundle)
        }
        Err(e) => match cached {
            Some(bundle) => {
                warn!(
                    "Could not refresh the managed certificate ({:#}); keeping the cached one, \
                     which expires in {} days",
                    e,
                    bundle.expires_in().as_secs() / 86_400
                );
                Some(bundle)
            }
            None => {
                warn!("No managed certificate for {} ({:#})", host, e);
                None
            }
        },
    }
}

/// Keep the certificate fresh without restarting.
///
/// A restart here would navigate the display browser, and an appliance that
/// blinks every ninety days for no visible reason is worse than the problem.
/// `RustlsConfig::reload_from_pem` swaps the certificate under the running
/// listener instead, so an established cast is not touched either.
pub fn spawn_renewal(
    config: axum_server::tls_rustls::RustlsConfig,
    cert_path: PathBuf,
    host: String,
    mut current: Bundle,
) {
    tokio::spawn(async move {
        let path = cache_path(&cert_path);
        loop {
            let wait = if current.needs_renewal() { RETRY_AFTER } else { CHECK_EVERY };
            tokio::time::sleep(wait).await;

            if !current.needs_renewal() {
                continue;
            }
            match fetch_and_cache(&path, &host).await {
                Ok(bundle) => {
                    match config
                        .reload_from_pem(bundle.fullchain.clone().into_bytes(), bundle.key.clone().into_bytes())
                        .await
                    {
                        Ok(()) => {
                            info!(
                                "Renewed the managed certificate for {}, now valid for {} days",
                                host,
                                bundle.expires_in().as_secs() / 86_400
                            );
                            current = bundle;
                        }
                        // Keep serving the old one: it is still valid for a
                        // while, and a failed swap must not take TLS down.
                        Err(e) => warn!("Renewed certificate could not be loaded: {:#}", e),
                    }
                }
                Err(e) => warn!("Renewing the managed certificate failed: {:#}", e),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle(domains: &[&str]) -> Bundle {
        Bundle {
            fullchain: String::new(),
            key: String::new(),
            not_after: 0,
            domains: domains.iter().map(|d| d.to_string()).collect(),
        }
    }

    #[test]
    fn a_wildcard_covers_exactly_one_label() {
        let b = bundle(&["*.clientctrl.cc", "clientctrl.cc"]);
        assert!(b.covers("192-168-178-15.clientctrl.cc"));
        assert!(b.covers("fd00--1.clientctrl.cc"));
        assert!(b.covers("clientctrl.cc"));
        // Two labels are outside a single wildcard.
        assert!(!b.covers("a.b.clientctrl.cc"));
        assert!(!b.covers("clientctrl.cc.evil.test"));
        assert!(!b.covers(".clientctrl.cc"));
        assert!(!b.covers("192-168-178-15.clientcontrol.cc"));
    }

    #[test]
    fn expiry_is_read_from_rfc3339() {
        let raw = r#"{"fullchain":"c","key":"k","not_after":"2026-11-21T00:07:09Z",
                      "domains":["*.clientctrl.cc"]}"#;
        let parsed = parse_bundle(raw).unwrap();
        assert_eq!(parsed.not_after, 1_795_219_629);
        assert_eq!(parsed.fullchain, "c");
    }

    #[test]
    fn a_response_missing_the_key_is_an_error() {
        let raw = r#"{"fullchain":"c","not_after":"2026-11-21T00:07:09Z"}"#;
        assert!(parse_bundle(raw).is_err());
    }
}
