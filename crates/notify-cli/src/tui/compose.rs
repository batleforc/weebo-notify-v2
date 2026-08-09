//! A guided "compose a notification" form (design §9.2).
//!
//! # Why a form exists at all
//!
//! The only path to `notify publish` from inside the TUI used to be typing
//! `:publish -c … -t … --action type:label:arg …` — a mini-DSL nobody
//! discovers by looking at the screen. This form is a guided front end over
//! exactly the same [`crate::publish::Content`] the CLI's own `publish`
//! subcommand fills in, ending in the same [`crate::publish::publish`] call —
//! a fix to either path is picked up by both surfaces, the same guarantee
//! the `:` line's parity mechanism already gives the rest of the TUI.
//!
//! # Two interaction primitives, not a text-editor widget
//!
//! Every field is reached by moving focus (`j`/`k`, never wrapping, same rule
//! as everywhere else in the TUI) and acted on with one verb, `activate`
//! (`Enter`). What `activate` does depends on the field: it toggles typing
//! mode on a text field, appends a tag/action draft to its list, or — on the
//! `Submit` row, a real field rather than a separate global key — reports
//! that the form should be sent. Fields whose value is one of a fixed set
//! (the channel, the three enum pickers) are **cyclable** instead: `h`/`l`
//! step them on a second axis, so field navigation is never overloaded with
//! changing a value.
//!
//! A full multi-line body editor is out of scope here — `body` is one
//! physical line. Anyone who needs more still has `:publish -b -` (stdin) or
//! the raw `:` line; this form is the fast path for the common case, not a
//! replacement for the grammar underneath it.

use clap::ValueEnum as _;
use notify_client_core::types::Channel;

use crate::publish::{self, PriorityArg, StateArg, StatusArg};

/// One field of the form, in display/focus order. `Submit` is a real row —
/// reaching it with `j`/`k` and activating it is the only way to send the
/// form, rather than a separate global keybinding to remember.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldRef {
    Channel,
    Title,
    Body,
    Tags,
    Actions,
    State,
    Status,
    Priority,
    Submit,
}

const FIELDS: [FieldRef; 9] = [
    FieldRef::Channel,
    FieldRef::Title,
    FieldRef::Body,
    FieldRef::Tags,
    FieldRef::Actions,
    FieldRef::State,
    FieldRef::Status,
    FieldRef::Priority,
    FieldRef::Submit,
];

impl FieldRef {
    const fn label(self) -> &'static str {
        match self {
            Self::Channel => "Canal",
            Self::Title => "Titre",
            Self::Body => "Corps",
            Self::Tags => "Tags",
            Self::Actions => "Actions",
            Self::State => "État",
            Self::Status => "Statut",
            Self::Priority => "Priorité",
            Self::Submit => "Publier",
        }
    }

    /// Whether `h`/`l` change this field's value — the channel and the three
    /// enum pickers, never the free-text ones.
    const fn cyclable(self) -> bool {
        matches!(
            self,
            Self::Channel | Self::State | Self::Status | Self::Priority
        )
    }
}

/// What [`ComposeForm::activate`] means the caller should do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ComposeActivation {
    /// Handled entirely inside the form — an edit toggle, a tag/action
    /// appended. Nothing further for the caller to do.
    Handled,
    /// The typed action DSL was invalid; the draft is left as-is so it can
    /// be corrected rather than retyped.
    Rejected(String),
    /// The `Submit` row was activated: build a [`publish::Content`] and send it.
    Submit,
}

/// One row of the popup: what [`ComposeForm::rows`] hands to `render.rs` so
/// the drawing code stays a pure function of already-computed strings.
pub(crate) struct ComposeRow {
    pub label: &'static str,
    pub value: String,
    pub cyclable: bool,
    pub is_submit: bool,
}

/// The guided form itself. Fields mirror [`publish::Content`] one for one
/// (minus `ttl`/`image`, left to the `:` line — see the module doc), plus the
/// channel picker `Content` has no equivalent for, since the CLI takes that
/// as a separate flag rather than part of the body.
#[derive(Debug, Clone)]
pub(crate) struct ComposeForm {
    focused: usize,
    editing: bool,
    channel: usize,
    /// Snapshotted at construction: the channel list cannot change while a
    /// modal owns the keyboard, so there is nothing to keep in sync.
    channel_count: usize,
    title: String,
    body: String,
    tags: Vec<String>,
    tag_draft: String,
    actions: Vec<String>,
    action_draft: String,
    /// `None` is "unset" — every one of these is optional on `Content`, and
    /// a picker that could not say "leave it alone" would force a choice
    /// the CLI itself never requires.
    state: Option<usize>,
    status: Option<usize>,
    priority: Option<usize>,
}

impl ComposeForm {
    pub(crate) fn new(channel_count: usize, default_channel: usize) -> Self {
        Self {
            focused: 0,
            editing: false,
            channel: default_channel.min(channel_count.saturating_sub(1)),
            channel_count,
            title: String::new(),
            body: String::new(),
            tags: Vec::new(),
            tag_draft: String::new(),
            actions: Vec::new(),
            action_draft: String::new(),
            state: None,
            status: None,
            priority: None,
        }
    }

    pub(crate) fn focused(&self) -> usize {
        self.focused
    }

    pub(crate) fn is_editing(&self) -> bool {
        self.editing
    }

    fn field(&self) -> FieldRef {
        FIELDS[self.focused]
    }

    /// Move focus between fields. Never wraps — holding the key at either
    /// end must not loop back round, the same rule `App::move_selection`
    /// already follows for every other list in the TUI.
    pub(crate) fn move_focus(&mut self, delta: i32) {
        self.focused = if delta.is_negative() {
            self.focused.saturating_sub(delta.unsigned_abs() as usize)
        } else {
            (self.focused + delta.unsigned_abs() as usize).min(FIELDS.len() - 1)
        };
    }

    /// Step a cyclable field's value. A no-op on any other field, so `h`/`l`
    /// need not be gated by the caller on which field happens to be focused.
    pub(crate) fn cycle(&mut self, delta: i32) {
        match self.field() {
            FieldRef::Channel => cycle_index(&mut self.channel, delta, self.channel_count),
            FieldRef::State => {
                cycle_option(&mut self.state, delta, StateArg::value_variants().len());
            }
            FieldRef::Status => {
                cycle_option(&mut self.status, delta, StatusArg::value_variants().len());
            }
            FieldRef::Priority => {
                cycle_option(
                    &mut self.priority,
                    delta,
                    PriorityArg::value_variants().len(),
                );
            }
            FieldRef::Title
            | FieldRef::Body
            | FieldRef::Tags
            | FieldRef::Actions
            | FieldRef::Submit => {}
        }
    }

    /// Act on the focused field.
    pub(crate) fn activate(&mut self) -> ComposeActivation {
        match self.field() {
            FieldRef::Submit => return ComposeActivation::Submit,
            FieldRef::Title | FieldRef::Body => self.editing = !self.editing,
            FieldRef::Tags => {
                if self.editing {
                    let tag = self.tag_draft.trim();
                    if !tag.is_empty() {
                        self.tags.push(tag.to_owned());
                        self.tag_draft.clear();
                    }
                } else {
                    self.editing = true;
                }
            }
            FieldRef::Actions => {
                if self.editing {
                    let candidate = self.action_draft.trim();
                    if !candidate.is_empty() {
                        // Validated inline, with `publish::parse_action` itself — not
                        // a second copy of the `type:label:arg` grammar — so a typo
                        // is caught here rather than at submit time, without the
                        // draft being lost.
                        if let Err(error) = publish::parse_action(candidate) {
                            return ComposeActivation::Rejected(error.to_string());
                        }
                        self.actions.push(candidate.to_owned());
                        self.action_draft.clear();
                    }
                } else {
                    self.editing = true;
                }
            }
            FieldRef::Channel | FieldRef::State | FieldRef::Status | FieldRef::Priority => {}
        }
        ComposeActivation::Handled
    }

    /// A typed character, while editing a text field. Ignored outside edit
    /// mode and on cyclable fields, so the caller need not gate the call.
    pub(crate) fn push_char(&mut self, character: char) {
        if !self.editing {
            return;
        }
        match self.field() {
            FieldRef::Title => self.title.push(character),
            FieldRef::Body => self.body.push(character),
            FieldRef::Tags => self.tag_draft.push(character),
            FieldRef::Actions => self.action_draft.push(character),
            FieldRef::Channel
            | FieldRef::State
            | FieldRef::Status
            | FieldRef::Priority
            | FieldRef::Submit => {}
        }
    }

    pub(crate) fn backspace(&mut self) {
        if !self.editing {
            return;
        }
        match self.field() {
            FieldRef::Title => {
                self.title.pop();
            }
            FieldRef::Body => {
                self.body.pop();
            }
            FieldRef::Tags => {
                self.tag_draft.pop();
            }
            FieldRef::Actions => {
                self.action_draft.pop();
            }
            FieldRef::Channel
            | FieldRef::State
            | FieldRef::Status
            | FieldRef::Priority
            | FieldRef::Submit => {}
        }
    }

    /// Build a [`publish::Content`] from the current fields, or say in French
    /// why not. Pure — no `Session`, exactly like the CLI's own argument
    /// parsing; the one impure step (`--image`) stays inside
    /// `Content::into_revision`, which this does not touch (§9.3).
    pub(crate) fn to_content(
        &self,
        channels: &[Channel],
    ) -> Result<(String, publish::Content), String> {
        let channel = channels
            .get(self.channel)
            .map(|channel| channel.id.as_str().to_owned())
            .ok_or_else(|| "crée ou rejoins un canal avant de publier (m → Canaux)".to_owned())?;

        if self.title.trim().is_empty() {
            return Err("un titre est nécessaire pour publier".to_owned());
        }

        // Re-validated here too, not just on append: cheap, and it catches a
        // change to `parse_action` before it reaches the server rather than
        // after.
        for raw in &self.actions {
            publish::parse_action(raw).map_err(|error| error.to_string())?;
        }

        Ok((
            channel,
            publish::Content {
                title: Some(self.title.clone()),
                body: (!self.body.is_empty()).then(|| self.body.clone()),
                state: self.state.map(|index| StateArg::value_variants()[index]),
                status: self.status.map(|index| StatusArg::value_variants()[index]),
                priority: self
                    .priority
                    .map(|index| PriorityArg::value_variants()[index]),
                ttl: None,
                tags: self.tags.clone(),
                actions: self.actions.clone(),
                image: None,
            },
        ))
    }

    /// What the popup draws — one row per field, values already formatted as
    /// strings so `render.rs` stays a pure function of them.
    pub(crate) fn rows(&self, channels: &[Channel]) -> Vec<ComposeRow> {
        FIELDS
            .iter()
            .map(|field| ComposeRow {
                label: field.label(),
                value: self.value_of(*field, channels),
                cyclable: field.cyclable(),
                is_submit: matches!(field, FieldRef::Submit),
            })
            .collect()
    }

    fn value_of(&self, field: FieldRef, channels: &[Channel]) -> String {
        match field {
            FieldRef::Channel => channels
                .get(self.channel)
                .map_or_else(|| "—".to_owned(), |channel| channel.id.as_str().to_owned()),
            FieldRef::Title => self.title.clone(),
            FieldRef::Body => self.body.clone(),
            FieldRef::Tags => joined(&self.tags, &self.tag_draft),
            FieldRef::Actions => joined(&self.actions, &self.action_draft),
            FieldRef::State => enum_label(self.state, StateArg::value_variants()),
            FieldRef::Status => enum_label(self.status, StatusArg::value_variants()),
            FieldRef::Priority => enum_label(self.priority, PriorityArg::value_variants()),
            FieldRef::Submit => String::new(),
        }
    }
}

/// Accepted entries plus whatever is still being typed, so the draft is
/// visible before it is appended rather than only after.
fn joined(accepted: &[String], draft: &str) -> String {
    let mut value = accepted.join(", ");
    if !draft.is_empty() {
        if !value.is_empty() {
            value.push_str(", ");
        }
        value.push_str(draft);
    }
    value
}

fn enum_label<T: clap::ValueEnum>(index: Option<usize>, variants: &[T]) -> String {
    index
        .and_then(|index| variants.get(index))
        .and_then(clap::ValueEnum::to_possible_value)
        .map_or_else(|| "—".to_owned(), |value| value.get_name().to_owned())
}

fn cycle_index(index: &mut usize, delta: i32, len: usize) {
    if len == 0 {
        return;
    }
    *index = if delta.is_negative() {
        index.saturating_sub(delta.unsigned_abs() as usize)
    } else {
        (*index + delta.unsigned_abs() as usize).min(len - 1)
    };
}

/// Cycle `None → Some(0) → … → Some(len - 1)` and back, never past either
/// end — modelled as one extra position (`0`) ahead of the `len` real
/// variants, so the existing clamped-stepping shape covers "unset" for free.
fn cycle_option(index: &mut Option<usize>, delta: i32, len: usize) {
    if len == 0 {
        return;
    }
    let mut position = index.map_or(0, |value| value + 1);
    cycle_index(&mut position, delta, len + 1);
    *index = position.checked_sub(1);
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "a failing assertion is a failing test"
    )]

    use super::*;

    fn channel(id: &str) -> Channel {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "origin": "dynamic",
            "members": [],
            "group_access": [],
            "policy": {
                "default_priority": "default",
                "receipts_enabled": false,
                "ttl": { "allow_forever": false, "default": "7d" },
            },
        }))
        .expect("a fixture channel must deserialise")
    }

    #[test]
    fn move_focus_stops_at_the_ends_rather_than_wrapping() {
        let mut form = ComposeForm::new(1, 0);

        form.move_focus(-1);
        assert_eq!(form.focused(), 0);

        form.move_focus(100);
        assert_eq!(form.focused(), FIELDS.len() - 1);
    }

    #[test]
    fn typing_only_affects_the_focused_field_while_editing() {
        let mut form = ComposeForm::new(1, 0);
        form.move_focus(1); // Title

        assert_eq!(form.activate(), ComposeActivation::Handled);
        assert!(form.is_editing());

        form.push_char('h');
        form.push_char('i');
        assert_eq!(form.title, "hi");
        assert!(
            form.body.is_empty(),
            "typing must not leak into another field"
        );

        form.backspace();
        assert_eq!(form.title, "h");
    }

    #[test]
    fn typing_is_ignored_outside_edit_mode() {
        let mut form = ComposeForm::new(1, 0);
        form.move_focus(1); // Title, not editing yet

        form.push_char('x');

        assert!(form.title.is_empty());
    }

    #[test]
    fn cycling_the_channel_field_stops_at_the_ends() {
        let mut form = ComposeForm::new(3, 0);

        form.cycle(-1);
        assert_eq!(form.channel, 0);

        form.cycle(10);
        assert_eq!(form.channel, 2, "index 2 is the last of three channels");
    }

    #[test]
    fn cycling_an_enum_field_walks_unset_then_every_variant_and_back() {
        let mut form = ComposeForm::new(1, 0);
        form.move_focus(5); // État — Open, Working, Closed
        assert_eq!(form.state, None);

        form.cycle(1);
        assert_eq!(form.state, Some(0));
        form.cycle(1);
        assert_eq!(form.state, Some(1));
        form.cycle(1);
        assert_eq!(form.state, Some(2));
        form.cycle(1);
        assert_eq!(
            form.state,
            Some(2),
            "stops at the last variant rather than wrapping"
        );

        form.cycle(-1);
        form.cycle(-1);
        form.cycle(-1);
        assert_eq!(
            form.state, None,
            "one step before the first variant is unset, not a wrap to the last"
        );
    }

    #[test]
    fn tags_are_appended_on_activate_and_the_draft_clears() {
        let mut form = ComposeForm::new(1, 0);
        form.move_focus(3); // Tags

        form.activate(); // start editing
        form.push_char('a');
        form.activate(); // append "a"

        assert_eq!(form.tags, ["a"]);
        assert_eq!(form.tag_draft, "");
        assert!(form.is_editing(), "still editing — ready for the next tag");
    }

    #[test]
    fn an_invalid_action_line_is_rejected_without_losing_the_draft() {
        let mut form = ComposeForm::new(1, 0);
        form.move_focus(4); // Actions
        form.activate();
        for character in "explode:Boom:now".chars() {
            form.push_char(character);
        }

        let outcome = form.activate();

        assert!(matches!(outcome, ComposeActivation::Rejected(_)));
        assert_eq!(
            form.action_draft, "explode:Boom:now",
            "the draft survives a rejection"
        );
        assert!(form.actions.is_empty());
    }

    #[test]
    fn to_content_refuses_an_empty_title() {
        let form = ComposeForm::new(1, 0);
        assert!(form.to_content(&[channel("ci-infra")]).is_err());
    }

    #[test]
    fn to_content_refuses_when_there_is_no_channel_to_publish_to() {
        let mut form = ComposeForm::new(0, 0);
        form.title = "etcd degraded".to_owned();

        assert!(form.to_content(&[]).is_err());
    }

    #[test]
    fn to_content_round_trips_a_filled_form() {
        let mut form = ComposeForm::new(1, 0);
        form.title = "etcd degraded".to_owned();
        form.body = "cluster ovh".to_owned();
        form.tags = vec!["prod".to_owned()];
        form.actions = vec!["ack:Ack:incident-1".to_owned()];
        form.state = Some(0);
        form.status = Some(1);

        let (channel_id, content) = form.to_content(&[channel("ci-infra")]).unwrap();

        assert_eq!(channel_id, "ci-infra");
        assert_eq!(content.title.as_deref(), Some("etcd degraded"));
        assert_eq!(content.body.as_deref(), Some("cluster ovh"));
        assert_eq!(content.tags, ["prod"]);
        assert_eq!(content.actions, ["ack:Ack:incident-1"]);
        assert!(matches!(content.state, Some(StateArg::Open)));
        assert!(matches!(content.status, Some(StatusArg::Failure)));
    }
}
