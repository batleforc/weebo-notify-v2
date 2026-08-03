//! The HTTP surface: REST today, SSE and WebSocket alongside it (design §8).
//!
//! Actix stops here. A handler reads the request, turns path segments and query
//! strings into domain values, calls exactly one use case, and serialises what
//! comes back — nothing in [`notify_app`] has ever seen an `HttpRequest`, and
//! nothing here decides who may do what.
//!
//! # Three things this layer owns
//!
//! **Who is calling.** [`Authenticated`] is the only way a handler gets a
//! [`notify_app::Caller`], so there is no path into the application layer that
//! skipped authentication.
//!
//! **What a refusal looks like.** Every failure becomes `application/problem+json`
//! with the stable `code` clients match on (design §8, RFC 9457). Infrastructure
//! failures deliberately carry no code and no detail: the operator gets the cause
//! in a log line, the caller gets a status.
//!
//! **What the contract says.** The OpenAPI document is generated from these
//! handlers and committed, so the Rust and TypeScript clients cannot drift from the
//! server (ADR-0009). The schemas come from the domain types themselves rather than
//! from a mirror of them (ADR-0013).
//!
//! # Wiring it
//!
//! ```ignore
//! use utoipa_actix_web::AppExt as _;
//!
//! let (app, api) = actix_web::App::new()
//!     .into_utoipa_app()
//!     .openapi(ApiDoc::openapi())              // info, tags, security schemes
//!     .configure(notify_http_actix::configure) // the routes
//!     .split_for_parts();
//!
//! let app = app.app_data(web::Data::new(state));
//! ```
//!
//! `configure` takes no state, which is what lets `dump-openapi` build the document
//! without booting anything.

mod extract;
mod problem;
mod routes;
mod state;

pub use extract::Authenticated;
pub use problem::{ApiError, Problem};
pub use routes::configure;
pub use state::ApiState;

/// The request and response shapes that exist only at this boundary.
///
/// Everything else on the wire is a domain type described by its own schema
/// (ADR-0013); these are the handful of places where the API deliberately says
/// something the domain does not — a minted secret, an outcome label, a channel
/// alongside a revision.
pub mod dto {
    pub use crate::routes::accounts::CreateAccountBody;
    pub use crate::routes::channels::{AddMemberBody, ChannelPolicyBody, CreateChannelBody};
    pub use crate::routes::identity::{CreateKeyBody, EnrolBody, MintedKey};
    pub use crate::routes::interactions::{ReceiptBody, RecordedInteraction, SilenceBody};
    pub use crate::routes::notifications::{InboxParams, PageParams, PublishBody, SearchParams};
    pub use crate::routes::profile::{AvatarUpload, ProfileBody};
    pub use crate::routes::stream::StreamParams;
    pub use crate::routes::sync::SyncParams;
}
