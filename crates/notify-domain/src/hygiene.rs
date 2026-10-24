//! Content hygiene at ingestion (§2.3): strips ANSI escapes, control characters,
//! bidi overrides and zero-width characters (terminal injection, trojan source).
//! HTML is kept: the body is markdown, escaped at render time.

use crate::error::{DomainError, Result};

const ESC: char = '\u{1b}';
const BEL: char = '\u{7}';

/// How much structure a field is allowed to keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextKind {
    /// Titles, labels, tags, keys: newlines and tabs collapse to a single space.
    SingleLine,
    /// Bodies: newlines and tabs survive, because markdown is made of them.
    MultiLine,
}

/// Strip escape sequences and control characters, then normalise whitespace.
/// Infallible; the result may be empty.
#[must_use]
pub fn sanitize(input: &str, kind: TextKind) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            ESC => consume_escape(&mut chars),
            '\r' => {
                // CRLF and lone CR both become a single LF.
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push(if kind == TextKind::MultiLine {
                    '\n'
                } else {
                    ' '
                });
            }
            '\n' | '\t' => out.push(if kind == TextKind::MultiLine { c } else { ' ' }),
            c if is_stripped(c) => {}
            c => out.push(c),
        }
    }

    match kind {
        TextKind::SingleLine => collapse_spaces(&out),
        TextKind::MultiLine => out.trim().to_owned(),
    }
}

/// Sanitise, then enforce presence and length for a named field (`field` is the
/// dotted JSON path reported back).
pub fn required_text(
    raw: &str,
    kind: TextKind,
    max_chars: usize,
    field: &'static str,
) -> Result<String> {
    let value = sanitize(raw, kind);
    if value.is_empty() {
        return Err(DomainError::Empty { field });
    }
    check_length(&value, max_chars, field)?;
    Ok(value)
}

/// Like [`required_text`], but input that sanitises to nothing is `None`.
pub fn optional_text(
    raw: Option<&str>,
    kind: TextKind,
    max_chars: usize,
    field: &'static str,
) -> Result<Option<String>> {
    let Some(raw) = raw else { return Ok(None) };
    let value = sanitize(raw, kind);
    if value.is_empty() {
        return Ok(None);
    }
    check_length(&value, max_chars, field)?;
    Ok(Some(value))
}

fn check_length(value: &str, max_chars: usize, field: &'static str) -> Result<()> {
    let actual = value.chars().count();
    if actual > max_chars {
        return Err(DomainError::TooLong {
            field,
            max: max_chars,
            actual,
        });
    }
    Ok(())
}

/// Consume the rest of an escape sequence after its `ESC`. Unterminated sequences
/// swallow the remaining input so nothing leaks through.
fn consume_escape(chars: &mut core::iter::Peekable<core::str::Chars<'_>>) {
    match chars.next() {
        // CSI — parameters and intermediates, then a final byte in 0x40..=0x7E.
        Some('[') => {
            for c in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&c) {
                    break;
                }
            }
        }
        // OSC and the string-argument families, terminated by BEL or ST (ESC \).
        Some(']' | 'P' | 'X' | '^' | '_') => {
            while let Some(c) = chars.next() {
                match c {
                    BEL => break,
                    ESC => {
                        if chars.peek() == Some(&'\\') {
                            chars.next();
                        }
                        break;
                    }
                    _ => {}
                }
            }
        }
        // Two-character escapes (ESC c, ESC 7, …), or a trailing lone ESC: drop.
        _ => {}
    }
}

/// Characters that never survive, whatever the field.
fn is_stripped(c: char) -> bool {
    c.is_control()
        || matches!(c,
            // Bidirectional overrides and isolates — trojan source.
            '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
            // Zero-width joiners, non-joiners and the BOM.
            | '\u{200b}'..='\u{200d}' | '\u{feff}')
}

/// Trim, and squeeze runs of whitespace into one space.
fn collapse_spaces(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut pending_space = false;
    for c in input.chars() {
        if c.is_whitespace() {
            pending_space = !out.is_empty();
        } else {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_csi_sequences() {
        // Clear screen + colour + reset, the terminal-injection classic.
        let raw = "\u{1b}[2J\u{1b}[31mdeploy failed\u{1b}[0m";
        assert_eq!(sanitize(raw, TextKind::SingleLine), "deploy failed");
    }

    #[test]
    fn strips_osc_window_title_sequence() {
        let raw = "\u{1b}]0;pwned\u{7}etcd degraded";
        assert_eq!(sanitize(raw, TextKind::SingleLine), "etcd degraded");
    }

    #[test]
    fn strips_osc_terminated_by_string_terminator() {
        let raw = "\u{1b}]8;;https://evil.example\u{1b}\\click here";
        assert_eq!(sanitize(raw, TextKind::SingleLine), "click here");
    }

    #[test]
    fn unterminated_escape_swallows_its_tail() {
        // Nothing after an unterminated OSC may leak through.
        assert_eq!(sanitize("ok \u{1b}]0;never", TextKind::SingleLine), "ok");
    }

    #[test]
    fn strips_bidi_and_zero_width_characters() {
        let raw = "safe\u{202e}gnitcefed\u{202c}\u{200b}\u{feff}";
        assert_eq!(sanitize(raw, TextKind::SingleLine), "safegnitcefed");
    }

    #[test]
    fn single_line_collapses_newlines_and_tabs() {
        assert_eq!(
            sanitize("a\nb\r\nc\td", TextKind::SingleLine),
            "a b c d",
            "a title must never break a terminal row"
        );
    }

    #[test]
    fn multi_line_keeps_markdown_structure() {
        assert_eq!(
            sanitize("# title\r\n\r\n- a\n- b", TextKind::MultiLine),
            "# title\n\n- a\n- b"
        );
    }

    #[test]
    fn html_is_left_alone() {
        // Inert by contract, escaped at render time.
        let raw = "<script>alert(1)</script> and 3 < 4";
        assert_eq!(sanitize(raw, TextKind::MultiLine), raw);
    }

    #[test]
    fn length_is_counted_in_characters_after_sanitising() {
        // The escapes must not eat the caller's budget.
        let raw = "\u{1b}[31mabc\u{1b}[0m";
        assert_eq!(
            required_text(raw, TextKind::SingleLine, 3, "t").unwrap(),
            "abc"
        );
        let err = required_text("abcd", TextKind::SingleLine, 3, "t").unwrap_err();
        assert_eq!(err.code(), "field.too_long");
    }

    #[test]
    fn text_made_only_of_escapes_is_empty_not_present() {
        let err =
            required_text("\u{1b}[2J", TextKind::SingleLine, 10, "payload.title").unwrap_err();
        assert_eq!(
            err,
            DomainError::Empty {
                field: "payload.title"
            }
        );
        assert_eq!(
            optional_text(Some("\u{1b}[2J"), TextKind::SingleLine, 10, "payload.icon").unwrap(),
            None
        );
    }
}
