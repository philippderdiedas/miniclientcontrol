//! Single sign-on with OpenID Connect: the provider's configuration, the
//! authorization-code flow, and verifying what comes back. Generic -- nothing
//! here knows one provider from another.

pub mod api;
pub mod config;
pub mod flow;
pub mod http;
pub mod jwt;
