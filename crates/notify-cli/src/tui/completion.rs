//! What the `:` line can suggest, derived from the clap tree (§9.2) so new subcommands
//! complete without TUI code. Pure, so it is testable without a terminal.

use clap::CommandFactory as _;

/// What could follow the line as typed, in order. An empty last word means "anything here";
/// flags are offered only once a `-` is typed.
#[must_use]
pub(crate) fn suggestions(line: &str) -> Vec<String> {
    let root = crate::Cli::command();

    // Split on spaces, not shell rules: a half-typed line often has an unclosed quote.
    let words: Vec<&str> = line.split(' ').collect();
    let (typed, before) = words.split_last().unwrap_or((&"", &[]));

    // Walk the words that name subcommands, skipping flags and values.
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
        // `help` at every level is noise; the TUI has its own `?` popup.
        .filter(|candidate| candidate.get_name() != "help")
        .map(|candidate| candidate.get_name().to_owned())
        .filter(|name| name.starts_with(typed))
        .collect();
    names.sort_unstable();
    names
}

/// The line with the completion applied, or `None` when there is nothing to add. Several
/// candidates complete only as far as they agree, like a shell.
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
    // A single candidate gets the trailing space for the next word.
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
        assert_eq!(complete("update --st").as_deref(), Some("update --stat"));
    }

    #[test]
    fn nothing_to_add_is_answered_with_nothing() {
        assert_eq!(complete("publish"), None, "already complete");
        assert_eq!(complete("zzz"), None, "no such command");
    }
}
