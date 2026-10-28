//! Which JSON webhook transforms this instance serves (RFC 0003 §6.1):
//! [`TransformRoutes`], built from `[[transforms]]` and swapped in place on
//! `SIGHUP` (`adopt`), like [`crate::feeds::FeedRoutes`]. A request in flight
//! keeps the transform it started with.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use notify_ports::{Metrics, Transform, TransformRegistry};
use notify_transform_cel::{CelTransform, Slots};
use sha2::{Digest as _, Sha256};

use crate::config::Config;

/// Each transform with the digest of its declaration, for `transform_info`.
type Table = Arc<HashMap<String, (Arc<CelTransform>, String)>>;

/// The declared transforms, shared and swappable, over one set of evaluation slots.
#[derive(Clone)]
pub(crate) struct TransformRoutes {
    transforms: Arc<RwLock<Table>>,
    slots: Slots,
}

impl core::fmt::Debug for TransformRoutes {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TransformRoutes")
            .field("transforms", &self.current().len())
            .finish_non_exhaustive()
    }
}

impl TransformRoutes {
    /// Fails on what `Config::validate` refuses. Reports nothing: a set that is
    /// built is not yet the set that serves ([`Self::report`]).
    pub(crate) fn build(config: &Config, slots: Slots) -> anyhow::Result<Self> {
        let mut table = HashMap::new();
        for declared in &config.transforms {
            let transform = declared.compile(slots.clone())?;
            let mut hex = base16ct::lower::encode_string(&Sha256::digest(format!("{declared:?}")));
            hex.truncate(16);
            table.insert(declared.id.clone(), (Arc::new(transform), hex));
        }
        Ok(Self {
            transforms: Arc::new(RwLock::new(Arc::new(table))),
            slots,
        })
    }

    /// Publish the set now served to `transform_info`, replacing the last one.
    pub(crate) fn report(&self, metrics: &dyn Metrics) {
        let loaded: Vec<(String, String)> = self
            .current()
            .iter()
            .map(|(id, (_, hash))| (id.clone(), hash.clone()))
            .collect();
        metrics.transforms_loaded(&loaded);
    }

    /// These routes' slots, for the rebuild a `SIGHUP` makes: one cap per process.
    pub(crate) fn slots(&self) -> Slots {
        self.slots.clone()
    }

    /// Replace what every holder serves with `other`'s transforms.
    pub(crate) fn adopt(&self, other: &Self) {
        let table = other.current();
        match self.transforms.write() {
            Ok(mut slot) => *slot = table,
            Err(poisoned) => *poisoned.into_inner() = table,
        }
    }

    fn current(&self) -> Table {
        match self.transforms.read() {
            Ok(slot) => Arc::clone(&slot),
            Err(poisoned) => Arc::clone(&poisoned.into_inner()),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.current().len()
    }
}

impl TransformRegistry for TransformRoutes {
    fn get(&self, id: &str) -> Option<Arc<dyn Transform>> {
        self.current()
            .get(id)
            .map(|(transform, _)| Arc::clone(transform) as Arc<dyn Transform>)
    }
}

/// `notify-server transform try <id> <body.json>` (decision 14): the operator's own
/// terminal and file, so CEL's full messages are printed.
pub(crate) fn try_one(
    config_path: &std::path::Path,
    id: &str,
    body: &std::path::Path,
) -> anyhow::Result<()> {
    let config = Config::load(config_path)?;
    let declared = config
        .transforms
        .iter()
        .find(|transform| transform.id == id)
        .ok_or_else(|| anyhow::anyhow!("no [[transforms]] has id `{id}`"))?;
    let transform = declared.compile(Slots::new(1))?;
    let body = std::fs::read(body)?;
    let elements = transform
        .explain(&body)
        .map_err(|error| anyhow::anyhow!("{error}: nothing would be published"))?;

    println!("{} element(s)", elements.len());
    for (index, element) in elements.iter().enumerate() {
        let mapped = &element.mapped;
        let (external_id, digested) = match &mapped.external_id {
            Some(value) => (format!("{id}:{value}"), false),
            None => (
                format!("{id}:{}", notify_ports::element_digest(&mapped.element)),
                true,
            ),
        };
        println!("\n[{index}]");
        println!("  title        {:?}", mapped.title);
        println!("  body         {:?}", mapped.body);
        println!("  link         {:?}", mapped.link);
        println!(
            "  external_id  {external_id}{}",
            if digested {
                "  (digest of the element)"
            } else {
                ""
            }
        );
        println!("  group_key    {:?}", mapped.group_key);
        println!("  tags         {:?}", mapped.tags);
        println!("  priority     {:?}", mapped.priority);
        println!("  status       {:?}", mapped.status);
        println!("  state        {:?}", mapped.state);
        println!("  ack          {:?}", mapped.ack);
        for (field, message) in &element.errors {
            println!("  ! {field}: {message}");
        }
    }
    Ok(())
}
