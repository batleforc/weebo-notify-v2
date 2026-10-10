//! Every operation this adapter promises, against a real directory; needs no external service.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a failing assertion is a failing test"
)]

use notify_attachments_fs::{FsAttachments, FsConfig, Purpose};
use notify_domain::AttachmentId;
use notify_ports::{AttachmentStore, UploadRequest};
use url::Url;

/// The store under test plus its temp directory, which must outlive it.
fn store() -> (FsAttachments, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("a temp directory");
    let store = FsAttachments::connect(&FsConfig {
        root: dir.path().to_path_buf(),
        public_base_url: Url::parse("http://localhost:8080/").expect("a URL"),
        token_secret: "test-secret".to_owned(),
    })
    .expect("the store builds");
    (store, dir)
}

#[tokio::test]
async fn the_directory_answers_a_health_check() {
    let (store, _dir) = store();

    store.health().await.expect("the directory is writable");
}

#[tokio::test]
async fn bytes_written_by_the_server_come_back_unchanged() {
    // The avatar path (§2.4) is the one upload through us, so it uses `put`/`get`.
    let (store, _dir) = store();
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
async fn a_write_leaves_only_whole_files_behind() {
    // Written to a temporary sibling and renamed into place, so `exists` never
    // answers yes to half an upload — and nothing temporary is left to collect.
    let (store, dir) = store();
    let id = AttachmentId::generate();
    store.put(id, "image/png", b"first".to_vec()).await.unwrap();
    store
        .put(id, "image/png", b"second".to_vec())
        .await
        .unwrap();

    let mut names: Vec<String> = std::fs::read_dir(dir.path().join(id.to_string()))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, ["object", "object.type"]);
    assert_eq!(store.get(id).await.unwrap().unwrap().bytes, b"second");
}

#[tokio::test]
async fn a_missing_object_is_an_answer_rather_than_an_error() {
    let (store, _dir) = store();
    let never_written = AttachmentId::generate();

    assert!(!store.exists(never_written).await.unwrap());
    assert!(store.get(never_written).await.unwrap().is_none());
    store
        .delete(never_written)
        .await
        .expect("deleting what is not there is the desired state");
}

#[tokio::test]
async fn a_thumbnail_round_trips_independently_and_delete_takes_it_along() {
    let (store, _dir) = store();
    let id = AttachmentId::generate();

    store
        .put(id, "image/png", b"full size".to_vec())
        .await
        .unwrap();
    store
        .put_thumbnail(id, "image/png", b"small".to_vec())
        .await
        .unwrap();

    assert_eq!(store.get(id).await.unwrap().unwrap().bytes, b"full size");
    assert_eq!(
        store.get_thumbnail(id).await.unwrap().unwrap().bytes,
        b"small"
    );

    store.delete(id).await.unwrap();
    assert!(
        store.get_thumbnail(id).await.unwrap().is_none(),
        "an orphaned thumbnail would be the same leak D25 closed for the object"
    );
}

#[tokio::test]
async fn a_presigned_upload_names_the_object_carries_a_verifiable_token_and_expires() {
    let (store, _dir) = store();

    let signed = store
        .presign_upload(&UploadRequest {
            content_type: "image/png".to_owned(),
            content_length: 1024,
        })
        .await
        .unwrap();

    assert!(
        signed.url.path().contains(&signed.id.to_string()),
        "the signed URL must name the object: {}",
        signed.url
    );
    assert_eq!(
        signed.headers.get("content-type"),
        Some(&"image/png".to_owned())
    );
    assert!(signed.expires_at > time::OffsetDateTime::now_utc());

    let token = token_of(&signed.url);
    store
        .verify(
            Purpose::Upload {
                content_type: "image/png",
                content_length: 1024,
            },
            signed.id,
            &token,
        )
        .expect("the token this store minted verifies against itself");

    assert!(
        store
            .verify(
                Purpose::Upload {
                    content_type: "text/html",
                    content_length: 1024,
                },
                signed.id,
                &token
            )
            .is_err(),
        "the content type is signed material: a presign for an image must not \
         authorise storing markup the download route would then serve"
    );
}

#[tokio::test]
async fn a_download_token_does_not_verify_as_an_upload() {
    let (store, _dir) = store();
    let id = AttachmentId::generate();

    let signed = store.presign_download(id).await.unwrap();
    let token = token_of(&signed.url);

    assert!(store.verify(Purpose::Download, id, &token).is_ok());
    assert!(
        store
            .verify(
                Purpose::Upload {
                    content_type: "image/png",
                    content_length: 1024,
                },
                id,
                &token
            )
            .is_err(),
        "a download credential must not double as an upload one"
    );
}

#[tokio::test]
async fn a_token_does_not_verify_for_a_different_id() {
    let (store, _dir) = store();

    let signed = store
        .presign_download(AttachmentId::generate())
        .await
        .unwrap();
    let token = token_of(&signed.url);

    assert!(
        store
            .verify(Purpose::Download, AttachmentId::generate(), &token)
            .is_err()
    );
}

fn token_of(url: &Url) -> String {
    url.query_pairs()
        .find(|(key, _)| key == "token")
        .map(|(_, value)| value.into_owned())
        .expect("a token in the query string")
}

#[tokio::test]
async fn an_upload_claim_is_granted_once_even_under_a_race() {
    let (store, _dir) = store();
    let id = AttachmentId::generate();

    let (first, second) = tokio::join!(store.claim_upload(id), store.claim_upload(id));
    assert!(
        first.unwrap() ^ second.unwrap(),
        "exactly one of two concurrent PUTs may land"
    );

    // Released (a failed body read) — the token gets its one upload back.
    store.delete(id).await.unwrap();
    assert!(store.claim_upload(id).await.unwrap());
}
