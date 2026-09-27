//! The attachment store, over a local filesystem directory (ADR-0022).
//!
//! # Why this exists alongside `notify-attachments-s3`
//!
//! Design §6.1 frames the object store as "the bucket" and its upload path as
//! `client → bucket`, never through the server. A self-hoster with no S3-compatible
//! service available — no RustFS, no MinIO, nothing — has no bucket to presign
//! against. This adapter is that deployment's alternative: bytes live as plain
//! files on disk, and the two bytes-carrying operations the S3 adapter never
//! implements (an unauthenticated upload PUT and download GET) are served by
//! `notify-http-actix`'s own routes instead of a third-party bucket. `verify`
//! below is what those routes call to check the bearer token in the query string —
//! the filesystem's answer to an S3 signature.
//!
//! # Layout
//!
//! One directory per attachment, named after its ULID id (never a raw client
//! string — every caller of this trait already holds a parsed [`AttachmentId`], so
//! there is no path-traversal surface here):
//!
//! ```text
//! {root}/{id}/object       # the bytes
//! {root}/{id}/object.type  # its content type, one line
//! {root}/{id}/thumb        # a derived thumbnail, if one exists
//! {root}/{id}/thumb.type   # its content type
//! ```
//!
//! `delete` removes the whole `{root}/{id}/` directory, thumbnail included, in one
//! call — the same "takes its thumbnail with it" contract [`AttachmentStore`]
//! documents for the S3 adapter.

pub mod token;

use std::path::{Path, PathBuf};

use notify_domain::AttachmentId;
use notify_ports::{
    AttachmentStore, PortError, PortResult, PresignedDownload, PresignedUpload, StoredObject,
    UploadRequest,
};
use time::{Duration, OffsetDateTime};
use url::Url;

pub use token::{Purpose, TokenError};

/// How long a presigned URL — S3's or this adapter's own — works for.
///
/// Matches `notify-attachments-s3`'s constant: it is a property of what "presign"
/// means in this project, not of which store answers it.
const PRESIGN_TTL: Duration = Duration::minutes(15);

/// What it takes to serve out of a directory.
#[derive(Debug, Clone)]
pub struct FsConfig {
    /// Where attachments live. Created if missing.
    pub root: PathBuf,
    /// This server's own externally-reachable base URL — the filesystem backend
    /// presigns against itself, so it has to know its own address (design §12.1
    /// covers the same need for the browser-facing URL).
    pub public_base_url: Url,
    /// Signs and verifies the bearer token embedded in every URL this adapter
    /// hands out. Required: an unsigned URL would be an open directory.
    pub token_secret: String,
}

/// An [`AttachmentStore`] over a local filesystem directory.
#[derive(Clone)]
pub struct FsAttachments {
    root: PathBuf,
    public_base_url: Url,
    secret: Vec<u8>,
}

impl core::fmt::Debug for FsAttachments {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // The secret has no business in a log line, even a debug one.
        f.debug_struct("FsAttachments")
            .field("root", &self.root)
            .field("public_base_url", &self.public_base_url.as_str())
            .finish_non_exhaustive()
    }
}

impl FsAttachments {
    /// Build a store, creating `root` if it does not exist yet.
    ///
    /// # Errors
    ///
    /// Fails if `root` cannot be created — wrong permissions, or a parent that does
    /// not exist and cannot be made.
    pub fn connect(config: &FsConfig) -> PortResult<Self> {
        std::fs::create_dir_all(&config.root)
            .map_err(|error| PortError::unavailable("attachments", error))?;

        Ok(Self {
            root: config.root.clone(),
            public_base_url: config.public_base_url.clone(),
            secret: config.token_secret.clone().into_bytes(),
        })
    }

    /// Check a bearer token minted by [`AttachmentStore::presign_upload`] or
    /// [`AttachmentStore::presign_download`], for `notify-http-actix`'s two
    /// unauthenticated routes.
    ///
    /// Deliberately not part of [`AttachmentStore`]: that trait stays HTTP-agnostic,
    /// and this method exists only because *this one adapter* has to serve its own
    /// bytes over HTTP where S3 would not.
    ///
    /// # Errors
    ///
    /// See [`TokenError`].
    pub fn verify(
        &self,
        purpose: Purpose<'_>,
        id: AttachmentId,
        given: &str,
    ) -> Result<(), TokenError> {
        token::verify(&self.secret, purpose, id, given, OffsetDateTime::now_utc())
    }

    /// Reserve `id` for the one upload its token allows: `false` if another upload
    /// already holds it. Atomic — the directory is created or it is not — so two
    /// concurrent PUTs on one token cannot both land.
    ///
    /// # Errors
    ///
    /// The filesystem refusing for any reason but "already there".
    pub async fn claim_upload(&self, id: AttachmentId) -> PortResult<bool> {
        tokio::fs::create_dir_all(&self.root)
            .await
            .map_err(|error| Self::fail("create_dir_all", error))?;
        match tokio::fs::create_dir(self.object_dir(id)).await {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(error) => Err(Self::fail("create_dir", error)),
        }
    }

    fn object_dir(&self, id: AttachmentId) -> PathBuf {
        self.root.join(id.to_string())
    }

    fn object_path(&self, id: AttachmentId) -> PathBuf {
        self.object_dir(id).join("object")
    }

    fn thumb_path(&self, id: AttachmentId) -> PathBuf {
        self.object_dir(id).join("thumb")
    }

    fn url_for(
        &self,
        purpose: Purpose<'_>,
        id: AttachmentId,
        expires_at: OffsetDateTime,
    ) -> PortResult<Url> {
        let token = token::sign(&self.secret, purpose, id, expires_at);
        let segment = match purpose {
            Purpose::Upload { .. } => "upload",
            Purpose::Download => "download",
        };

        let mut url = self
            .public_base_url
            .join(&format!("api/v1/attachments/fs/{id}/{segment}"))
            .map_err(|error| PortError::corrupt("attachment.url", error))?;
        url.query_pairs_mut().append_pair("token", &token);
        Ok(url)
    }

    fn fail(operation: &'static str, error: impl core::fmt::Display) -> PortError {
        tracing::warn!(%error, operation, "the filesystem store refused");
        PortError::unavailable("attachments", StoreFailure(error.to_string()))
    }

    async fn write(path: PathBuf, content_type: &str, bytes: Vec<u8>) -> PortResult<()> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| Self::fail("create_dir_all", error))?;
        }
        tokio::fs::write(&path, &bytes)
            .await
            .map_err(|error| Self::fail("write", error))?;
        tokio::fs::write(type_path(&path), content_type.as_bytes())
            .await
            .map_err(|error| Self::fail("write", error))?;
        Ok(())
    }

    /// What `get` and `get_thumbnail` share: read a path, `None` for a missing one.
    async fn read(path: PathBuf) -> PortResult<Option<StoredObject>> {
        let bytes = match tokio::fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(Self::fail("read", error)),
        };

        let content_type = match tokio::fs::read_to_string(type_path(&path)).await {
            Ok(value) => value,
            // The bytes are the source of truth; a sidecar that went missing is a
            // store that lost metadata, not an object that never existed. PNG is
            // what avatars and thumbnails are stored as (§2.4, D30) — the same
            // fallback the S3 adapter uses for a bucket that does not echo headers.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => "image/png".to_owned(),
            Err(error) => return Err(Self::fail("read", error)),
        };

        Ok(Some(StoredObject {
            content_type,
            bytes,
        }))
    }
}

fn type_path(object_path: &Path) -> PathBuf {
    let mut sidecar = object_path.as_os_str().to_owned();
    sidecar.push(".type");
    PathBuf::from(sidecar)
}

#[async_trait::async_trait]
impl AttachmentStore for FsAttachments {
    async fn presign_upload(&self, request: &UploadRequest) -> PortResult<PresignedUpload> {
        let id = AttachmentId::generate();
        let expires_at = OffsetDateTime::now_utc() + PRESIGN_TTL;
        let url = self.url_for(
            Purpose::Upload {
                content_type: &request.content_type,
            },
            id,
            expires_at,
        )?;

        Ok(PresignedUpload {
            id,
            url,
            // The client sends this header on the PUT, and it is **inside the
            // signature** rather than merely advertised here: the upload route feeds
            // the header it actually receives through the same MAC, so a PUT
            // declaring anything else fails to verify. That is the same guarantee
            // S3 gets from listing `content-type` among its SigV4 signed headers,
            // and this adapter claimed it for a while before it had it.
            headers: [("content-type".to_owned(), request.content_type.clone())].into(),
            expires_at,
        })
    }

    async fn presign_download(&self, id: AttachmentId) -> PortResult<PresignedDownload> {
        let expires_at = OffsetDateTime::now_utc() + PRESIGN_TTL;
        let url = self.url_for(Purpose::Download, id, expires_at)?;

        Ok(PresignedDownload { url, expires_at })
    }

    async fn exists(&self, id: AttachmentId) -> PortResult<bool> {
        tokio::fs::try_exists(self.object_path(id))
            .await
            .map_err(|error| Self::fail("try_exists", error))
    }

    async fn delete(&self, id: AttachmentId) -> PortResult<()> {
        match tokio::fs::remove_dir_all(self.object_dir(id)).await {
            Ok(()) => Ok(()),
            // Deleting something already gone is the desired state, same as the S3
            // adapter: the purge job (§7.2bis) retries, and retries have to be free.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(Self::fail("remove_dir_all", error)),
        }
    }

    async fn put(&self, id: AttachmentId, content_type: &str, bytes: Vec<u8>) -> PortResult<()> {
        Self::write(self.object_path(id), content_type, bytes).await
    }

    async fn get(&self, id: AttachmentId) -> PortResult<Option<StoredObject>> {
        Self::read(self.object_path(id)).await
    }

    async fn put_thumbnail(
        &self,
        id: AttachmentId,
        content_type: &str,
        bytes: Vec<u8>,
    ) -> PortResult<()> {
        Self::write(self.thumb_path(id), content_type, bytes).await
    }

    async fn get_thumbnail(&self, id: AttachmentId) -> PortResult<Option<StoredObject>> {
        Self::read(self.thumb_path(id)).await
    }

    async fn health(&self) -> PortResult<()> {
        // Write and remove a probe file: the cheapest operation that proves the
        // directory exists and is actually writable, not merely listable.
        let probe = self.root.join(".health");
        tokio::fs::write(&probe, b"ok")
            .await
            .map_err(|error| Self::fail("write", error))?;
        tokio::fs::remove_file(&probe)
            .await
            .map_err(|error| Self::fail("remove_file", error))
    }
}

/// A filesystem failure, in the shape [`PortError::unavailable`] wants.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct StoreFailure(String);
