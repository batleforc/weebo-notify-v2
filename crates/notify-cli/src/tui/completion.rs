//! What the `:` line can suggest, derived from the clap tree (design §9.2).
//!
//! §9.2 asks for "complétion contextuelle depuis l'arbre clap", and the emphasis is
//! on *from the tree*: a list written here would be a second copy of the command
//! surface, and the day the two disagree the wrong one is the one being read.
//! Everything below walks [`crate::Cli::command()`], so a subcommand added to the
//! shell shows up in the TUI without a line of TUI code — the same rule the parser
//! already follows.
//!
//! # Why this is a pure function
//!
//! No terminal, no state, no `App`. Completion is "given this prefix, what could
//! come next", which is a question about a string and a tree, so it is answered in
//! a function a test can call — and the tests are the only place this logic has
//! ever been observed, because nobody in this workspace has a terminal.

use clap::CommandFactory as _;

/// What could follow the line as typed, in the order they should be offered.
///
/// The last word is the one being completed; an empty last word (the line ends in a
/// space) means "everything that can come here". Flags are offered only once the
/// caller has typed a `-`, because a bare prompt full of `--output` helps nobody.
#[must_use]
pub(crate) fn suggestions(line: &str) -> Vec<String> {
    let root = crate::Cli::command();

    // Splitting on spaces rather than with shell rules: completion happens while the
    // line is being typed, so it is frequently *not* valid shell — an unclosed quote
    // is the normal state of a line somebody is in the middle of writing.
    let words: Vec<&str> = line.split(' ').collect();
    let (typed, before) = words.split_last().unwrap_or((&"", &[]));

    // Walk down to the command the words name, ignoring anything that is not one:
    // flags and values are noise for this purpose, and stopping at the first
    // unknown word is what makes `:publish -c ci-infra <tab>` still complete
    // against `publish`.
    let mut current = &root;
    for word in before {
        if word.starts_with('-') || word.is_empty() {
            continue;
        }
        if let Some(next) = current
            .get_subcommands()
            .find(|candidate| candidate.get_name() == *word)
        {
            current = next;
        }
    }

    if typed.starts_with('-') {
        let mut flags: Vec<String> = current
            .get_arguments()
            .filter_map(clap::Arg::get_long)
            .map(|long| format!("--{long}"))
            .filter(|flag| flag.starts_with(typed))
            .collect();
        flags.sort_unstable();
        flags.dedup();
        return flags;
    }

    let mut names: Vec<String> = current
        .get_subcommands()
        // `help` is a subcommand clap adds to every level, and offering it at every
        // level is noise: the TUI has its own `?` popup.
        .filter(|candidate| candidate.get_name() != "help")
        .map(|candidate| candidate.get_name().to_owned())
        .filter(|name| name.starts_with(typed))
        .collect();
    names.sort_unstable();
    names
}

/// The line with the completion applied, or `None` when there is nothing to add.
///
/// One candidate completes it. Several complete as far as they agree — the shell
/// behaviour everybody already has in their fingers, and it never picks for you.
#[must_use]
pub(crate) fn complete(line: &str) -> Option<String> {
    let candidates = suggestions(line);
    let shared = common_prefix(&candidates)?;

    let typed = line.rsplit(' ').next().unwrap_or("");
    if shared.len() <= typed.len() {
        return None;
    }

    let head = &line[..line.len() - typed.len()];
    let completed = format!("{head}{shared}");
    // A single candidate is finished business: add the space that starts the next
    // word, so `:chan<tab>` lands on `:channel ` ready for `create`.
    Some(if candidates.len() == 1 {
        format!("{completed} ")
    } else {
        completed
    })
}

/// The longest prefix every candidate shares, or `None` when there are none.
fn common_prefix(candidates: &[String]) -> Option<String> {
    let first = candidates.first()?;
    let mut length = first.len();

    for candidate in &candidates[1..] {
        length = length.min(
            first
                .chars()
                .zip(candidate.chars())
                .take_while(|(a, b)| a == b)
                .map(|(a, _)| a.len_utf8())
                .sum(),
        );
    }

    Some(first[..length].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_line_offers_the_top_level_commands() {
        let offered = suggestions("");

        assert!(offered.contains(&"publish".to_owned()));
        assert!(offered.contains(&"channel".to_owned()));
        assert!(
            !offered.contains(&"help".to_owned()),
            "the TUI has its own help popup (§9.2)"
        );
    }

    #[test]
    fn a_prefix_narrows_to_what_starts_with_it() {
        assert_eq!(suggestions("pub"), ["publish"]);
    }

    #[test]
    fn a_subcommand_completes_against_its_parent_and_not_the_root() {
        // The "contextuelle" in §9.2: after `channel`, the candidates are its own.
        let offered = suggestions("channel ");

        assert!(offered.contains(&"create".to_owned()));
        assert!(offered.contains(&"set-ttl".to_owned()));
        assert!(offered.contains(&"set-close-after".to_owned()));
        assert!(
            !offered.contains(&"publish".to_owned()),
            "a root command is not a channel subcommand"
        );
    }

    #[test]
    fn flags_are_offered_only_once_a_dash_is_typed() {
        // A prompt that answers a bare tab with twenty flags is a prompt nobody
        // tabs at twice.
        assert!(suggestions("publish ").iter().all(|s| !s.starts_with('-')));

        let flags = suggestions("publish --ch");
        assert_eq!(flags, ["--channel"]);
    }

    #[test]
    fn flags_come_from_the_subcommand_the_line_is_in() {
        let flags = suggestions("device enrol --l");

        assert_eq!(flags, ["--label"], "that flag belongs to `device enrol`");
    }

    #[test]
    fn a_flag_between_the_words_does_not_lose_the_context() {
        // `:update -c ci-infra --st<tab>` is the shape a real line has when
        // somebody wants completion, and walking naively would stop at `-c`.
        let flags = suggestions("update -c ci-infra --sta");

        assert!(flags.contains(&"--state".to_owned()));
        assert!(flags.contains(&"--status".to_owned()));
    }

    #[test]
    fn one_candidate_completes_and_opens_the_next_word() {
        assert_eq!(complete("chan").as_deref(), Some("channel "));
    }

    #[test]
    fn several_candidates_complete_as_far_as_they_agree_and_no_further() {
        // `--state` and `--status` share `--stat`, and picking one for the user is
        // the behaviour a shell has never had.
        assert_eq!(complete("update --st").as_deref(), Some("update --stat"));
    }

    #[test]
    fn nothing_to_add_is_answered_with_nothing() {
        assert_eq!(complete("publish"), None, "already complete");
        assert_eq!(complete("zzz"), None, "no such command");
    }
}
