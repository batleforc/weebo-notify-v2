//! Talking to a server: the generated client, plus the credential and the error
//! shape every front end wants.

use notify_client_api::Client;
use notify_client_api::types::{
    Actions, AddMemberBody, AttachmentId, Channel, ChannelId, ChannelPolicyBody, CreateChannelBody,
    Device, Dnd, DndDraft, EnrolBody, ExchangeBody, ExternalId, Image, MintedToken, Notification,
    PresignBody, Principal, Priority, Progress, PublishBody, Published, PushTransport,
    RevisionDraft, Scope, State, Status, Ttl, UnreadCount,
};

use crate::config::Profile;
use crate::error::ClientError;

/// An authenticated connection to one server.
#[derive(Debug, Clone)]
pub struct Session {
    api: Client,
    /// The same client the generated one holds, kept for the one call that cannot
    /// go through it — see `sync`.
    http: reqwest::Client,
    base: String,
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

    /// `GET /channels` — every channel this caller can see.
    ///
    /// Follows the cursor to the end rather than returning one page. A channel list
    /// is a sidebar and a completion source, not a feed: a front end that showed
    /// the first fifty and stopped would be quietly wrong for anybody in more.
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
            // Followed until `next` is null, never until a page looks short: rows
            // can expire between two calls (design §8).
            match page.next {
                Some(next) => cursor = Some(next.to_string()),
                None => return Ok(all),
            }
        }
    }

    /// `GET /unread` — how many notifications this caller has not read, per channel.
    ///
    /// The badge §9.2 draws in the TUI sidebar and §10bis.3bis/3ter want on the tray
    /// and the dock. It lives here rather than in any one of them because the design
    /// asks for "trois rendus, une seule vérité" — a count computed separately per
    /// front end is three truths that disagree the moment one of them is stale.
    ///
    /// Not paginated, unlike [`Session::channels`]: the answer holds one entry per
    /// channel with something unread, so it is bounded by a sidebar rather than by
    /// a feed.
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

    /// `PATCH /channels/{id}` — change part of a channel's policy (§8).
    ///
    /// Partial by construction: the fields left `None` are not sent at all, so two
    /// people editing two different knobs from two screens do not overwrite each
    /// other. A `max_ttl` of `Ttl::Forever` (`"0"`) lifts the ceiling — §7.2bis's own
    /// spelling for "unbounded", rather than a second way of saying it.
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

    /// `GET /preferences/dnd` — the caller's own do-not-disturb (§7.4).
    ///
    /// Somebody who has never set one reads a preference that mutes nothing rather
    /// than a 404: "off" and "never configured" are the same thing to a client, and
    /// making it tell them apart only invites two code paths that must agree.
    pub async fn dnd(&self) -> Result<Dnd, ClientError> {
        let response = self.api.read_dnd().await.map_err(ClientError::from)?;
        Ok(response.into_inner())
    }

    /// `PUT /preferences/dnd` — replace it (§7.4).
    ///
    /// Whole, not partial: quiet hours are a list, and a body that omitted a window
    /// could not be told apart from one asking to drop it. A caller that wants to
    /// change one thing reads first — which is what the CLI does.
    ///
    /// Takes a [`DndDraft`] and answers with a [`Dnd`]: whose preference it is is
    /// not something a client says, so a client that does not know its own id can
    /// still set one.
    pub async fn set_dnd(&self, wanted: &DndDraft) -> Result<Dnd, ClientError> {
        let response = self.api.set_dnd(wanted).await.map_err(ClientError::from)?;
        Ok(response.into_inner())
    }

    /// `POST /channels/{id}/members` — add a principal to a channel.
    ///
    /// "Join" and "invite" are the same call with a different principal, which is
    /// why there is one method: the server decides whether the caller may, and a
    /// second method would only duplicate that decision badly.
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

    /// `POST /devices` — enrol this machine.
    ///
    /// `stream` transport and no key: the CLI receives over SSE on TLS, so there is
    /// no third party carrying the payload and nothing to re-encrypt for (§6.2).
    /// The mobile shells are the ones that need a device key, and they are M6.
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

    /// `GET /notifications` — one page of the inbox.
    ///
    /// One page on purpose, unlike [`Self::channels`]: an inbox is unbounded, and a
    /// client that walked it all would download a year of history to print ten
    /// lines.
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
    /// Upload an attachment and hand back the reference a payload carries.
    ///
    /// Two calls and no bytes through the server (design §6.1): a presign, then a
    /// `PUT` straight at the bucket with the headers the signature covers. Sending
    /// them verbatim is not politeness — they are *in* the signature, so a `PUT`
    /// that omits or edits one is refused by the bucket.
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
                // `i64` is progenitor's rendering of an unsigned integer; the
                // cast cannot lose anything for a file that fits in memory.
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
            // The bucket refused, not us — so `Unexpected` and not `Refused`, which
            // carries a `problem+json` this answer does not have. Its body is S3 XML
            // that no user wants to read, so the status is what travels.
            return Err(ClientError::Unexpected {
                status: response.status().as_u16(),
            });
        }

        Ok(signed.id)
    }

    /// `POST /auth/token/exchange` — a CI job trades its forge ID token for one of
    /// ours (design §5.3).
    ///
    /// The one call on this type that does **not** need the session to be
    /// authenticated: it is where a credential is obtained. A profile with no token
    /// is therefore the normal way to reach it, which is exactly the state a CI
    /// runner starts in.
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

    /// The authenticated HTTP client, for the one call the generated one cannot
    /// make (see `sync`).
    pub(crate) fn http(&self) -> &reqwest::Client {
        &self.http
    }
}
