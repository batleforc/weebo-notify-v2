use notify_domain::{Priority, State, Status};

use super::*;

const AUDIT: &str = include_str!("../../http-actix/tests/fixtures/mlab-audit.json");
const NEWS: &str = include_str!("../../http-actix/tests/fixtures/mlab-news.json");

/// The §1 news transform, with one field replaced.
fn news() -> TransformSpec {
    TransformSpec {
        id: "mlab-news".to_owned(),
        channels: vec![ChannelId::parse("security").unwrap()],
        split: Some("body.items".to_owned()),
        title: "item.title".to_owned(),
        body: Some(r#"item.source + " · " + item.published_at"#.to_owned()),
        link: Some("item.url".to_owned()),
        external_id: Some("item.id".to_owned()),
        group_key: Some(r#""mlab-news""#.to_owned()),
        tags: Some(r#"["news", item.source]"#.to_owned()),
        priority: Some(
            r#"(item.title.matches("CVE-\\d{4}-\\d+") && item.severity in ["high", "critical"])
                ? "high"
                : {"info": "low", "medium": "default", "high": "high", "critical": "urgent"}[item.severity]"#
                .to_owned(),
        ),
        status: Some(r#"item.severity in ["high", "critical"] ? "warning" : "info""#.to_owned()),
        state: Some(
            r#"(item.title.matches("CVE-\\d{4}-\\d+") && item.severity in ["high", "critical"]) ? "open" : null"#
                .to_owned(),
        ),
        ack: Some(r#"item.title.matches("CVE-") ? "Vu" : null"#.to_owned()),
        ..TransformSpec::default()
    }
}

fn audit() -> TransformSpec {
    TransformSpec {
        id: "mlab-audit".to_owned(),
        channels: vec![ChannelId::parse("security").unwrap()],
        title: r#"body.data.label + ": " + body.data.target"#.to_owned(),
        body: Some(r#""`" + body.event + "` · `" + body.data.action + "`""#.to_owned()),
        external_id: Some("body.delivery_id".to_owned()),
        tags: Some(r#"["audit", body.event]"#.to_owned()),
        status: Some(r#"body.data.action.endsWith(".deleted") ? "warning" : "info""#.to_owned()),
        raw: RawMode::Payload,
        ..TransformSpec::default()
    }
}

fn compile(spec: TransformSpec) -> CelTransform {
    CelTransform::compile(spec, Slots::new(1)).unwrap()
}

fn refused(spec: TransformSpec) -> String {
    CelTransform::compile(spec, Slots::new(1))
        .unwrap_err()
        .to_string()
}

async fn run(transform: &CelTransform, body: &str) -> Vec<Mapped> {
    transform.evaluate(body.as_bytes().to_vec()).await.unwrap()
}

#[tokio::test]
async fn the_audit_example_maps_the_real_event() {
    let mapped = run(&compile(audit()), AUDIT).await;

    let [one] = mapped.as_slice() else { panic!() };
    assert_eq!(one.title.as_deref(), Some("Webhook created: mlab-evt"));
    assert_eq!(one.status, Some(Status::Info));
    assert_eq!(one.tags, ["audit", "audit.webhooks"]);
    assert_eq!(
        one.external_id.as_deref(),
        Some("35d9f387-e22b-4174-99ef-e42abdf95f2a")
    );
    assert!(one.failed.is_empty(), "{:?}", one.failed);
    assert_eq!(one.element, *one.payload);
}

#[tokio::test]
async fn the_news_example_splits_and_decides_by_conditions() {
    let mapped = run(&compile(news()), NEWS).await;

    assert_eq!(mapped.len(), 3);
    let priorities: Vec<_> = mapped.iter().map(|m| m.priority).collect();
    assert_eq!(
        priorities,
        [
            Some(Priority::Low),
            Some(Priority::Low),
            Some(Priority::High)
        ]
    );
    let statuses: Vec<_> = mapped.iter().map(|m| m.status).collect();
    assert_eq!(
        statuses,
        [
            Some(Status::Info),
            Some(Status::Info),
            Some(Status::Warning)
        ]
    );
    assert!(mapped.iter().all(|m| m.state.is_none() && m.ack.is_none()));
    assert!(mapped.iter().all(|m| m.failed.is_empty()));
    assert_eq!(mapped[2].element["id"], "news-8814");
    assert_eq!(mapped[0].group_key.as_deref(), Some("mlab-news"));
}

#[tokio::test]
async fn a_cve_of_severity_high_stays_open_with_an_ack() {
    let body = r#"{"items":[{"id":"n1","title":"CVE-2026-1234 in libfoo","source":"s",
        "published_at":"x","url":"https://e.example/1","severity":"high"}]}"#;

    let mapped = run(&compile(news()), body).await;

    assert_eq!(mapped[0].priority, Some(Priority::High));
    assert_eq!(mapped[0].status, Some(Status::Warning));
    assert_eq!(mapped[0].state, Some(State::Open));
    assert_eq!(mapped[0].ack.as_deref(), Some("Vu"));
}

#[tokio::test]
async fn a_failing_field_is_unset_with_a_kind_that_quotes_nothing() {
    let spec = TransformSpec {
        body: Some("item.cve".to_owned()),
        link: Some("item.count + 1".to_owned()),
        group_key: Some("item.count".to_owned()),
        tags: Some(r#"["a", 1]"#.to_owned()),
        status: Some(r"item.level".to_owned()),
        ..news()
    };
    let body = r#"{"items":[{"id":"n1","title":"secret-8124fd73","count":3,"level":"loud",
        "severity":"info"}]}"#;

    let mapped = run(&compile(spec), body).await;

    let one = &mapped[0];
    assert_eq!(one.body, None);
    assert_eq!(one.link, None);
    assert_eq!(one.group_key, None);
    assert_eq!(one.tags, [] as [std::string::String; 0]);
    assert_eq!(one.status, None);
    assert_eq!(
        one.failed,
        [
            ("body", "no_such_key"),
            ("link", "type"),
            ("group_key", "type"),
            ("tags", "type"),
            ("status", "type"),
        ]
    );
}

#[tokio::test]
async fn null_unsets_without_failing_and_a_number_is_an_external_id() {
    let spec = TransformSpec {
        external_id: Some("item.n".to_owned()),
        state: Some("null".to_owned()),
        ..news()
    };
    let mapped = run(
        &compile(spec),
        r#"{"items":[{"n":42,"title":"t","severity":"info"}]}"#,
    )
    .await;

    assert_eq!(mapped[0].external_id.as_deref(), Some("42"));
    assert_eq!(mapped[0].state, None);
    assert!(!mapped[0].failed.iter().any(|(field, _)| *field == "state"));
}

#[tokio::test]
async fn a_body_made_of_cel_source_stays_text() {
    let spec = TransformSpec {
        split: None,
        title: "body.x".to_owned(),
        body: Some("body.y".to_owned()),
        ..audit()
    };
    let mapped = run(&compile(spec), r#"{"x":"body.y","y":"1 + 1"}"#).await;

    assert_eq!(mapped[0].title.as_deref(), Some("body.y"));
    assert_eq!(mapped[0].body.as_deref(), Some("1 + 1"));
}

#[tokio::test]
async fn shapes_that_cannot_be_split_are_refused_whole() {
    let transform = compile(TransformSpec {
        max_items: Some(2),
        ..news()
    });
    for (body, error) in [
        ("not json", TransformError::NotJson),
        (r#"{"items":{"a":1}}"#, TransformError::NotList),
        (
            r#"{"items":[{},{},{}]}"#,
            TransformError::TooMany { max: 2, actual: 3 },
        ),
    ] {
        assert_eq!(
            transform
                .evaluate(body.as_bytes().to_vec())
                .await
                .unwrap_err(),
            error,
            "{body}"
        );
    }
    let deep = format!("{}{}", "[".repeat(200), "]".repeat(200));
    assert_eq!(
        transform.evaluate(deep.into_bytes()).await.unwrap_err(),
        TransformError::NotJson
    );
    assert_eq!(
        run(&transform, r#"{"items":[]}"#).await,
        [] as [notify_ports::Mapped; 0]
    );
}

#[tokio::test]
async fn with_every_slot_taken_the_next_request_is_busy_without_evaluating() {
    let slots = Slots::new(1);
    let transform = CelTransform::compile(audit(), slots.clone()).unwrap();
    let _held = Arc::clone(&slots.0).try_acquire_owned().unwrap();

    assert_eq!(
        transform
            .evaluate(AUDIT.as_bytes().to_vec())
            .await
            .unwrap_err(),
        TransformError::Busy
    );
}

#[test]
fn a_typo_fails_at_load_with_its_column_and_the_quoting_hint() {
    let message = refused(TransformSpec {
        status: Some(r"item.severity ==".to_owned()),
        ..news()
    });
    assert!(message.contains("status: does not compile"), "{message}");
    assert!(message.contains(":1:"), "{message}");

    let unquoted = refused(TransformSpec {
        group_key: Some("mlab-news".to_owned()),
        ..news()
    });
    assert!(unquoted.contains("`mlab`"), "{unquoted}");
    assert!(unquoted.contains("needs CEL quotes"), "{unquoted}");
}

#[test]
fn only_body_and_item_are_in_scope() {
    assert!(
        refused(TransformSpec {
            split: None,
            max_items: None,
            ..news()
        })
        .contains("exists only with `split`")
    );
    assert!(
        refused(TransformSpec {
            title: "env.HOME".to_owned(),
            ..news()
        })
        .contains("`env`")
    );
}

#[test]
fn only_bounded_comprehensions_load() {
    for accepted in [
        r#"item.labels.filter(x, x.startsWith("cve"))"#,
        "body.l.map(x, x + 1u)",
        r#"body.l.exists(x, x == "nope") ? "a" : "b""#,
        "body.l.map(x, x).filter(y, y > 1u)",
    ] {
        let spec = TransformSpec {
            tags: Some(accepted.to_owned()),
            ..news()
        };
        CelTransform::compile(spec, Slots::new(1))
            .unwrap_or_else(|error| panic!("{accepted}: {error}"));
    }
    for (refused_source, why) in [
        ("body.l.filter(x, x in body.l)", "reads `body`"),
        (r"body.l.map(x, item.title + x)", "reads `item`"),
        ("body.l.map(x, x.map(y, y))", "nests"),
        (r#"body.l.filter(x, x.matches("a"))"#, "matches()"),
    ] {
        let message = refused(TransformSpec {
            tags: Some(refused_source.to_owned()),
            ..news()
        });
        assert!(message.contains(why), "{refused_source}: {message}");
    }
}

#[test]
fn the_seventeenth_body_reference_is_refused() {
    let sixteen = vec!["item.a"; 16].join(" + ");
    CelTransform::compile(
        TransformSpec {
            body: Some(sixteen.clone()),
            ..news()
        },
        Slots::new(1),
    )
    .unwrap();
    let message = refused(TransformSpec {
        body: Some(format!("{sixteen} + body.b")),
        ..news()
    });
    assert!(message.contains("more than 16"), "{message}");
}

#[test]
fn matches_needs_a_literal_pattern_that_compiles() {
    assert!(
        refused(TransformSpec {
            state: Some(r#"item.title.matches(item.pattern) ? "open" : null"#.to_owned()),
            ..news()
        })
        .contains("not a string literal")
    );
    assert!(
        refused(TransformSpec {
            state: Some(r#"item.title.matches("(") ? "open" : null"#.to_owned()),
            ..news()
        })
        .contains("does not compile")
    );
}

#[test]
fn a_misspelt_result_literal_is_refused_in_every_result_position() {
    for (field, source) in [
        ("priority", r#""hihg""#),
        ("priority", r#"item.x ? "high" : "hihg""#),
        ("priority", r#"{"a": "low", "b": "hihg"}[item.severity]"#),
        ("status", r#"item.x ? "warn" : "info""#),
        ("state", r#"item.x ? "opened" : null"#),
    ] {
        let mut spec = news();
        match field {
            "priority" => spec.priority = Some(source.to_owned()),
            "status" => spec.status = Some(source.to_owned()),
            _ => spec.state = Some(source.to_owned()),
        }
        let message = refused(spec);
        assert!(message.contains("is not a"), "{source}: {message}");
    }
}

#[test]
fn the_declaration_itself_is_checked() {
    for (spec, why) in [
        (
            TransformSpec {
                id: "Mlab".to_owned(),
                ..news()
            },
            "[a-z0-9-]",
        ),
        (
            TransformSpec {
                id: "feed".to_owned(),
                ..news()
            },
            "reserved",
        ),
        (
            TransformSpec {
                channels: Vec::new(),
                ..news()
            },
            "channels",
        ),
        (
            TransformSpec {
                max_items: Some(51),
                ..news()
            },
            "between 1 and 50",
        ),
        (
            TransformSpec {
                max_items: Some(5),
                ..audit()
            },
            "without `split`",
        ),
        (
            TransformSpec {
                raw: RawMode::Item,
                ..audit()
            },
            "needs `split`",
        ),
        (
            TransformSpec {
                title: "x".repeat(MAX_SOURCE_CHARS + 1),
                ..news()
            },
            "longer than",
        ),
    ] {
        let message = refused(spec);
        assert!(message.contains(why), "{message}");
    }
}

#[test]
fn try_shows_the_interpreters_own_message() {
    let transform = compile(TransformSpec {
        body: Some("item.cve".to_owned()),
        ..news()
    });

    let explained = transform.explain(NEWS.as_bytes()).unwrap();

    assert!(
        explained[0].errors[0].1.contains("cve"),
        "{:?}",
        explained[0].errors
    );
}

/// RFC 0003 §10.3: 50 elements, 7 programs each reading the body 16 times
/// through `in`, over a body near 256 KiB, stay under the 1 s a producer waits.
#[tokio::test]
#[cfg_attr(
    debug_assertions,
    ignore = "a timing: run with `cargo test --release` (about 0.7 s there)"
)]
async fn the_worst_accepted_request_evaluates_within_the_deadline() {
    let list: Vec<String> = (0..25_000).map(|n| format!("v{n:05}")).collect();
    let items: Vec<serde_json::Value> = (0..50)
        .map(|n| serde_json::json!({ "k": format!("v{n:05}") }))
        .collect();
    let body = serde_json::json!({ "items": items, "l": list }).to_string();
    assert!(body.len() < 256 * 1024 && body.len() > 200 * 1024);
    // `||` over a value that is absent, so every test scans the whole list.
    let sixteen = vec![r#"("absent" in body.l)"#; 16].join(" || ");
    let program = format!(r#"{sixteen} ? "a" : "b""#);
    let spec = TransformSpec {
        max_items: Some(50),
        title: program.clone(),
        body: Some(program.clone()),
        link: Some(program.clone()),
        external_id: Some(program.clone()),
        group_key: Some(program.clone()),
        ack: Some(program.clone()),
        tags: Some(format!("[{program}]")),
        ..news()
    };

    let started = std::time::Instant::now();
    let mapped = compile(spec).evaluate(body.into_bytes()).await.unwrap();

    assert_eq!(mapped.len(), 50);
    assert!(
        started.elapsed() < EVAL_TIMEOUT,
        "took {:?}",
        started.elapsed()
    );
}

/// A bounded comprehension is linear in its list (code review point 10, RFC 0003
/// §10.3): over a body near the 256 KiB cap it stays far under the deadline, and
/// four times the elements cost about four times as much, not sixteen.
#[tokio::test]
#[cfg_attr(
    debug_assertions,
    ignore = "a timing: run with `cargo test --release` (about 35 ms per program there)"
)]
async fn a_bounded_comprehension_is_linear_in_its_list() {
    let timed = |program: &str, n: usize| {
        let body = format!(r#"{{"a":[{}]}}"#, vec!["0"; n].join(","));
        let spec = TransformSpec {
            split: None,
            title: r#""t""#.to_owned(),
            tags: Some(program.to_owned()),
            ..audit()
        };
        let transform = CelTransform::compile(spec, Slots::new(1)).unwrap();
        let started = std::time::Instant::now();
        transform.explain(body.as_bytes()).unwrap();
        started.elapsed()
    };
    for program in [
        "body.a.map(x, x)",
        "body.a.filter(x, x == 0u)",
        "body.a.exists(x, x == 1u)",
    ] {
        // Best of three, so one scheduling hiccup does not decide the ratio.
        let best = |n| (0..3).map(|_| timed(program, n)).min().unwrap();
        let small = best(30_000);
        let large = best(120_000);
        assert!(
            large < std::time::Duration::from_millis(250),
            "{program}: {large:?} for 120 000 elements"
        );
        assert!(
            large < small * 8,
            "{program}: {small:?} for 30 000, {large:?} for 120 000: not linear"
        );
    }
}

/// Strings too (the code review's point 10, asked again for `x + x`): a step that
/// concatenates its own variable is linear in the list.
#[tokio::test]
#[cfg_attr(
    debug_assertions,
    ignore = "a timing: run with `cargo test --release` (about 30 ms there)"
)]
async fn a_string_step_is_linear_in_its_list() {
    let timed = |n: usize| {
        let body = format!(r#"{{"s":[{}]}}"#, vec![r#""ab""#; n].join(","));
        let spec = TransformSpec {
            split: None,
            title: r#""t""#.to_owned(),
            tags: Some("body.s.map(x, x + x)".to_owned()),
            ..audit()
        };
        let transform = CelTransform::compile(spec, Slots::new(1)).unwrap();
        (0..3)
            .map(|_| {
                let started = std::time::Instant::now();
                transform.explain(body.as_bytes()).unwrap();
                started.elapsed()
            })
            .min()
            .unwrap()
    };
    let small = timed(12_000);
    let large = timed(48_000);
    assert!(large < std::time::Duration::from_millis(250), "{large:?}");
    assert!(large < small * 8, "{small:?} then {large:?}: not linear");
}

/// Every `+` recopies what it built so far, so the terms a step may read are
/// bounded: 256 of them over three 64 KiB strings took 1.6 s, and the variable a
/// comprehension binds now counts against the 16 reads as `body` does.
#[test]
fn a_step_may_not_read_its_variable_more_than_sixteen_times() {
    let terms = |n: usize| format!("body.s.map(x, {})", vec!["x"; n].join(" + "));
    CelTransform::compile(
        TransformSpec {
            tags: Some(terms(15)),
            ..news()
        },
        Slots::new(1),
    )
    .unwrap();
    let message = refused(TransformSpec {
        tags: Some(terms(17)),
        ..news()
    });
    assert!(message.contains("more than 16"), "{message}");
}

/// A thousand `1u + …` passed every other check and overflowed the evaluating
/// thread's stack, which aborts the whole process: depth is refused at load.
#[test]
fn an_expression_deeper_than_the_limit_is_refused_and_the_limit_evaluates() {
    let chain = |n: usize| format!("[string({})]", vec!["1u"; n].join("+"));
    let message = refused(TransformSpec {
        tags: Some(chain(1_000)),
        ..news()
    });
    assert!(message.contains("deeper than"), "{message}");

    // The deepest chain that loads still evaluates, on a small stack.
    let deepest = (1..MAX_DEPTH)
        .rev()
        .find(|n| {
            CelTransform::compile(
                TransformSpec {
                    tags: Some(chain(*n)),
                    ..news()
                },
                Slots::new(1),
            )
            .is_ok()
        })
        .unwrap();
    let transform = compile(TransformSpec {
        title: r#""t""#.to_owned(),
        tags: Some(chain(deepest)),
        ..audit()
    });
    // Through `evaluate`, so on tokio's blocking pool and its default stack, as served.
    let evaluated = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(transform.evaluate(br#"{"a":1}"#.to_vec()));
    assert_eq!(evaluated.map(|elements| elements.len()), Ok(1));
}

/// For each shape, the deepest expression the load checks accept compiles on an
/// 8 MiB thread (the server compiles on its main thread: boot, `SIGHUP`, `transform
/// try`) and evaluates through `evaluate`, on tokio's blocking pool and its 2 MiB
/// stacks. Measured on 2026-10-28 straight through `cel`: in release nothing
/// overflowed up to 512 levels (the parser refuses past 96 with an error), only a
/// chain of `+` between 512 and 1 000; in a debug build the parser itself overflows
/// a 2 MiB thread at about 12 nested brackets, ternaries or calls.
#[test]
fn brackets_inside_a_literal_do_not_count_as_nesting() {
    assert_eq!(super::bracket_depth(r#"f("((((", '[[') + [1]"#), 1);
    assert_eq!(super::bracket_depth(r#""a\"(" + (1)"#), 1);
    assert_eq!(super::bracket_depth("((([x])))"), 4);
    // Raw: the backslash escapes nothing, so the quote after it closes the literal.
    assert_eq!(super::bracket_depth(r#"r"\" + f((1))"#), 2);
    assert_eq!(super::bracket_depth(r"R'\' + f((1))"), 2);
    assert_eq!(super::bracket_depth(r#"br"\" + f((1))"#), 2);
    assert_eq!(super::bracket_depth(r"bR'((' + [1]"), 1);
    // `rb` is not a CEL prefix (the parser refuses it): `b'…'` is an ordinary literal.
    assert_eq!(super::bracket_depth(r#"rb"\" + f((1))" + [1]"#), 1);
    // Not raw: `\"` stays inside, and so does everything up to the real end.
    assert_eq!(super::bracket_depth(r#""\" + f((1))" + [1]"#), 1);
    // An identifier ending in `r` is not a prefix.
    assert_eq!(super::bracket_depth(r#"f(bar"\" + ((1)))"#), 1);
    // Triple quotes end at three quotes only, raw or not.
    assert_eq!(super::bracket_depth(r#""""a"(b""" + (1)"#), 1);
    assert_eq!(super::bracket_depth(r"r'''\'(('''+ [1]"), 1);
    assert_eq!(super::bracket_depth(r"'''it's ((''' + [(1)]"), 2);
    // Unterminated: the rest is literal, as the parser will say.
    assert_eq!(super::bracket_depth(r#"(1) + "(("#), 1);
}

#[test]
fn the_deepest_accepted_expression_of_every_shape_evaluates() {
    /// A shape's name, and its source at a given depth.
    type Shape = (&'static str, fn(usize) -> String);
    let shapes: [Shape; 7] = [
        ("add", |n| format!("[string({})]", vec!["1u"; n].join("+"))),
        ("ternary", |n| {
            format!("[{}\"t\"{}]", "true ? (".repeat(n), ") : \"f\"".repeat(n))
        }),
        ("call", |n| {
            format!("[{}\"t\"{}]", "string(".repeat(n), ")".repeat(n))
        }),
        ("list", |n| {
            format!("{}[\"t\"]{}", "[".repeat(n), "][0]".repeat(n))
        }),
        ("map", |n| {
            format!("[{}\"t\"{}]", "{\"k\": ".repeat(n), "}[\"k\"]".repeat(n))
        }),
        ("and", |n| {
            format!("[string({})]", vec!["true"; n].join(" && "))
        }),
        ("chain", |n| {
            format!("body.l{}", ".map(x, string(x))".repeat(n))
        }),
    ];
    for (shape, source) in shapes {
        let accepts = |n: usize| {
            let spec = TransformSpec {
                title: r#""t""#.to_owned(),
                tags: Some(source(n)),
                ..audit()
            };
            std::thread::Builder::new()
                .stack_size(8 * 1024 * 1024)
                .spawn(move || CelTransform::compile(spec, Slots::new(1)).ok())
                .unwrap()
                .join()
                .unwrap()
        };
        let (deepest, transform) = (1..=MAX_DEPTH)
            .rev()
            .find_map(|n| accepts(n).map(|transform| (n, transform)))
            .unwrap_or_else(|| panic!("{shape}: nothing loads"));
        // `&&` and `||` build a balanced tree: any length stays shallow.
        if shape != "and" {
            assert!(
                accepts(MAX_DEPTH * 4).is_none(),
                "{shape}: far too deep, loaded"
            );
        }

        let evaluated = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(transform.evaluate(br#"{"l":[0,1]}"#.to_vec()));
        let elements = evaluated.unwrap_or_else(|error| panic!("{shape} at {deepest}: {error}"));
        assert_eq!(elements.len(), 1, "{shape} at {deepest}");
        // The other fields read keys this body does not have; the one measured is `tags`.
        assert!(
            !elements[0].failed.iter().any(|(field, _)| *field == "tags"),
            "{shape} at {deepest}: {:?}",
            elements[0].failed
        );
        assert!(!elements[0].tags.is_empty(), "{shape} at {deepest}");
    }
}

/// One shape at one depth, on a 2 MiB thread like tokio's blocking pool, straight
/// through `cel` (no load checks). A stack overflow aborts, so one process per run:
/// `PROBE_SHAPE=ternary PROBE_N=32 cargo test probe_shape -- --ignored`.
#[test]
#[ignore = "probe"]
fn probe_shape() {
    let (Ok(shape), Ok(n)) = (std::env::var("PROBE_SHAPE"), std::env::var("PROBE_N")) else {
        return;
    };
    let n: usize = n.parse().unwrap();
    let source = match shape.as_str() {
        "add" => vec!["1u"; n].join("+"),
        "ternary" => format!("{}0u{}", "true ? (".repeat(n), ") : 0u".repeat(n)),
        "ternary_flat" => format!("{}0u", "true ? 0u : ".repeat(n)),
        "call" => format!("{}1u{}", "int(".repeat(n), ")".repeat(n)),
        "not" => format!("{}true", "!".repeat(n)),
        "list" => format!("{}1u{}", "[".repeat(n), "]".repeat(n)),
        "map" => format!("{}1u{}", "{\"k\": ".repeat(n), "}".repeat(n)),
        "select" => format!("body{}", ".a".repeat(n)),
        "and" => vec!["true"; n].join(" && "),
        "chain" => format!("body.l{}", ".map(x, x)".repeat(n)),
        "chain_filter" => format!("body.l{}", ".filter(x, x == 0u)".repeat(n)),
        "chain_exists" => {
            format!(
                "body.l{}.exists(x, x == 1u)",
                ".map(x, x)".repeat(n.saturating_sub(1))
            )
        }
        other => panic!("{other}"),
    };
    let mut nested = serde_json::json!(1);
    for _ in 0..n.min(120) {
        nested = serde_json::json!({ "a": nested });
    }
    let body = serde_json::json!({ "l": [0, 0, 0], "a": nested["a"].clone() });
    let ok = std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(move || {
            let env = std::sync::Arc::new(cel::Env::stdlib());
            let program = env.compile(&source).map_err(|e| {
                eprintln!(
                    "PROBE compile error: {}",
                    e.to_string().lines().next().unwrap_or("")
                );
                e.to_string()
            })?;
            eprintln!("PROBE compiled");
            let mut context = cel::Context::with_env(env);
            context.add_variable_from_value("body", cel::to_value(&body).unwrap());
            program
                .execute(&context)
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
        .unwrap()
        .join()
        .unwrap();
    eprintln!(
        "PROBE {shape} {n} {}",
        if ok.is_ok() { "ok" } else { "error" }
    );
}
