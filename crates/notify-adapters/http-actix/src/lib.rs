//! The HTTP surface (design §8): handlers turn requests into domain values, call one
//! use case, and serialise the answer. Refusals are `application/problem+json` (RFC 9457);
//! the OpenAPI document is generated from these handlers (ADR-0009, ADR-0013).

mod extract;
mod problem;
mod routes;
mod slots;
mod state;

pub use extract::{Authenticated, KeyAdministrator};
pub use problem::{ApiError, Problem};
pub use routes::configure;
pub use slots::StreamSlots;
pub use state::ApiState;

/// The request and response shapes that exist only at this boundary (ADR-0013).
pub mod dto {
    pub use crate::routes::accounts::{CreateAccountBody, UserCard, UserQuery};
    pub use crate::routes::channels::{AddMemberBody, ChannelPolicyBody, CreateChannelBody};
    pub use crate::routes::client_config::ClientConfig;
    pub use crate::routes::compat::{
        Alert, AlertmanagerBody, CompatQuery, GotifyBody, GotifyMessage, JsonIngested, NtfyBody,
        NtfyMessage,
    };
    pub use crate::routes::connections::Connection;
    pub use crate::routes::identity::{
        ApiKeyLimitBody, CreateKeyBody, EnrolBody, MintedKey, Whoami,
    };
    pub use crate::routes::interactions::{ReceiptBody, RecordedInteraction, SilenceBody};
    pub use crate::routes::notifications::{InboxParams, PageParams, PublishBody, SearchParams};
    pub use crate::routes::profile::{AvatarUpload, ProfileBody};
    pub use crate::routes::push::WebPushKey;
    pub use crate::routes::stream::StreamParams;
    pub use crate::routes::sync::SyncParams;
}
