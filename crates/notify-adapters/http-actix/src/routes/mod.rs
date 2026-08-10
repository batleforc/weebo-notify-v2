//! The route table, and the small conversions the path segments need.

pub(crate) mod accounts;
pub(crate) mod admin;
pub(crate) mod attachments;
pub(crate) mod attachments_fs;
pub(crate) mod channels;
pub(crate) mod cli;
pub(crate) mod federation;
pub(crate) mod identity;
pub(crate) mod interactions;
pub(crate) mod notifications;
pub(crate) mod preferences;
pub(crate) mod profile;
pub(crate) mod push;
pub(crate) mod stream;
pub(crate) mod sync;

use notify_domain::{DomainError, Principal, ServiceAccountId, UserId};
use utoipa_actix_web::service_config::ServiceConfig;

/// The two headers every route that returns **stored bytes** owes.
///
/// Three routes serve bytes somebody else supplied — the filesystem download, the
/// attachment thumbnail and the avatar — and all three used to echo a stored
/// content type with neither of these. That is the difference between "a stored
/// object of an unexpected type is inert" and "it renders", and it is not a
/// judgement any one of the three should make for itself:
///
/// * **`nosniff`** stops a browser from second-guessing the declared type. The type
///   is allowlisted on the way in, so sniffing could only ever disagree with a
///   value we already checked;
/// * **`Content-Disposition: attachment`** governs *top-level navigation* only —
///   browsers ignore it for subresource loads — so an `<img src>` in the web client
///   is unaffected, while somebody following the raw URL downloads a file instead of
///   rendering a page.
///
/// Defence in depth rather than the control itself: with the content type now bound
/// into the upload signature and re-checked against the allowlist, nothing hostile
/// should reach a store to begin with.
pub(crate) fn served_bytes(
    builder: &mut actix_web::HttpResponseBuilder,
) -> &mut actix_web::HttpResponseBuilder {
    builder
        .insert_header(("x-content-type-options", "nosniff"))
        .insert_header(("content-disposition", "attachment"))
}

/// Register every M1 route under `/api/v1`.
///
/// Takes no state: the composition root supplies that separately, which is what
/// lets `notify-server dump-openapi` build the document without a database, an
/// object store or an identity provider (design §3.2).
pub fn configure(config: &mut ServiceConfig<'_>) {
    // Only `attachments_fs::upload` reads a raw body, and 25 MiB is design §8.2's
    // ceiling for one — everything else on this scope is JSON, governed by its own
    // default limit rather than this one.
    config.app_data(attachments_fs::payload_config());
    config.service(
        utoipa_actix_web::scope("/api/v1")
            .service(notifications::publish)
            .service(notifications::upsert)
            .service(notifications::inbox)
            .service(notifications::channel_inbox)
            .service(notifications::get_one)
            .service(notifications::revisions)
            .service(notifications::delete_one)
            .service(notifications::unread)
            .service(notifications::search)
            .service(interactions::record)
            .service(interactions::list_interactions)
            .service(interactions::silence)
            .service(interactions::receipt)
            .service(interactions::readership)
            .service(sync::sync)
            .service(stream::stream)
            .service(stream::websocket)
            .service(channels::list_channels)
            .service(channels::read_channel)
            .service(channels::create_channel)
            .service(channels::update_channel)
            .service(channels::delete_channel)
            .service(channels::add_member)
            .service(channels::remove_member)
            .service(identity::whoami)
            .service(identity::enrol_device)
            .service(identity::list_devices)
            .service(identity::revoke_device)
            .service(identity::create_key)
            .service(identity::list_keys)
            .service(identity::revoke_key)
            .service(identity::set_key_limit)
            .service(attachments::presign)
            .service(attachments::download)
            .service(attachments::thumbnail)
            .service(attachments_fs::fs_upload)
            .service(attachments_fs::fs_download)
            .service(federation::exchange)
            .service(push::webpush_key)
            .service(preferences::read_dnd)
            .service(preferences::set_dnd)
            .service(profile::read_profile)
            .service(profile::update_profile)
            .service(profile::set_avatar)
            .service(profile::read_avatar)
            .service(accounts::list_accounts)
            .service(accounts::create_account)
            .service(accounts::deactivate_account)
            .service(admin::audit)
            .service(admin::inventory)
            .service(admin::self_join)
            .service(admin::trust_policies)
            .service(admin::reconciliation)
            .service(admin::dlq)
            .service(admin::requeue_dead_letter)
            .service(cli::download),
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
