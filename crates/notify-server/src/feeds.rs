//! Which feeds this instance polls (RFC 0002 §6.1): [`FeedRoutes`], built from
//! `[[channels.managed.feeds]]` and swapped in place on `SIGHUP` (`adopt`), like
//! [`crate::sinks::SinkRoutes`].

use std::sync::{Arc, RwLock};

use notify_domain::{ChannelId, ServiceAccountId};
use notify_ports::{DeclaredFeed, FeedLang, FeedRegistry};

use crate::config::{self, Config};

/// The declared feeds, shared and swappable.
#[derive(Clone, Default)]
pub(crate) struct FeedRoutes {
    feeds: Arc<RwLock<Arc<Vec<DeclaredFeed>>>>,
}

impl core::fmt::Debug for FeedRoutes {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Not derived: a url may carry a feed token.
        f.debug_struct("FeedRoutes")
            .field("feeds", &self.current().len())
            .finish()
    }
}

impl FeedRoutes {
    /// Fails on what `Config::validate` refuses, or without a fingerprint key.
    pub(crate) fn build(config: &Config) -> anyhow::Result<Self> {
        let mut feeds = Vec::new();
        let mut key = None;
        for channel in &config.channels.managed {
            let id = ChannelId::parse(&channel.id)?;
            for (position, feed) in channel.feeds.iter().enumerate() {
                let url = feed.validate(&channel.id, position)?;
                let key = match &mut key {
                    Some(key) => key,
                    None => key.insert(config.sinks.fingerprint_key()?),
                };
                let redacted = notify_ports::redact_url(&url, key);
                if feed.allow_private_network {
                    tracing::warn!(
                        channel = %channel.id,
                        feed = position,
                        url = %redacted,
                        "a feed may fetch private addresses (allow_private_network)"
                    );
                }
                feeds.push(DeclaredFeed {
                    channel: id.clone(),
                    position,
                    url,
                    redacted,
                    publisher: ServiceAccountId::parse(&feed.publisher)?,
                    every: time::Duration::try_from(feed.every())?,
                    tags: feed.tags.clone(),
                    max_per_poll: feed.max_per_poll(),
                    allow_private_network: feed.allow_private_network,
                    lang: match feed.lang {
                        config::FeedLang::En => FeedLang::En,
                        config::FeedLang::Fr => FeedLang::Fr,
                    },
                });
            }
        }
        Ok(Self {
            feeds: Arc::new(RwLock::new(Arc::new(feeds))),
        })
    }

    /// Replace what every holder polls with `other`'s feeds — what a `SIGHUP` does.
    pub(crate) fn adopt(&self, other: &Self) {
        let feeds = other.current();
        match self.feeds.write() {
            Ok(mut slot) => *slot = feeds,
            Err(poisoned) => *poisoned.into_inner() = feeds,
        }
    }

    fn current(&self) -> Arc<Vec<DeclaredFeed>> {
        match self.feeds.read() {
            Ok(slot) => Arc::clone(&slot),
            Err(poisoned) => Arc::clone(&poisoned.into_inner()),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.current().len()
    }
}

impl FeedRegistry for FeedRoutes {
    fn feeds(&self) -> Vec<DeclaredFeed> {
        self.current().as_ref().clone()
    }
}
