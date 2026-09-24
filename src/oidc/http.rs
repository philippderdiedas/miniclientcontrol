//! The two requests OpenID Connect needs -- a GET for discovery and keys, a form
//! POST to the token endpoint -- on hyper over rustls, like `managed_cert.rs`
//! and the webhooks. Not `reqwest`: its `rustls` feature hard-wires `aws-lc-rs`,
//! which needs a C toolchain the armv7 cross image does not have.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use http_body_util::{BodyExt, Limited};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};

/// Discovery documents and key sets are small; anything larger is not one.
const MAX_BODY: usize = 256 * 1024;

pub fn allowed_url(url: &str) -> bool {
    let Ok(url) = url::Url::parse(url) else { return false };
    match url.scheme() {
        "https" => true,
        // The test provider and local development; never across a network.
        "http" => matches!(url.host_str(), Some("127.0.0.1") | Some("::1") | Some("[::1]") | Some("localhost")),
        _ => false,
    }
}

pub async fn get_json(url: &str) -> Result<Value> {
    request(url, "GET", None, &[]).await
}

/// A form POST, with the client's credentials as HTTP Basic when `basic` is
/// given (RFC 6749 section 2.3.1: each part form-encoded before joining).
pub async fn post_form(url: &str, form: &[(&str, &str)], basic: Option<(&str, &str)>) -> Result<Value> {
    let body = form
        .iter()
        .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let mut headers = vec![("content-type".to_string(), "application/x-www-form-urlencoded".to_string())];
    if let Some((id, secret)) = basic {
        let raw = format!("{}:{}", urlencoding::encode(id), urlencoding::encode(secret));
        let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
        headers.push(("authorization".to_string(), format!("Basic {encoded}")));
    }
    request(url, "POST", Some(body), &headers).await
}

async fn request(url: &str, method: &str, body: Option<String>, extra: &[(String, String)]) -> Result<Value> {
    if !allowed_url(url) {
        return Err(anyhow!("refusing {url}: only https, or http on this machine"));
    }
    let parsed = url::Url::parse(url).context("parsing the URL")?;
    let https = parsed.scheme() == "https";
    let host = parsed.host_str().ok_or_else(|| anyhow!("{url} has no host"))?.to_string();
    let port = parsed.port_or_known_default().unwrap_or(if https { 443 } else { 80 });
    let path = match parsed.query() {
        Some(query) => format!("{}?{}", parsed.path(), query),
        None => parsed.path().to_string(),
    };
    let connect_host = host.trim_start_matches('[').trim_end_matches(']').to_string();
    let authority = match parsed.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.clone(),
    };

    let body = body.unwrap_or_default();
    let mut builder = hyper::Request::builder()
        .method(method)
        .uri(&path)
        .header("host", &authority)
        .header("accept", "application/json")
        .header("user-agent", concat!("miniclientcontrol/", env!("CARGO_PKG_VERSION")))
        .header("content-length", body.len().to_string());
    for (name, value) in extra {
        builder = builder.header(name.as_str(), value.as_str());
    }
    let request = builder.body(body).context("building the request")?;

    let tcp = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::net::TcpStream::connect((connect_host.as_str(), port)),
    )
    .await
    .context("connecting timed out")?
    .with_context(|| format!("connecting to {authority}"))?;

    let exchange = async {
        if https {
            let roots = RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
            let config = ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
            let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
            let server_name = connect_host
                .clone()
                .try_into()
                .map_err(|_| anyhow!("'{connect_host}' is not a valid server name"))?;
            let tls = connector.connect(server_name, tcp).await.context("the TLS handshake failed")?;
            send(TokioIo::new(tls), request).await
        } else {
            send(TokioIo::new(tcp), request).await
        }
    };
    let response = tokio::time::timeout(Duration::from_secs(15), exchange)
        .await
        .context("the request timed out")??;

    let status = response.status();
    let bytes = Limited::new(response.into_body(), MAX_BODY)
        .collect()
        .await
        .map_err(|e| anyhow!("reading the response: {e}"))?
        .to_bytes();
    if !status.is_success() {
        return Err(anyhow!("{authority} answered {status}"));
    }
    serde_json::from_slice(&bytes).context("the response is not JSON")
}

async fn send<S>(io: TokioIo<S>, request: hyper::Request<String>) -> Result<hyper::Response<hyper::body::Incoming>>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, connection) = hyper::client::conn::http1::handshake(io)
        .await
        .context("the HTTP handshake failed")?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    sender.send_request(request).await.context("sending the request")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_https_or_http_on_this_machine() {
        assert!(allowed_url("https://idp.example.org/realms/x"));
        assert!(allowed_url("http://127.0.0.1:3071"));
        assert!(allowed_url("http://localhost:8080/"));
        assert!(!allowed_url("http://192.168.1.9/"));
        assert!(!allowed_url("ftp://idp.example.org/"));
        assert!(!allowed_url("not a url"));
    }
}
