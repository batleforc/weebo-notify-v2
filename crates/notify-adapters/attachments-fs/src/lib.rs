//! The attachment store over a local directory (ADR-0022), for deployments with no S3 bucket.
//! `notify-http-actix` serves the presigned PUT/GET itself and calls [`FsAttachmentStore::verify`].
//! Layout: `{root}/{id}/object`, `object.type`, `thumb`, `thumb.type`; `delete` removes `{root}/{id}/`.

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

/// How long a presigned download URL works; matches `notify-attachments-s3`.
const PRESIGN_TTL: Duration = Duration::minutes(15);

/// How long a presigned upload URL works for. Shorter than a download: it is a
/// write credential, and a publisher uses it seconds after asking for it.
const UPLOAD_TTL: Duration = Duration::minutes(5);

/// What it takes to serve out of a directory.
#[derive(Debug, Clone)]
pub struct FsConfig {
    /// Where attachments live. Created if missing.
    pub root: PathBuf,
    /// This server's externally reachable base URL, since it presigns against itself.
    pub public_base_url: Url,
    /// Signs the bearer token in every URL; required, or the directory would be open.
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
    /// Fails if `root` cannot be created.
    pub fn connect(config: &FsConfig) -> PortResult<Self> {
        std::fs::create_dir_all(&config.root)
            .map_err(|error| PortError::unavailable("attachments", error))?;

        Ok(Self {
            root: config.root.clone(),
            public_base_url: config.public_base_url.clone(),
            secret: config.token_secret.clone().into_bytes(),
        })
    }

    /// Check a bearer token minted by `presign_upload`/`presign_download`, for the two
    /// unauthenticated routes; not on [`AttachmentStore`], which stays HTTP-agnostic.
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

    /// Reserve `id` for the one upload its token allows: `false` if already held.
    /// Atomic directory creation, so two concurrent PUTs on one token cannot both land.
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

    /// Write the sidecar, then the bytes, each through [`write_atomically`].
    ///
    /// Bytes last because `exists` asks about the bytes: once it says yes, the
    /// object is whole and its content type is already beside it.
    async fn write(path: PathBuf, content_type: &str, bytes: Vec<u8>) -> PortResult<()> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| Self::fail("create_dir_all", error))?;
        }
        let content_type = content_type.as_bytes().to_vec();
        tokio::task::spawn_blocking(move || {
            write_atomically(&type_path(&path), &content_type)?;
            write_atomically(&path, &bytes)
        })
        .await
        .map_err(|error| Self::fail("write", error))?
        .map_err(|error| Self::fail("write", error))
    }

    /// What `get` and `get_thumbnail` share: read a path, `None` for a missing one.
    ///
    /// Whole into memory, because the port hands back a `Vec<u8>`: right for an
    /// avatar or a thumbnail, and for the thumbnailer, which decodes the bytes
    /// anyway. Bounded in practice: nothing larger than `MAX_ATTACHMENT_BYTES` can be
    /// written here (the upload route caps the body). The download route, which
    /// only forwards bytes, goes through [`Self::open`] instead.
    async fn read(path: PathBuf) -> PortResult<Option<StoredObject>> {
        let bytes = match tokio::fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(Self::fail("read", error)),
        };

        Ok(Some(StoredObject {
            content_type: Self::read_type(&path).await?,
            bytes,
        }))
    }

    async fn read_type(path: &Path) -> PortResult<String> {
        match tokio::fs::read_to_string(type_path(path)).await {
            Ok(value) => Ok(value),
            // A missing sidecar is lost metadata, not a missing object; PNG is the stored-image default.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok("image/png".to_owned())
            }
            Err(error) => Err(Self::fail("read", error)),
        }
    }

    /// Open an attachment for the download route to stream, `None` if it is not
    /// there.
    ///
    /// Inherent rather than on [`AttachmentStore`], for the reason [`Self::verify`]
    /// is: this is the only backend whose bytes go out through our own HTTP
    /// server — S3 answers a presigned GET itself — so a port method would have
    /// one real implementation and a stub in every other.
    ///
    /// Safe against a concurrent `put` or `delete`: both replace or unlink the
    /// path, never write into the file, so an open handle keeps reading the whole
    /// object it opened and `length` stays true for it.
    ///
    /// # Errors
    ///
    /// The filesystem refusing for any reason but "not found".
    pub async fn open(&self, id: AttachmentId) -> PortResult<Option<OpenedObject>> {
        let path = self.object_path(id);
        let file = match tokio::fs::File::open(&path).await {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(Self::fail("open", error)),
        };
        let length = file
            .metadata()
            .await
            .map_err(|error| Self::fail("metadata", error))?
            .len();

        Ok(Some(OpenedObject {
            content_type: Self::read_type(&path).await?,
            length,
            file,
        }))
    }
}

/// An object opened by [`FsAttachments::open`].
#[derive(Debug)]
pub struct OpenedObject {
    /// What it was stored as.
    pub content_type: String,
    /// Its size in bytes, for `content-length`.
    pub length: u64,
    /// The bytes, unread.
    pub file: tokio::fs::File,
}

/// Write `bytes` to a sibling temporary file, flush it to disk, then rename it
/// over `path`.
///
/// A plain write is visible while it is still happening: `exists` would say yes
/// to half an upload, and a crash mid-write would leave a truncated file that
/// every later read serves. A rename within one directory is atomic, so a reader
/// sees the old file or the whole new one and nothing in between.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;

    static UNIQUE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    // Unique per writer, so two derivations of one thumbnail racing each other
    // never share a temporary file; the last rename wins, and both are whole.
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(format!(
        ".tmp-{}-{}",
        std::process::id(),
        UNIQUE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let temporary = PathBuf::from(temporary);

    let written = (|| {
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)
    })();
    if written.is_err() {
        // The write's own error is the one returned; a leftover file is only logged.
        if let Err(error) = std::fs::remove_file(&temporary) {
            tracing::warn!(%error, path = ?temporary, "could not remove a temporary attachment file");
        }
    }
    written?;

    // The rename itself lives in the directory, so the directory is synced too —
    // otherwise a power cut can forget the rename after the data was flushed.
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
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
        let expires_at = OffsetDateTime::now_utc() + UPLOAD_TTL;
        let url = self.url_for(
            Purpose::Upload {
                content_type: &request.content_type,
                content_length: request.content_length,
            },
            id,
            expires_at,
        )?;

        Ok(PresignedUpload {
            id,
            url,
            // Type and length are inside the MAC: a PUT declaring anything else fails to
            // verify, and the route reads no more than the signed length.
            headers: [
                ("content-type".to_owned(), request.content_type.clone()),
                (
                    "content-length".to_owned(),
                    request.content_length.to_string(),
                ),
            ]
            .into(),
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
            // Already gone is the desired state: the purge job (§7.2bis) retries.
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
        // A probe file proves the directory is writable, not merely listable.
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
