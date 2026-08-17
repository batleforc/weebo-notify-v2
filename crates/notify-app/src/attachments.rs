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
use notify_ports::{
    AttachmentRegistry, AttachmentStore, Clock, Failure, PresignedUpload, UploadRequest,
};

use crate::authz::{Authorizer, Caller};

/// What clients may upload (design §2.3 — attachments are images).
///
/// An allowlist rather than a block list, for the reason every allowlist here
/// exists: a type we do not name cannot surprise us. Note what this is *not*
/// about — the server never decodes these bytes (§6.1, "trust the bucket"), so
/// unlike the avatar list (§2.4) this is about what clients can render, not about
/// keeping a decoder away from something hostile.
const ALLOWED: &[&str] = &["image/png", "image/jpeg", "image/webp", "image/gif"];

/// Reduce a `Content-Type` header to the bare type this list is written against.
///
/// `image/png; charset=binary` is the same type wearing decoration, and a client
/// that sends it should not be refused for punctuation. Lowercased because the
/// header is case-insensitive.
///
/// Public because the filesystem backend's own upload route
/// (`notify-http-actix::routes::attachments_fs`) has to answer the *same* question
/// about the *same* header, and two copies of this rule is exactly how a
/// presign-time check and an upload-time check come to disagree — which is the gap
/// the content type was able to walk through before it was bound into the token.
#[must_use]
pub fn normalised_content_type(raw: &str) -> String {
    raw.split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

/// Whether a **normalised** content type is one clients may upload.
///
/// Takes the output of [`normalised_content_type`] rather than a raw header, so
/// there is one place that decides what "the same type" means.
#[must_use]
pub fn is_allowed_attachment_type(declared: &str) -> bool {
    ALLOWED.contains(&declared)
}

/// The two halves of attachment storage, handed round together.
///
/// The bucket holds the bytes; the registry remembers which ids we signed for
/// (design §7.2bis, D25). They are separate ports because they are separate
/// machines — S3 and Postgres — and a grouping type rather than two parameters
/// because **every** caller needs both: signing writes to one and records in the
/// other, and collecting reads one to decide what to delete from the other. A
/// caller holding only the store is a caller that can create objects nothing will
/// ever collect, which is the bug this pair exists to prevent.
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
    /// Wire it.
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
                // The *normalised* type, so the signature pins exactly what was
                // checked. Signing the raw header would let `image/png; x=<junk>`
                // be validated as one thing and imposed as another.
                content_type: declared,
                content_length,
            })
            .await?;

        // After the signature, never before: a row claiming an upload nobody was
        // given a URL for would be a row the sweep deletes an object for that never
        // existed. Recorded at all because this is the *only* trace of the id until
        // somebody publishes with it — and if nobody ever does, it is the only thing
        // that can find the bytes again (design §7.2bis, D25).
        self.attachments
            .registry
            .record(signed.id, channel, self.clock.now())
            .await?;

        Ok(signed)
    }
}

/// `GET /api/v1/attachments/{id}` — the way bytes come back out (design §6.1, §8).
///
/// A **signed URL and not the bytes**, unlike the avatar route next door. That is
/// §6.1's rule rather than a preference: attachments never travel through the
/// server, in either direction, which is what keeps a 25 MiB screenshot off this
/// process's heap. The avatar is the single documented exception, and it is one
/// because the server has to decode and resize it anyway (§2.4).
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
    /// Wire it.
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

    /// Sign a download, if the caller can read something that shows it.
    ///
    /// # Why the refusal is `NotificationGone` and not a scope error
    ///
    /// An attachment id is a bare ULID with no channel in it, so "you may not read
    /// this" and "there is no such object" have to look identical from outside —
    /// otherwise the endpoint answers, one request at a time, which ids exist on
    /// this instance. It is the same reasoning as revoking somebody else's device.
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

        // Checked after the authorisation and before the signature: a signed URL
        // for bytes that never arrived is a broken image a client cannot tell from
        // a broken bucket, and a presign is a promise rather than an upload.
        if !self.attachments.store.exists(id).await? {
            return Err(DomainError::NotificationGone.into());
        }

        Ok(self.attachments.store.presign_download(id).await?)
    }
}

/// `GET /api/v1/attachments/{id}/thumbnail` — the small square a row shows
/// (design §2.3, §10.3, D30).
///
/// Served through the API and not by a presigned URL, for the reason next to
/// [`ReadAttachment::execute`]'s doc for the avatar: a thumbnail renders next to
/// every visible row of a possibly virtualised list, and a signature per render
/// would mean a signing round trip per row.
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
    /// Wire it.
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

    /// The bytes, if the caller may see them.
    ///
    /// # Why one refusal covers three different reasons
    ///
    /// The same rule [`ReadAttachment::execute`] documents, extended by one more
    /// case: a caller who cannot read the notification, an id no notification
    /// shows, and an attachment nothing was ever derived from (an unsupported
    /// format, an upload that never arrived, or one this build has not gotten to
    /// yet) all answer identically. Telling the third apart from the first two
    /// would mean confirming an attachment's *existence* to somebody who cannot
    /// read what shows it — exactly the enumeration `ReadAttachment` refuses.
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
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

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

        assert!(!signed.url.as_str().is_empty());
    }

    #[tokio::test]
    async fn a_signed_upload_is_recorded_so_it_can_be_collected_later() {
        // The only trace of this id until somebody publishes with it. Without the
        // row, an upload nobody ever references is invisible to every query this
        // schema can answer, and the bytes stay for ever (D25).
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
        // The row is written after the signature, so a refusal never leaves one
        // behind — a sweep chasing an object that was never signed for would be
        // harmless but permanently confusing in the counters.
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

        assert!(known.is_empty());
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
        // The read side of §6.1, which had a port method, an S3 implementation and
        // no caller: an attachment could be uploaded and referenced, and no client
        // could ever fetch the bytes.
        let fixture = fixture();
        let reference = notify_domain::AttachmentId::generate();
        shown_in(&fixture.store, &fixture.bucket, &fixture.alice, reference).await;

        let signed = reader_of(&fixture)
            .execute(&Caller::member(fixture.reader.clone()), reference)
            .await
            .unwrap();

        assert!(!signed.url.as_str().is_empty());
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
        // Presigning an upload does not entitle anybody to read it back: what
        // authorises a download is a *notification* somebody may read, so an id that
        // was signed for and never published stays private to whoever uploaded it.
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
        // A presign is a promise, not an upload (the port says so). Signing a URL
        // for bytes that never landed gives a client a broken image it cannot tell
        // from a broken bucket.
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
        // Best-effort derivation (D30) means this is an ordinary state, not a bug:
        // an unsupported format, or a publish/revise that has not run yet.
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
