//! Which role a route needs.
//!
//! One table, so the question "who may do this" has one answer. It fails
//! closed: a write this table does not know needs an admin, so a route added
//! later without a row is locked rather than open.

use axum::http::Method;

/// What a route needs from the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    /// No account at all: the login page and the login itself.
    Open,
    /// Any signed-in role.
    Read,
    /// A content write: a manager or admin writes directly, an editor's is
    /// recorded as a proposal instead of executed.
    Content,
    Manager,
    Admin,
}

pub fn required(method: &Method, path: &str) -> Need {
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let reading = method == Method::GET || method == Method::HEAD;
    match (reading, segments.as_slice()) {
        (true, ["login.html"]) => Need::Open,
        (false, ["api", "login"]) if method == Method::POST => Need::Open,

        // A webhook target's headers are where somebody else's API token lives,
        // so even reading the list is an admin's.
        (true, ["api", "oidc", "info" | "start" | "callback"]) => Need::Open,
        (_, ["api", "oidc", "config"]) => Need::Admin,
        (true, ["api", "webhooks", ..]) | (true, ["api", "users", ..]) => Need::Admin,
        // Every account's tokens: an admin's, reading included.
        (_, ["api", "tokens", ..]) => Need::Admin,
        (true, ["api", "changesets", "draft", ..]) => Need::Read,
        (true, ["api", "changesets", "mine"]) => Need::Read,
        (true, ["api", "changesets", ..]) => Need::Manager,
        (true, _) => Need::Read,

        (false, ["api", "logout"]) => Need::Read,
        (false, ["api", "me", "password"]) => Need::Read,
        // One's own tokens; the handlers refuse a request made with a token.
        (false, ["api", "me", "tokens", ..]) => Need::Read,
        // Any account: every tool call is replayed through the router as the
        // caller, so each one meets this table again on its own path.
        (false, ["mcp"]) => Need::Read,
        (false, ["api", "changesets", "draft", ..]) => Need::Read,
        (false, ["api", "changesets", "mine", "seen" | "hide-decided"]) => Need::Read,
        (false, ["api", "changesets", _, "approve" | "reject"]) => Need::Manager,
        // Any account, for its own bundles only -- the handlers check ownership.
        (false, ["api", "changesets", _, "withdraw" | "hide"]) => Need::Read,

        (false, ["api", "assets"]) if method == Method::POST => Need::Content,
        (false, ["api", "assets", _]) => Need::Content,
        (false, ["api", "playlists"]) if method == Method::POST => Need::Content,
        (false, ["api", "playlists", _]) => Need::Content,
        (false, ["api", "playlist"]) if method == Method::POST => Need::Content,
        (false, ["api", "playlist", _]) => Need::Content,
        (false, ["api", "playlist", _, "move"]) => Need::Content,
        (false, ["api", "playlist", _, "duplicate"]) => Need::Content,
        (false, ["api", "displays", _, "schedule"]) => Need::Content,

        (false, ["api", "override"]) => Need::Manager,
        (false, ["api", "displays", _, "override"]) => Need::Manager,
        (false, ["api", "control", "current"]) => Need::Manager,
        (false, ["api", "displays", _, "control", "current"]) => Need::Manager,

        _ => Need::Admin,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Method;

    #[test]
    fn every_route_has_its_minimum_role() {
        let cases: &[(Method, &str, Need)] = &[
            (Method::GET, "/login.html", Need::Open),
            (Method::POST, "/api/login", Need::Open),
            (Method::GET, "/api/me", Need::Read),
            (Method::POST, "/api/logout", Need::Read),
            (Method::PUT, "/api/me/password", Need::Read),
            (Method::GET, "/api/me/tokens", Need::Read),
            (Method::POST, "/api/me/tokens", Need::Read),
            (Method::DELETE, "/api/me/tokens/3", Need::Read),
            (Method::PUT, "/api/me/tokens/3", Need::Read),
            (Method::GET, "/api/tokens", Need::Admin),
            (Method::PUT, "/api/tokens/3", Need::Admin),
            (Method::DELETE, "/api/tokens/3", Need::Admin),
            (Method::POST, "/mcp", Need::Read),
            (Method::GET, "/tokens.html", Need::Read),
            (Method::GET, "/api/playlist", Need::Read),
            (Method::GET, "/playlist.html", Need::Read),
            (Method::PUT, "/api/settings", Need::Admin),
            (Method::GET, "/api/settings", Need::Read),
            (Method::POST, "/api/webhooks", Need::Admin),
            (Method::PUT, "/api/webhooks/3", Need::Admin),
            (Method::GET, "/api/webhooks", Need::Admin),
            (Method::GET, "/api/users", Need::Admin),
            (Method::POST, "/api/users", Need::Admin),
            (Method::POST, "/api/audio", Need::Admin),
            (Method::PUT, "/api/displays/foyer", Need::Admin),
            (Method::DELETE, "/api/displays/foyer/cast/session", Need::Admin),
            (Method::POST, "/api/assets", Need::Content),
            (Method::PUT, "/api/assets/4", Need::Content),
            (Method::DELETE, "/api/assets/4", Need::Content),
            (Method::POST, "/api/playlists", Need::Content),
            (Method::PUT, "/api/playlists/2", Need::Content),
            (Method::DELETE, "/api/playlists/2", Need::Content),
            (Method::POST, "/api/playlist", Need::Content),
            (Method::PUT, "/api/playlist/9", Need::Content),
            (Method::DELETE, "/api/playlist/9", Need::Content),
            (Method::POST, "/api/playlist/9/move", Need::Content),
            (Method::POST, "/api/playlist/9/duplicate", Need::Content),
            (Method::PUT, "/api/displays/foyer/schedule", Need::Content),
            (Method::POST, "/api/override", Need::Manager),
            (Method::DELETE, "/api/override", Need::Manager),
            (Method::POST, "/api/displays/foyer/override", Need::Manager),
            (Method::POST, "/api/control/current", Need::Manager),
            (Method::POST, "/api/displays/foyer/control/current", Need::Manager),
            (Method::GET, "/api/changesets", Need::Manager),
            (Method::POST, "/api/changesets/3/approve", Need::Manager),
            (Method::POST, "/api/changesets/3/reject", Need::Manager),
            (Method::GET, "/api/changesets/draft", Need::Read),
            (Method::POST, "/api/changesets/draft/submit", Need::Read),
            (Method::DELETE, "/api/changesets/draft", Need::Read),
            (Method::GET, "/api/changesets/mine", Need::Read),
            (Method::POST, "/api/changesets/mine/seen", Need::Read),
            (Method::POST, "/api/changesets/mine/hide-decided", Need::Read),
            (Method::POST, "/api/changesets/3/withdraw", Need::Read),
            (Method::POST, "/api/changesets/3/hide", Need::Read),
            (Method::GET, "/api/displays/foyer/screenshot", Need::Read),
            (Method::GET, "/api/oidc/info", Need::Open),
            (Method::GET, "/api/oidc/start", Need::Open),
            (Method::GET, "/api/oidc/callback", Need::Open),
            (Method::GET, "/api/oidc/config", Need::Admin),
            (Method::PUT, "/api/oidc/config", Need::Admin),
        ];
        for (method, path, need) in cases {
            assert_eq!(required(method, path), *need, "{method} {path}");
        }
    }

    #[test]
    fn an_unknown_write_needs_an_admin() {
        // A route added later without a row must fail closed, not open.
        assert_eq!(required(&Method::POST, "/api/something-new"), Need::Admin);
        assert_eq!(required(&Method::GET, "/api/something-new"), Need::Read);
    }
}
