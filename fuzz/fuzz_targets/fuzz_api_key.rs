//! The API key split runs before authentication: an accepted key has an 8-character
//! prefix and a non-empty secret, whatever was presented.
#![no_main]

use libfuzzer_sys::fuzz_target;
use notify_domain::PresentedApiKey;

fuzz_target!(|raw: &str| {
    let Ok(presented) = PresentedApiKey::parse(raw) else {
        return;
    };
    assert!(!presented.secret.is_empty(), "{raw:?} gave an empty secret");
    assert_eq!(
        presented.prefix.as_str().len(),
        8,
        "{raw:?} gave a prefix of the wrong length"
    );
});
