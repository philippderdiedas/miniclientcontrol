//! What a screen shows, as a small JPEG: the page on screen with its overlay,
//! override or guest page -- which is why it is not the asset's own file.
//! Taken on request only and cached, because on a Pi every capture costs.

use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use chromiumoxide::cdp::browser_protocol::page::{CaptureScreenshotFormat, CaptureScreenshotParams, Viewport};

use crate::models::{AppState, Display};

const FRESH: Duration = Duration::from_secs(10);
/// A capture hangs on a frozen screen (measured); the last picture is the answer then.
const TIMEOUT: Duration = Duration::from_secs(5);
const MAX_WIDTH: f64 = 640.0;

/// The visible part of the page. `Page.captureScreenshot` directly, not
/// `Page::screenshot`, which activates the target first: racing the loop's own
/// switch to another page, that would bring the old tab back to the front.
async fn take(page: &chromiumoxide::Page) -> Option<Vec<u8>> {
    let view: Vec<f64> = page
        .evaluate("[innerWidth, innerHeight, scrollX, scrollY]")
        .await
        .ok()?
        .into_value()
        .ok()?;
    let (width, height, x, y) = (*view.first()?, *view.get(1)?, *view.get(2)?, *view.get(3)?);
    if width <= 0.0 || height <= 0.0 {
        return None;
    }
    let scale = (MAX_WIDTH / width).min(1.0);
    let params = CaptureScreenshotParams::builder()
        .format(CaptureScreenshotFormat::Jpeg)
        .quality(60)
        .clip(Viewport { x, y, width, height, scale })
        .build();
    let shot = page.execute(params).await.ok()?;
    let data: &str = shot.result.data.as_ref();
    base64::engine::general_purpose::STANDARD.decode(data).ok()
}

/// A fresh picture if one can be had within `TIMEOUT`, else the last one.
pub async fn capture(display: &Display) -> Option<(Vec<u8>, Duration)> {
    if let Some((at, bytes)) = display.screenshot.lock().await.as_ref() {
        if at.elapsed() < FRESH {
            return Some((bytes.clone(), at.elapsed()));
        }
    }
    let page = display.screen_page.lock().await.clone();
    if let Some(page) = page {
        if let Ok(Some(bytes)) = tokio::time::timeout(TIMEOUT, take(&page)).await {
            *display.screenshot.lock().await = Some((Instant::now(), bytes.clone()));
            return Some((bytes, Duration::ZERO));
        }
    }
    display.screenshot.lock().await.as_ref().map(|(at, bytes)| (bytes.clone(), at.elapsed()))
}

pub async fn handler(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    let Some(display) = state.display(&name) else {
        return (StatusCode::NOT_FOUND, axum::Json(serde_json::json!({ "error": "Unbekannter Bildschirm." })))
            .into_response();
    };
    let Some((bytes, age)) = capture(&display).await else {
        return (StatusCode::SERVICE_UNAVAILABLE, axum::Json(serde_json::json!({ "error": "Noch kein Bild." })))
            .into_response();
    };
    let mut response = (
        [
            (header::CONTENT_TYPE, "image/jpeg".to_string()),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        bytes,
    )
        .into_response();
    let headers = response.headers_mut();
    if let Ok(value) = age.as_secs().to_string().parse() {
        headers.insert("x-screenshot-age", value);
    }
    if let Some(since) = *display.frozen_since.lock().await {
        if let Ok(value) = since.to_rfc3339().parse() {
            headers.insert("x-screen-frozen-since", value);
        }
    }
    response
}
