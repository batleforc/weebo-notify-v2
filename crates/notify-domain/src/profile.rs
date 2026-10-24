//! A person's profile (§2.4). Locally edited fields are recorded as claimed so
//! OIDC syncs skip them; there is deliberately no endpoint to read someone else's.

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
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileField {
    /// The name shown next to revisions, interactions and receipts.
    DisplayName,
    /// The timezone quiet hours are expressed in (§7.4).
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
    /// An IANA timezone name such as `Europe/Paris`, validated against the real database.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    /// A BCP 47 tag such as `fr-FR`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,
    /// The object holding their uploaded picture (§2.4). Never synced from OIDC
    /// `picture`: fetching token-supplied URLs would be an SSRF primitive.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar: Option<AttachmentId>,
    /// The fields this person set themselves, rather than following the identity provider.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub overridden: BTreeSet<ProfileField>,
}

/// What an edit says about one field (absent from the edit = `None` around this).
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
    /// Refuses text that fails hygiene, and unknown IANA timezones.
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

    /// Refresh from identity-provider claims, leaving claimed fields alone; returns
    /// whether anything changed.
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

/// Check a locale looks like a BCP 47 tag (shallower than the timezone check: a
/// bad locale only falls back to the default language).
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
        // Bearer auth syncs on every request (§5.1), so "nothing changed" must be true.
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
        // Following the IdP again releases the claim.
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

        // Only the locale is edited; the display name and its claim survive.
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

        // Display names are sanitised (stripped, not refused).
        profile.edit(edit_name(Some("alice\u{1b}[31m"))).unwrap();
        assert_eq!(profile.display_name.as_deref(), Some("alice"));

        // Nothing but control characters releases the field.
        profile.edit(edit_name(Some("\u{1b}\u{7}"))).unwrap();
        assert_eq!(profile.display_name, None);
        assert!(!profile.is_overridden(ProfileField::DisplayName));
    }
}
