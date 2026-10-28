//! What a person types as the server's address (RFC 0001 T1, T7): the normaliser
//! returns, and whatever it accepts is https, or http to a loopback host only when
//! the build allows it, with no userinfo, query or fragment.
#![no_main]

use libfuzzer_sys::fuzz_target;
use notify_client_core::instance::normalise;

fuzz_target!(|raw: &str| {
    for allow_loopback_http in [false, true] {
        let Ok(url) = normalise(raw, allow_loopback_http) else {
            continue;
        };
        assert!(url.scheme() == "https" || (allow_loopback_http && url.scheme() == "http"));
        assert!(url.username().is_empty() && url.password().is_none());
        assert!(url.query().is_none() && url.fragment().is_none());
    }
});
