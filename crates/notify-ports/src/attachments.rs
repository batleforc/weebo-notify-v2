//! Attachments: presigned URLs to an S3-compatible bucket (design §6.1).
//!
//! Uploads go **client → bucket**, never through the server. That keeps a 25 MiB
//! screenshot off our heap and out of our request timeouts, and it means the only
//! media the server ever decodes is an avatar (design §2.4, §15).
//!
//! The bucket is inside the trust boundary and strictly private: presigning is the
//! only way in or out. The constraints baked into a presigned upload —
//! `content-length-range`, an imposed content type, a short expiry — are the whole
//! access control, so they belong in the signature rather than in a check we run
//! afterwards on something already uploaded.
//!
//! Only standard S3 operations, ever. The contract is "S3-compatible", not
//! "RustFS": swapping in MinIO, Garage or real S3 must be configuration, not code
//! (design §15).

use notify_domain::AttachmentId;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use url::Url;

use crate::error::PortResult;

/// What a client asks to upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadRequest {
    /// Declared content type. Imposed in the signature, so an upload that lies
    /// about it is rejected by the bucket rather than by us.
    pub content_type: String,
    /// Declared size, checked against the channel's ceiling before signing and
    /// pinned as a `content-length-range` condition.
    pub content_length: u64,
}

/// A signed upload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresignedUpload {
    /// The id to reference in a payload once the upload succeeds.
    pub id: AttachmentId,
    /// Where to PUT the bytes.
    pub url: Url,
    /// Headers the client must send verbatim — they are covered by the signature.
    pub headers: Vec<(String, String)>,
    /// When the signature stops working. Short: a presigned URL is a bearer
    /// credential, and one that lives for a day is one that leaks into a log for a
    /// day.
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
}

/// A signed download.
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

    /// Sign a download for a caller already authorised to see the notification
    /// that references it.
    async fn presign_download(&self, id: AttachmentId) -> PortResult<PresignedDownload>;

    /// Whether the object actually arrived.
    ///
    /// A presign is a promise, not an upload: a client can be signed and then fail,
    /// leaving a payload referencing bytes that do not exist. Checking beats
    /// rendering a broken image.
    async fn exists(&self, id: AttachmentId) -> PortResult<bool>;

    /// Write bytes the server itself is holding.
    ///
    /// The exception to "uploads never go through the server", and there is exactly
    /// one: an avatar (design §2.4). It has to be decoded, capped, resized and
    /// stripped of its metadata before it is stored, and none of that can happen if
    /// the bytes go straight from a browser to the bucket. Everything else presigns.
    async fn put(&self, id: AttachmentId, content_type: &str, bytes: Vec<u8>) -> PortResult<()>;

    /// Read bytes back, for the same reason.
    ///
    /// An avatar is served *through* the API rather than by a presigned URL: it is
    /// shown next to every revision its owner published, so a signature per render
    /// would mean a signing round trip per row of an inbox.
    async fn get(&self, id: AttachmentId) -> PortResult<Option<StoredObject>>;

    /// Delete an object, when its notification is purged.
    async fn delete(&self, id: AttachmentId) -> PortResult<()>;

    /// Whether the bucket is reachable, for `/readyz` (design §13).
    async fn health(&self) -> PortResult<()>;
}
