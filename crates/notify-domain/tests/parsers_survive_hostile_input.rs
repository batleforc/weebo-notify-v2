//! Randomised input against every parser that reads untrusted strings
//! (advisory SEC-P2-35).
//!
//! These five all take a value straight off the wire and turn it into a domain
//! type, and every one is hand-written rather than derived:
//!
//! | Parser | Where the input comes from |
//! | --- | --- |
//! | [`Cursor`] | `?cursor=` and `Last-Event-ID` |
//! | [`PresentedApiKey`] | the `Authorization` header |
//! | [`Pattern`] | trust-policy subjects, matched against a forge's `sub` claim |
//! | [`ChannelId`] | a path segment |
//! | [`AttachmentId`] | a path segment |
//!
//! # Why this is not `cargo-fuzz`
//!
//! It is not as good as coverage-guided fuzzing and does not pretend to be: there
//! is no coverage feedback and no corpus, so it explores far less. What it has
//! instead is that **it runs**. `cargo-fuzz` needs a nightly toolchain this
//! repository does not pin, which would make the targets another artefact that is
//! committed and never executed — the state D7, D12 and D17 are already open for,
//! and the one this review has objected to twice.
//!
//! So this is the floor, not the ceiling. It runs on every `cargo test`, on a fixed
//! seed so a failure reproduces exactly, and it asserts the two properties that
//! actually matter for a parser on a public endpoint:
//!
//! 1. **it returns** — no panic, no unwrap, no arithmetic overflow, no unbounded
//!    allocation, whatever the bytes;
//! 2. **it round-trips** — anything it accepts renders back to something it accepts
//!    again, and to the same value. A parser that accepts what it will not emit is
//!    where two components come to disagree about the same string.
//!
//! When the toolchain gains nightly, add `cargo-fuzz` targets over these same
//! entry points and keep this file: a fast deterministic check that runs in CI is
//! worth having next to a slow thorough one that runs nightly.

#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "a failing assertion is a failing test, and the panic message names the input that caused it — which is the whole value of a randomised suite"
)]

use notify_domain::{AttachmentId, ChannelId, Cursor, Pattern, PresentedApiKey};

/// A tiny xorshift, so the suite is deterministic and needs no dependency.
///
/// A failure that cannot be reproduced is a failure nobody fixes; a fixed seed
/// means the sequence is the same on every machine and in every CI run.
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

/// Bytes chosen to sit on the edges these parsers actually have: their own
/// separators, their own prefixes, whitespace, and things that break naive
/// splitting or integer parsing.
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
fn no_parser_panics_on_anything() {
    let mut rng = Rng::new(SEED);

    for round in 0..ROUNDS {
        let raw = hostile(&mut rng, 12);

        // Each of these must *return*. A panic here is a `500` on a public
        // endpoint at best, and on the credential path it is a denial of service
        // anybody can trigger with a header.
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
fn a_pattern_that_parses_never_panics_on_any_candidate() {
    // `Pattern::matches` is the trust-policy subject check: the pattern comes from
    // an operator, the candidate from a forge's token. Both sides get hostile input
    // here because only one of them is trusted.
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
    // The property that catches a whole class of confusion bug: if a parser accepts
    // a string it would not itself emit, two components can hold "the same" id and
    // disagree. Anything accepted must render to something accepted again, equal to
    // the first.
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
    // The split in `PresentedApiKey::parse` runs before authentication, on the
    // `Authorization` header. Whatever it hands back has to be a value the rest of
    // the system agrees is well formed, or the difference is an authentication path
    // operating on something nothing else would accept.
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
