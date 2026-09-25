//! A playlist item that splits the screen: widgets on a fixed 24×24 grid, each
//! showing a URL or an asset. Grafana's `gridPos` without the scrolling -- a
//! screen does not scroll -- so it maps 1:1 onto CSS Grid.

use serde::{Deserialize, Serialize};

use crate::models::ScrollMode;

pub const GRID: u8 = 24;
pub const MAX_WIDGETS: usize = 12;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(untagged)]
pub enum Source {
    Url { url: String },
    Asset { asset_id: i64 },
    // A built-in the controller renders. Internally tagged by `kind`, so an
    // untagged `{ "kind": ... }` object matches only this -- Url needs `url`,
    // Asset needs `asset_id`.
    Builtin(crate::builtin::Builtin),
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Widget {
    pub x: u8,
    pub y: u8,
    pub w: u8,
    pub h: u8,
    pub source: Source,
    #[serde(default)]
    pub scroll_config: ScrollMode,
    #[serde(default)]
    pub fit_mode: Option<String>,
    #[serde(default)]
    pub fit_background: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Layout {
    pub widgets: Vec<Widget>,
    /// The colour behind and between the widgets (uncovered grid cells). Hex;
    /// `None` is the default black the layout page falls back to.
    #[serde(default)]
    pub background: Option<String>,
}

impl Layout {
    /// Refused rather than clamped: this is drawn by someone looking at the
    /// editor, who can be told, and a silently moved widget reads as a bug.
    pub fn check(&self) -> Result<(), String> {
        if self.widgets.is_empty() {
            return Err("Ein Layout braucht mindestens ein Widget.".into());
        }
        if self.widgets.len() > MAX_WIDGETS {
            return Err(format!("Höchstens {MAX_WIDGETS} Widgets pro Layout."));
        }
        for (i, w) in self.widgets.iter().enumerate() {
            let n = i + 1;
            if w.w == 0 || w.h == 0 {
                return Err(format!("Widget {n} ist leer."));
            }
            if u16::from(w.x) + u16::from(w.w) > u16::from(GRID) || u16::from(w.y) + u16::from(w.h) > u16::from(GRID) {
                return Err(format!("Widget {n} ragt aus dem Raster."));
            }
            if let Source::Url { url } = &w.source {
                let url = url.trim();
                if !(url.starts_with("http://") || url.starts_with("https://")) {
                    return Err(format!("Widget {n}: die URL muss mit http:// oder https:// beginnen."));
                }
            }
            for (j, other) in self.widgets.iter().enumerate().skip(i + 1) {
                // In u16: a later widget is not bounds-checked yet, and its
                // x + w could overflow a u8.
                let (ax, ay, aw, ah) = (u16::from(w.x), u16::from(w.y), u16::from(w.w), u16::from(w.h));
                let (bx, by, bw, bh) = (u16::from(other.x), u16::from(other.y), u16::from(other.w), u16::from(other.h));
                let apart = ax + aw <= bx || bx + bw <= ax || ay + ah <= by || by + bh <= ay;
                if !apart {
                    return Err(format!("Widget {n} und {} überlappen.", j + 1));
                }
            }
        }
        Ok(())
    }

    /// Asset ids this layout refers to, for the existence check.
    pub fn asset_ids(&self) -> Vec<i64> {
        self.widgets.iter().filter_map(|w| match w.source {
            Source::Asset { asset_id } => Some(asset_id),
            Source::Url { .. } | Source::Builtin(_) => None,
        }).collect()
    }

    /// Tidy content before storing: cap each built-in widget's fields (the same
    /// caps a standalone built-in gets), and drop a background that is not a hex
    /// colour. Geometry is `check`'s job; this only touches content.
    pub fn sanitized(mut self) -> Self {
        for w in &mut self.widgets {
            if let Source::Builtin(b) = &w.source {
                w.source = Source::Builtin(b.clone().sanitized());
            }
        }
        if let Some(bg) = &self.background {
            if !crate::settings::is_hex_colour(bg) {
                self.background = None;
            }
        }
        self
    }
}

/// `GET /api/layout/{id}`, for the display browser's `layout.html`: each
/// widget's place and the URL its frame loads -- the widget's own URL, or the
/// viewer an asset plays in, built exactly as a playlist item's would be.
pub async fn widgets_for_display(
    axum::extract::State(state): axum::extract::State<crate::models::AppState>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let layout: Option<sqlx::types::Json<Option<Layout>>> =
        sqlx::query_scalar("SELECT COALESCE(layout, 'null') FROM playlist_items WHERE id = ?")
            .bind(id)
            .fetch_optional(&state.pool)
            .await
            .unwrap_or(None);
    let Some(sqlx::types::Json(Some(layout))) = layout else {
        return axum::http::StatusCode::NOT_FOUND.into_response();
    };
    let mut widgets = Vec::with_capacity(layout.widgets.len());
    for widget in &layout.widgets {
        let src = match &widget.source {
            Source::Url { url } => url.clone(),
            // No display scope here: a layout resolves against the primary, the
            // same way `/api/overlay` does, so a cast-QR built-in uses the
            // chooser/primary URL.
            Source::Builtin(b) => crate::builtin::resolve(&state, None, b).await,
            Source::Asset { asset_id } => {
                let asset: Option<(String, String)> =
                    sqlx::query_as("SELECT local_path, mimetype FROM assets WHERE id = ?")
                        .bind(asset_id)
                        .fetch_optional(&state.pool)
                        .await
                        .unwrap_or(None);
                match asset {
                    Some((local_path, mimetype)) => crate::browser::asset_target_url(
                        state.args.port,
                        &local_path,
                        Some(&mimetype),
                        &widget.scroll_config,
                        crate::models::FitMode::from_value(widget.fit_mode.as_deref().unwrap_or("contain")),
                        widget.fit_background.as_deref().unwrap_or(crate::models::DEFAULT_FIT_BACKGROUND),
                    ),
                    None => format!("http://127.0.0.1:{}/no_content.svg", state.args.port),
                }
            }
        };
        let scroll = crate::browser::scroll_mode_settings(&widget.scroll_config).payload();
        widgets.push(serde_json::json!({
            "x": widget.x, "y": widget.y, "w": widget.w, "h": widget.h, "src": src, "scroll": scroll,
        }));
    }
    axum::Json(serde_json::json!({ "widgets": widgets, "background": layout.background })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(x: u8, y: u8, w: u8, h: u8) -> Widget {
        Widget { x, y, w, h, source: Source::Url { url: "https://a.test/".into() },
                 scroll_config: ScrollMode::None, fit_mode: None, fit_background: None }
    }

    #[test]
    fn a_builtin_widget_round_trips_and_needs_no_asset() {
        let w = Widget {
            x: 0, y: 0, w: 12, h: 12,
            source: Source::Builtin(crate::builtin::Builtin::Clock {
                format_24h: true, show_seconds: false, show_date: false,
                timezone: String::new(), text_size: None,
                style: crate::builtin::Style::default(),
            }),
            scroll_config: ScrollMode::None, fit_mode: None, fit_background: None,
        };
        let layout = Layout { widgets: vec![w], ..Default::default() };
        assert!(layout.check().is_ok());
        assert!(layout.asset_ids().is_empty());
        let v = serde_json::to_value(&layout).unwrap();
        assert_eq!(serde_json::from_value::<Layout>(v).unwrap(), layout);
    }

    #[test]
    fn an_l_shape_is_valid() {
        let l = Layout { widgets: vec![url(0, 0, 18, 20), url(18, 0, 6, 20), url(0, 20, 24, 4)], ..Default::default() };
        assert_eq!(l.check(), Ok(()));
    }

    #[test]
    fn the_refusals() {
        assert!(Layout { widgets: vec![], ..Default::default() }.check().is_err());
        assert!(Layout { widgets: vec![url(20, 0, 5, 1)], ..Default::default() }.check().unwrap_err().contains("ragt"));
        assert!(Layout { widgets: vec![url(0, 0, 0, 1)], ..Default::default() }.check().unwrap_err().contains("leer"));
        assert!(Layout { widgets: vec![url(0, 0, 12, 12), url(11, 11, 4, 4)], ..Default::default() }.check().unwrap_err().contains("überlappen"));
        assert!(Layout { widgets: (0..13).map(|i| url(i, 0, 1, 1)).collect(), ..Default::default() }.check().unwrap_err().contains("Höchstens"));
        let mut bad = url(0, 0, 1, 1);
        bad.source = Source::Url { url: "javascript:alert(1)".into() };
        assert!(Layout { widgets: vec![bad], ..Default::default() }.check().unwrap_err().contains("http"));
    }

    #[test]
    fn huge_numbers_do_not_overflow() {
        assert!(Layout { widgets: vec![url(0, 0, 1, 1), url(250, 250, 250, 250)], ..Default::default() }.check().is_err());
    }

    #[test]
    fn touching_edges_do_not_overlap() {
        assert_eq!(Layout { widgets: vec![url(0, 0, 12, 24), url(12, 0, 12, 24)], ..Default::default() }.check(), Ok(()));
    }

    #[test]
    fn the_json_shape() {
        let v = serde_json::json!({"widgets": [
            {"x": 0, "y": 0, "w": 12, "h": 24, "source": {"url": "https://a.test/"}},
            {"x": 12, "y": 0, "w": 12, "h": 24, "source": {"asset_id": 7}}
        ]});
        let l: Layout = serde_json::from_value(v).unwrap();
        assert_eq!(l.asset_ids(), vec![7]);
        assert_eq!(l.widgets[0].scroll_config, ScrollMode::None);
    }
}
