//! A pattern is operator config, the candidate a token claim: both are hostile. Whatever
//! parses re-parses to the same pattern, and matching any candidate returns.
#![no_main]

use libfuzzer_sys::fuzz_target;
use notify_domain::Pattern;

fuzz_target!(|input: (&str, &str)| {
    let (raw, candidate) = input;
    let Ok(pattern) = Pattern::parse(raw) else {
        return;
    };
    let again = Pattern::parse(pattern.as_str());
    assert_eq!(
        again.as_ref().map(Pattern::as_str).ok(),
        Some(pattern.as_str()),
        "{raw:?} is not stable"
    );
    // Whether it matches is not under test; that it returns is.
    std::hint::black_box(pattern.matches(candidate));
});
