//! Talking to a server: the generated client, plus the credential and error shape front ends want.

use notify_client_api::Client;
use notify_client_api::types::{
    Actions, AddMemberBody, AttachmentId, Channel, ChannelId, ChannelPolicyBody, CreateChannelBody,
    Device, Dnd, DndDraft, EnrolBody, ExchangeBody, ExternalId, Image, Interaction, MintedToken,
    Notification, PresignBody, Principal, Priority, Progress, PublishBody, Published,
    PushTransport, RevisionDraft, Scope, SilenceBody, State, Status, Ttl, UnreadCount,
};

use crate::config::Profile;
use crate::error::ClientError;

/// An authenticated connection to one server.
#[derive(Debug, Clone)]
pub struct Session {
    api: Client,
    /// The generated client's HTTP client, for the one call it can't make (`sync`).
    http: reqwest::Client,
    base: String,
}

/// A revision as a front end describes it; fields left `None` keep their current value on update.
#[derive(Debug, Default, Clone)]
pub struct Revision {
    /// The headline: required on a creation, kept when absent on an update (§2.2).
    pub title: Option<String>,
    /// Markdown body.
    pub body: Option<String>,
    /// Lifecycle position; supplying it at creation makes the notification a lifecycle one.
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
            // Redacted wherever reqwest or a middleware prints headers.
            value.set_sensitive(true);
            headers.insert(reqwest::header::AUTHORIZATION, value);
        }

        let http = reqwest::Client::builder()
            .default_headers(headers)
            // No pooled keep-alive: an idle TUI's first click would hit a socket the server
            // (5 s keep-alive) already closed, failing with `IncompleteMessage` instead of reconnecting.
            .pool_max_idle_per_host(0)
            .build()
            .map_err(ClientError::Transport)?;

        let base = profile.url.as_str().trim_end_matches('/').to_owned();

        Ok(Self {
            api: Client::new_with_client(&base, http.clone()),
            http,
            base,
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

    /// `PUT /channels/{id}/notifications/{external_id}` — the idempotent upsert a CI job wants.
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

    /// `GET /channels` — every channel this caller can see, following the cursor to the end.
    pub async fn channels(&self) -> Result<Vec<Channel>, ClientError> {
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;

        loop {
            let page = self
                .api
                .list_channels(cursor.as_deref(), Some(100))
                .await
                .map_err(ClientError::from)?
                .into_inner();

            all.extend(page.items);
            // Until `next` is null, never until a page looks short (§8).
            match page.next {
                Some(next) => cursor = Some(next.to_string()),
                None => return Ok(all),
            }
        }
    }

    /// `GET /unread` — unread counts per channel (one source for the TUI, tray and dock badges).
    pub async fn unread(&self) -> Result<Vec<UnreadCount>, ClientError> {
        Ok(self
            .api
            .unread()
            .await
            .map_err(ClientError::from)?
            .into_inner())
    }

    /// `POST /channels` — create one, with the caller as its first manager.
    pub async fn create_channel(
        &self,
        id: &str,
        display_name: Option<&str>,
    ) -> Result<Channel, ClientError> {
        let response = self
            .api
            .create_channel(&CreateChannelBody {
                id: ChannelId::from(id.to_owned()),
                display_name: display_name.map(ToOwned::to_owned),
            })
            .await
            .map_err(ClientError::from)?;

        Ok(response.into_inner())
    }

    /// `PATCH /channels/{id}` — change part of a channel's policy (§8); `None` fields aren't sent.
    /// A `max_ttl` of `Ttl::Forever` lifts the ceiling.
    pub async fn update_channel_policy(
        &self,
        channel: &str,
        policy: ChannelPolicyBody,
    ) -> Result<Channel, ClientError> {
        let response = self
            .api
            .update_channel(&ChannelId::from(channel.to_owned()), &policy)
            .await
            .map_err(ClientError::from)?;

        Ok(response.into_inner())
    }

    /// `GET /preferences/dnd` — the caller's own do-not-disturb (§7.4); never set reads as off.
    pub async fn dnd(&self) -> Result<Dnd, ClientError> {
        let response = self.api.read_dnd().await.map_err(ClientError::from)?;
        Ok(response.into_inner())
    }

    /// `PUT /preferences/dnd` — replace it whole (§7.4); callers changing one thing read first.
    pub async fn set_dnd(&self, wanted: &DndDraft) -> Result<Dnd, ClientError> {
        let response = self.api.set_dnd(wanted).await.map_err(ClientError::from)?;
        Ok(response.into_inner())
    }

    /// `POST /channels/{id}/members` — add a principal (join and invite are the same call).
    pub async fn add_member(
        &self,
        channel: &str,
        principal: Principal,
        scopes: Vec<Scope>,
    ) -> Result<(), ClientError> {
        self.api
            .add_member(
                &ChannelId::from(channel.to_owned()),
                &AddMemberBody { principal, scopes },
            )
            .await
            .map_err(ClientError::from)?;

        Ok(())
    }

    /// `POST /devices` — enrol this machine as a keyless `stream` device (SSE over TLS, §6.2).
    pub async fn enrol_device(&self, label: &str) -> Result<Device, ClientError> {
        let response = self
            .api
            .enrol_device(&EnrolBody {
                label: label.to_owned(),
                transport: PushTransport::Stream,
                public_key: None,
            })
            .await
            .map_err(ClientError::from)?;

        Ok(response.into_inner())
    }

    /// `GET /devices` — the caller's devices, revoked ones included.
    pub async fn devices(&self) -> Result<Vec<Device>, ClientError> {
        let response = self.api.list_devices().await.map_err(ClientError::from)?;

        Ok(response.into_inner())
    }

    /// `POST /notifications/{id}/interactions` — answer an `ack`, `reply` or `choice` action (§2.3).
    /// `view` is client-side and `silence` is [`Self::silence`].
    pub async fn interact(&self, id: &str, interaction: Interaction) -> Result<(), ClientError> {
        self.api
            .record(id, &interaction)
            .await
            .map_err(ClientError::from)?;

        Ok(())
    }

    /// `POST /notifications/{id}/silence` — a `silence` action (design §2.3).
    pub async fn silence(&self, id: &str, body: SilenceBody) -> Result<(), ClientError> {
        self.api
            .silence(id, &body)
            .await
            .map_err(ClientError::from)?;

        Ok(())
    }

    /// `GET /notifications` — one page of the inbox (the inbox is unbounded).
    pub async fn inbox(
        &self,
        channels: Option<&str>,
        limit: u16,
    ) -> Result<Vec<Notification>, ClientError> {
        let response = self
            .api
            .inbox(
                channels,
                None,
                None,
                None,
                Some(i32::from(limit)),
                None,
                None,
                None,
                None,
            )
            .await
            .map_err(ClientError::from)?;

        Ok(response.into_inner().items)
    }
}

impl Session {
    /// Upload an attachment and return the reference a payload carries: presign, then `PUT` to
    /// the bucket with exactly the signed headers (§6.1).
    pub async fn upload_attachment(
        &self,
        channel: &str,
        content_type: &str,
        bytes: Vec<u8>,
    ) -> Result<AttachmentId, ClientError> {
        let signed = self
            .api
            .presign(&PresignBody {
                channel: channel.to_owned(),
                content_type: content_type.to_owned(),
                // progenitor renders an unsigned integer as `i64`.
                content_length: i64::try_from(bytes.len()).unwrap_or(i64::MAX),
            })
            .await
            .map_err(ClientError::from)?
            .into_inner();

        let mut request = self.http.put(signed.url.clone());
        for (name, value) in &signed.headers {
            request = request.header(name, value);
        }

        let response = request
            .body(bytes)
            .send()
            .await
            .map_err(ClientError::Transport)?;

        if !response.status().is_success() {
            // The bucket refused (S3 XML, no `problem+json`), so only the status travels.
            return Err(ClientError::Unexpected {
                status: response.status().as_u16(),
            });
        }

        Ok(signed.id)
    }

    /// `POST /auth/token/exchange` — trade a CI forge ID token for one of ours (§5.3); needs no
    /// prior credential.
    pub async fn exchange(&self, id_token: &str) -> Result<MintedToken, ClientError> {
        let response = self
            .api
            .exchange(&ExchangeBody {
                token: id_token.to_owned(),
            })
            .await
            .map_err(ClientError::from)?;

        Ok(response.into_inner())
    }
}

impl Session {
    /// The server this session talks to, without a trailing slash.
    pub(crate) fn base_url(&self) -> &str {
        &self.base
    }

    /// The authenticated HTTP client, for the one call the generated one can't make (`sync`).
    pub(crate) fn http(&self) -> &reqwest::Client {
        &self.http
    }
}
