use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use tracing::warn;

use crate::models::{AppState, Display};

/// The URL a guest is told to open. Resolved every time rather than stored: it
/// depends on `--public-url`, on the port actually bound, and on the machine's
/// current address, all of which can change without anybody editing anything.
///
/// `display` names the screen the guest should land on. `None` returns the bare
/// root -- what a chooser QR encodes, letting the guest pick among several
/// screens. `Some` appends `?screen=<name>`, so scanning it goes straight to one
/// screen without a picker. Which one a caller passes follows `cast_qr_target`:
/// `cast_state`, `cast_qr` and `settings::overlay_payload` all read that setting
/// to decide, and must keep agreeing -- otherwise the address a screen prints,
/// the picture beside it and the overlay's own QR could each send a guest
/// somewhere different.
pub fn sender_url(state: &AppState, display: Option<&Display>) -> String {
    let base = if state.managed_cert {
        crate::tls::managed_base_url(state.cast_tls_port)
    } else {
        None
    };
    let base = base.unwrap_or_else(|| {
        crate::tls::public_base_url(&state.args.public_url, state.cast_tls_port)
    });
    match display {
        Some(display) => format!("{base}?screen={}", urlencoding::encode(&display.name)),
        None => base,
    }
}

/// Which screen this QR should send a guest to. Same rule as every other
/// scoped cast route -- `display::resolve`: a named screen resolves or 404s,
/// an omitted one resolves while exactly one display is declared and 409s
/// once several are.
#[derive(Deserialize)]
pub struct ScreenQuery {
    #[serde(default)]
    screen: Option<String>,
}

/// QR code for the guest URL, as SVG.
///
/// Rendered here rather than in the page: the device is often offline, so a
/// client-side library would have to be vendored, and an SVG the display can
/// scale is a few hundred bytes.
///
/// Resolved exactly like `cast_state`: `display::resolve` first, then
/// `cast_qr_target` decides whether the picture names this screen or hands
/// over the bare chooser address. Reading anywhere but that setting would make
/// this a second source of truth -- the address a panel prints and the
/// picture beside it must never disagree.
pub async fn cast_qr(State(state): State<AppState>, Query(query): Query<ScreenQuery>) -> Response {
    let display = match crate::display::resolve(&state, query.screen.as_deref()) {
        Ok(display) => display,
        Err(response) => return response,
    };
    let target = state.settings.read().await.cast_qr_target;
    let text = match target {
        crate::settings::CastQrTarget::Screen => sender_url(&state, Some(&display)),
        crate::settings::CastQrTarget::Chooser => sender_url(&state, None),
    };
    qr_svg(&text)
}

/// The QR code as a module matrix, one string of `0`/`1` per row.
///
/// This exists instead of a URL because the overlay lives in *someone else's*
/// document: Chromium's Local Network Access blocks a page on a public origin
/// from loading anything off `127.0.0.1` without a permission click, and a kiosk
/// has nobody to click it. Handing over the modules and drawing them as inline
/// SVG needs no request at all -- which also sidesteps an `img-src` CSP, where
/// even a `data:` URL would be refused.
pub fn qr_matrix(text: &str) -> Option<Vec<String>> {
    let code = qrcode::QrCode::new(text.as_bytes())
        .inspect_err(|e| warn!("Could not encode '{}' as a QR code: {}", text, e))
        .ok()?;
    let width = code.width();
    let modules = code.to_colors();
    Some(
        modules
            .chunks(width)
            .map(|row| {
                row.iter()
                    .map(|color| match color {
                        qrcode::Color::Dark => '1',
                        qrcode::Color::Light => '0',
                    })
                    .collect()
            })
            .collect(),
    )
}

/// Render any text as a QR code SVG.
///
/// Server-side because the device is often offline, so a client-side library
/// would have to be vendored, and because an SVG scales to whatever the panel
/// is. Shared with the overlay, which needs the same thing for arbitrary text.
pub fn qr_svg(text: &str) -> Response {
    let code = match qrcode::QrCode::new(text.as_bytes()) {
        Ok(code) => code,
        Err(e) => {
            warn!("Could not encode '{}' as a QR code: {}", text, e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "qr encoding failed").into_response();
        }
    };
    let svg = code
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(240, 240)
        // white margin, so it still scans against a dark page background
        .light_color(qrcode::render::svg::Color("#ffffff"))
        .dark_color(qrcode::render::svg::Color("#000000"))
        .build();

    (
        [(axum::http::header::CONTENT_TYPE, "image/svg+xml")],
        svg,
    )
        .into_response()
}

pub(super) fn cast_display_url(port: u16, display: &str) -> String {
    format!(
        "http://127.0.0.1:{}/cast_display.html?screen={}",
        port,
        urlencoding::encode(display)
    )
}
