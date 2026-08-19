//! Publishing an extra `.local` name for this device.
//!
//! Avahi announces `<hostname>.local` on its own, but nothing else. A machine
//! driving two displays needs a name per display — `kiosk2-links.local` and
//! `kiosk2-rechts.local` rather than one `kiosk2.local` that cannot say which
//! screen is meant — and those have to be published explicitly.
//!
//! Done by supervising `avahi-publish`, which holds the record for as long as it
//! runs, rather than by speaking to Avahi over D-Bus: it is a few lines instead of
//! a dependency, and the record disappearing when the controller stops is exactly
//! the behaviour wanted.

use std::sync::Arc;
use std::time::Duration;

use tokio::process::{Child, Command};
use tracing::{error, info, warn};

use crate::models::Args;
use crate::tls;

const AVAHI_PUBLISH: &str = "avahi-publish";

/// The name we have to announce ourselves, if any.
///
/// `None` when the address is not an mDNS name at all, or when it is this
/// machine's own hostname — Avahi already publishes that one, and a second
/// record for it would just be noise.
fn alias(args: &Args) -> Option<String> {
    let name = match tls::public_url(&args.public_url) {
        tls::PublicUrl::Host(host) => host,
        // `mdns` resolves to the system hostname, which Avahi handles.
        tls::PublicUrl::Mdns(_) | tls::PublicUrl::LanAddress | tls::PublicUrl::Base(_) => {
            return None
        }
    };
    if !name.ends_with(".local") {
        return None;
    }
    if tls::system_hostname().is_some_and(|host| format!("{}.local", host) == name) {
        return None;
    }
    Some(name)
}

fn spawn(name: &str) -> std::io::Result<Child> {
    let address = tls::primary_local_ipv4();
    info!("Publishing {} as {} over mDNS", name, address);
    Command::new(AVAHI_PUBLISH)
        // -a: publish an address record; -R: without the reverse entry, which is
        // not ours to claim on a shared network.
        .arg("-a")
        .arg("-R")
        .arg(name)
        .arg(address.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
}

pub async fn supervise(args: Arc<Args>) {
    let Some(name) = alias(&args) else {
        return;
    };

    let mut warned = false;
    loop {
        match spawn(&name) {
            Ok(mut child) => {
                warned = false;
                let status = child.wait().await;
                warn!("avahi-publish for {} exited ({:?}), restarting", name, status);
            }
            Err(e) => {
                // Once, not every five seconds: on a box without Avahi this would
                // otherwise fill the log forever.
                if !warned {
                    error!(
                        "Cannot publish '{}': {} ({}). Guests will only reach this \
                         device by address unless avahi-daemon and avahi-utils are \
                         installed.",
                        name, e, AVAHI_PUBLISH
                    );
                    warned = true;
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}
