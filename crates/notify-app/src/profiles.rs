//! Reading and editing one's own profile (design §2.4, `GET`/`PUT
//! /api/v1/users/me/profile`).
//!
//! # Only ever your own
//!
//! Both use cases take the profile from the [`Caller`] and never from a path
//! parameter, because §2.4 says there is no endpoint that reads somebody else's:
//! a profile travels **embedded** where it is legitimate — as the `author` of a
//! revision, in a member list, next to a receipt — and that is what mechanically
//! enforces "visible only through a shared channel".
//!
//! A principal that is not a user has no profile to read: an API key acts for
//! whoever owns it, and a channel key owns nothing at all (§5.2).

use std::sync::Arc;

use notify_domain::{AttachmentId, DomainError, Principal, Profile, ProfileEdit, User};
use notify_ports::{
    AttachmentStore, AvatarProcessor, Failure, ImageRejection, StoredObject, UserRepository,
};

use crate::authz::Caller;

/// `GET /api/v1/users/me/profile` (design §8).
#[derive(Clone)]
pub struct ReadProfile {
    users: Arc<dyn UserRepository>,
}

impl core::fmt::Debug for ReadProfile {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ReadProfile").finish_non_exhaustive()
    }
}

impl ReadProfile {
    /// Wire it.
    #[must_use]
    pub fn new(users: Arc<dyn UserRepository>) -> Self {
        Self { users }
    }

    /// The caller's own profile.
    pub async fn execute(&self, caller: &Caller) -> Result<Profile, Failure> {
        Ok(self.load(caller).await?.profile)
    }

    /// The caller as a stored user, or a refusal that says why not.
    async fn load(&self, caller: &Caller) -> Result<User, Failure> {
        let Principal::User { id } = caller.principal else {
            return Err(Failure::Domain(DomainError::NotAPerson));
        };

        self.users
            .get(id)
            .await?
            // A token verified against JWKS names a user the login path created, so
            // an absent row is a database that lost one — but the caller gets the
            // same refusal either way rather than a 500 that says "your account is
            // missing".
            .ok_or(Failure::Domain(DomainError::NotAPerson))
    }
}

/// `PUT /api/v1/users/me/profile` (design §8).
#[derive(Clone)]
pub struct UpdateProfile {
    users: Arc<dyn UserRepository>,
    read: ReadProfile,
}

impl core::fmt::Debug for UpdateProfile {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("UpdateProfile").finish_non_exhaustive()
    }
}

impl UpdateProfile {
    /// Wire it.
    ///
    /// No clock: nothing here is stamped. M3 adds `edited_at` with the DND
    /// preferences, and a constructor argument nobody reads until then is API noise
    /// every caller has to satisfy.
    #[must_use]
    pub fn new(users: Arc<dyn UserRepository>) -> Self {
        Self {
            read: ReadProfile::new(users.clone()),
            users,
        }
    }

    /// Apply an edit and return the profile as it now stands.
    ///
    /// The edit is partial by construction (see [`ProfileEdit`]): an absent field
    /// is left alone, and an explicit `null` hands the field back to the identity
    /// provider instead of blanking it.
    pub async fn execute(&self, caller: &Caller, edit: ProfileEdit) -> Result<Profile, Failure> {
        caller.ensure_person()?;
        let mut user = self.read.load(caller).await?;
        user.edit_profile(edit)?;
        self.users.upsert(&user).await?;
        Ok(user.profile)
    }
}

/// `PUT /api/v1/users/me/avatar` (design §2.4, §8).
///
/// The only upload that goes through the server, and the order of what happens is
/// the point: the bytes are **processed before they are stored**, so nothing a
/// caller sent ever reaches the bucket. What lands there is a re-encoded 256×256
/// PNG with no metadata — see [`AvatarProcessor`], which owns those rules.
#[derive(Clone)]
pub struct SetAvatar {
    users: Arc<dyn UserRepository>,
    read: ReadProfile,
    processor: Arc<dyn AvatarProcessor>,
    attachments: Arc<dyn AttachmentStore>,
}

impl core::fmt::Debug for SetAvatar {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SetAvatar").finish_non_exhaustive()
    }
}

impl SetAvatar {
    /// Wire it.
    #[must_use]
    pub fn new(
        users: Arc<dyn UserRepository>,
        processor: Arc<dyn AvatarProcessor>,
        attachments: Arc<dyn AttachmentStore>,
    ) -> Self {
        Self {
            read: ReadProfile::new(users.clone()),
            users,
            processor,
            attachments,
        }
    }

    /// Replace the caller's avatar.
    pub async fn execute(&self, caller: &Caller, bytes: &[u8]) -> Result<Profile, Failure> {
        caller.ensure_person()?;
        let mut user = self.read.load(caller).await?;

        let processed = self.processor.process(bytes).await?.map_err(rejected)?;

        let id = AttachmentId::generate();
        self.attachments
            .put(id, &processed.content_type, processed.bytes)
            .await?;

        let previous = user.profile.avatar.replace(id);
        self.users.upsert(&user).await?;

        // The old object goes only after the new one is stored and referenced. The
        // other order leaves a window where the profile points at bytes that are
        // gone, and a failure in between would make it permanent; this order leaves
        // an orphan at worst, which the purge job can find.
        if let Some(previous) = previous
            && let Err(error) = self.attachments.delete(previous).await
        {
            tracing::warn!(%error, %previous, "could not delete a replaced avatar");
        }

        Ok(user.profile)
    }
}

/// `GET /api/v1/avatars/{id}` (design §2.4).
///
/// Served through the API rather than by a presigned URL, and by object id rather
/// than by user id. Both follow from §2.4: an avatar is shown next to every
/// revision its owner published, so a signature per render would be a signing round
/// trip per row of an inbox — and a `/users/{id}/avatar` route would be the profile
/// lookup that section refuses to add.
#[derive(Clone)]
pub struct ReadAvatar {
    attachments: Arc<dyn AttachmentStore>,
}

impl core::fmt::Debug for ReadAvatar {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ReadAvatar").finish_non_exhaustive()
    }
}

impl ReadAvatar {
    /// Wire it.
    #[must_use]
    pub fn new(attachments: Arc<dyn AttachmentStore>) -> Self {
        Self { attachments }
    }

    /// The bytes, for any authenticated caller.
    ///
    /// Any *authenticated* caller: the id is an unguessable ULID that only reaches
    /// somebody through a profile or an `author` they were already allowed to see,
    /// so the reference is the capability. Checking a shared channel here would mean
    /// resolving which user an object belongs to on every image request, and would
    /// still be defeated by the person simply being in the channel.
    pub async fn execute(
        &self,
        _caller: &Caller,
        id: AttachmentId,
    ) -> Result<Option<StoredObject>, Failure> {
        Ok(self.attachments.get(id).await?)
    }
}

/// Turn a refused image into the domain's vocabulary.
///
/// Everything the processor refuses is the caller's file being wrong, so it is a
/// `422` with a reason somebody can act on rather than an infrastructure failure.
fn rejected(rejection: ImageRejection) -> Failure {
    Failure::Domain(match rejection {
        ImageRejection::TooLarge => DomainError::TooLarge {
            what: "avatar",
            max: notify_domain::limits::MAX_AVATAR_BYTES,
            actual: 0,
        },
        ImageRejection::Unsupported => DomainError::Malformed {
            field: "avatar",
            reason: "not a supported image; use PNG, JPEG or WebP",
        },
        ImageRejection::Animated => DomainError::Malformed {
            field: "avatar",
            reason: "animated images are not accepted",
        },
        ImageRejection::Corrupt => DomainError::Malformed {
            field: "avatar",
            reason: "the image could not be decoded",
        },
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use notify_domain::{FieldEdit, Principal, ProfileField, UserId};
    use notify_ports::Clock as _;
    use notify_ports::test_utils::{InMemory, TestClock};

    use super::*;

    async fn fixture() -> (InMemory, TestClock, Caller, UserId) {
        let store = InMemory::new();
        let clock = TestClock::new();
        let id = UserId::generate();
        let user = User::from_claims(
            id,
            "alice",
            Some("alice@weebo.local"),
            Some("Alice Martin"),
            clock.now(),
        )
        .unwrap();
        UserRepository::upsert(&store, &user).await.unwrap();

        let caller = Caller::member_of(Principal::User { id }, vec![]);
        (store, clock, caller, id)
    }

    #[tokio::test]
    async fn a_profile_starts_as_what_the_provider_said() {
        let (store, _, caller, _) = fixture().await;

        let profile = ReadProfile::new(Arc::new(store))
            .execute(&caller)
            .await
            .unwrap();

        assert_eq!(profile.display_name.as_deref(), Some("Alice Martin"));
        assert!(profile.overridden.is_empty());
    }

    #[tokio::test]
    async fn a_key_cannot_rename_its_owner() {
        let (store, _, caller, _) = fixture().await;
        let as_key = Caller {
            grant: crate::authz::Grant::ApiKey {
                id: notify_domain::ApiKeyId::generate(),
                scopes: vec![notify_domain::Scope::Publish],
                channels: Vec::new(),
                publish_per_minute: None,
                owner_groups: Vec::new(),
            },
            ..caller
        };

        let refused = UpdateProfile::new(Arc::new(store))
            .execute(
                &as_key,
                ProfileEdit {
                    display_name: Some(FieldEdit::Set("mallory".to_owned())),
                    ..ProfileEdit::default()
                },
            )
            .await;

        assert!(matches!(
            refused,
            Err(Failure::Domain(DomainError::SessionRequired))
        ));
    }

    #[tokio::test]
    async fn editing_a_field_claims_it_and_survives_a_reload() {
        let (store, _clock, caller, id) = fixture().await;
        let store = Arc::new(store);

        let updated = UpdateProfile::new(store.clone())
            .execute(
                &caller,
                ProfileEdit {
                    display_name: Some(FieldEdit::Set("alice".to_owned())),
                    timezone: Some(FieldEdit::Set("Europe/Paris".to_owned())),
                    ..ProfileEdit::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(updated.display_name.as_deref(), Some("alice"));
        assert!(updated.is_overridden(ProfileField::Timezone));

        // The shown name has to follow, or the timeline keeps saying "Alice Martin"
        // next to everything she publishes (§2.4).
        let stored = UserRepository::get(store.as_ref(), id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.display_name.as_deref(), Some("alice"));
    }

    #[tokio::test]
    async fn a_principal_without_an_account_has_no_profile_to_read() {
        // A channel key is anonymous by design (§5.2): there is nobody to describe.
        let (store, _, _, _) = fixture().await;
        let anonymous = Caller::member_of(
            Principal::ChannelKey {
                channel: notify_domain::ChannelId::parse("ci-infra").unwrap(),
                key: notify_domain::ApiKeyId::generate(),
            },
            vec![],
        );

        let refused = ReadProfile::new(Arc::new(store)).execute(&anonymous).await;

        assert!(refused.is_err());
    }
}
