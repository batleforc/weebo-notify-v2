//! A JSON webhook body is whatever its producer sends (RFC 0003 T12): evaluating
//! the documented transforms over it returns, and never panics.
#![no_main]

use std::sync::LazyLock;

use libfuzzer_sys::fuzz_target;
use notify_domain::ChannelId;
use notify_transform_cel::{CelTransform, Slots, TransformSpec};

static TRANSFORMS: LazyLock<Vec<CelTransform>> = LazyLock::new(|| {
    let Ok(channel) = ChannelId::parse("security") else {
        return Vec::new();
    };
    let audit = TransformSpec {
        id: "mlab-audit".to_owned(),
        channels: vec![channel.clone()],
        title: r#"body.data.label + ": " + body.data.target"#.to_owned(),
        external_id: Some("body.delivery_id".to_owned()),
        tags: Some(r#"["audit", body.event]"#.to_owned()),
        status: Some(r#"body.data.action.endsWith(".deleted") ? "warning" : "info""#.to_owned()),
        ..TransformSpec::default()
    };
    let news = TransformSpec {
        id: "mlab-news".to_owned(),
        channels: vec![channel],
        split: Some("body.items".to_owned()),
        title: "item.title".to_owned(),
        external_id: Some("item.id".to_owned()),
        priority: Some(
            r#"(item.title.matches("CVE-\\d{4}-\\d+") && item.severity in ["high", "critical"])
                ? "high"
                : {"info": "low", "medium": "default", "high": "high", "critical": "urgent"}[item.severity]"#
                .to_owned(),
        ),
        tags: Some("item.labels.filter(x, x.startsWith(\"cve\"))".to_owned()),
        ..TransformSpec::default()
    };
    [audit, news]
        .into_iter()
        .filter_map(|spec| CelTransform::compile(spec, Slots::new(1)).ok())
        .collect()
});

fuzz_target!(|body: &[u8]| {
    for transform in TRANSFORMS.iter() {
        if let Ok(elements) = transform.explain(body) {
            assert!(elements.len() <= notify_transform_cel::MAX_MAX_ITEMS);
        }
    }
});
