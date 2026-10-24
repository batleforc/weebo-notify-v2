//! The attachment store over any S3-compatible bucket (design §6.1, §15).
//! Standard operations only (PUT/GET/HEAD/DELETE/list/presign) and path-style addressing,
//! so MinIO, Garage or real S3 is configuration, not code.

use notify_domain::{AttachmentId, limits::MAX_ATTACHMENT_BYTES};
use notify_ports::{
    AttachmentStore, PortError, PortResult, PresignedDownload, PresignedUpload, StoredObject,
    UploadRequest,
};
use s3::creds::Credentials;
use s3::{Bucket, Region};
use time::{Duration, OffsetDateTime};
use url::Url;

/// How long a presigned URL works: short, since it is a bearer credential that leaks into logs.
const PRESIGN_TTL: Duration = Duration::minutes(15);

/// How long a presigned upload URL works for.
///
/// Shorter than a download, because it is a *write* credential: until it expires,
/// whoever holds it could replace the bytes a published notification shows. A
/// publisher PUTs seconds after it asks, so five minutes costs nobody anything.
/// `If-None-Match: *` (see `presign_upload`) closes the rest of that window on
/// stores that honour it.
const UPLOAD_TTL: Duration = Duration::minutes(5);

/// The ceiling on one request to the store, connect to last byte.
///
/// Explicit rather than the client's 60 s default, which is what a stalled store
/// costs every caller waiting on it — the purge job above all, which runs the
/// deletes one after another. 30 s still moves a 25 MiB object (§8.2) over a link
/// slower than 1 MB/s.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Retries after the first attempt, for a failure worth retrying (see
/// [`is_transient`]). Two, and short ones: a store that is down stays down, and
/// the callers — a request, a purge pass — have their own retry on a longer clock.
const RETRIES: u32 = 2;

/// The first backoff; doubled each retry, with up to half of it added as jitter.
const BACKOFF: std::time::Duration = std::time::Duration::from_millis(200);

/// What it takes to reach the bucket.
#[derive(Debug, Clone)]
pub struct S3Config {
    /// The endpoint, e.g. `http://localhost:9000`.
    pub endpoint: Url,
    /// The bucket name.
    pub bucket: String,
    /// Region name; required by the signature, meaningless to most self-hosted stores.
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
    /// Build a client without contacting the bucket; reachability is `/readyz`'s job via `health`.
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

        // The client's own retry is turned off: it retries *every* error once
        // after a full second — a `404` from `exists`, a `412` from a refused
        // conditional write — and none of those will answer differently. Ours
        // (`retrying`) only retries what can. Process-wide, and nothing else
        // here uses this client.
        s3::set_retries(0);

        let bucket = Bucket::new(&config.bucket, region, credentials)
            .map_err(|error| PortError::unavailable("attachments", error))?
            .with_path_style()
            .with_request_timeout(REQUEST_TIMEOUT)
            .map_err(|error| PortError::unavailable("attachments", error))?;

        Ok(Self { bucket })
    }

    /// Where an object lives: the bare id (ULIDs already sort by creation time).
    fn key(id: AttachmentId) -> String {
        id.to_string()
    }

    /// Where a derived thumbnail lives: a subpath of the object's key, so the id alone finds it (§2.3).
    fn thumbnail_key(id: AttachmentId) -> String {
        format!("{}/thumb", Self::key(id))
    }

    fn fail(operation: &'static str, error: impl core::fmt::Display) -> PortError {
        tracing::warn!(%error, operation, "the object store refused");
        PortError::unavailable("attachments", StoreFailure(error.to_string()))
    }

    /// What `get` and `get_thumbnail` share: fetch a key, `None` for a missing one.
    async fn get_key(&self, key: String) -> PortResult<Option<StoredObject>> {
        let response = match retrying(|| self.bucket.get_object(&key)).await {
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
            // A store that does not echo the header; PNG is the stored-image default (§2.4).
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
        let expiry = u32::try_from(UPLOAD_TTL.whole_seconds()).unwrap_or(300);

        // Content type and length are signed, so the bucket refuses an upload lying about either (§6.1).
        let mut headers = http::HeaderMap::new();
        headers.insert(
            "content-type",
            request
                .content_type
                .parse()
                .map_err(|error| PortError::corrupt("attachment.content_type", error))?,
        );
        headers.insert(http::header::CONTENT_LENGTH, request.content_length.into());
        // Create, never replace: the URL works once even while it is still valid.
        // Without this its bearer could swap the bytes after the notification was
        // published and its thumbnail derived from the first ones — the guarantee
        // the filesystem store gets from `claim_upload`. Signed, so a client cannot
        // drop it; a store that does not implement conditional writes ignores it
        // and is left with the short expiry alone.
        headers.insert(
            http::header::IF_NONE_MATCH,
            http::HeaderValue::from_static("*"),
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
                ("if-none-match".to_owned(), "*".to_owned()),
            ]
            .into(),
            expires_at: OffsetDateTime::now_utc().saturating_add(UPLOAD_TTL),
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
            expires_at: OffsetDateTime::now_utc().saturating_add(PRESIGN_TTL),
        })
    }

    async fn exists(&self, id: AttachmentId) -> PortResult<bool> {
        match retrying(|| self.bucket.head_object(Self::key(id))).await {
            Ok((_, 200..=299)) => Ok(true),
            Ok((_, 404)) => Ok(false),
            Ok((_, status)) => Err(Self::fail("head_object", format!("status {status}"))),
            Err(error) if is_missing(&error) => Ok(false),
            Err(error) => Err(Self::fail("head_object", error)),
        }
    }

    async fn delete(&self, id: AttachmentId) -> PortResult<()> {
        match retrying(|| self.bucket.delete_object(Self::key(id))).await {
            // Already gone is the desired state: the purge job (§7.2bis) retries.
            Ok(_) => {}
            Err(error) if is_missing(&error) => {}
            Err(error) => return Err(Self::fail("delete_object", error)),
        }

        // Most attachments have no thumbnail, so "already gone" is the common case.
        match retrying(|| self.bucket.delete_object(Self::thumbnail_key(id))).await {
            Ok(_) => Ok(()),
            Err(error) if is_missing(&error) => Ok(()),
            Err(error) => Err(Self::fail("delete_object", error)),
        }
    }

    async fn put(&self, id: AttachmentId, content_type: &str, bytes: Vec<u8>) -> PortResult<()> {
        retrying(|| {
            self.bucket
                .put_object_with_content_type(Self::key(id), &bytes, content_type)
        })
        .await
        .map_err(|error| Self::fail("put_object", error))?;
        Ok(())
    }

    async fn get(&self, id: AttachmentId) -> PortResult<Option<StoredObject>> {
        // `get` buffers the whole client-written object; the `HEAD` bounds objects the signed length did not.
        let key = Self::key(id);
        match retrying(|| self.bucket.head_object(&key)).await {
            Ok((head, 200..=299)) => {
                let size = head.content_length.unwrap_or(0);
                if u64::try_from(size).unwrap_or(u64::MAX) > MAX_ATTACHMENT_BYTES {
                    return Err(PortError::corrupt(
                        "attachment.size",
                        format!("{size} bytes"),
                    ));
                }
            }
            Ok((_, 404)) => return Ok(None),
            Ok((_, status)) => return Err(Self::fail("head_object", format!("status {status}"))),
            Err(error) if is_missing(&error) => return Ok(None),
            Err(error) => return Err(Self::fail("head_object", error)),
        }
        self.get_key(key).await
    }

    async fn put_thumbnail(
        &self,
        id: AttachmentId,
        content_type: &str,
        bytes: Vec<u8>,
    ) -> PortResult<()> {
        retrying(|| {
            self.bucket
                .put_object_with_content_type(Self::thumbnail_key(id), &bytes, content_type)
        })
        .await
        .map_err(|error| Self::fail("put_object", error))?;
        Ok(())
    }

    async fn get_thumbnail(&self, id: AttachmentId) -> PortResult<Option<StoredObject>> {
        self.get_key(Self::thumbnail_key(id)).await
    }

    async fn health(&self) -> PortResult<()> {
        // One key of one page proves credentials, network and bucket in one standard call.
        self.bucket
            .list_page(String::new(), None, None, None, Some(1))
            .await
            .map(|_| ())
            .map_err(|error| Self::fail("list", error))
    }
}

/// Whether the store says the object is absent: a normal answer for `exists`, `delete` and `get`.
fn is_missing(error: &s3::error::S3Error) -> bool {
    matches!(error, s3::error::S3Error::HttpFailWithBody(404, _))
}

/// Run `attempt`, again up to [`RETRIES`] times while it fails transiently.
/// Every operation here is idempotent; presigning is local and not wrapped.
async fn retrying<T, F, Fut>(mut attempt: F) -> Result<T, s3::error::S3Error>
where
    F: FnMut() -> Fut,
    Fut: core::future::Future<Output = Result<T, s3::error::S3Error>>,
{
    let mut retry = 0;
    loop {
        match attempt().await {
            Err(error) if retry < RETRIES && is_transient(&error) => {
                let delay = backoff(retry);
                tracing::debug!(%error, ?delay, "the object store failed transiently; retrying");
                tokio::time::sleep(delay).await;
                retry = retry.saturating_add(1);
            }
            outcome => return outcome,
        }
    }
}

/// Throttling (`429`, `503 SlowDown`), 5xx, timeouts and dropped connections; never another 4xx.
fn is_transient(error: &s3::error::S3Error) -> bool {
    use s3::error::S3Error;
    match error {
        S3Error::HttpFailWithBody(status, _) => *status == 429 || *status >= 500,
        S3Error::Reqwest(error) => error.is_timeout() || error.is_connect() || error.is_request(),
        S3Error::Io(_) => true,
        _ => false,
    }
}

/// `BACKOFF · 2^retry`, plus up to half that again of clock-nanosecond jitter.
fn backoff(retry: u32) -> std::time::Duration {
    let base = BACKOFF.saturating_mul(1 << retry.min(8));
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    let spread = u32::try_from(base.as_millis() / 2)
        .unwrap_or(u32::MAX)
        .max(1);
    base.saturating_add(std::time::Duration::from_millis(u64::from(
        nanos.checked_rem(spread).unwrap_or(0),
    )))
}

#[cfg(test)]
mod tests {
    use s3::error::S3Error;

    use super::*;

    #[test]
    fn throttling_and_server_errors_are_retried() {
        for status in [429, 500, 502, 503, 504] {
            assert!(is_transient(&S3Error::HttpFailWithBody(
                status,
                String::new()
            )));
        }
    }

    #[test]
    fn an_answer_that_will_not_change_is_not_retried() {
        // 404: missing. 403: a signature refused. 412: a conditional write refused.
        for status in [400, 403, 404, 412] {
            assert!(!is_transient(&S3Error::HttpFailWithBody(
                status,
                String::new()
            )));
        }
    }

    #[test]
    fn the_backoff_grows_and_stays_bounded() {
        for retry in 0..RETRIES {
            let delay = backoff(retry);
            let base = BACKOFF * (1 << retry);
            assert!(delay >= base && delay < base + base / 2 + std::time::Duration::from_millis(1));
        }
    }

    #[tokio::test]
    async fn a_transient_failure_is_retried_until_it_succeeds_and_no_further() {
        let mut calls = 0;
        let outcome = retrying(|| {
            calls += 1;
            let failing = calls <= RETRIES;
            async move {
                if failing {
                    Err(S3Error::HttpFailWithBody(503, "SlowDown".to_owned()))
                } else {
                    Ok(calls)
                }
            }
        })
        .await;
        assert!(matches!(outcome, Ok(3)));

        let mut calls = 0;
        let outcome: Result<(), _> = retrying(|| {
            calls += 1;
            async { Err(S3Error::HttpFailWithBody(404, String::new())) }
        })
        .await;
        assert!(outcome.is_err());
        assert_eq!(calls, 1, "a 404 is asked once");
    }
}

/// The S3 client's error is not always `Send + Sync`, and the message is what matters.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct StoreFailure(String);
