//! Seeded random input against every hand-written parser of untrusted strings
//! (SEC-P2-35): each must return without panicking and round-trip what it accepts.
//! The stable-toolchain floor under the `cargo-fuzz` targets in `fuzz/` (`task fuzz`).

#![expect(
    clippy::arithmetic_side_effects,
    reason = "test arithmetic on small, known values"
)]
#![expect(
    clippy::indexing_slicing,
    reason = "a failing assertion is a failing test, and the panic message names the input that caused it — which is the whole value of a randomised suite"
)]

use notify_domain::{AttachmentId, ChannelId, Cursor, Pattern, PresentedApiKey};

/// A tiny xorshift, so the suite is deterministic and needs no dependency.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, ceiling: usize) -> usize {
        if ceiling == 0 {
            return 0;
        }
        usize::try_from(self.next() % ceiling as u64).unwrap_or(0)
    }

    fn pick<'a, T>(&mut self, from: &'a [T]) -> &'a T {
        &from[self.below(from.len())]
    }
}

/// Fragments on the edges these parsers have: separators, prefixes, whitespace,
/// and things that break naive splitting or integer parsing.
const ALPHABET: &[&str] = &[
    "_",
    ".",
    "-",
    "/",
    ":",
    "*",
    " ",
    "\t",
    "\n",
    "\0",
    "wnk_",
    "ntf_",
    "att_",
    "0",
    "9",
    "a",
    "z",
    "A",
    "Z",
    "é",
    "🙂",
    "\u{202e}",
    "-1",
    "18446744073709551615",
    "999999999999999999999",
    "+",
    "%",
    "\\",
    "'",
    "\"",
    "..",
    "//",
    "..%2f",
];

/// One pseudo-random string, up to `max` fragments long.
fn hostile(rng: &mut Rng, max: usize) -> String {
    let parts = rng.below(max) + 1;
    let mut out = String::new();
    for _ in 0..parts {
        out.push_str(rng.pick(ALPHABET));
    }
    out
}

/// The seed. Change it to explore elsewhere; a failure will name the input.
const SEED: u64 = 0x5EED_C0FF_EE15_600D;
const ROUNDS: usize = 20_000;

#[test]
#[expect(
    clippy::let_underscore_must_use,
    let_underscore_drop,
    reason = "whether each parser returns is under test, not what it returns"
)]
fn no_parser_panics_on_anything() {
    let mut rng = Rng::new(SEED);

    for round in 0..ROUNDS {
        let raw = hostile(&mut rng, 12);

        // Each must return: a panic here is a 500 or a DoS on the credential path.
        let context = || format!("round {round}, input {raw:?}");

        let _ = std::panic::catch_unwind(|| raw.parse::<Cursor>())
            .unwrap_or_else(|_| panic!("Cursor::parse panicked — {}", context()));
        let _ = std::panic::catch_unwind(|| PresentedApiKey::parse(&raw))
            .unwrap_or_else(|_| panic!("PresentedApiKey::parse panicked — {}", context()));
        let _ = std::panic::catch_unwind(|| ChannelId::parse(&raw))
            .unwrap_or_else(|_| panic!("ChannelId::parse panicked — {}", context()));
        let _ = std::panic::catch_unwind(|| raw.parse::<AttachmentId>())
            .unwrap_or_else(|_| panic!("AttachmentId::parse panicked — {}", context()));
        let _ = std::panic::catch_unwind(|| Pattern::parse(&raw))
            .unwrap_or_else(|_| panic!("Pattern::parse panicked — {}", context()));
    }
}

#[test]
#[expect(
    clippy::let_underscore_must_use,
    reason = "whether matching returns is under test, not what it answers"
)]
fn a_pattern_that_parses_never_panics_on_any_candidate() {
    // Both sides get hostile input: the pattern is operator config, the candidate a token claim.
    let mut rng = Rng::new(SEED ^ 0xA5A5);

    for _ in 0..ROUNDS {
        let Ok(pattern) = Pattern::parse(&hostile(&mut rng, 6)) else {
            continue;
        };
        let candidate = hostile(&mut rng, 8);
        let _ = pattern.matches(&candidate);
    }
}

#[test]
fn everything_accepted_renders_back_to_itself() {
    // Anything accepted must render to something accepted again, equal to the first.
    let mut rng = Rng::new(SEED ^ 0x1234);
    let mut accepted = 0_usize;

    for _ in 0..ROUNDS {
        let raw = hostile(&mut rng, 10);

        if let Ok(channel) = ChannelId::parse(&raw) {
            accepted += 1;
            let rendered = channel.to_string();
            let again = ChannelId::parse(&rendered)
                .unwrap_or_else(|_| panic!("ChannelId {rendered:?} does not re-parse"));
            assert_eq!(channel, again, "ChannelId {raw:?} is not stable");
        }

        if let Ok(pattern) = Pattern::parse(&raw) {
            accepted += 1;
            let again = Pattern::parse(pattern.as_str())
                .unwrap_or_else(|_| panic!("Pattern {raw:?} does not re-parse"));
            assert_eq!(pattern.as_str(), again.as_str());
        }

        if let Ok(cursor) = raw.parse::<Cursor>() {
            accepted += 1;
            let rendered = cursor.to_string();
            let again = rendered
                .parse::<Cursor>()
                .unwrap_or_else(|_| panic!("Cursor {rendered:?} does not re-parse"));
            assert_eq!(cursor, again, "Cursor {raw:?} is not stable");
        }
    }

    assert!(
        accepted > 0,
        "no generated input was ever accepted, so this test proved nothing — the \
         alphabet no longer reaches the parsers it is aimed at"
    );
}

#[test]
fn a_presented_key_never_yields_a_prefix_the_domain_would_refuse() {
    // The API key split runs before authentication, so its output must be well formed.
    let mut rng = Rng::new(SEED ^ 0xBEEF);

    for _ in 0..ROUNDS {
        // Aim at the real shape as well as at noise, or almost nothing parses.
        let raw = if rng.below(2) == 0 {
            hostile(&mut rng, 10)
        } else {
            format!(
                "wnk_{}_{}",
                hostile(&mut rng, 3).replace(['_', ' ', '\n', '\t'], "a"),
                hostile(&mut rng, 5)
            )
        };

        if let Ok(presented) = PresentedApiKey::parse(&raw) {
            assert!(
                !presented.secret.is_empty(),
                "accepted {raw:?} with an empty secret"
            );
            assert_eq!(
                presented.prefix.as_str().len(),
                8,
                "accepted {raw:?} with a prefix of the wrong length"
            );
        }
    }
}
