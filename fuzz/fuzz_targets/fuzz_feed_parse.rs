//! A feed body is whatever a third-party host sends (RFC 0002 T3). Parsing returns,
//! never reads past the entry cap, and every entry it keeps has an identity.
#![no_main]

use libfuzzer_sys::fuzz_target;
use notify_feeds_http::{MAX_ENTRIES, parse};

fuzz_target!(|body: &[u8]| {
    let Ok(base) = url::Url::parse("https://example.org/feed.xml") else {
        return;
    };
    let Ok(feed) = parse(body, &base) else {
        return;
    };
    assert!(feed.entries.len() <= MAX_ENTRIES);
    assert!(feed.entries.iter().all(|entry| !entry.id.is_empty()));
});
