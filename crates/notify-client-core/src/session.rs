//! Talking to a server: the generated client, plus the credential and the error
//! shape every front end wants.

use notify_client_api::Client;
use notify_client_api::types::{
    Actions, ChannelId, ExternalId, Image, Priority, Progress, PublishBody, Published,
    RevisionDraft, State, Status, Ttl,
};

use crate::config::Profile;
use crate::error::ClientError;

/// An authenticated connection to one server.
#[derive(Debug, Clone)]
pub struct Session {
    api: Client,
}

/// A revision as a front end describes it, before it becomes a request body.
///
/// Every field optional except where the server insists, because that is the shape
/// of the flow it exists for: `notify publish -t "…"` first, then
/// `notify update --state closed --status success` with nothing else restated.
#[derive(Debug, Default, Clone)]
pub struct Revision {
    /// The headline. Required on a creation; absent on an update keeps the current
    /// one (design §2.2).
    pub title: Option<String>,
    /// Markdown body.
    pub body: Option<String>,
    /// Lifecycle position. Supplying it at creation is what makes the notification
    /// a lifecycle one rather than fire-and-forget.
    pub state: Option<State>,
    /// Outcome.
    pub status: Option<Status>,
    /// How loudly.
    pub priority: Option<Priority>,
    /// Retention, e.g. `48h`.
    pub ttl: Option<Ttl>,
    /// Free-form labels.
    pub tags: Vec<String>,
    /// Buttons. Replaced wholesale on every revision (design §2.3).
    pub actions: Option<Actions>,
    /// A determinate progress bar.
    pub progress: Option<Progress>,
    /// An inline image or an attachment reference.
    pub image: Option<Image>,
    /// Which client-side group this belongs to.
    pub group_key: Option<String>,
    /// An icon name.
    pub icon: Option<String>,
}

impl Revision {
    fn into_draft(self) -> RevisionDraft {
        RevisionDraft {
            title: self.title,
            body: self.body,
            state: self.state,
            status: self.status,
            priority: self.priority,
            ttl: self.ttl,
            tags: self.tags,
            actions: self.actions,
            progress: self.progress,
            image: self.image,
            group_key: self.group_key,
            icon: self.icon,
        }
    }
}

impl Session {
    /// Open one against a profile.
    pub fn open(profile: &Profile) -> Result<Self, ClientError> {
        let mut headers = reqwest::header::HeaderMap::new();

        if let Some(token) = &profile.token {
            let mut value = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|_| ClientError::BadCredential)?;
            // Marked sensitive so it is redacted wherever reqwest or a middleware
            // decides to print its headers.
            value.set_sensitive(true);
            headers.insert(reqwest::header::AUTHORIZATION, value);
        }

        let http = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .map_err(ClientError::Transport)?;

        Ok(Self {
            api: Client::new_with_client(profile.url.as_str().trim_end_matches('/'), http),
        })
    }

    /// `POST /notifications` — create one (design §8.3).
    pub async fn publish(
        &self,
        channel: &str,
        revision: Revision,
        idempotency_key: Option<&str>,
    ) -> Result<Published, ClientError> {
        let draft = revision.into_draft();
        let body = PublishBody {
            channel: ChannelId::from(channel.to_owned()),
            title: draft.title,
            body: draft.body,
            state: draft.state,
            status: draft.status,
            priority: draft.priority,
            ttl: draft.ttl,
            tags: draft.tags,
            actions: draft.actions,
            progress: draft.progress,
            image: draft.image,
            group_key: draft.group_key,
            icon: draft.icon,
        };

        let response = self
            .api
            .publish(idempotency_key, &body)
            .await
            .map_err(ClientError::from)?;

        Ok(response.into_inner())
    }

    /// `PUT /channels/{id}/notifications/{external_id}` — the idempotent upsert.
    ///
    /// The one a CI job wants: the same call creates on the first run and revises on
    /// the retry, with no branch on the caller's side.
    pub async fn update(
        &self,
        channel: &str,
        external_id: &str,
        revision: Revision,
    ) -> Result<Published, ClientError> {
        let response = self
            .api
            .upsert(
                &ChannelId::from(channel.to_owned()),
                &ExternalId::from(external_id.to_owned()),
                &revision.into_draft(),
            )
            .await
            .map_err(ClientError::from)?;

        Ok(response.into_inner())
    }
}
