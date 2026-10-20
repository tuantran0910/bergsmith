//! OIDC identity, server-side sessions, scoped authorization, and audit.
//!
//! People authenticate through the authorization-code flow with PKCE. The
//! coordinator keeps sessions in PostgreSQL and hands the browser only
//! random, `HttpOnly` tokens. Authorization is evaluated from active role
//! grants on every command, at admission and before a publication send.
//!
//! This module is the library behind the `/auth` routes. The application API
//! that consumes it is BS-018; nothing here assumes a particular UI.

pub mod administration;
pub mod audit;
pub mod authorization;
pub mod oidc;
mod random;
pub mod routes;
pub mod session;

/// The name of the session cookie the coordinator sets.
pub const SESSION_COOKIE: &str = "bergsmith_session";

/// The header that carries the session-bound CSRF token on unsafe requests.
pub const CSRF_HEADER: &str = "x-csrf-token";
