//! Attachments: presigned URLs to a private S3-compatible bucket (§6.1, §15).
//! Uploads go client → bucket; the signature's constraints are the access control.

use notify_domain::{AttachmentId, ChannelId};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use url::Url;

use crate::error::PortResult;

/// What a client asks to upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadRequest {
    /// Declared content type, imposed in the signature.
    pub content_type: String,
    /// Declared size, checked against the channel's ceiling and pinned in the signature.
    pub content_length: u64,
}

/// A signed upload.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresignedUpload {
    /// The id to reference in a payload once the upload succeeds.
    pub id: AttachmentId,
    /// Where to PUT the bytes.
    pub url: Url,
    /// Headers the client must send verbatim; they are covered by the signature.
    pub headers: std::collections::BTreeMap<String, String>,
    /// When the signature stops working (short-lived: it is a bearer credential).
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
}

/// A signed download.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresignedDownload {
    /// Where to GET the bytes.
    pub url: Url,
    /// When the signature stops working.
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
}

/// An object read back out of the bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredObject {
    /// What it was stored as.
    pub content_type: String,
    /// The bytes.
    pub bytes: Vec<u8>,
}

/// The object store.
#[async_trait::async_trait]
pub trait AttachmentStore: Send + Sync {
    /// Sign an upload (design §8, `POST /attachments/presign`).
    async fn presign_upload(&self, request: &UploadRequest) -> PortResult<PresignedUpload>;

    /// Sign a download for a caller already authorised to see the referencing notification.
    async fn presign_download(&self, id: AttachmentId) -> PortResult<PresignedDownload>;

    /// Whether the object actually arrived (a presign is not an upload).
    async fn exists(&self, id: AttachmentId) -> PortResult<bool>;

    /// Write bytes the server holds itself; only avatars, which must be processed
    /// before storage (§2.4). Everything else presigns.
    async fn put(&self, id: AttachmentId, content_type: &str, bytes: Vec<u8>) -> PortResult<()>;

    /// Read bytes back; avatars are served through the API, not presigned.
    async fn get(&self, id: AttachmentId) -> PortResult<Option<StoredObject>>;

    /// Write a thumbnail derived server-side from an accepted attachment (§2.3).
    async fn put_thumbnail(
        &self,
        id: AttachmentId,
        content_type: &str,
        bytes: Vec<u8>,
    ) -> PortResult<()>;

    /// Read a derived thumbnail back. `None` if the attachment or thumbnail is
    /// missing or derivation was refused.
    async fn get_thumbnail(&self, id: AttachmentId) -> PortResult<Option<StoredObject>>;

    /// Delete an object and its thumbnail, on purge or when its id was never used (§7.2bis).
    async fn delete(&self, id: AttachmentId) -> PortResult<()>;

    /// Whether the bucket is reachable, for `/readyz` (design §13).
    async fn health(&self) -> PortResult<()>;
}

/// The database's record of what URLs were signed, so orphans can be swept
/// (§7.2bis). A row never means an object exists.
#[async_trait::async_trait]
pub trait AttachmentRegistry: Send + Sync {
    /// Remember that a URL was signed; called after signing so failures leave no row.
    async fn record(
        &self,
        id: AttachmentId,
        channel: &ChannelId,
        at: OffsetDateTime,
    ) -> PortResult<()>;

    /// Whether an upload of `id` was signed for `channel` after `signed_after` and not
    /// forgotten yet: a publish may only name an object its own channel asked to upload,
    /// and not one the orphan sweep is about to collect.
    async fn signed_for(
        &self,
        id: AttachmentId,
        channel: &ChannelId,
        signed_after: OffsetDateTime,
    ) -> PortResult<bool>;

    /// Ids signed before `older_than` that no live revision references. The age
    /// bound protects uploads whose notification is not published yet.
    async fn unreferenced(
        &self,
        older_than: OffsetDateTime,
        limit: u32,
    ) -> PortResult<Vec<AttachmentId>>;

    /// Drop up to `limit` rows whose id some revision now references; returns how many.
    /// Their objects are the purge sweep's concern now.
    async fn forget_referenced(&self, limit: u32) -> PortResult<u64>;

    /// Drop rows for ids whose objects were actually deleted.
    async fn forget(&self, ids: &[AttachmentId]) -> PortResult<u64>;
}
