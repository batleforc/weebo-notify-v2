//! A cursor comes back from clients (`?after=`, `Last-Event-ID`): whatever parses renders back.
#![no_main]

use libfuzzer_sys::fuzz_target;
use notify_domain::Cursor;

fuzz_target!(|raw: &str| {
    let Ok(cursor) = raw.parse::<Cursor>() else {
        return;
    };
    let rendered = cursor.to_string();
    assert_eq!(
        rendered.parse::<Cursor>().ok(),
        Some(cursor),
        "{raw:?} -> {rendered:?}"
    );
});
