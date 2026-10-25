//! An attachment id arrives in download URLs and revision bodies: whatever parses renders back.
#![no_main]

use libfuzzer_sys::fuzz_target;
use notify_domain::AttachmentId;

fuzz_target!(|raw: &str| {
    let Ok(id) = raw.parse::<AttachmentId>() else {
        return;
    };
    let rendered = id.to_string();
    assert_eq!(
        rendered.parse::<AttachmentId>().ok(),
        Some(id),
        "{raw:?} -> {rendered:?}"
    );
});
