//! A guided menu over `channel`/`dnd`/`device` management (design §9.2).
//!
//! # A discoverability layer, not a second implementation
//!
//! Everything here is a curated, hand-labelled view of `manage.rs`'s
//! subcommands — not generated from the clap tree, because the wording and
//! grouping are worth choosing by hand — but every leaf bottoms out in a `:`
//! line, exactly the string that would otherwise have to be typed and
//! remembered. Running one goes through the same [`super::app::run_command`]
//! the `:` line itself calls; seeding one hands off to the ordinary `:` line
//! editing, so `completion.rs`'s tab completion and `history.rs`'s recall
//! keep working for free. The parity mechanism of §9 is reused, not
//! duplicated — only its *entry point* is made discoverable without the
//! grammar being memorised first.
//!
//! # A stack, because "back" is `pop`
//!
//! [`Menu`] holds a stack of `(items at this level, cursor)`. Drawing is
//! always "whatever's on top", and going back one level is popping it — the
//! same "closed and empty read the same way" simplicity `App::selected_action`
//! already uses, one level deeper.

/// One entry of the menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MenuItem {
    /// A folder: activating it opens its children.
    Group {
        label: &'static str,
        items: Vec<MenuItem>,
    },
    /// Runs immediately, through the same path the `:` line uses — for
    /// commands that need no argument (`channel list`, `dnd status`, `dnd off`).
    Run {
        label: &'static str,
        line: &'static str,
    },
    /// Seeds the `:` line and hands off to it — for commands that need
    /// arguments a menu has no business guessing (`channel create <id>`).
    Prefill {
        label: &'static str,
        line: &'static str,
    },
}

impl MenuItem {
    pub(crate) const fn label(&self) -> &'static str {
        match self {
            Self::Group { label, .. } | Self::Run { label, .. } | Self::Prefill { label, .. } => {
                label
            }
        }
    }
}

/// Where the menu is: a stack of levels, root first. Never empty while the
/// menu itself exists — [`Menu::back`] refuses to pop the last frame, and
/// reports that instead so the caller can close the menu entirely.
#[derive(Debug, Clone)]
pub(crate) struct Menu {
    stack: Vec<(Vec<MenuItem>, usize)>,
}

/// What activating the highlighted item means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MenuOutcome {
    /// A [`MenuItem::Group`] was activated; the menu now shows its children.
    Push,
    /// A [`MenuItem::Run`] line — hand it to `run_command` and close the menu.
    RunLine(String),
    /// A [`MenuItem::Prefill`] line — seed the `:` line and close the menu.
    PrefillLine(String),
    /// Nothing is highlighted (an empty level, which the fixed root content
    /// below never produces, but `activate` stays total rather than panicking
    /// if one ever did).
    Nothing,
}

impl Menu {
    /// Open at the root: Canaux, Ne pas déranger, Appareils — mirroring
    /// `manage.rs`'s `ChannelCommand`/`DndCommand`/`DeviceCommand` exactly.
    pub(crate) fn root() -> Self {
        Self {
            stack: vec![(Self::root_items(), 0)],
        }
    }

    fn root_items() -> Vec<MenuItem> {
        vec![
            MenuItem::Group {
                label: "Canaux",
                items: vec![
                    MenuItem::Run {
                        label: "Lister",
                        line: "channel list",
                    },
                    MenuItem::Prefill {
                        label: "Créer…",
                        line: "channel create ",
                    },
                    MenuItem::Prefill {
                        label: "Rejoindre…",
                        line: "channel join ",
                    },
                    MenuItem::Prefill {
                        label: "Politique de rétention…",
                        line: "channel set-ttl ",
                    },
                ],
            },
            MenuItem::Group {
                label: "Ne pas déranger",
                items: vec![
                    MenuItem::Run {
                        label: "Statut",
                        line: "dnd status",
                    },
                    MenuItem::Prefill {
                        label: "Activer…",
                        line: "dnd on ",
                    },
                    MenuItem::Run {
                        label: "Désactiver",
                        line: "dnd off",
                    },
                    MenuItem::Prefill {
                        label: "Planifier…",
                        line: "dnd schedule ",
                    },
                ],
            },
            MenuItem::Group {
                label: "Appareils",
                items: vec![
                    MenuItem::Run {
                        label: "Lister",
                        line: "device list",
                    },
                    MenuItem::Prefill {
                        label: "Enrôler…",
                        line: "device enrol ",
                    },
                ],
            },
        ]
    }

    /// What the popup draws: the items of whichever level is on top.
    pub(crate) fn items(&self) -> &[MenuItem] {
        self.stack.last().map_or(&[], |(items, _)| items.as_slice())
    }

    pub(crate) fn cursor(&self) -> usize {
        self.stack.last().map_or(0, |(_, cursor)| *cursor)
    }

    /// The highlighted item of the current level, if any.
    pub(crate) fn current(&self) -> Option<&MenuItem> {
        let (items, cursor) = self.stack.last()?;
        items.get(*cursor)
    }

    /// Move the cursor within the current level. Clamped at both ends, same
    /// rule as everywhere else in the TUI — holding the key at the end must
    /// not wrap to the other one.
    pub(crate) fn move_cursor(&mut self, delta: i32) {
        let Some((items, cursor)) = self.stack.last_mut() else {
            return;
        };
        let len = items.len();
        if len == 0 {
            return;
        }
        *cursor = if delta.is_negative() {
            cursor.saturating_sub(delta.unsigned_abs() as usize)
        } else {
            (*cursor + delta.unsigned_abs() as usize).min(len - 1)
        };
    }

    /// Act on the highlighted item.
    pub(crate) fn activate(&mut self) -> MenuOutcome {
        match self.current().cloned() {
            Some(MenuItem::Group { items, .. }) => {
                self.stack.push((items, 0));
                MenuOutcome::Push
            }
            Some(MenuItem::Run { line, .. }) => MenuOutcome::RunLine(line.to_owned()),
            Some(MenuItem::Prefill { line, .. }) => MenuOutcome::PrefillLine(line.to_owned()),
            None => MenuOutcome::Nothing,
        }
    }

    /// Pop one level. `false` means there was only the root left to pop —
    /// the caller closes the menu entirely rather than showing an empty one.
    pub(crate) fn back(&mut self) -> bool {
        if self.stack.len() <= 1 {
            return false;
        }
        self.stack.pop();
        true
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "a failing assertion is a failing test")]

    use super::super::completion;
    use super::*;

    fn leaves(items: &[MenuItem]) -> Vec<&MenuItem> {
        items
            .iter()
            .flat_map(|item| match item {
                MenuItem::Group { items, .. } => leaves(items),
                leaf @ (MenuItem::Run { .. } | MenuItem::Prefill { .. }) => vec![leaf],
            })
            .collect()
    }

    #[test]
    fn the_root_menu_groups_channel_dnd_and_device_management() {
        let menu = Menu::root();
        let labels: Vec<&str> = menu.items().iter().map(MenuItem::label).collect();
        assert_eq!(labels, ["Canaux", "Ne pas déranger", "Appareils"]);
    }

    #[test]
    fn activating_a_group_pushes_its_children_and_back_pops_them() {
        let mut menu = Menu::root();

        assert_eq!(menu.activate(), MenuOutcome::Push);
        let labels: Vec<&str> = menu.items().iter().map(MenuItem::label).collect();
        assert_eq!(
            labels,
            ["Lister", "Créer…", "Rejoindre…", "Politique de rétention…"]
        );

        assert!(menu.back(), "one level to pop before the root");
        assert_eq!(
            menu.items().len(),
            3,
            "back at depth 1 returns to the root groups"
        );

        assert!(
            !menu.back(),
            "back at the root reports the menu should close, rather than showing an empty one"
        );
    }

    #[test]
    fn activating_a_run_item_reports_its_line_without_changing_depth() {
        let mut menu = Menu::root();
        menu.activate(); // into "Canaux"

        assert_eq!(
            menu.activate(), // "Lister", a Run item
            MenuOutcome::RunLine("channel list".to_owned())
        );
        assert_eq!(menu.items().len(), 4, "a Run item does not push a level");
    }

    #[test]
    fn activating_a_prefill_item_reports_its_line() {
        let mut menu = Menu::root();
        menu.activate(); // into "Canaux"
        menu.move_cursor(1); // "Créer…"

        assert_eq!(
            menu.activate(),
            MenuOutcome::PrefillLine("channel create ".to_owned())
        );
    }

    #[test]
    fn moving_the_cursor_stops_at_the_ends_rather_than_wrapping() {
        let mut menu = Menu::root();

        menu.move_cursor(-1);
        assert_eq!(menu.current().map(MenuItem::label), Some("Canaux"));

        menu.move_cursor(10);
        assert_eq!(menu.current().map(MenuItem::label), Some("Appareils"));
    }

    #[test]
    fn every_menu_line_is_a_real_command_the_clap_tree_knows_about() {
        // The drift guard: a hand-curated line that no longer names a real
        // subcommand would fail silently in `run_command`'s own clap parser,
        // with the parser's error as the only symptom. Catching it here, at
        // the source of the wording, is cheaper than finding it by clicking
        // through the menu.
        for item in leaves(&Menu::root_items()) {
            let (MenuItem::Run { line, .. } | MenuItem::Prefill { line, .. }) = item else {
                unreachable!("`leaves` only ever returns Run/Prefill items")
            };

            let mut words = line.split_whitespace();
            let first = words.next().expect("a menu line has at least one word");
            assert!(
                completion::suggestions("").contains(&first.to_owned()),
                "`{first}` (from `{line}`) is not a real top-level subcommand"
            );

            if let Some(second) = words.next() {
                let prefix = format!("{first} ");
                assert!(
                    completion::suggestions(&prefix).contains(&second.to_owned()),
                    "`{second}` (from `{line}`) is not a real subcommand of `{first}`"
                );
            }
        }
    }
}
