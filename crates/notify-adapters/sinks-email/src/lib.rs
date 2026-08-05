//! The email sink: one revision, one plain-text mail (design §7.1bis).
//!
//! The other v1 sink, and the one whose hard part is not the protocol. A webhook
//! consumer parses; a person reads. So this adapter is mostly a decision about what
//! belongs in a mail somebody gets at three in the morning, and the rule it follows
//! is the one §2.3 already sets for every other surface: the title is what there is
//! room for, everything else is optional.
//!
//! # Only revisions are mailed
//!
//! The stream carries five kinds of event (§8.1). Four of them — a receipt, an
//! interaction, a tombstone, a preference — are a client's business and nobody's
//! inbox: "alice has read your notification" is not a mail, it is a badge. A webhook
//! gets all five because a program can ignore what it does not care about; a mailbox
//! cannot. The other four are accepted and dropped, reported as delivered, because a
//! sink that retried them for a day would fill a DLQ with events that were never
//! going anywhere.
//!
//! # Plain text, and no HTML alternative
//!
//! The body is markdown by contract (§2.3), and markdown reads fine as text. An HTML
//! part would mean rendering publisher-supplied markdown into a mail client — the
//! single most forgiving HTML sanitiser surface there is, for a gain of some bold
//! type. §2.3 spends its length on exactly this class of risk; this is the cheap way
//! to stay out of it.
//!
//! # What is transient and what is not
//!
//! SMTP answers that question itself: a `4xx` reply is "try again", a `5xx` is "this
//! address will not take it". `lettre` preserves the distinction, so the
//! classification here is that distinction and nothing invented on top.

use core::fmt::Write as _;

use lettre::message::{Mailbox, header::ContentType};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport as _, Message, Tokio1Executor};
use notify_ports::{
    Event, EventPayload, PortError, PortResult, RevisionEvent, SinkDelivery, SinkDispatcher,
    SinkOutcome, SinkTarget,
};

/// Everything the mail sink needs that is not per-delivery (design §11,
/// `[sinks.smtp]`).
#[derive(Debug, Clone)]
pub struct SmtpSettings {
    /// Where the relay is: `smtp://host:port` for a plaintext hop inside the cluster
    /// (or Mailpit in development), `smtps://…` for TLS.
    pub url: url::Url,
    /// The envelope sender, as `Name <address>` or bare address.
    pub from: String,
    /// Relay credentials, when it wants them.
    pub username: Option<String>,
    /// Resolved by the configuration layer, never read from here.
    pub password: Option<String>,
    /// Where this instance is reachable, for the link at the bottom of the mail.
    ///
    /// Optional, and no link is written without it: §10.4 wants mails to point at
    /// `/n/<id>`, and guessing a hostname would produce mails full of links to
    /// somewhere nobody is serving.
    pub permalink_base: Option<url::Url>,
}

/// A `SinkDispatcher` that sends mail.
#[derive(Clone)]
pub struct EmailSink {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
    permalink_base: Option<url::Url>,
}

impl core::fmt::Debug for EmailSink {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Not derived: the transport holds the relay credentials.
        f.debug_struct("EmailSink").finish_non_exhaustive()
    }
}

impl EmailSink {
    /// Build one from the configured relay.
    ///
    /// # Errors
    ///
    /// A relay URL `lettre` cannot parse, or a `from` that is not a mailbox — both
    /// refused here, at startup, rather than at the first delivery. An operator who
    /// typed the address wrong finds out when the process starts, not when the first
    /// incident fires.
    pub fn new(settings: &SmtpSettings) -> PortResult<Self> {
        let from: Mailbox = settings
            .from
            .parse()
            .map_err(|error| PortError::corrupt("sinks.smtp.from", error))?;

        let mut builder = AsyncSmtpTransport::<Tokio1Executor>::from_url(settings.url.as_str())
            .map_err(|error| PortError::corrupt("sinks.smtp.url", error))?;

        if let Some(username) = &settings.username {
            builder = builder.credentials(Credentials::new(
                username.clone(),
                settings.password.clone().unwrap_or_default(),
            ));
        }

        Ok(Self {
            transport: builder.build(),
            from,
            permalink_base: settings.permalink_base.clone(),
        })
    }

    /// The mail one revision becomes.
    fn compose(
        &self,
        to: &str,
        delivery: &SinkDelivery,
        revision: &RevisionEvent,
    ) -> PortResult<Message> {
        let to: Mailbox = to
            .parse()
            .map_err(|error| PortError::corrupt("sink.email.address", error))?;

        // The channel in the subject, because somebody who gets mail from three
        // channels filters on it, and a subject line is the only thing a filter has.
        let subject = format!("[{}] {}", delivery.channel, revision.payload.title);

        Message::builder()
            .from(self.from.clone())
            .to(to)
            .subject(subject)
            .header(ContentType::TEXT_PLAIN)
            // The delivery id travels here too: a mail that arrives twice — the
            // at-least-once contract of §7.1bis — is recognisable as the same one.
            .message_id(Some(format!("<{}@weebo-notify>", delivery.delivery_id)))
            .body(self.render(delivery, revision))
            .map_err(|error| PortError::corrupt("sink.email.message", error))
    }

    /// The text of the mail.
    fn render(&self, delivery: &SinkDelivery, revision: &RevisionEvent) -> String {
        let mut out = String::new();

        if let Some(body) = &revision.payload.body {
            out.push_str(body);
            out.push_str("\n\n");
        }

        // Spelled the way the API spells them, by asking serde rather than by
        // writing a second vocabulary here that would drift the day one is renamed.
        let _ = write!(
            out,
            "state: {} · status: {}\nchannel: {}\n",
            word(&revision.state),
            word(&revision.status),
            delivery.channel
        );

        if let Some(base) = &self.permalink_base {
            // The web permalink of §10.4, which is also the desktop deep link's
            // mirror (§10bis.3quater) — one address that opens the right thing
            // wherever the reader happens to be.
            if let Ok(link) = base.join(&format!("/n/{}", revision.notification_id)) {
                let _ = write!(out, "\n{link}\n");
            }
        }

        out
    }
}

#[async_trait::async_trait]
impl SinkDispatcher for EmailSink {
    fn handles(&self) -> &'static [&'static str] {
        &["email"]
    }

    async fn deliver(
        &self,
        delivery: &SinkDelivery,
        target: &SinkTarget,
    ) -> PortResult<SinkOutcome> {
        let SinkTarget::Email { address } = target else {
            return Ok(SinkOutcome::Rejected {
                reason: "email dispatcher handed a target of another kind".to_owned(),
            });
        };

        let Some(revision) = revision_of(&delivery.event) else {
            // Nothing to write to a person. Reported as delivered rather than
            // rejected: it *is* finished with, and a dead letter would say something
            // is stuck when nothing is.
            tracing::debug!(
                delivery = %delivery.delivery_id,
                kind = delivery.event.kind().as_str(),
                "not mailing an event that is not a revision"
            );
            return Ok(SinkOutcome::Delivered);
        };

        let message = self.compose(address, delivery, revision)?;

        Ok(match self.transport.send(message).await {
            Ok(_) => SinkOutcome::Delivered,
            Err(error) if error.is_permanent() => SinkOutcome::Rejected {
                reason: error.to_string(),
            },
            Err(error) => SinkOutcome::Retry {
                reason: error.to_string(),
            },
        })
    }
}

/// One of the domain's plain enums as the wire spells it.
///
/// Empty rather than a placeholder if serde ever answers something other than a
/// string: a missing word in a mail is a cosmetic loss, and there is nothing here
/// worth failing a delivery over.
fn word<T: serde::Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(text)) => text,
        _ => String::new(),
    }
}

/// The revision inside an event, when it is one.
fn revision_of(event: &Event) -> Option<&RevisionEvent> {
    match &event.payload {
        EventPayload::Revision(revision) => Some(revision),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use notify_domain::{
        Author, ChannelId, ChannelPolicy, ContentDraft, DeliveryId, Notification, NotificationId,
        Principal, RevisionDraft, SinkId, State, UserId,
    };
    use notify_ports::{Audience, Event, EventPayload, RevisionEvent, SinkDelivery};
    use time::OffsetDateTime;

    use super::*;

    fn settings() -> SmtpSettings {
        SmtpSettings {
            url: url::Url::parse("smtp://localhost:1025").unwrap(),
            from: "weebo-notify <notify@weebo.poc>".to_owned(),
            username: None,
            password: None,
            permalink_base: Some(url::Url::parse("https://notify.weebo.poc").unwrap()),
        }
    }

    fn delivery(payload: EventPayload) -> SinkDelivery {
        SinkDelivery {
            delivery_id: DeliveryId::generate(),
            channel: ChannelId::parse("cluster-ovh-1").unwrap(),
            sink: SinkId::new("email", "oncall@weebo.poc").unwrap(),
            event: Event {
                cursor: notify_domain::Cursor::from_sequence(1),
                audience: Audience::Channel {
                    id: ChannelId::parse("cluster-ovh-1").unwrap(),
                },
                recorded_at: OffsetDateTime::UNIX_EPOCH,
                payload,
            },
            attempts: 0,
            first_failed_at: None,
        }
    }

    fn revision() -> RevisionEvent {
        let (notification, written) = Notification::create(
            NotificationId::generate(),
            ChannelId::parse("cluster-ovh-1").unwrap(),
            None,
            RevisionDraft::lifecycle(
                State::Open,
                ContentDraft {
                    body: Some("etcd has lost quorum".to_owned()),
                    ..ContentDraft::titled("etcd degraded")
                },
            ),
            Author::anonymous(Principal::User {
                id: UserId::generate(),
            }),
            OffsetDateTime::UNIX_EPOCH,
            &ChannelPolicy::default(),
        )
        .unwrap();

        RevisionEvent::new(&notification, &written.revision)
    }

    #[test]
    fn a_revision_becomes_a_mail_a_person_can_read() {
        let sink = EmailSink::new(&settings()).unwrap();
        let revision = revision();
        let delivery = delivery(EventPayload::Revision(Box::new(revision.clone())));

        let message = sink
            .compose("oncall@weebo.poc", &delivery, &revision)
            .unwrap();
        let rendered = String::from_utf8(message.formatted()).unwrap();

        assert!(
            rendered.contains("Subject: [cluster-ovh-1] etcd degraded"),
            "a subject line is the only thing a mail filter has"
        );
        assert!(rendered.contains("etcd has lost quorum"));
        assert!(
            rendered.contains("https://notify.weebo.poc/n/ntf_"),
            "§10.4 wants the mail to point back at the notification"
        );
    }

    #[test]
    fn without_a_public_address_no_link_is_invented() {
        let sink = EmailSink::new(&SmtpSettings {
            permalink_base: None,
            ..settings()
        })
        .unwrap();
        let revision = revision();
        let delivery = delivery(EventPayload::Revision(Box::new(revision.clone())));

        assert!(!sink.render(&delivery, &revision).contains("http"));
    }

    #[tokio::test]
    async fn an_event_that_is_not_a_revision_is_finished_with_rather_than_stuck() {
        let sink = EmailSink::new(&settings()).unwrap();
        let delivery = delivery(EventPayload::Tombstone {
            notification_id: NotificationId::generate(),
            reason: notify_domain::TombstoneReason::Expired,
        });

        // No SMTP server is listening in a unit test, so a delivery that *tried* to
        // send would fail — which is exactly what makes this assertion meaningful.
        let outcome = sink
            .deliver(
                &delivery,
                &SinkTarget::Email {
                    address: "oncall@weebo.poc".to_owned(),
                },
            )
            .await
            .unwrap();

        assert_eq!(outcome, SinkOutcome::Delivered);
    }
}
