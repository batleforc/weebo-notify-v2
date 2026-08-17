//! The attachment store, over anything that speaks S3 (design §6.1, §15).
//!
//! # Standard operations only
//!
//! The contract this project signs up to is "S3-compatible", not "RustFS" (§15).
//! RustFS is the default because it is small and self-hostable, not because we are
//! married to it — so swapping in MinIO, Garage or real S3 has to be configuration
//! rather than code. In practice that means this file uses `PUT`, `GET`, `HEAD`,
//! `DELETE`, a listing and presigning, and nothing else. No lifecycle rules, no
//! object tagging, no bucket policies: an operator can apply those from outside,
//! and a feature only some implementations have is one we cannot depend on.
//!
//! # Path style, always
//!
//! Virtual-hosted addressing (`bucket.host`) needs a wildcard DNS entry and a
//! wildcard certificate. Every self-hosted store supports path style
//! (`host/bucket`), which is what a `docker compose up` can actually serve.

use notify_domain::AttachmentId;
use notify_ports::{
    AttachmentStore, PortError, PortResult, PresignedDownload, PresignedUpload, StoredObject,
    UploadRequest,
};
use s3::creds::Credentials;
use s3::{Bucket, Region};
use time::{Duration, OffsetDateTime};
use url::Url;

/// How long a presigned URL works for.
///
/// Short on purpose: a presigned URL is a bearer credential, and one that lives for
/// a day is one that leaks into an access log for a day (see the port's docs).
const PRESIGN_TTL: Duration = Duration::minutes(15);

/// What it takes to reach the bucket.
#[derive(Debug, Clone)]
pub struct S3Config {
    /// The endpoint, e.g. `http://localhost:9000`.
    pub endpoint: Url,
    /// The bucket name.
    pub bucket: String,
    /// Region name. Meaningless to most self-hosted stores and required by the
    /// signature, so it has a default rather than being asked for.
    pub region: String,
    /// Access key id.
    pub access_key: String,
    /// Secret access key.
    pub secret_key: String,
}

/// An [`AttachmentStore`] over an S3-compatible bucket.
#[derive(Debug, Clone)]
pub struct S3Attachments {
    bucket: Box<Bucket>,
}

impl S3Attachments {
    /// Build a client.
    ///
    /// Nothing is contacted here. Whether the bucket is reachable is what
    /// [`AttachmentStore::health`] answers, and `/readyz` asks it — a replica that
    /// cannot reach the bucket should report itself unready, not refuse to boot and
    /// take down the endpoints that have nothing to do with attachments.
    ///
    /// # Errors
    ///
    /// Fails if the credentials or the region cannot be assembled.
    pub fn connect(config: &S3Config) -> PortResult<Self> {
        let credentials = Credentials::new(
            Some(&config.access_key),
            Some(&config.secret_key),
            None,
            None,
            None,
        )
        .map_err(|error| PortError::unavailable("attachments", error))?;

        let region = Region::Custom {
            region: config.region.clone(),
            endpoint: config.endpoint.as_str().trim_end_matches('/').to_owned(),
        };

        let bucket = Bucket::new(&config.bucket, region, credentials)
            .map_err(|error| PortError::unavailable("attachments", error))?
            .with_path_style();

        Ok(Self { bucket })
    }

    /// Where an object lives in the bucket.
    ///
    /// The id and nothing else. No date prefixes and no sharding: the ids are ULIDs
    /// so they already sort by creation time, and a layout is something operators
    /// end up depending on.
    fn key(id: AttachmentId) -> String {
        id.to_string()
    }

    /// Where a derived thumbnail lives (design §2.3, D30).
    ///
    /// A subpath of the object's own key rather than a second id: ids are ULIDs,
    /// so they contain no `/`, and this keeps a thumbnail findable — and
    /// deletable — from nothing but the id its object was signed under, with no
    /// second table mapping one to the other.
    fn thumbnail_key(id: AttachmentId) -> String {
        format!("{}/thumb", Self::key(id))
    }

    fn fail(operation: &'static str, error: impl core::fmt::Display) -> PortError {
        tracing::warn!(%error, operation, "the object store refused");
        // `Corrupt` carries a message rather than a source, which is what an S3
        // error is by the time it is worth reporting: a status and a body.
        PortError::unavailable("attachments", StoreFailure(error.to_string()))
    }

    /// What `get` and `get_thumbnail` share: fetch a key, `None` for a missing one.
    async fn get_key(&self, key: String) -> PortResult<Option<StoredObject>> {
        let response = match self.bucket.get_object(&key).await {
            Ok(response) => response,
            Err(error) if is_missing(&error) => return Ok(None),
            Err(error) => return Err(Self::fail("get_object", error)),
        };

        if response.status_code() == 404 {
            return Ok(None);
        }

        let content_type = response
            .headers()
            .get("content-type")
            .cloned()
            // The bucket is the only writer and it was told the type on the way in;
            // a missing header is a store that does not echo them rather than a
            // mystery file. PNG is what avatars — and thumbnails — are stored as
            // (§2.4, D30).
            .unwrap_or_else(|| "image/png".to_owned());

        Ok(Some(StoredObject {
            content_type,
            bytes: response.to_vec(),
        }))
    }
}

#[async_trait::async_trait]
impl AttachmentStore for S3Attachments {
    async fn presign_upload(&self, request: &UploadRequest) -> PortResult<PresignedUpload> {
        let id = AttachmentId::generate();
        let expiry = u32::try_from(PRESIGN_TTL.whole_seconds()).unwrap_or(900);

        // The content type rides in the signature rather than in a check afterwards:
        // an upload that lies about it is refused by the bucket, which is the only
        // thing that sees those bytes (§6.1).
        let mut headers = http::HeaderMap::new();
        headers.insert(
            "content-type",
            request
                .content_type
                .parse()
                .map_err(|error| PortError::corrupt("attachment.content_type", error))?,
        );

        let url = self
            .bucket
            .presign_put(Self::key(id), expiry, Some(headers), None)
            .await
            .map_err(|error| Self::fail("presign_put", error))?;

        Ok(PresignedUpload {
            id,
            url: Url::parse(&url).map_err(|error| PortError::corrupt("attachment.url", error))?,
            headers: [
                ("content-type".to_owned(), request.content_type.clone()),
                (
                    "content-length".to_owned(),
                    request.content_length.to_string(),
                ),
            ]
            .into(),
            expires_at: OffsetDateTime::now_utc() + PRESIGN_TTL,
        })
    }

    async fn presign_download(&self, id: AttachmentId) -> PortResult<PresignedDownload> {
        let expiry = u32::try_from(PRESIGN_TTL.whole_seconds()).unwrap_or(900);
        let url = self
            .bucket
            .presign_get(Self::key(id), expiry, None)
            .await
            .map_err(|error| Self::fail("presign_get", error))?;

        Ok(PresignedDownload {
            url: Url::parse(&url).map_err(|error| PortError::corrupt("attachment.url", error))?,
            expires_at: OffsetDateTime::now_utc() + PRESIGN_TTL,
        })
    }

    async fn exists(&self, id: AttachmentId) -> PortResult<bool> {
        // `HEAD`, not `GET`: the question is whether the object is there, and
        // downloading 25 MiB to answer it would be an odd way to find out.
        match self.bucket.head_object(Self::key(id)).await {
            Ok((_, 200..=299)) => Ok(true),
            Ok((_, 404)) => Ok(false),
            Ok((_, status)) => Err(Self::fail("head_object", format!("status {status}"))),
            Err(error) if is_missing(&error) => Ok(false),
            Err(error) => Err(Self::fail("head_object", error)),
        }
    }

    async fn delete(&self, id: AttachmentId) -> PortResult<()> {
        match self.bucket.delete_object(Self::key(id)).await {
            // Deleting something already gone is the desired state rather than an
            // error: the purge job (§7.2bis) retries, and retries have to be free.
            Ok(_) => {}
            Err(error) if is_missing(&error) => {}
            Err(error) => return Err(Self::fail("delete_object", error)),
        }

        // Most attachments never had one — an unsupported format, or a reference
        // purged before anything referenced it — so "already gone" is the common
        // case here too, not an edge one.
        match self.bucket.delete_object(Self::thumbnail_key(id)).await {
            Ok(_) => Ok(()),
            Err(error) if is_missing(&error) => Ok(()),
            Err(error) => Err(Self::fail("delete_object", error)),
        }
    }

    async fn put(&self, id: AttachmentId, content_type: &str, bytes: Vec<u8>) -> PortResult<()> {
        self.bucket
            .put_object_with_content_type(Self::key(id), &bytes, content_type)
            .await
            .map_err(|error| Self::fail("put_object", error))?;
        Ok(())
    }

    async fn get(&self, id: AttachmentId) -> PortResult<Option<StoredObject>> {
        self.get_key(Self::key(id)).await
    }

    async fn put_thumbnail(
        &self,
        id: AttachmentId,
        content_type: &str,
        bytes: Vec<u8>,
    ) -> PortResult<()> {
        self.bucket
            .put_object_with_content_type(Self::thumbnail_key(id), &bytes, content_type)
            .await
            .map_err(|error| Self::fail("put_object", error))?;
        Ok(())
    }

    async fn get_thumbnail(&self, id: AttachmentId) -> PortResult<Option<StoredObject>> {
        self.get_key(Self::thumbnail_key(id)).await
    }

    async fn health(&self) -> PortResult<()> {
        // One page of one key: the cheapest request that proves credentials, network
        // and bucket existence at once, and still a standard operation.
        self.bucket
            .list_page(String::new(), None, None, None, Some(1))
            .await
            .map(|_| ())
            .map_err(|error| Self::fail("list", error))
    }
}

/// Whether the store is telling us the object is not there.
///
/// Its own function because "absent" is a normal answer on three of these calls and
/// an error on none of them: `exists` says false, `delete` is already done, `get`
/// says `None`. Getting that wrong turns a purge into a failing job.
fn is_missing(error: &s3::error::S3Error) -> bool {
    matches!(error, s3::error::S3Error::HttpFailWithBody(404, _))
}

/// An object-store failure, in the shape [`PortError::unavailable`] wants.
///
/// The S3 client's own error type is not `Send + Sync + 'static` in every variant,
/// and what is worth propagating is the message anyway: a status and a body.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct StoreFailure(String);
