//! TLS for the cast sender page.
//!
//! `getDisplayMedia` (and `RTCPeerConnection`) only exist in a secure context.
//! The display browser is fine — it reaches the controller over loopback, which
//! counts as secure — but the *sender* is a laptop somewhere on the LAN opening
//! `http://10.x.x.x:3000`, which does not. So the sender page needs its own
//! HTTPS listener.
//!
//! There is no CA to get a real certificate from on an offline appliance, so we
//! generate a self-signed one. The browser warns once; clicking through grants
//! the origin secure-context status, which is all WebRTC needs.

use anyhow::{Context, Result};
use axum_server::tls_rustls::RustlsConfig;
use std::net::{IpAddr, Ipv4Addr, UdpSocket};
use std::path::{Path, PathBuf};
use tracing::{info, warn};

const CERT_MARKER: &str = "-----BEGIN CERTIFICATE-----";

pub const DEFAULT_CAST_TLS_PORT: u16 = 3443;
/// How far past the default to look when no port was asked for.
const AUTO_PORT_ATTEMPTS: u16 = 20;

/// Bind the cast HTTPS socket up front, so a port clash is a startup error
/// rather than a background task that quietly logs and leaves casting dead.
///
/// An explicitly configured port is a promise to whoever was handed the URL, so
/// a clash there is fatal — moving silently would point them at a dead address,
/// and the usual cause is an older instance of this very binary still holding
/// the port. Without one, the next free port is fine and merely gets logged.
pub fn bind_cast_listener(preferred: Option<u16>) -> Result<std::net::TcpListener> {
    let bind = |port: u16| std::net::TcpListener::bind((Ipv4Addr::UNSPECIFIED, port));

    let Some(port) = preferred else {
        let last = DEFAULT_CAST_TLS_PORT + AUTO_PORT_ATTEMPTS;
        for port in DEFAULT_CAST_TLS_PORT..last {
            match bind(port) {
                Ok(listener) => {
                    if port != DEFAULT_CAST_TLS_PORT {
                        warn!(
                            "Cast HTTPS port {} is taken, listening on {} instead",
                            DEFAULT_CAST_TLS_PORT, port
                        );
                    }
                    return Ok(listener);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => continue,
                Err(e) => {
                    return Err(e).with_context(|| format!("binding the cast HTTPS port {}", port))
                }
            }
        }
        anyhow::bail!(
            "no free port for the cast HTTPS listener between {} and {}",
            DEFAULT_CAST_TLS_PORT,
            last - 1
        );
    };

    bind(port).with_context(|| {
        format!(
            "binding the cast HTTPS port {}. Another process already has it -- \
             often an older miniclientcontrol that outlived its session. Pass a \
             different --cast-tls-port, or omit the flag to take the next free one",
            port
        )
    })
}

/// Best-effort primary LAN address of this machine.
///
/// Connecting a UDP socket performs no traffic but makes the kernel pick the
/// source address it would route from, which is the address a client on the LAN
/// would reach us on. Falls back to loopback when there is no route at all.
pub fn primary_local_ipv4() -> Ipv4Addr {
    let probe = || -> Option<Ipv4Addr> {
        let socket = UdpSocket::bind(("0.0.0.0", 0)).ok()?;
        socket.connect(("10.254.254.254", 1)).ok()?;
        match socket.local_addr().ok()?.ip() {
            IpAddr::V4(addr) => Some(addr),
            IpAddr::V6(_) => None,
        }
    };
    probe().unwrap_or(Ipv4Addr::LOCALHOST)
}

pub fn system_hostname() -> Option<String> {
    let raw = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| std::fs::read_to_string("/etc/hostname"))
        .ok()?;
    let name = raw.trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// Names the certificate must cover, sorted and deduplicated so the list can be
/// compared byte-for-byte against the one a previous run recorded.
fn subject_alt_names(extra: &[String]) -> Vec<String> {
    let mut names = vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        primary_local_ipv4().to_string(),
    ];
    if let Some(host) = system_hostname() {
        names.push(host.clone());
        // avahi/nss-mdns resolves this on the LAN, and it survives a DHCP change
        // that would invalidate the IP entry
        names.push(format!("{}.local", host));
    }
    names.extend(extra.iter().cloned());
    names.sort();
    names.dedup();
    names
}

fn sans_sidecar_path(cert_path: &Path) -> PathBuf {
    let mut name = cert_path.as_os_str().to_os_string();
    name.push(".sans");
    PathBuf::from(name)
}

fn generate(cert_path: &Path, sans: &[String]) -> Result<(String, String)> {
    let params = rcgen::CertificateParams::new(sans.to_vec())
        .context("building certificate parameters")?;
    let mut params = params;
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "miniclientcontrol cast");
    // A self-signed cert is trusted via a manual browser exception, so the 398-day
    // limit browsers enforce on publicly-chained certs does not apply here.
    params.not_before = rcgen::date_time_ymd(2020, 1, 1);
    params.not_after = rcgen::date_time_ymd(2040, 1, 1);

    let key_pair = rcgen::KeyPair::generate().context("generating key pair")?;
    let cert = params
        .self_signed(&key_pair)
        .context("self-signing certificate")?;

    let cert_pem = cert.pem();
    let key_pem = key_pair.serialize_pem();

    if let Some(parent) = cert_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
    }
    // key first, certificate second -- `split_pem` relies on that order
    std::fs::write(cert_path, format!("{}{}", key_pem, cert_pem))
        .with_context(|| format!("writing {}", cert_path.display()))?;
    // the file carries the private key, so it must not be world-readable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(cert_path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("locking down {}", cert_path.display()))?;
    }
    std::fs::write(sans_sidecar_path(cert_path), sans.join("\n"))
        .with_context(|| format!("writing {}", sans_sidecar_path(cert_path).display()))?;

    info!(
        "Generated self-signed cast certificate at {} for {}",
        cert_path.display(),
        sans.join(", ")
    );

    Ok((cert_pem, key_pem))
}

/// Split the combined PEM file back into (certificate, key).
fn split_pem(contents: &str) -> Option<(String, String)> {
    let index = contents.find(CERT_MARKER)?;
    let key = contents[..index].trim().to_string();
    let cert = contents[index..].trim().to_string();
    if key.is_empty() || cert.is_empty() {
        return None;
    }
    Some((cert, key))
}

/// Load the cast certificate, generating a fresh one when it is missing,
/// unreadable, or no longer covers the addresses this machine answers on.
///
/// Regenerating on a name change is deliberate: after a DHCP lease moves the
/// device, a stale certificate would fail name validation in the browser, and a
/// name mismatch is a scarier warning than an unknown issuer.
pub async fn load_cast_tls(cert_path: &Path, extra_sans: &[String]) -> Result<RustlsConfig> {
    // rustls 0.23 requires a process-wide crypto provider. `ring` is chosen in
    // Cargo.toml because aws-lc-rs needs a C toolchain that the armv7 cross image
    // does not have.
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        rustls::crypto::ring::default_provider()
            .install_default()
            .map_err(|_| anyhow::anyhow!("failed to install the ring crypto provider"))?;
    }

    let sans = subject_alt_names(extra_sans);

    let existing = std::fs::read_to_string(cert_path).ok().and_then(|contents| {
        let recorded = std::fs::read_to_string(sans_sidecar_path(cert_path)).unwrap_or_default();
        if recorded.trim() != sans.join("\n") {
            warn!(
                "Cast certificate no longer covers {} -- regenerating",
                sans.join(", ")
            );
            return None;
        }
        split_pem(&contents)
    });

    let (cert_pem, key_pem) = match existing {
        Some(pair) => pair,
        None => generate(cert_path, &sans)?,
    };

    RustlsConfig::from_pem(cert_pem.into_bytes(), key_pem.into_bytes())
        .await
        .context("loading cast certificate into rustls")
}

// ------------------------------------------------------------- public name

/// How this device is advertised to guests.
///
/// Kept next to the certificate helpers on purpose: whatever name guests type has
/// to be in the certificate, or they get a name-mismatch warning on top of the
/// unknown-issuer one.
pub enum PublicUrl {
    /// The primary LAN address.
    LanAddress,
    /// `<hostname>.local`, resolved by Avahi on the guest's machine.
    Mdns(String),
    /// A host name supplied by the operator.
    Host(String),
    /// A full base URL, for a device behind a proxy that owns the port.
    Base(String),
}

pub fn public_url(setting: &str) -> PublicUrl {
    let setting = setting.trim();
    match setting {
        "" | "none" => PublicUrl::LanAddress,
        "mdns" => match system_hostname() {
            Some(host) => PublicUrl::Mdns(format!("{}.local", host)),
            None => {
                warn!("--public-url=mdns but the hostname is unreadable; using the LAN address");
                PublicUrl::LanAddress
            }
        },
        other if other.starts_with("http://") || other.starts_with("https://") => {
            PublicUrl::Base(other.trim_end_matches('/').to_string())
        }
        other => PublicUrl::Host(other.to_string()),
    }
}

/// The name a guest will type, when it is not an address we already cover.
pub fn public_host(setting: &str) -> Option<String> {
    match public_url(setting) {
        PublicUrl::LanAddress => None,
        // already covered by the default SAN list
        PublicUrl::Mdns(_) => None,
        PublicUrl::Host(host) => Some(host),
        PublicUrl::Base(base) => base
            .split("://")
            .nth(1)
            .map(|rest| rest.split('/').next().unwrap_or(rest))
            .map(|host_port| host_port.rsplit_once(':').map_or(host_port, |(h, _)| h).to_string()),
    }
}

/// The base URL handed to guests, always ending in a slash.
pub fn public_base_url(setting: &str, tls_port: u16) -> String {
    match public_url(setting) {
        PublicUrl::LanAddress => format!("https://{}:{}/", primary_local_ipv4(), tls_port),
        PublicUrl::Mdns(host) | PublicUrl::Host(host) => {
            format!("https://{}:{}/", host, tls_port)
        }
        // A full URL is taken at its word: the port belongs to whatever is
        // proxying, not to our listener.
        PublicUrl::Base(base) => format!("{}/", base),
    }
}

/// Warn early if `--public-url=mdns` was asked for but nothing can resolve it.
///
/// Silent failure here is nasty: the display shows an address that simply does
/// not work for anyone, and nothing in the logs says why.
pub async fn check_mdns(setting: &str) {
    let PublicUrl::Mdns(host) = public_url(setting) else {
        return;
    };
    let resolved = tokio::net::lookup_host(format!("{}:443", host))
        .await
        .map(|mut addrs| addrs.next().is_some())
        .unwrap_or(false);

    match resolved {
        true => info!("Advertising this device as {}", host),
        false => warn!(
            "--public-url=mdns is set but '{}' does not resolve here. Guests will \
             only reach it if Avahi (avahi-daemon plus nss-mdns in /etc/nsswitch.conf) \
             is running on this device and on their machines.",
            host
        ),
    }
}
