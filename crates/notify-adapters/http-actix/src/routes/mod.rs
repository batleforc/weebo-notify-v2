//! The route table, and the small conversions the path segments need.

pub(crate) mod accounts;
pub(crate) mod channels;
pub(crate) mod identity;
pub(crate) mod interactions;
pub(crate) mod notifications;
pub(crate) mod preferences;
pub(crate) mod profile;
pub(crate) mod stream;
pub(crate) mod sync;

use notify_domain::{DomainError, Principal, ServiceAccountId, UserId};
use utoipa_actix_web::service_config::ServiceConfig;

/// Register every M1 route under `/api/v1`.
///
/// Takes no state: the composition root supplies that separately, which is what
/// lets `notify-server dump-openapi` build the document without a database, an
/// object store or an identity provider (design §3.2).
pub fn configure(config: &mut ServiceConfig<'_>) {
    config.service(
        utoipa_actix_web::scope("/api/v1")
            .service(notifications::publish)
            .service(notifications::upsert)
            .service(notifications::inbox)
            .service(notifications::channel_inbox)
            .service(notifications::get_one)
            .service(notifications::revisions)
            .service(notifications::delete_one)
            .service(notifications::search)
            .service(interactions::record)
            .service(interactions::list_interactions)
            .service(interactions::silence)
            .service(interactions::receipt)
            .service(sync::sync)
            .service(stream::stream)
            .service(stream::websocket)
            .service(channels::list_channels)
            .service(channels::create_channel)
            .service(channels::update_channel)
            .service(channels::delete_channel)
            .service(channels::add_member)
            .service(channels::remove_member)
            .service(identity::enrol_device)
            .service(identity::list_devices)
            .service(identity::revoke_device)
            .service(identity::create_key)
            .service(identity::list_keys)
            .service(identity::revoke_key)
            .service(preferences::read_dnd)
            .service(preferences::set_dnd)
            .service(profile::read_profile)
            .service(profile::update_profile)
            .service(profile::set_avatar)
            .service(profile::read_avatar)
            .service(accounts::list_accounts)
            .service(accounts::create_account)
            .service(accounts::deactivate_account),
    );
}

/// Read a principal out of a URL path segment.
///
/// A user id is tried first and it has to be: `usr_01J…` is also a syntactically
/// valid service-account slug, so the looser parse would swallow it.
pub(crate) fn principal_from_reference(raw: &str) -> Result<Principal, DomainError> {
    if let Ok(id) = raw.parse::<UserId>() {
        return Ok(Principal::User { id });
    }
    ServiceAccountId::parse(raw).map(|id| Principal::ServiceAccount { id })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use super::*;

    #[test]
    fn a_user_id_is_read_as_a_user_and_not_as_a_slug() {
        let id = UserId::generate();

        let principal = principal_from_reference(&id.to_string()).unwrap();

        assert_eq!(principal, Principal::User { id });
    }

    #[test]
    fn a_named_reference_is_a_service_account() {
        let principal = principal_from_reference("bot-argocd").unwrap();

        assert!(matches!(principal, Principal::ServiceAccount { .. }));
    }

    #[test]
    fn something_that_is_neither_is_refused_rather_than_guessed_at() {
        assert!(principal_from_reference("not a principal!").is_err());
    }
}
