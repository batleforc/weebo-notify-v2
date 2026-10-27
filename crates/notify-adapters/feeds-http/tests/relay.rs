//! RFC 0002 §6.P 3, 5 and 6 against a local relay: conditional requests, refused
//! addresses and the size cap, decompressed bytes included.

#![expect(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "a failing assertion is a failing test, the relay helper included"
)]

use std::io::Write as _;
use std::sync::{Arc, Mutex};

use notify_feeds_http::{HttpFeeds, MAX_BODY};
use notify_ports::{FeedFetcher as _, FeedRequest, FetchError, Fetched};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use url::Url;

const ATOM: &str = r#"<?xml version="1.0"?><feed xmlns="http://www.w3.org/2005/Atom"><title>R</title>
<entry><id>1</id><title>one</title></entry><entry><id>2</id><title>two</title></entry>
<entry><id>3</id><title>three</title></entry></feed>"#;

/// A relay on loopback answering each connection with the next of `responses`.
/// Hands back its port and the request heads it saw.
async fn relay(responses: Vec<Vec<u8>>) -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        for response in responses {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0_u8; 8192];
            let read = socket.read(&mut buffer).await.unwrap();
            log.lock()
                .unwrap()
                .push(String::from_utf8_lossy(&buffer[..read]).into_owned());
            // The client may hang up mid-body once it has seen enough: not an error here.
            let _written = socket.write_all(&response).await;
        }
    });
    (port, seen)
}

fn response(head: &str, body: &[u8]) -> Vec<u8> {
    let mut bytes = format!(
        "HTTP/1.1 {head}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

fn request(url: &str, allow_private_network: bool) -> FeedRequest {
    FeedRequest {
        url: Url::parse(url).unwrap(),
        etag: None,
        last_modified: None,
        allow_private_network,
    }
}

#[tokio::test]
async fn an_unchanged_feed_is_asked_conditionally_and_costs_a_304() {
    let (port, seen) = relay(vec![
        response(
            "200 OK\r\netag: W/\"v1\"\r\nlast-modified: Tue, 27 Oct 2026 10:00:00 GMT",
            ATOM.as_bytes(),
        ),
        response("304 Not Modified", b""),
    ])
    .await;
    let feeds = HttpFeeds::new().unwrap();
    let mut ask = request(&format!("http://127.0.0.1:{port}/feed.xml"), true);

    let Fetched::Feed(feed) = feeds.fetch(&ask).await.unwrap() else {
        panic!("a feed")
    };
    assert_eq!(feed.entries.len(), 3);
    assert_eq!(feed.etag.as_deref(), Some("W/\"v1\""));

    ask.etag = feed.etag;
    ask.last_modified = feed.last_modified;
    assert_eq!(feeds.fetch(&ask).await.unwrap(), Fetched::NotModified);

    let seen = seen.lock().unwrap();
    let second = seen[1].to_ascii_lowercase();
    assert!(second.contains("if-none-match: w/\"v1\""), "{second}");
    assert!(second.contains("if-modified-since: tue, 27 oct 2026 10:00:00 gmt"));
    assert!(seen[0].contains("weebo-notify/"), "the user agent names us");
}

#[tokio::test]
async fn a_validator_carrying_crlf_is_not_sent() {
    let (port, seen) = relay(vec![response("304 Not Modified", b"")]).await;
    let mut ask = request(&format!("http://127.0.0.1:{port}/feed.xml"), true);
    ask.etag = Some("\"v1\"\r\nx-injected: 1".to_owned());

    HttpFeeds::new().unwrap().fetch(&ask).await.unwrap();

    let seen = seen.lock().unwrap();
    assert!(!seen[0].to_ascii_lowercase().contains("x-injected"));
    assert!(!seen[0].to_ascii_lowercase().contains("if-none-match"));
}

#[tokio::test]
async fn a_private_address_is_never_dialled_without_the_opt_in() {
    let (port, seen) = relay(vec![response("200 OK", ATOM.as_bytes())]).await;
    let feeds = HttpFeeds::new().unwrap();

    for url in [
        format!("http://127.0.0.1:{port}/feed.xml?feed_token=secret"),
        format!("http://localhost:{port}/feed.xml?feed_token=secret"),
    ] {
        let outcome = feeds.fetch(&request(&url, false)).await;
        assert_eq!(outcome, Err(FetchError::RefusedAddress), "{url}");
    }

    assert!(seen.lock().unwrap().is_empty(), "the relay was reached");
}

#[tokio::test]
async fn an_error_status_carries_its_retry_after_and_no_url() {
    let (port, _) = relay(vec![
        response("503 Service Unavailable\r\nretry-after: 120", b""),
        response("404 Not Found\r\nretry-after: 120", b""),
    ])
    .await;
    let feeds = HttpFeeds::new().unwrap();
    let ask = request(
        &format!("http://127.0.0.1:{port}/f?feed_token=secret"),
        true,
    );

    assert_eq!(
        feeds.fetch(&ask).await,
        Err(FetchError::Status {
            status: 503,
            retry_after: Some(std::time::Duration::from_secs(120)),
        })
    );
    assert_eq!(
        feeds.fetch(&ask).await,
        Err(FetchError::Status {
            status: 404,
            retry_after: None,
        })
    );
}

#[tokio::test]
async fn an_unreachable_host_is_reported_without_its_url() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let outcome = HttpFeeds::new()
        .unwrap()
        .fetch(&request(
            &format!("http://127.0.0.1:{port}/f?feed_token=secret"),
            true,
        ))
        .await;

    let Err(FetchError::Unreachable(reason)) = outcome else {
        panic!("unreachable, got {outcome:?}")
    };
    assert!(!reason.contains("feed_token"), "{reason}");
}

#[tokio::test]
async fn a_body_past_the_cap_is_refused() {
    let big = vec![b' '; MAX_BODY + 1024 * 1024];
    let mut chunked =
        b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n".to_vec();
    for chunk in big.chunks(64 * 1024) {
        chunked.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
        chunked.extend_from_slice(chunk);
        chunked.extend_from_slice(b"\r\n");
    }
    chunked.extend_from_slice(b"0\r\n\r\n");
    let (port, _) = relay(vec![response("200 OK", &big), chunked]).await;
    let feeds = HttpFeeds::new().unwrap();
    let ask = request(&format!("http://127.0.0.1:{port}/feed.xml"), true);

    assert_eq!(
        feeds.fetch(&ask).await,
        Err(FetchError::TooLarge),
        "content-length"
    );
    assert_eq!(
        feeds.fetch(&ask).await,
        Err(FetchError::TooLarge),
        "counted read"
    );
}

#[tokio::test]
async fn a_gzip_bomb_is_cut_at_the_cap_on_inflated_bytes() {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    let zeros = vec![b' '; 1024 * 1024];
    for _ in 0..50 {
        encoder.write_all(&zeros).unwrap();
    }
    let bomb = encoder.finish().unwrap();
    assert!(
        bomb.len() < MAX_BODY,
        "the compressed body passes the early check"
    );
    let (port, seen) = relay(vec![response("200 OK\r\ncontent-encoding: gzip", &bomb)]).await;

    let outcome = HttpFeeds::new()
        .unwrap()
        .fetch(&request(&format!("http://127.0.0.1:{port}/feed.xml"), true))
        .await;

    assert_eq!(outcome, Err(FetchError::TooLarge));
    assert!(
        seen.lock().unwrap()[0]
            .to_ascii_lowercase()
            .contains("accept-encoding: gzip")
    );
}

/// The one real host the RFC was probed against (§6.P); needs egress.
#[tokio::test]
#[ignore = "reaches github.com"]
async fn the_project_release_feed_parses() {
    let outcome = HttpFeeds::new()
        .unwrap()
        .fetch(&request(
            "https://github.com/batleforc/weebo-notify-v2/releases.atom",
            false,
        ))
        .await
        .unwrap();

    let Fetched::Feed(feed) = outcome else {
        panic!("a feed")
    };
    assert_ne!(feed.entries.len(), 0);
    assert!(feed.etag.is_some());
}
