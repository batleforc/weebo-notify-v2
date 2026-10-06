//! `notify publish` and `notify update`: argument parsing and rendering only (§9.3).

use std::io::Read as _;

use clap::{Args, ValueEnum};
use notify_client_core::types::{
    Action, ActionKey, Actions, Image, Priority, Published, State, Status, Ttl,
};
use notify_client_core::{Revision, Session};

use crate::{CliError, OutputFormat};

/// The fields both commands share.
#[derive(Debug, Args)]
pub(crate) struct Content {
    /// Headline. Required when creating; omit on an update to keep the current one.
    #[arg(long, short = 't')]
    pub title: Option<String>,

    /// Markdown body; `-` reads it from stdin.
    #[arg(long, short = 'b')]
    pub body: Option<String>,

    /// Lifecycle position; supplying it at creation makes the notification a lifecycle one (§2.2).
    #[arg(long, value_enum)]
    pub state: Option<StateArg>,

    /// Outcome.
    #[arg(long, value_enum)]
    pub status: Option<StatusArg>,

    /// How loudly.
    #[arg(long, value_enum)]
    pub priority: Option<PriorityArg>,

    /// Retention, e.g. `48h`, `7d`, or `0` for forever where the channel allows it.
    #[arg(long)]
    pub ttl: Option<String>,

    /// A tag. Repeat the flag for several.
    #[arg(long = "tag")]
    pub tags: Vec<String>,

    /// A button, as `type:label:argument`. Repeat for several, up to five.
    ///
    /// `view:Runbook:https://…` opens a link, `ack:Ack:incident` acknowledges,
    /// `reply:Comment:Notes…` asks for text (the placeholder is optional).
    #[arg(long = "action", value_name = "TYPE:LABEL:ARG")]
    pub actions: Vec<String>,

    /// An image to inline, e.g. `./graph.png`.
    #[arg(long, value_name = "PATH")]
    pub image: Option<std::path::PathBuf>,
}

/// One `--action` flag, split into at most three parts so the argument (usually a URL) may
/// contain colons; a label cannot.
pub(crate) fn parse_action(raw: &str) -> Result<Action, CliError> {
    let mut parts = raw.splitn(3, ':');
    let kind = parts.next().unwrap_or_default();
    let label = parts.next().unwrap_or_default().to_owned();
    let argument = parts.next();

    if label.trim().is_empty() {
        return Err(CliError::Usage(format!(
            "`--action {raw}`: every button needs a label — `type:label:argument`"
        )));
    }

    match kind {
        "view" => {
            let url = argument.ok_or_else(|| {
                CliError::Usage(format!("`--action {raw}`: a `view` button needs a URL"))
            })?;
            // The scheme is checked by the server and by the opening client (§2.3).
            Ok(Action::View {
                label,
                url: url.to_owned(),
            })
        }
        "ack" => {
            let key = argument.ok_or_else(|| {
                CliError::Usage(format!(
                    "`--action {raw}`: an `ack` button needs a key — it is what makes \
                     it idempotent per user (§2.3)"
                ))
            })?;
            Ok(Action::Ack {
                key: ActionKey::from(key.to_owned()),
                label,
            })
        }
        "reply" => Ok(Action::Reply {
            label,
            placeholder: argument.map(ToOwned::to_owned),
        }),
        // `choice` and `silence` need structured fields; they go through the API (§2.3).
        "choice" | "silence" => Err(CliError::Usage(format!(
            "`--action {raw}`: `{kind}` carries more than a label and one argument, \
             so it has no flag spelling — publish it through the API (§2.3)"
        ))),
        other => Err(CliError::Usage(format!(
            "`--action {raw}`: `{other}` is not an action type; \
             expected view, ack or reply"
        ))),
    }
}

/// Turn a file into what a payload carries: inline under the 48 KiB cap (§2.3), uploaded above it.
async fn resolve_image(
    session: &Session,
    channel: &str,
    path: &std::path::Path,
) -> Result<Image, CliError> {
    match read_image(path)? {
        Inlined::Small(image) => Ok(image),
        Inlined::TooBig {
            bytes,
            content_type,
        } => {
            let id = session
                .upload_attachment(channel, content_type, bytes)
                .await?;
            Ok(Image::Attachment { ref_: id })
        }
    }
}

/// Parse every `--action` flag; `None` without flags, since §2.3 replaces actions wholesale
/// and an empty list would clear them on an unrelated update.
fn fold_actions(raw: &[String]) -> Result<Option<Actions>, CliError> {
    if raw.is_empty() {
        return Ok(None);
    }

    let parsed = raw
        .iter()
        .map(|one| parse_action(one))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some(Actions::from(parsed)))
}

/// What reading a file decided.
enum Inlined {
    /// Small enough to travel in the payload.
    Small(Image),
    /// Past the cap: these bytes need the bucket.
    TooBig {
        bytes: Vec<u8>,
        content_type: &'static str,
    },
}

/// Read an image and say which side of the inline cap it falls on (design §2.3).
fn read_image(path: &std::path::Path) -> Result<Inlined, CliError> {
    use base64::Engine as _;

    let bytes = std::fs::read(path)
        .map_err(|error| CliError::Usage(format!("could not read {}: {error}", path.display())))?;

    let media_type = match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        // Guessed from the name: the server decides what it accepts.
        other => {
            return Err(CliError::Usage(format!(
                "{}: unsupported image extension {}",
                path.display(),
                other.unwrap_or("(none)")
            )));
        }
    };

    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let uri = format!("data:{media_type};base64,{encoded}");

    // The cap is on the base64 URI that travels, not the file.
    if uri.len() > MAX_DATA_URI_BYTES {
        return Ok(Inlined::TooBig {
            bytes,
            content_type: media_type,
        });
    }

    Ok(Inlined::Small(Image::DataUri { uri }))
}

/// What §2.3 allows inline (mirrors `notify_domain::limits::MAX_DATA_URI_BYTES`).
const MAX_DATA_URI_BYTES: usize = 48 * 1024;

impl Content {
    /// Turn the flags into a revision, reading stdin and uploading `--image` if needed (§6.1).
    pub(crate) async fn into_revision(
        self,
        session: &Session,
        channel: &str,
    ) -> Result<Revision, CliError> {
        let body = match self.body.as_deref() {
            Some("-") => Some(read_stdin()?),
            other => other.map(ToOwned::to_owned),
        };

        let image = match self.image.as_deref() {
            Some(path) => Some(resolve_image(session, channel, path).await?),
            None => None,
        };
        Ok(Revision {
            title: self.title,
            body,
            state: self.state.map(Into::into),
            status: self.status.map(Into::into),
            priority: self.priority.map(Into::into),
            ttl: self.ttl.map(Ttl::from),
            tags: self.tags,
            actions: fold_actions(&self.actions)?,
            image,
            ..Revision::default()
        })
    }
}

fn read_stdin() -> Result<String, CliError> {
    let mut buffer = String::new();
    std::io::stdin()
        .read_to_string(&mut buffer)
        .map_err(|error| CliError::Usage(format!("could not read the body from stdin: {error}")))?;
    Ok(buffer)
}

/// Create a notification.
pub(crate) async fn publish(
    session: &Session,
    channel: &str,
    content: Content,
    idempotency_key: Option<&str>,
    output: OutputFormat,
) -> Result<String, CliError> {
    let published = session
        .publish(
            channel,
            content.into_revision(session, channel).await?,
            idempotency_key,
        )
        .await?;

    render(&published, output)
}

/// Create or revise by the publisher's own identifier.
pub(crate) async fn update(
    session: &Session,
    channel: &str,
    external_id: &str,
    content: Content,
    output: OutputFormat,
) -> Result<String, CliError> {
    let published = session
        .update(
            channel,
            external_id,
            content.into_revision(session, channel).await?,
        )
        .await?;

    render(&published, output)
}

fn render(published: &Published, output: OutputFormat) -> Result<String, CliError> {
    match output {
        OutputFormat::Json => {
            // The whole response, verbatim.
            serde_json::to_string_pretty(published)
                .map_err(|error| CliError::Api(format!("could not render the response: {error}")))
        }
        OutputFormat::Table => {
            let mut out = format!(
                "{}\n  channel   {}\n  state     {:?} / {:?}\n  revision  {}\n  cursor    {}",
                published.notification.id,
                published.notification.channel,
                published.revision.state,
                published.revision.status,
                published.revision.seq,
                published.cursor,
            );

            // Surface a clamp the publisher couldn't otherwise see (§7.2bis, T6).
            if let Some(requested) = &published.retention.clamped_from {
                use std::fmt::Write as _;

                let _ = write!(
                    out,
                    "\n  ttl       {} — clamped from the {} you asked for",
                    published.retention.ttl,
                    // progenitor's string newtype derefs to `String` but has no `Display`.
                    **requested
                );
            }
            if published.replayed {
                out.push_str(
                    "\n  replayed  yes — this is the stored answer to an earlier identical call",
                );
            }

            Ok(out)
        }
    }
}

/// The constrained values as clap sees them; the mapping is exhaustive so a new domain value
/// fails to compile here (§9.1).
macro_rules! value_enum {
    ($name:ident => $target:path { $($variant:ident),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, ValueEnum)]
        #[value(rename_all = "snake_case")]
        pub(crate) enum $name {
            $($variant),+
        }

        impl From<$name> for $target {
            fn from(value: $name) -> Self {
                match value {
                    $($name::$variant => <$target>::$variant),+
                }
            }
        }
    };
}

value_enum!(StateArg => State { Open, Working, Closed });
value_enum!(StatusArg => Status { Success, Failure, Warning, Info, Cancelled });
value_enum!(PriorityArg => Priority { Min, Low, Default, High, Urgent });

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use super::*;

    #[test]
    fn a_view_button_keeps_the_colons_in_its_url() {
        let action = parse_action("view:Voir le run:https://forge.example/runs/42").unwrap();

        assert!(matches!(
            action,
            Action::View { ref label, ref url }
                if label == "Voir le run" && url == "https://forge.example/runs/42"
        ));
    }

    #[test]
    fn an_ack_without_a_key_is_refused_rather_than_defaulted() {
        // A generated key would make the button idempotent per invocation, not per user (§2.3).
        assert!(parse_action("ack:Ack").is_err());
        assert!(parse_action("ack:Ack:incident-4521").is_ok());
    }

    #[test]
    fn a_reply_placeholder_is_optional() {
        assert!(matches!(
            parse_action("reply:Comment").unwrap(),
            Action::Reply {
                placeholder: None,
                ..
            }
        ));
        assert!(matches!(
            parse_action("reply:Comment:Notes d'incident").unwrap(),
            Action::Reply {
                placeholder: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn the_two_shapes_a_flag_cannot_express_say_so() {
        for raw in ["choice:Escalade:oncall", "silence:Silence 4h:group"] {
            let error = parse_action(raw).unwrap_err().to_string();
            assert!(
                error.contains("API"),
                "a refusal has to say where the shape does fit: {error}"
            );
        }
    }

    #[test]
    fn a_button_with_no_label_is_refused() {
        assert!(parse_action("view::https://example.com").is_err());
        assert!(parse_action("view").is_err());
    }

    #[test]
    fn an_unknown_type_names_the_ones_that_exist() {
        let error = parse_action("explode:Boom:now").unwrap_err().to_string();
        assert!(error.contains("view"), "{error}");
    }

    #[test]
    fn no_action_flag_leaves_the_buttons_alone() {
        assert!(fold_actions(&[]).unwrap().is_none());
        assert_eq!(
            fold_actions(&["view:Run:https://example.com".to_owned()])
                .unwrap()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn an_image_too_big_to_inline_becomes_an_upload() {
        let dir = std::env::temp_dir().join(format!("wnk-img-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("big.png");
        std::fs::write(&path, vec![0u8; 64 * 1024]).unwrap();

        // Past the cap the bytes come back for the bucket (§2.3).
        assert!(matches!(read_image(&path).unwrap(), Inlined::TooBig { .. }));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_small_image_inlines_with_its_media_type() {
        let dir = std::env::temp_dir().join(format!("wnk-img-ok-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("graph.png");
        std::fs::write(&path, b"not really a png, but small").unwrap();

        let inlined = read_image(&path).unwrap();

        assert!(
            matches!(
                inlined,
                Inlined::Small(Image::DataUri { ref uri })
                    if uri.starts_with("data:image/png;base64,")
            ),
            "a small file inlines, carrying the media type its extension implies"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
