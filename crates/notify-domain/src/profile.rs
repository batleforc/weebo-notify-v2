//! What a person chose to look like, and who gets to change it (design §2.4).
//!
//! # The set of overridden fields is the feature
//!
//! §2.4 asks for one behaviour: *a field edited locally stops being overwritten by
//! later OIDC syncs*. Without a record of what was edited there are only two
//! possible behaviours, and both are wrong — either the provider overwrites
//! somebody's choice at every login, or it stops updating anything after the first
//! one, and a display name fixed in the IdP never arrives.
//!
//! So what is stored is the **field**, not the value: "this `display_name` is
//! theirs". A sync then skips exactly the fields somebody claimed and refreshes the
//! rest, and handing a field back to the provider is possible — which is the part a
//! "sticky value" design cannot do without a support ticket.
//!
//! # No endpoint reads somebody else's
//!
//! There is deliberately no `GET /users/{id}/profile` (§2.4). A profile travels
//! **embedded** where it is legitimate — as the `author` of a revision, in a member
//! list, next to a receipt — which is what mechanically enforces "visible only
//! through a shared channel". Adding a read endpoint later means redoing that
//! analysis, not adding a route.

use std::collections::BTreeSet;
use std::str::FromStr as _;

use serde::{Deserialize, Serialize};

use crate::error::{DomainError, Result};
use crate::hygiene::{TextKind, optional_text};
use crate::ids::AttachmentId;
use crate::limits::MAX_TITLE_CHARS;

/// The longest locale tag worth accepting: `zh-Hant-TW` and friends fit easily.
const MAX_LOCALE_CHARS: usize = 35;

/// A field of a profile that can be claimed by its owner.
///
/// Named rather than positional: this set is persisted, and a variant added in the
/// middle of a tuple would silently re-map what people had claimed.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileField {
    /// The name shown next to revisions, interactions and receipts.
    DisplayName,
    /// The timezone quiet hours are expressed in (design §7.4).
    Timezone,
    /// The language the UI answers in.
    Locale,
}

/// Everything a client shows about a person, beyond their identity.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    /// What clients show. Falls back to the identity when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// An IANA name such as `Europe/Paris`.
    ///
    /// Validated against the real database rather than a shape: this is the default
    /// quiet hours are resolved in (§7.4), so a plausible-looking `Europe/Pris`
    /// accepted today becomes a notification that does not arrive in six months,
    /// with nothing connecting the two.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    /// A BCP 47 tag such as `fr-FR`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,
    /// The object holding their picture, once one has been uploaded (§2.4).
    ///
    /// Not in [`ProfileField`], and that is deliberate: an avatar is never synced
    /// from the identity provider, so there is nothing for its owner to claim it
    /// *from*. OIDC does publish a `picture` claim, and following it would mean the
    /// server fetching an arbitrary URL found in a token — a request-forgery
    /// primitive pointed at whatever the cluster can reach. An avatar arrives by
    /// upload or not at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar: Option<AttachmentId>,
    /// The fields this person set themselves.
    ///
    /// Public because the API surfaces it: a settings screen has to be able to say
    /// "this one is yours, and this one follows your identity provider", which is
    /// the difference between an editable field and a mysterious one.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub overridden: BTreeSet<ProfileField>,
}

/// What an edit says about one field.
///
/// Three states, and they are genuinely three — which is why this is an enum and
/// not the `Option<Option<String>>` it would otherwise be:
///
/// * the field is **absent** from the edit — `Option::None` around this type;
/// * [`Self::Follow`] — hand it back to the identity provider, so the next login
///   refreshes it. This is the way *out* of a claim, and without it somebody who
///   typed a display name once would maintain it by hand forever;
/// * [`Self::Set`] — claim it, with this value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldEdit {
    /// Release the field and let the provider own it again.
    Follow,
    /// Claim the field with this value.
    Set(String),
}

impl FieldEdit {
    /// The value to store: `None` releases the field.
    #[must_use]
    fn value(&self) -> Option<&str> {
        match self {
            Self::Follow => None,
            Self::Set(value) => Some(value),
        }
    }
}

/// A requested change to a profile. Absent fields are left exactly as they are.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProfileEdit {
    /// The name to show.
    pub display_name: Option<FieldEdit>,
    /// The timezone to resolve quiet hours in.
    pub timezone: Option<FieldEdit>,
    /// The language to answer in.
    pub locale: Option<FieldEdit>,
}

impl Profile {
    /// Apply an edit made by the profile's owner.
    ///
    /// # Errors
    ///
    /// Refuses text that fails the hygiene rules, and a timezone the IANA database
    /// does not know.
    pub fn edit(&mut self, edit: ProfileEdit) -> Result<()> {
        if let Some(display_name) = edit.display_name {
            self.claim_or_release(
                ProfileField::DisplayName,
                optional_text(
                    display_name.value(),
                    TextKind::SingleLine,
                    MAX_TITLE_CHARS,
                    "profile.display_name",
                )?,
            );
        }

        if let Some(timezone) = edit.timezone {
            let checked = optional_text(
                timezone.value(),
                TextKind::SingleLine,
                MAX_LOCALE_CHARS,
                "profile.timezone",
            )?;
            if let Some(name) = checked.as_deref() {
                validate_timezone(name)?;
            }
            self.claim_or_release(ProfileField::Timezone, checked);
        }

        if let Some(locale) = edit.locale {
            let checked = optional_text(
                locale.value(),
                TextKind::SingleLine,
                MAX_LOCALE_CHARS,
                "profile.locale",
            )?;
            if let Some(tag) = checked.as_deref() {
                validate_locale(tag)?;
            }
            self.claim_or_release(ProfileField::Locale, checked);
        }

        Ok(())
    }

    /// Set a field and mark it claimed, or clear it and hand it back to the IdP.
    fn claim_or_release(&mut self, field: ProfileField, value: Option<String>) {
        let claimed = value.is_some();
        match field {
            ProfileField::DisplayName => self.display_name = value,
            ProfileField::Timezone => self.timezone = value,
            ProfileField::Locale => self.locale = value,
        }
        if claimed {
            self.overridden.insert(field);
        } else {
            self.overridden.remove(&field);
        }
    }

    /// Refresh from identity-provider claims, leaving claimed fields alone.
    ///
    /// Returns whether anything actually changed, which is what lets the caller
    /// skip a database write — see [`crate::identity::User::sync_from_claims`].
    #[must_use]
    pub fn follow_claims(&mut self, display_name: Option<&str>) -> bool {
        if self.overridden.contains(&ProfileField::DisplayName) {
            return false;
        }

        let incoming = display_name.map(ToOwned::to_owned);
        if incoming == self.display_name {
            return false;
        }
        self.display_name = incoming;
        true
    }

    /// Whether this field is the person's own choice rather than the IdP's.
    #[must_use]
    pub fn is_overridden(&self, field: ProfileField) -> bool {
        self.overridden.contains(&field)
    }
}

/// Check a timezone against the compiled-in IANA database.
fn validate_timezone(name: &str) -> Result<()> {
    chrono_tz::Tz::from_str(name)
        .map(|_| ())
        .map_err(|_| DomainError::Malformed {
            field: "profile.timezone",
            reason: "not an IANA timezone name, such as `Europe/Paris`",
        })
}

/// Check a locale looks like a BCP 47 tag.
///
/// Deliberately shallower than the timezone check: a locale that is not one of ours
/// degrades to the default language, while a bad timezone silently moves somebody's
/// quiet hours. The cost of being wrong is not the same, so neither is the check.
fn validate_locale(tag: &str) -> Result<()> {
    let shaped = !tag.is_empty()
        && tag.split('-').all(|part| {
            !part.is_empty() && part.len() <= 8 && part.chars().all(|c| c.is_ascii_alphanumeric())
        });

    if shaped {
        Ok(())
    } else {
        Err(DomainError::Malformed {
            field: "profile.locale",
            reason: "not a BCP 47 language tag, such as `fr-FR`",
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use super::*;

    fn edit_name(name: Option<&str>) -> ProfileEdit {
        ProfileEdit {
            display_name: Some(
                name.map_or(FieldEdit::Follow, |value| FieldEdit::Set(value.to_owned())),
            ),
            ..ProfileEdit::default()
        }
    }

    #[test]
    fn a_fresh_profile_follows_its_identity_provider() {
        let mut profile = Profile::default();

        assert!(profile.follow_claims(Some("Alice Martin")));
        assert_eq!(profile.display_name.as_deref(), Some("Alice Martin"));
        assert!(!profile.is_overridden(ProfileField::DisplayName));
    }

    #[test]
    fn a_second_login_with_the_same_claims_changes_nothing() {
        // The caller uses this to skip a write, and bearer auth replays on every
        // single request (§5.1) — so "nothing changed" has to be cheap and true.
        let mut profile = Profile::default();
        assert!(profile.follow_claims(Some("Alice Martin")));
        assert!(!profile.follow_claims(Some("Alice Martin")));
    }

    #[test]
    fn an_edited_field_stops_following_the_identity_provider() {
        let mut profile = Profile::default();
        assert!(profile.follow_claims(Some("Alice Martin")));

        profile.edit(edit_name(Some("alice"))).unwrap();

        assert_eq!(profile.display_name.as_deref(), Some("alice"));
        assert!(profile.is_overridden(ProfileField::DisplayName));

        // The IdP renaming her must not take her choice away.
        assert!(!profile.follow_claims(Some("Alice MARTIN (Contractor)")));
        assert_eq!(profile.display_name.as_deref(), Some("alice"));
    }

    #[test]
    fn setting_a_field_back_to_null_hands_it_back_to_the_provider() {
        // The way out. Without it, typing a display name once means maintaining it
        // by hand forever.
        let mut profile = Profile::default();
        profile.edit(edit_name(Some("alice"))).unwrap();
        profile.edit(edit_name(None)).unwrap();

        assert!(!profile.is_overridden(ProfileField::DisplayName));
        assert!(profile.follow_claims(Some("Alice Martin")));
        assert_eq!(profile.display_name.as_deref(), Some("Alice Martin"));
    }

    #[test]
    fn an_absent_field_is_left_exactly_as_it_was() {
        let mut profile = Profile::default();
        profile.edit(edit_name(Some("alice"))).unwrap();

        // Only the locale is in this edit; the display name must survive untouched,
        // override included.
        profile
            .edit(ProfileEdit {
                locale: Some(FieldEdit::Set("fr-FR".to_owned())),
                ..ProfileEdit::default()
            })
            .unwrap();

        assert_eq!(profile.display_name.as_deref(), Some("alice"));
        assert!(profile.is_overridden(ProfileField::DisplayName));
        assert_eq!(profile.locale.as_deref(), Some("fr-FR"));
    }

    #[test]
    fn a_timezone_is_checked_against_the_real_database() {
        let mut profile = Profile::default();

        profile
            .edit(ProfileEdit {
                timezone: Some(FieldEdit::Set("Europe/Paris".to_owned())),
                ..ProfileEdit::default()
            })
            .unwrap();
        assert_eq!(profile.timezone.as_deref(), Some("Europe/Paris"));

        // A plausible typo is exactly the case a shape check would wave through.
        let refused = profile.edit(ProfileEdit {
            timezone: Some(FieldEdit::Set("Europe/Pris".to_owned())),
            ..ProfileEdit::default()
        });
        assert!(matches!(
            refused,
            Err(DomainError::Malformed {
                field: "profile.timezone",
                ..
            })
        ));
        assert_eq!(
            profile.timezone.as_deref(),
            Some("Europe/Paris"),
            "a refused edit must leave the profile as it was"
        );
    }

    #[test]
    fn a_locale_has_to_look_like_a_language_tag() {
        let mut profile = Profile::default();

        for good in ["fr", "fr-FR", "zh-Hant-TW"] {
            profile
                .edit(ProfileEdit {
                    locale: Some(FieldEdit::Set(good.to_owned())),
                    ..ProfileEdit::default()
                })
                .unwrap();
            assert_eq!(profile.locale.as_deref(), Some(good));
        }

        for bad in ["fr_FR", "français", "fr-", "-"] {
            assert!(
                profile
                    .edit(ProfileEdit {
                        locale: Some(FieldEdit::Set(bad.to_owned())),
                        ..ProfileEdit::default()
                    })
                    .is_err(),
                "{bad} is not a language tag"
            );
        }
    }

    #[test]
    fn profile_text_goes_through_the_same_hygiene_as_everything_else() {
        let mut profile = Profile::default();

        // A display name lands in terminals (§9.2) and browsers (§10.3) next to
        // every revision its owner publishes, so it is sanitised on the way in like
        // any other text — stripped rather than refused, as `hygiene` does
        // everywhere else.
        profile.edit(edit_name(Some("alice\u{1b}[31m"))).unwrap();
        assert_eq!(profile.display_name.as_deref(), Some("alice"));

        // A name that is nothing but control characters is not a name: it comes
        // back empty, which releases the field rather than claiming an empty one.
        profile.edit(edit_name(Some("\u{1b}\u{7}"))).unwrap();
        assert_eq!(profile.display_name, None);
        assert!(!profile.is_overridden(ProfileField::DisplayName));
    }
}
