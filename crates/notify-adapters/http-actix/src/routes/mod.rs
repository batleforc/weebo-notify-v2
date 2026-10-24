//! The route table, and the small conversions the path segments need.

pub(crate) mod accounts;
pub(crate) mod admin;
pub(crate) mod attachments;
pub(crate) mod attachments_fs;
pub(crate) mod channels;
pub(crate) mod cli;
pub(crate) mod compat;
pub(crate) mod connections;
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

/// `nosniff` + `Content-Disposition: attachment` for routes serving stored bytes, so a
/// hostile upload downloads instead of rendering (defence in depth; types are allowlisted).
pub(crate) fn served_bytes(
    builder: &mut actix_web::HttpResponseBuilder,
) -> &mut actix_web::HttpResponseBuilder {
    builder
        .insert_header(("x-content-type-options", "nosniff"))
        .insert_header(("content-disposition", "attachment"))
}

/// Register every route under `/api/v1`; stateless so `dump-openapi` needs no infra (§3.2).
pub fn configure(config: &mut ServiceConfig<'_>) {
    // No scope-wide payload limit: raw-body routes enforce their own ceiling.
    config.service(
        utoipa_actix_web::scope("/api/v1")
            .app_data(crate::problem::json_config())
            .app_data(crate::problem::query_config())
            .app_data(crate::problem::path_config())
            // Defaults: a route that sets its own (`served_bytes`, an avatar's
            // `cache-control`) keeps it. Every response here is either for one
            // authenticated caller or a refusal — nothing a shared cache should
            // keep, nothing a browser should sniff into a document, and no URL
            // (a `?token=` one included) worth sending on as a referrer. HSTS is
            // the ingress's (ADR-0023).
            .wrap(
                actix_web::middleware::DefaultHeaders::new()
                    .add(("x-content-type-options", "nosniff"))
                    .add(("referrer-policy", "no-referrer"))
                    .add(("cache-control", "no-store")),
            )
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
            .service(connections::list_streams)
            .service(connections::disconnect_stream)
            .service(accounts::search_users)
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
            .service(cli::download)
            .service(compat::gotify)
            .service(compat::ntfy)
            .service(compat::ntfy_put)
            .service(compat::ntfy_json)
            .service(compat::alertmanager),
    );
}

/// Read a principal from a path segment; user id first, since `usr_01J…` is also a valid slug.
pub(crate) fn principal_from_reference(raw: &str) -> Result<Principal, DomainError> {
    if let Ok(id) = raw.parse::<UserId>() {
        return Ok(Principal::User { id });
    }
    ServiceAccountId::parse(raw).map(|id| Principal::ServiceAccount { id })
}

#[cfg(test)]
mod tests {
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
