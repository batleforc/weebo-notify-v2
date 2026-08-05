//! `POST /api/v1/attachments/presign` — the way bytes reach the bucket
//! (design §6.1, §8).
//!
//! # Why this exists as its own use case rather than as a handler
//!
//! Because the refusals are the feature. §6.1 makes a presigned URL "the only
//! chemin d'accès" to a strictly private bucket, which means **the signature is
//! the access control** — there is no second gate afterwards, because after the
//! signature the client talks to the bucket and not to us. Everything that is
//! going to be checked has to be checked here, before signing.
//!
//! # Who may upload
//!
//! Somebody who could publish the notification the attachment is for, which is why
//! the request names a channel and this authorises `Scope::Publish` on it. The
//! alternative — any authenticated principal — would make a subscribe-only member,
//! or any user of the instance, able to write arbitrary bytes into our object
//! store. That is a storage-filling primitive handed to the widest audience the
//! API has, in exchange for saving one field on a request body.
//!
//! The channel is not otherwise used: an attachment is not scoped to it, and the
//! id it yields can be referenced from anywhere the publisher may publish. Tying
//! storage to a channel would mean deciding what happens to the object when the
//! notification moves or the channel is archived, which §7.2bis does not ask for.

use std::sync::Arc;

use notify_domain::{ChannelId, DomainError, Scope, limits::MAX_ATTACHMENT_BYTES};
use notify_ports::{AttachmentStore, Failure, PresignedUpload, UploadRequest};

use crate::authz::{Authorizer, Caller};

/// What clients may upload (design §2.3 — attachments are images).
///
/// An allowlist rather than a block list, for the reason every allowlist here
/// exists: a type we do not name cannot surprise us. Note what this is *not*
/// about — the server never decodes these bytes (§6.1, "trust the bucket"), so
/// unlike the avatar list (§2.4) this is about what clients can render, not about
/// keeping a decoder away from something hostile.
const ALLOWED: &[&str] = &["image/png", "image/jpeg", "image/webp", "image/gif"];

/// Sign an upload.
#[derive(Clone)]
pub struct PresignAttachment {
    attachments: Arc<dyn AttachmentStore>,
    authorizer: Authorizer,
}

impl core::fmt::Debug for PresignAttachment {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PresignAttachment").finish_non_exhaustive()
    }
}

impl PresignAttachment {
    /// Wire it.
    #[must_use]
    pub fn new(attachments: Arc<dyn AttachmentStore>, authorizer: Authorizer) -> Self {
        Self {
            attachments,
            authorizer,
        }
    }

    /// Authorise, check, then sign — in that order.
    pub async fn execute(
        &self,
        caller: &Caller,
        channel: &ChannelId,
        content_type: &str,
        content_length: u64,
    ) -> Result<PresignedUpload, Failure> {
        // First, because everything after it costs something: a refusal here is a
        // channel lookup, and a refusal after signing is a URL already issued.
        self.authorizer
            .authorize(caller, channel, Scope::Publish)
            .await?;

        if content_length == 0 {
            return Err(DomainError::Malformed {
                field: "attachment.content_length",
                reason: "an upload of nothing is a request nobody meant to make",
            }
            .into());
        }
        if content_length > MAX_ATTACHMENT_BYTES {
            // The comparison is in `u64`; only the *reporting* narrows, and it
            // saturates rather than truncating. A wrapped cast on a 32-bit target
            // would tell a caller their 4 GiB upload was a few bytes — a refusal
            // whose message contradicts itself is worse than no message.
            return Err(DomainError::TooLarge {
                what: "attachment",
                max: usize::try_from(MAX_ATTACHMENT_BYTES).unwrap_or(usize::MAX),
                actual: usize::try_from(content_length).unwrap_or(usize::MAX),
            }
            .into());
        }

        // Compared without parameters: `image/png; charset=binary` is the same type
        // wearing decoration, and a client that sends it should not be refused for
        // punctuation. Lowercased because the header is case-insensitive.
        let declared = content_type
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        if !ALLOWED.contains(&declared.as_str()) {
            return Err(DomainError::Malformed {
                field: "attachment.content_type",
                reason: "attachments are images: png, jpeg, webp or gif (design §2.3)",
            }
            .into());
        }

        Ok(self
            .attachments
            .presign_upload(&UploadRequest {
                // The *normalised* type, so the signature pins exactly what was
                // checked. Signing the raw header would let `image/png; x=<junk>`
                // be validated as one thing and imposed as another.
                content_type: declared,
                content_length,
            })
            .await?)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use notify_domain::{Channel, Membership, Principal, UserId};
    use notify_ports::test_utils::{InMemory, InMemoryAttachments};

    use super::*;

    struct Fixture {
        presign: PresignAttachment,
        alice: Principal,
        reader: Principal,
    }

    fn channel() -> ChannelId {
        ChannelId::parse("ci-infra").unwrap()
    }

    fn fixture() -> Fixture {
        let store = InMemory::new();
        let alice = Principal::User {
            id: UserId::generate(),
        };
        let reader = Principal::User {
            id: UserId::generate(),
        };

        let mut seeded = Channel::dynamic(channel(), None).unwrap();
        seeded.upsert_member(
            Membership::new(alice.clone(), vec![Scope::Publish, Scope::Subscribe]).unwrap(),
        );
        seeded.upsert_member(Membership::new(reader.clone(), vec![Scope::Subscribe]).unwrap());
        store.seed_channel(seeded);

        Fixture {
            presign: PresignAttachment::new(
                Arc::new(InMemoryAttachments::new()),
                Authorizer::new(Arc::new(store)),
            ),
            alice,
            reader,
        }
    }

    #[tokio::test]
    async fn a_publisher_gets_a_signed_upload() {
        let fixture = fixture();

        let signed = fixture
            .presign
            .execute(
                &Caller::member(fixture.alice.clone()),
                &channel(),
                "image/png",
                1_024,
            )
            .await
            .unwrap();

        assert!(!signed.url.as_str().is_empty());
    }

    #[tokio::test]
    async fn reading_a_channel_does_not_entitle_you_to_write_to_the_bucket() {
        // Without this, every member of the instance could fill the object store,
        // which is a storage-filling primitive handed to the widest audience there
        // is (design §6.1).
        let fixture = fixture();

        assert_eq!(
            fixture
                .presign
                .execute(
                    &Caller::member(fixture.reader.clone()),
                    &channel(),
                    "image/png",
                    1_024,
                )
                .await
                .unwrap_err()
                .code(),
            Some("scope.missing")
        );
    }

    #[tokio::test]
    async fn something_too_big_is_refused_before_anything_is_signed() {
        let fixture = fixture();

        let error = fixture
            .presign
            .execute(
                &Caller::member(fixture.alice.clone()),
                &channel(),
                "image/png",
                MAX_ATTACHMENT_BYTES + 1,
            )
            .await
            .unwrap_err();

        assert_eq!(
            error.code(),
            Some("payload.too_large"),
            "a signed URL for an oversized object is one we issued against our own \
             rule and cannot take back"
        );
    }

    #[tokio::test]
    async fn a_type_we_do_not_serve_is_refused() {
        let fixture = fixture();

        assert!(
            fixture
                .presign
                .execute(
                    &Caller::member(fixture.alice.clone()),
                    &channel(),
                    "application/x-executable",
                    1_024,
                )
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_content_type_wearing_parameters_is_still_that_type() {
        let fixture = fixture();

        assert!(
            fixture
                .presign
                .execute(
                    &Caller::member(fixture.alice.clone()),
                    &channel(),
                    "IMAGE/PNG; charset=binary",
                    1_024,
                )
                .await
                .is_ok(),
            "refusing a client for punctuation is a support ticket, not a defence"
        );
    }

    #[tokio::test]
    async fn an_empty_upload_is_refused() {
        let fixture = fixture();

        assert!(
            fixture
                .presign
                .execute(
                    &Caller::member(fixture.alice.clone()),
                    &channel(),
                    "image/png",
                    0,
                )
                .await
                .is_err()
        );
    }
}
