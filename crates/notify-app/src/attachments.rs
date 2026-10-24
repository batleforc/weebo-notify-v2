//! Attachments (§6.1, §8): presign uploads, sign downloads, serve thumbnails. The
//! signature is the access control (private bucket), so every check happens before
//! signing; uploading requires `Scope::Publish` on the named channel.

use std::sync::Arc;

use notify_domain::{ChannelId, DomainError, Scope, limits::MAX_ATTACHMENT_BYTES};
use notify_ports::{
    AttachmentRegistry, AttachmentStore, Clock, Failure, PresignedUpload, UploadRequest,
};

use crate::authz::{Authorizer, Caller};

/// What clients may upload (§2.3: images). An allowlist about what clients can
/// render; the server never decodes these bytes (§6.1).
const ALLOWED: &[&str] = &["image/png", "image/jpeg", "image/webp", "image/gif"];

/// Reduce a `Content-Type` header to its lowercase bare type. Public so the
/// filesystem upload route applies the same rule as presign.
#[must_use]
pub fn normalised_content_type(raw: &str) -> String {
    raw.split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

/// Whether a normalised content type (see [`normalised_content_type`]) may be uploaded.
#[must_use]
pub fn is_allowed_attachment_type(declared: &str) -> bool {
    ALLOWED.contains(&declared)
}

/// The bucket (bytes) and the registry of signed ids (§7.2bis), handed round
/// together so no caller can create objects the sweep will never collect.
#[derive(Clone)]
pub struct Attachments {
    /// The bucket.
    pub store: Arc<dyn AttachmentStore>,
    /// What was signed, so it can be swept.
    pub registry: Arc<dyn AttachmentRegistry>,
}

impl core::fmt::Debug for Attachments {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Attachments").finish_non_exhaustive()
    }
}

/// Sign an upload.
#[derive(Clone)]
pub struct PresignAttachment {
    attachments: Attachments,
    authorizer: Authorizer,
    clock: Arc<dyn Clock>,
}

impl core::fmt::Debug for PresignAttachment {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PresignAttachment").finish_non_exhaustive()
    }
}

impl PresignAttachment {
    #[must_use]
    pub fn new(attachments: Attachments, authorizer: Authorizer, clock: Arc<dyn Clock>) -> Self {
        Self {
            attachments,
            authorizer,
            clock,
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
        // First, because everything after it costs something.
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
            // Only the reported size narrows, and it saturates rather than wraps.
            return Err(DomainError::TooLarge {
                what: "attachment",
                max: usize::try_from(MAX_ATTACHMENT_BYTES).unwrap_or(usize::MAX),
                actual: usize::try_from(content_length).unwrap_or(usize::MAX),
            }
            .into());
        }

        let declared = normalised_content_type(content_type);
        if !is_allowed_attachment_type(&declared) {
            return Err(DomainError::Malformed {
                field: "attachment.content_type",
                reason: "attachments are images: png, jpeg, webp or gif (design §2.3)",
            }
            .into());
        }

        let signed = self
            .attachments
            .store
            .presign_upload(&UploadRequest {
                // The normalised type, so the signature pins exactly what was checked.
                content_type: declared,
                content_length,
            })
            .await?;

        // After the signature: the only trace of the id until somebody publishes with it,
        // which lets the sweep find never-referenced bytes (§7.2bis).
        self.attachments
            .registry
            .record(signed.id, channel, self.clock.now())
            .await?;

        Ok(signed)
    }
}

/// `GET /api/v1/attachments/{id}` — a signed URL, never the bytes: attachments never
/// travel through the server (§6.1).
#[derive(Clone)]
pub struct ReadAttachment {
    attachments: Attachments,
    notifications: Arc<dyn notify_ports::NotificationRepository>,
    authorizer: Authorizer,
}

impl core::fmt::Debug for ReadAttachment {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ReadAttachment").finish_non_exhaustive()
    }
}

impl ReadAttachment {
    #[must_use]
    pub fn new(
        attachments: Attachments,
        notifications: Arc<dyn notify_ports::NotificationRepository>,
        authorizer: Authorizer,
    ) -> Self {
        Self {
            attachments,
            notifications,
            authorizer,
        }
    }

    /// Sign a download, if the caller can read something that shows it. Refusals look
    /// like `NotificationGone` so ids cannot be enumerated.
    pub async fn execute(
        &self,
        caller: &Caller,
        id: notify_domain::AttachmentId,
    ) -> Result<notify_ports::PresignedDownload, Failure> {
        let channels = self.authorizer.readable_channels(caller).await?;
        if !self
            .notifications
            .attachment_is_visible(&id, &channels)
            .await?
        {
            return Err(DomainError::NotificationGone.into());
        }

        // A presign is a promise, not an upload: never sign for bytes that never arrived.
        if !self.attachments.store.exists(id).await? {
            return Err(DomainError::NotificationGone.into());
        }

        Ok(self.attachments.store.presign_download(id).await?)
    }
}

/// `GET /api/v1/attachments/{id}/thumbnail` — the small square a row shows (§2.3,
/// §10.3). Served through the API: a signature per row would cost a round trip each.
#[derive(Clone)]
pub struct ReadAttachmentThumbnail {
    attachments: Attachments,
    notifications: Arc<dyn notify_ports::NotificationRepository>,
    authorizer: Authorizer,
}

impl core::fmt::Debug for ReadAttachmentThumbnail {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ReadAttachmentThumbnail")
            .finish_non_exhaustive()
    }
}

impl ReadAttachmentThumbnail {
    #[must_use]
    pub fn new(
        attachments: Attachments,
        notifications: Arc<dyn notify_ports::NotificationRepository>,
        authorizer: Authorizer,
    ) -> Self {
        Self {
            attachments,
            notifications,
            authorizer,
        }
    }

    /// The bytes, if the caller may see them. Unreadable, unreferenced and never-derived
    /// all answer identically, so existence is never confirmed.
    pub async fn execute(
        &self,
        caller: &Caller,
        id: notify_domain::AttachmentId,
    ) -> Result<notify_ports::StoredObject, Failure> {
        let channels = self.authorizer.readable_channels(caller).await?;
        if !self
            .notifications
            .attachment_is_visible(&id, &channels)
            .await?
        {
            return Err(DomainError::NotificationGone.into());
        }

        self.attachments
            .store
            .get_thumbnail(id)
            .await?
            .ok_or_else(|| DomainError::NotificationGone.into())
    }
}

#[cfg(test)]
mod tests {
    use notify_domain::{Channel, Membership, Principal, UserId};
    use notify_ports::test_utils::{InMemory, InMemoryAttachments, TestClock};
    use time::OffsetDateTime;

    use super::*;

    struct Fixture {
        presign: PresignAttachment,
        store: InMemory,
        bucket: InMemoryAttachments,
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

        let bucket = InMemoryAttachments::new();
        Fixture {
            presign: PresignAttachment::new(
                Attachments {
                    store: Arc::new(bucket.clone()),
                    registry: Arc::new(store.clone()),
                },
                Authorizer::new(Arc::new(store.clone())),
                Arc::new(TestClock::new()),
            ),
            store,
            bucket,
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

        assert_ne!(signed.url.as_str(), "");
    }

    #[tokio::test]
    async fn a_signed_upload_is_recorded_so_it_can_be_collected_later() {
        // The only trace of this id until somebody publishes with it.
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

        let known = notify_ports::AttachmentRegistry::unreferenced(
            &fixture.store,
            OffsetDateTime::now_utc() + time::Duration::days(2),
            10,
        )
        .await
        .unwrap();

        assert!(
            known.contains(&signed.id),
            "a signature nobody recorded is an object nobody can find"
        );
    }

    #[tokio::test]
    async fn a_refused_request_records_nothing() {
        // The row is written after the signature, so a refusal leaves none behind.
        let fixture = fixture();

        assert!(
            fixture
                .presign
                .execute(
                    &Caller::member(fixture.reader.clone()),
                    &channel(),
                    "image/png",
                    1_024,
                )
                .await
                .is_err()
        );

        let known = notify_ports::AttachmentRegistry::unreferenced(
            &fixture.store,
            OffsetDateTime::now_utc() + time::Duration::days(2),
            10,
        )
        .await
        .unwrap();

        assert_eq!(known, [] as [notify_domain::AttachmentId; 0]);
    }

    /// Publish a notification showing `reference`, and pretend the upload landed.
    async fn shown_in(
        store: &InMemory,
        bucket: &InMemoryAttachments,
        publisher: &Principal,
        reference: notify_domain::AttachmentId,
    ) {
        bucket.mark_uploaded(reference);
        let (notification, written) = notify_domain::Notification::create(
            notify_domain::NotificationId::generate(),
            channel(),
            None,
            notify_domain::RevisionDraft::simple(notify_domain::ContentDraft {
                image: Some(notify_domain::Image::Attachment { reference }),
                ..notify_domain::ContentDraft::titled("screenshot")
            }),
            notify_domain::Author::anonymous(publisher.clone()),
            OffsetDateTime::now_utc(),
            &notify_domain::ChannelPolicy::default(),
        )
        .unwrap();
        notify_ports::NotificationRepository::insert(store, &notification, &written.revision)
            .await
            .unwrap();
    }

    fn reader_of(fixture: &Fixture) -> ReadAttachment {
        ReadAttachment::new(
            Attachments {
                store: Arc::new(fixture.bucket.clone()),
                registry: Arc::new(fixture.store.clone()),
            },
            Arc::new(fixture.store.clone()),
            Authorizer::new(Arc::new(fixture.store.clone())),
        )
    }

    #[tokio::test]
    async fn a_member_can_sign_a_download_of_what_a_notification_shows() {
        // The read side of §6.1.
        let fixture = fixture();
        let reference = notify_domain::AttachmentId::generate();
        shown_in(&fixture.store, &fixture.bucket, &fixture.alice, reference).await;

        let signed = reader_of(&fixture)
            .execute(&Caller::member(fixture.reader.clone()), reference)
            .await
            .unwrap();

        assert_ne!(signed.url.as_str(), "");
    }

    #[tokio::test]
    async fn somebody_who_cannot_read_the_channel_cannot_sign_its_attachments() {
        let fixture = fixture();
        let reference = notify_domain::AttachmentId::generate();
        shown_in(&fixture.store, &fixture.bucket, &fixture.alice, reference).await;

        let stranger = Caller::member(Principal::User {
            id: UserId::generate(),
        });

        assert_eq!(
            reader_of(&fixture)
                .execute(&stranger, reference)
                .await
                .unwrap_err()
                .code(),
            Some("notification.gone"),
            "the refusal must not distinguish \"not yours\" from \"no such object\", \
             or the endpoint enumerates the bucket one id at a time"
        );
    }

    #[tokio::test]
    async fn an_id_no_notification_shows_is_not_downloadable() {
        // Presigning does not entitle anybody to read back: a readable notification does.
        let fixture = fixture();
        let orphan = notify_domain::AttachmentId::generate();
        fixture.bucket.mark_uploaded(orphan);

        assert_eq!(
            reader_of(&fixture)
                .execute(&Caller::member(fixture.alice.clone()), orphan)
                .await
                .unwrap_err()
                .code(),
            Some("notification.gone")
        );
    }

    #[tokio::test]
    async fn a_reference_whose_upload_never_arrived_is_not_signed_for() {
        // A presign is a promise, not an upload.
        let fixture = fixture();
        let never_uploaded = notify_domain::AttachmentId::generate();
        let (notification, written) = notify_domain::Notification::create(
            notify_domain::NotificationId::generate(),
            channel(),
            None,
            notify_domain::RevisionDraft::simple(notify_domain::ContentDraft {
                image: Some(notify_domain::Image::Attachment {
                    reference: never_uploaded,
                }),
                ..notify_domain::ContentDraft::titled("promised a screenshot")
            }),
            notify_domain::Author::anonymous(fixture.alice.clone()),
            OffsetDateTime::now_utc(),
            &notify_domain::ChannelPolicy::default(),
        )
        .unwrap();
        notify_ports::NotificationRepository::insert(
            &fixture.store,
            &notification,
            &written.revision,
        )
        .await
        .unwrap();

        assert!(
            reader_of(&fixture)
                .execute(&Caller::member(fixture.reader.clone()), never_uploaded)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn reading_a_channel_does_not_entitle_you_to_write_to_the_bucket() {
        // Otherwise every member of the instance could fill the object store (§6.1).
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

    fn thumbnail_reader_of(fixture: &Fixture) -> ReadAttachmentThumbnail {
        ReadAttachmentThumbnail::new(
            Attachments {
                store: Arc::new(fixture.bucket.clone()),
                registry: Arc::new(fixture.store.clone()),
            },
            Arc::new(fixture.store.clone()),
            Authorizer::new(Arc::new(fixture.store.clone())),
        )
    }

    #[tokio::test]
    async fn a_member_can_read_a_thumbnail_derived_for_what_a_notification_shows() {
        let fixture = fixture();
        let reference = notify_domain::AttachmentId::generate();
        shown_in(&fixture.store, &fixture.bucket, &fixture.alice, reference).await;
        fixture
            .bucket
            .put_thumbnail(reference, "image/png", b"thumb-bytes".to_vec())
            .await
            .unwrap();

        let object = thumbnail_reader_of(&fixture)
            .execute(&Caller::member(fixture.reader.clone()), reference)
            .await
            .unwrap();

        assert_eq!(object.bytes, b"thumb-bytes");
    }

    #[tokio::test]
    async fn a_visible_attachment_with_no_derived_thumbnail_is_not_downloadable() {
        // Best-effort derivation makes a missing thumbnail an ordinary state.
        let fixture = fixture();
        let reference = notify_domain::AttachmentId::generate();
        shown_in(&fixture.store, &fixture.bucket, &fixture.alice, reference).await;

        assert_eq!(
            thumbnail_reader_of(&fixture)
                .execute(&Caller::member(fixture.reader.clone()), reference)
                .await
                .unwrap_err()
                .code(),
            Some("notification.gone")
        );
    }

    #[tokio::test]
    async fn somebody_who_cannot_read_the_channel_cannot_read_its_thumbnail_either() {
        let fixture = fixture();
        let reference = notify_domain::AttachmentId::generate();
        shown_in(&fixture.store, &fixture.bucket, &fixture.alice, reference).await;
        fixture
            .bucket
            .put_thumbnail(reference, "image/png", b"thumb-bytes".to_vec())
            .await
            .unwrap();

        let stranger = Caller::member(Principal::User {
            id: UserId::generate(),
        });

        assert_eq!(
            thumbnail_reader_of(&fixture)
                .execute(&stranger, reference)
                .await
                .unwrap_err()
                .code(),
            Some("notification.gone"),
            "the refusal must not distinguish \"not yours\" from \"nothing derived\", \
             or the endpoint enumerates the bucket one id at a time"
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
