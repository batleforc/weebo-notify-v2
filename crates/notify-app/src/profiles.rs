//! Reading and editing one's own profile (§2.4). Always the caller's: there is no
//! endpoint reading somebody else's; profiles travel embedded where legitimate.

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
            // An absent row gets the same refusal, not a 500.
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
    #[must_use]
    pub fn new(users: Arc<dyn UserRepository>) -> Self {
        Self {
            read: ReadProfile::new(users.clone()),
            users,
        }
    }

    /// Apply a partial edit (see [`ProfileEdit`]) and return the resulting profile; an
    /// explicit `null` hands the field back to the identity provider.
    pub async fn execute(&self, caller: &Caller, edit: ProfileEdit) -> Result<Profile, Failure> {
        caller.ensure_person()?;
        let mut user = self.read.load(caller).await?;
        user.edit_profile(edit)?;
        self.users.upsert(&user).await?;
        Ok(user.profile)
    }
}

/// `PUT /api/v1/users/me/avatar` (§2.4, §8). Bytes are processed before they are
/// stored: only a re-encoded 256×256 PNG reaches the bucket (see [`AvatarProcessor`]).
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

        // Its own write, returning what it replaced: the profile read above may be
        // stale by now (decoding and storing take a while), and two uploads at once
        // must each delete what they actually replaced.
        let previous = self.users.set_avatar(user.id, id).await?;
        user.profile.avatar = Some(id);

        // The old object goes only after the new one is stored and referenced: at worst an orphan.
        if let Some(previous) = previous
            && let Err(error) = self.attachments.delete(previous).await
        {
            tracing::warn!(%error, %previous, "could not delete a replaced avatar");
        }

        Ok(user.profile)
    }
}

/// `GET /api/v1/avatars/{id}` (§2.4). Served through the API, by object id: no
/// signature per row, and no per-user profile lookup route.
#[derive(Clone)]
pub struct ReadAvatar {
    users: Arc<dyn UserRepository>,
    attachments: Arc<dyn AttachmentStore>,
}

impl core::fmt::Debug for ReadAvatar {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ReadAvatar").finish_non_exhaustive()
    }
}

impl ReadAvatar {
    #[must_use]
    pub fn new(users: Arc<dyn UserRepository>, attachments: Arc<dyn AttachmentStore>) -> Self {
        Self { users, attachments }
    }

    /// The bytes, for any authenticated caller: the unguessable id is the capability.
    /// Only if `id` is an avatar — the store is shared, so anything else reads as absent.
    pub async fn execute(
        &self,
        _caller: &Caller,
        id: AttachmentId,
    ) -> Result<Option<StoredObject>, Failure> {
        if !self.users.is_avatar(id).await? {
            return Ok(None);
        }
        Ok(self.attachments.get(id).await?)
    }
}

/// Turn a refused image into a 422 the caller can act on.
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

        // The shown name has to follow (§2.4).
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
