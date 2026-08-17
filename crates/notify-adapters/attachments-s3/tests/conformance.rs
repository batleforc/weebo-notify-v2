//! Every operation this adapter issues, against a real S3-compatible store.
//!
//! The point is the same one the persistence suite makes: an object store is a
//! contract with somebody else's implementation, and "it compiles" says nothing
//! about whether RustFS answers a `HEAD` on a missing key with a 404 or with an
//! error. §15 says the contract is *S3-compatible* rather than *RustFS*, and the
//! only way that claim stays true is by running it.
//!
//! Everything lives under `mod e2e` so `task test` skips it and `task test:e2e`
//! runs it. Without `WNK_TEST_S3_ENDPOINT` each case reports that it was skipped
//! rather than failing, so a laptop with no stack still gets a green `task test`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a failing assertion is a failing test"
)]

mod e2e {
    use notify_attachments_s3::{S3Attachments, S3Config};
    use notify_domain::AttachmentId;
    use notify_ports::{AttachmentStore, UploadRequest};
    use url::Url;

    /// The store under test, or `None` when there is not one.
    fn store() -> Option<S3Attachments> {
        let endpoint = std::env::var("WNK_TEST_S3_ENDPOINT").ok()?;

        Some(
            S3Attachments::connect(&S3Config {
                endpoint: Url::parse(&endpoint).expect("WNK_TEST_S3_ENDPOINT is a URL"),
                bucket: std::env::var("WNK_TEST_S3_BUCKET")
                    .unwrap_or_else(|_| "weebo-notify-attachments".to_owned()),
                region: "us-east-1".to_owned(),
                access_key: std::env::var("WNK_TEST_S3_ACCESS_KEY")
                    .unwrap_or_else(|_| "weebo-dev".to_owned()),
                secret_key: std::env::var("WNK_TEST_S3_SECRET_KEY")
                    .unwrap_or_else(|_| "weebo-dev-secret".to_owned()),
            })
            .expect("the client builds"),
        )
    }

    macro_rules! store {
        () => {
            match store() {
                Some(store) => store,
                None => {
                    eprintln!("no WNK_TEST_S3_ENDPOINT — skipping");
                    return;
                }
            }
        };
    }

    #[tokio::test]
    async fn e2e_the_bucket_answers_a_health_check() {
        let store = store!();

        store.health().await.expect("the bucket is reachable");
    }

    #[tokio::test]
    async fn e2e_bytes_written_by_the_server_come_back_unchanged() {
        // The avatar path (§2.4): the one upload that goes through us, so the one
        // that needs `put` and `get` rather than a presigned URL.
        let store = store!();
        let id = AttachmentId::generate();
        let bytes = b"\x89PNG\r\n\x1a\n and then some".to_vec();

        store.put(id, "image/png", bytes.clone()).await.unwrap();

        let stored = store.get(id).await.unwrap().expect("the object is there");
        assert_eq!(stored.bytes, bytes);
        assert_eq!(stored.content_type, "image/png");

        assert!(store.exists(id).await.unwrap());

        store.delete(id).await.unwrap();
        assert!(!store.exists(id).await.unwrap());
        assert!(store.get(id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn e2e_a_missing_object_is_an_answer_rather_than_an_error() {
        // Three calls, one question. Getting this wrong turns the purge job into a
        // job that fails on exactly the objects it has already cleaned up.
        let store = store!();
        let never_written = AttachmentId::generate();

        assert!(!store.exists(never_written).await.unwrap());
        assert!(store.get(never_written).await.unwrap().is_none());
        store
            .delete(never_written)
            .await
            .expect("deleting what is not there is the desired state");
    }

    #[tokio::test]
    async fn e2e_a_presigned_upload_names_the_object_and_expires() {
        let store = store!();

        let signed = store
            .presign_upload(&UploadRequest {
                content_type: "image/png".to_owned(),
                content_length: 1024,
            })
            .await
            .unwrap();

        // Path style, so the bucket is in the path rather than in the hostname —
        // the addressing every self-hosted store can serve without wildcard DNS.
        assert!(
            signed.url.path().contains(&signed.id.to_string()),
            "the signed URL must name the object: {}",
            signed.url
        );
        assert!(
            signed.url.query().unwrap_or_default().contains("X-Amz-"),
            "an unsigned URL would be an open bucket: {}",
            signed.url
        );
        assert!(signed.expires_at > time::OffsetDateTime::now_utc());
    }
}
