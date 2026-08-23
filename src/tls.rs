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
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, UdpSocket};
use std::path::{Path, PathBuf};
use tracing::{info, warn};

const CERT_MARKER: &str = "-----BEGIN CERTIFICATE-----";

pub const DEFAULT_CAST_TLS_PORT: u16 = 3443;
/// Tried before anything else when no port was asked for.
///
/// A guest reads the address off a QR code or types it, and `https://host/` is
/// meaningfully shorter than `https://host:3443/`. Binding it needs a privilege
/// this process usually does not have, so it is an attempt and not a
/// requirement -- see the capability note in docs/deployment.md.
pub const PREFERRED_CAST_TLS_PORT: u16 = 443;
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
        // 443 first, because it drops out of the URL entirely. A device without
        // CAP_NET_BIND_SERVICE gets PermissionDenied here, which is ordinary and
        // not worth a warning -- only an unexpected error is.
        match bind(PREFERRED_CAST_TLS_PORT) {
            Ok(listener) => {
                info!("Cast HTTPS listening on {}", PREFERRED_CAST_TLS_PORT);
                return Ok(listener);
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::AddrInUse
                ) => {}
            Err(e) => warn!(
                "Could not use port {} for the cast HTTPS listener: {}",
                PREFERRED_CAST_TLS_PORT, e
            ),
        }

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

// ------------------------------------------------- the managed public name

/// The zone whose wildcard certificate the managed name borrows.
///
/// Its DNS answers `<address-with-dashes>.clientctrl.cc` with the address spelled
/// out in the label, so a private address gets a public name that a public
/// certificate can cover. Note the zone is `clientctrl.cc` while the API serving
/// the certificate is on `clientcontrol.cc` -- two different names, not a typo.
pub const MANAGED_DOMAIN: &str = "clientctrl.cc";

/// A name of the form `192-168-178-15.clientctrl.cc`.
#[derive(Clone, Debug)]
pub struct ManagedName {
    pub host: String,
    pub addr: IpAddr,
}

/// Spell an address as a single DNS label.
///
/// One label and not several: the wildcard is `*.clientctrl.cc`, which covers
/// exactly one level, so an encoding that produced a dot would fall outside the
/// certificate.
fn dashed_label(addr: IpAddr) -> Option<String> {
    match addr {
        IpAddr::V4(v4) => Some(v4.to_string().replace('.', "-")),
        IpAddr::V6(v6) => {
            // `to_string` gives the RFC 5952 form, so `::` becomes `--` and the
            // label stays as short as it can be -- this ends up in a QR code.
            let text = v6.to_string();
            // An IPv4-mapped or -compatible address renders with dots.
            if text.contains('.') {
                return None;
            }
            Some(text.replace(':', "-"))
        }
    }
}

fn is_global_v6(addr: &Ipv6Addr) -> bool {
    // 2000::/3. `Ipv6Addr::is_global` is still unstable, hence the bit test.
    (addr.segments()[0] & 0xe000) == 0x2000
}

fn is_ula_v6(addr: &Ipv6Addr) -> bool {
    // fc00::/7, likewise unstable in std.
    (addr.segments()[0] & 0xfe00) == 0xfc00
}

/// IPv6 addresses this machine answers on, read from `/proc/net/if_inet6`.
///
/// The UDP-connect trick used for IPv4 picks the source address for one
/// destination, which cannot tell a ULA apart from a global address without a
/// route to probe for. Reading the table is both simpler and complete.
///
/// Columns are: address as 32 hex digits, interface index, prefix length, scope,
/// flags, name. Only globally scoped addresses are of interest -- link-local is
/// useless without a zone index, and the zone's DNS does not answer for it.
fn local_ipv6_addresses() -> Vec<Ipv6Addr> {
    const TENTATIVE: u32 = 0x40;
    const DEPRECATED: u32 = 0x20;
    const DADFAILED: u32 = 0x08;
    const TEMPORARY: u32 = 0x01;

    let Ok(table) = std::fs::read_to_string("/proc/net/if_inet6") else {
        return Vec::new();
    };

    let mut found: Vec<(bool, Ipv6Addr)> = Vec::new();
    for line in table.lines() {
        let mut columns = line.split_whitespace();
        let (Some(raw), Some(_index), Some(_prefix), Some(scope), Some(flags)) = (
            columns.next(),
            columns.next(),
            columns.next(),
            columns.next(),
            columns.next(),
        ) else {
            continue;
        };
        if raw.len() != 32 {
            continue;
        }
        // Scope 0 is global; 0x20 link, 0x10 host, 0x40 site.
        if u32::from_str_radix(scope, 16).unwrap_or(u32::MAX) != 0 {
            continue;
        }
        let flags = u32::from_str_radix(flags, 16).unwrap_or(0);
        if flags & (TENTATIVE | DEPRECATED | DADFAILED) != 0 {
            continue;
        }
        let mut segments = [0u16; 8];
        let mut ok = true;
        for (i, segment) in segments.iter_mut().enumerate() {
            match u16::from_str_radix(&raw[i * 4..i * 4 + 4], 16) {
                Ok(value) => *segment = value,
                Err(_) => {
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            // A privacy address rotates, and a name built on one would stop
            // resolving to this device -- so it sorts last rather than out.
            found.push((flags & TEMPORARY != 0, Ipv6Addr::from(segments)));
        }
    }
    found.sort_by_key(|(temporary, _)| *temporary);
    found.into_iter().map(|(_, addr)| addr).collect()
}

/// Pick the address this device should be named after, if any qualifies.
///
/// **IPv4 first, on purpose.** A dashed IPv4 label is a fraction of the length of
/// an IPv6 one, and the name's whole reason for existing is a QR code that a
/// guest scans from across a room.
///
/// Only a private IPv4 counts: a device with a public address is not the case
/// this feature is for, and pointing a public name at it would publish it.
/// Link-local (169.254/16) and CGNAT (100.64/10) are both excluded --
/// `Ipv4Addr::is_private` is exactly RFC 1918 and neither of those is in it.
pub fn managed_name() -> Option<ManagedName> {
    let v4 = primary_local_ipv4();
    if v4.is_private() {
        let host = dashed_label(IpAddr::V4(v4))?;
        return Some(ManagedName {
            host: format!("{}.{}", host, MANAGED_DOMAIN),
            addr: IpAddr::V4(v4),
        });
    }

    let v6 = local_ipv6_addresses();
    // A routable address before a ULA: both work inside the LAN, and the routable
    // one also works from outside it.
    let pick = v6
        .iter()
        .find(|addr| is_global_v6(addr))
        .or_else(|| v6.iter().find(|addr| is_ula_v6(addr)))?;
    let host = dashed_label(IpAddr::V6(*pick))?;
    Some(ManagedName {
        host: format!("{}.{}", host, MANAGED_DOMAIN),
        addr: IpAddr::V6(*pick),
    })
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
/// `:443` is left off: it is the default for the scheme, and the whole point of
/// preferring that port is a shorter address on the screen.
fn authority(host: &str, tls_port: u16) -> String {
    match tls_port {
        PREFERRED_CAST_TLS_PORT => host.to_string(),
        other => format!("{}:{}", host, other),
    }
}

/// The managed base URL for whatever address this machine answers on *now*.
///
/// Recomputed rather than stored, exactly like `public_base_url`: a DHCP lease
/// changes the address and therefore the name, and the wildcard covers the new
/// one without any new certificate. Returns `None` once no address qualifies —
/// the caller then falls back to the plain address.
pub fn managed_base_url(tls_port: u16) -> Option<String> {
    let name = managed_name()?;
    Some(format!("https://{}/", authority(&name.host, tls_port)))
}

pub fn public_base_url(setting: &str, tls_port: u16) -> String {
    match public_url(setting) {
        PublicUrl::LanAddress => {
            format!("https://{}/", authority(&primary_local_ipv4().to_string(), tls_port))
        }
        PublicUrl::Mdns(host) | PublicUrl::Host(host) => {
            format!("https://{}/", authority(&host, tls_port))
        }
        // A full URL is taken at its word: the port belongs to whatever is
        // proxying, not to our listener.
        PublicUrl::Base(base) => format!("{}/", base),
    }
}

/// Warn when this machine's own resolver will not answer for the managed name.
///
/// A public name that answers with a private address is the exact pattern
/// DNS-rebinding protection blocks, and a guest whose resolver does that gets
/// "server not found" rather than anything we could explain.
///
/// **This measures our resolver, not theirs**, so it is a hint and never a
/// verdict. It is right in the common case -- device and guest on the same
/// router, handed the same DNS by DHCP -- and wrong in both directions
/// otherwise: a device with its own upstream will pass while guests fail, and a
/// guest on DNS-over-HTTPS bypasses the router and succeeds while we warn.
///
/// So it only ever warns. Falling back on this signal would strand every
/// DoH-using guest on a self-signed certificate to avoid a problem they do not
/// have.
/// What the local resolver had to say about our own name.
#[derive(Debug, PartialEq, Eq)]
pub enum Resolution {
    /// It answered with the address we expect.
    Correct,
    /// It answered with nothing -- the rebinding-protection case.
    Refused,
    /// It answered with something else, so an answer is being rewritten.
    Rewritten,
}

fn judge_resolution(expected: IpAddr, resolved: &[IpAddr]) -> Resolution {
    if resolved.contains(&expected) {
        Resolution::Correct
    } else if resolved.is_empty() {
        Resolution::Refused
    } else {
        Resolution::Rewritten
    }
}

pub async fn check_managed_name(name: &ManagedName) {
    let resolved: Vec<IpAddr> = tokio::net::lookup_host((name.host.as_str(), 443))
        .await
        .map(|addrs| addrs.map(|addr| addr.ip()).collect())
        .unwrap_or_default();

    if judge_resolution(name.addr, &resolved) == Resolution::Correct {
        info!("{} resolves to {} here", name.host, name.addr);
    } else if judge_resolution(name.addr, &resolved) == Resolution::Refused {
        warn!(
            "{} does not resolve on this device. The usual cause is DNS-rebinding \
             protection refusing a public name that answers with a private address \
             (dnsmasq's stop-dns-rebind, Pi-hole, many consumer routers). Guests \
             using the same resolver will not reach this device either; guests on \
             their own resolver still might. --managed-cert off falls back to the \
             bare address.",
            name.host
        );
    } else {
        warn!(
            "{} resolves to {:?} on this device, not {}. Something between here and \
             the zone is rewriting the answer.",
            name.host, resolved, name.addr
        );
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

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(s: &str) -> IpAddr {
        IpAddr::V4(s.parse().unwrap())
    }
    fn v6(s: &str) -> IpAddr {
        IpAddr::V6(s.parse().unwrap())
    }

    #[test]
    fn ipv4_becomes_one_dashed_label() {
        assert_eq!(dashed_label(v4("192.168.178.15")).unwrap(), "192-168-178-15");
        assert_eq!(dashed_label(v4("10.0.0.1")).unwrap(), "10-0-0-1");
    }

    #[test]
    fn ipv6_uses_the_compressed_form() {
        // Measured against the zone: both `fd00--1` and the fully expanded
        // `fd00-0-0-0-0-0-0-1` resolve, so the short one is safe and scans better.
        assert_eq!(dashed_label(v6("fd00::1")).unwrap(), "fd00--1");
        assert_eq!(dashed_label(v6("2001:db8::1")).unwrap(), "2001-db8--1");
    }

    #[test]
    fn ipv4_mapped_addresses_are_refused() {
        // These render with dots, which would make a second label and fall
        // outside the `*.clientctrl.cc` wildcard.
        assert!(dashed_label(v6("::ffff:192.168.1.1")).is_none());
    }

    #[test]
    fn only_rfc1918_counts_as_private() {
        for private in ["10.0.0.1", "172.16.5.4", "192.168.178.15", "172.31.255.254"] {
            let IpAddr::V4(addr) = v4(private) else { unreachable!() };
            assert!(addr.is_private(), "{private} should count");
        }
        // Deliberately excluded: CGNAT and link-local are not RFC 1918, and a
        // public address is not the case this feature is for.
        for other in ["100.64.0.1", "169.254.1.1", "8.8.8.8", "172.32.0.1"] {
            let IpAddr::V4(addr) = v4(other) else { unreachable!() };
            assert!(!addr.is_private(), "{other} should not count");
        }
    }

    #[test]
    fn the_default_https_port_is_left_out_of_the_url() {
        // The whole reason for preferring 443 -- a shorter address on screen and
        // fewer modules in the QR code.
        assert_eq!(authority("host.example", 443), "host.example");
        assert_eq!(authority("host.example", 3443), "host.example:3443");
        assert_eq!(
            public_base_url("signage.example.com", 443),
            "https://signage.example.com/"
        );
        assert_eq!(
            public_base_url("signage.example.com", 3443),
            "https://signage.example.com:3443/"
        );
    }

    #[test]
    fn a_full_base_url_keeps_whatever_the_operator_wrote() {
        // The port there belongs to whatever is proxying, not to our listener.
        assert_eq!(
            public_base_url("https://signage.example.com:8443", 443),
            "https://signage.example.com:8443/"
        );
    }

    #[test]
    fn the_resolver_verdict() {
        let ours: IpAddr = "192.168.178.15".parse().unwrap();
        let other: IpAddr = "10.0.0.1".parse().unwrap();

        assert_eq!(judge_resolution(ours, &[ours]), Resolution::Correct);
        // Several answers are fine as long as ours is among them.
        assert_eq!(judge_resolution(ours, &[other, ours]), Resolution::Correct);
        // Nothing at all is what rebinding protection looks like.
        assert_eq!(judge_resolution(ours, &[]), Resolution::Refused);
        // An answer that is not ours means something rewrote it.
        assert_eq!(judge_resolution(ours, &[other]), Resolution::Rewritten);
    }

    #[test]
    fn ipv6_classification() {
        let global: Ipv6Addr = "2a10:c5c1:cafe:210::1".parse().unwrap();
        let ula: Ipv6Addr = "fd00::1".parse().unwrap();
        let link: Ipv6Addr = "fe80::1".parse().unwrap();

        assert!(is_global_v6(&global) && !is_ula_v6(&global));
        assert!(is_ula_v6(&ula) && !is_global_v6(&ula));
        // Link-local is neither, and the zone does not answer for it anyway.
        assert!(!is_global_v6(&link) && !is_ula_v6(&link));
    }
}
