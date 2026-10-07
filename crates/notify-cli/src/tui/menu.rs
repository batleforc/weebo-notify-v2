//! A guided menu over `channel`/`dnd`/`device` management (§9.2): hand-labelled, but every
//! leaf is a `:` line run through [`super::app::run_command`] or seeded into the `:` editor.
//! A stack of levels, so "back" is a pop.

/// One entry of the menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MenuItem {
    /// A folder: activating it opens its children.
    Group {
        label: &'static str,
        items: Vec<MenuItem>,
    },
    /// Runs immediately through the `:` line's path (commands needing no argument).
    Run {
        label: &'static str,
        line: &'static str,
    },
    /// Seeds the `:` line, for commands needing arguments (`channel create <id>`).
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

/// Where the menu is: a stack of levels, root first, never empty ([`Menu::back`] won't pop the root).
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
    /// Nothing is highlighted (an empty level; keeps `activate` total).
    Nothing,
}

impl Menu {
    /// Open at the root, mirroring `manage.rs`'s channel/dnd/device subcommands.
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
                    MenuItem::Prefill {
                        label: "Clôture automatique…",
                        line: "channel set-close-after ",
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

    /// Move the cursor within the current level, clamped (never wraps).
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

    /// Pop one level; `false` when only the root was left (the caller closes the menu).
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
            [
                "Lister",
                "Créer…",
                "Rejoindre…",
                "Politique de rétention…",
                "Clôture automatique…"
            ]
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
        assert_eq!(menu.items().len(), 5, "a Run item does not push a level");
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
        // Drift guard: every hand-written line must still parse as a real subcommand.
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
