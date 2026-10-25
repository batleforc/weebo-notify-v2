//! A channel id comes from URLs, bodies and token claims: whatever parses renders back to itself.
#![no_main]

use libfuzzer_sys::fuzz_target;
use notify_domain::ChannelId;

fuzz_target!(|raw: &str| {
    let Ok(channel) = ChannelId::parse(raw) else {
        return;
    };
    let rendered = channel.to_string();
    let again = ChannelId::parse(&rendered);
    assert_eq!(
        again.ok().as_ref(),
        Some(&channel),
        "{raw:?} -> {rendered:?}"
    );
});
