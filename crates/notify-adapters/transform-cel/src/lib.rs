//! JSON webhook transforms in CEL (RFC 0003 §6.4).
//!
//! Every program is compiled and walked at load: only `body` (and `item` with
//! `split`) is in scope, a comprehension is never nested and its step reads only
//! its own variables, at most [`MAX_REFERENCES`] reads of the body per program,
//! and every `matches()` pattern is a literal that compiles. Under those rules a
//! program's cost is linear in what it reads (§10.3), so evaluation needs no step
//! budget: it runs on a blocking thread, one per core at most ([`Slots`]), and the
//! caller waits [`EVAL_TIMEOUT`] at most. The body is only ever a CEL value, never
//! source; an evaluation error keeps only a fixed kind, since CEL's text quotes it.

use std::sync::Arc;
use std::time::Duration;

use cel::common::ast::{EntryExpr, Expr, IdedExpr, LiteralValue};
use cel::{Context, Env, ExecutionError, Program, Value};
use notify_domain::ChannelId;
use notify_ports::{FeedLang, Mapped, RawMode, Transform, TransformError};
use tokio::sync::Semaphore;

/// How long a caller waits for an evaluation (decision 9).
pub const EVAL_TIMEOUT: Duration = Duration::from_secs(1);

/// Reads of `body`, `item` and a comprehension's own variables one program may
/// make: bounds what a concatenation builds, and each `+` recopies what it has so far.
pub const MAX_REFERENCES: usize = 16;

/// The deepest expression tree: evaluation recurses once per level, and too many
/// `+` in a row overflow a blocking thread's 2 MiB stack, which aborts the process.
/// Measured on 2026-10-28: between 300 and 1 000 levels in release, about 60 in a debug
/// build; 32 keeps twice that margin in the worst build.
pub const MAX_DEPTH: usize = 32;

/// The longest program source.
pub const MAX_SOURCE_CHARS: usize = 4_096;

/// `max_items` when `split` is set and it is not.
pub const DEFAULT_MAX_ITEMS: usize = 20;

/// The highest `max_items` (decision 3).
pub const MAX_MAX_ITEMS: usize = 50;

/// What a `[[transforms]]` entry declares, before compiling.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TransformSpec {
    pub id: String,
    pub channels: Vec<ChannelId>,
    pub lang: FeedLang,
    pub split: Option<String>,
    pub max_items: Option<usize>,
    pub title: String,
    pub title_fallback: Option<String>,
    pub body: Option<String>,
    pub link: Option<String>,
    pub external_id: Option<String>,
    pub group_key: Option<String>,
    pub tags: Option<String>,
    pub priority: Option<String>,
    pub status: Option<String>,
    pub state: Option<String>,
    pub ack: Option<String>,
    pub update: bool,
    pub raw: RawMode,
}

/// A transform that does not load, with what to fix.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("[[transforms]] `{id}` {field}: {message}")]
pub struct CompileError {
    pub id: String,
    pub field: &'static str,
    pub message: String,
}

/// The evaluation slots every transform shares: one evaluation per slot at a time.
#[derive(Debug, Clone)]
pub struct Slots(Arc<Semaphore>);

impl Slots {
    /// One per core.
    #[must_use]
    pub fn per_core() -> Self {
        Self::new(std::thread::available_parallelism().map_or(1, std::num::NonZero::get))
    }

    #[must_use]
    pub fn new(slots: usize) -> Self {
        Self(Arc::new(Semaphore::new(slots)))
    }
}

/// The fields a program may set, in evaluation order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Title,
    Body,
    Link,
    ExternalId,
    GroupKey,
    Tags,
    Priority,
    Status,
    State,
    Ack,
}

impl Field {
    const fn name(self) -> &'static str {
        match self {
            Self::Title => "title",
            Self::Body => "body",
            Self::Link => "link",
            Self::ExternalId => "external_id",
            Self::GroupKey => "group_key",
            Self::Tags => "tags",
            Self::Priority => "priority",
            Self::Status => "status",
            Self::State => "state",
            Self::Ack => "ack",
        }
    }

    /// Whether `text` is a value of this field's enum, for the fields that have one.
    fn accepts(self, text: &str) -> bool {
        let text = text.to_owned();
        match self {
            Self::Priority => variant::<notify_domain::Priority>(text).is_ok(),
            Self::Status => variant::<notify_domain::Status>(text).is_ok(),
            Self::State => variant::<notify_domain::State>(text).is_ok(),
            _ => true,
        }
    }

    /// Put a program's result into `mapped`; `null` leaves the field unset.
    fn store(self, mapped: &mut Mapped, value: &Value) -> Result<(), &'static str> {
        if matches!(value, Value::Null) {
            return Ok(());
        }
        let text = || match value {
            Value::String(text) => Ok::<_, &'static str>(text.to_string()),
            _ => Err("type"),
        };
        match self {
            Self::Title => mapped.title = Some(text()?),
            Self::Body => mapped.body = Some(text()?),
            Self::Link => mapped.link = Some(text()?),
            Self::GroupKey => mapped.group_key = Some(text()?),
            Self::Ack => mapped.ack = Some(text()?),
            Self::ExternalId => {
                mapped.external_id = Some(match value {
                    Value::Int(number) => number.to_string(),
                    Value::UInt(number) => number.to_string(),
                    _ => text()?,
                });
            }
            Self::Tags => {
                let Value::List(list) = value else {
                    return Err("type");
                };
                mapped.tags = list
                    .iter()
                    .map(|tag| match tag {
                        Value::String(tag) => Ok(tag.to_string()),
                        _ => Err("type"),
                    })
                    .collect::<Result<_, _>>()?;
            }
            Self::Priority => mapped.priority = Some(variant(text()?)?),
            Self::Status => mapped.status = Some(variant(text()?)?),
            Self::State => mapped.state = Some(variant(text()?)?),
        }
        Ok(())
    }
}

/// `"high"` as `Priority::High`, and so on, through the domain's own spelling.
fn variant<T: serde::de::DeserializeOwned>(text: String) -> Result<T, &'static str> {
    serde_json::from_value(serde_json::Value::String(text)).map_err(|_| "type")
}

/// An interpreter error, reduced to a kind that quotes nothing (T10).
fn kind(error: &ExecutionError) -> &'static str {
    match error {
        ExecutionError::NoSuchKey(_) => "no_such_key",
        ExecutionError::NoSuchOverload(_) => "overload",
        ExecutionError::UnexpectedType { .. }
        | ExecutionError::UnsupportedTargetType { .. }
        | ExecutionError::NotSupportedAsMethod { .. }
        | ExecutionError::UnsupportedKeyType(_)
        | ExecutionError::UnsupportedIndex(..)
        | ExecutionError::ValuesNotComparable(..)
        | ExecutionError::UnsupportedBinaryOperator(..) => "type",
        _ => "other",
    }
}

/// One compiled transform.
pub struct CelTransform {
    compiled: Arc<Compiled>,
    slots: Slots,
}

struct Compiled {
    spec: TransformSpec,
    env: Arc<Env>,
    split: Option<Program>,
    max_items: usize,
    fields: Vec<(Field, Program)>,
}

impl core::fmt::Debug for CelTransform {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CelTransform")
            .field("id", &self.compiled.spec.id)
            .finish_non_exhaustive()
    }
}

impl CelTransform {
    /// Compile and check every program of `spec` (RFC 0003 §4.3).
    pub fn compile(spec: TransformSpec, slots: Slots) -> Result<Self, CompileError> {
        let fail = |field: &'static str, message: String| CompileError {
            id: spec.id.clone(),
            field,
            message,
        };
        let id_ok = !spec.id.is_empty()
            && spec.id.len() <= 64
            && spec
                .id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !id_ok {
            return Err(fail(
                "id",
                "must be 1 to 64 characters of [a-z0-9-]: it is a URL segment, an external-id \
                 prefix and a metric label"
                    .to_owned(),
            ));
        }
        // `feed:` is the external-id namespace of RFC 0002's feeds: a transform named
        // so could revise, or silence, a feed's notification in a shared channel.
        if spec.id == "feed" {
            return Err(fail(
                "id",
                "`feed` is reserved: feed items use the `feed:` external ids".to_owned(),
            ));
        }
        if spec.channels.is_empty() {
            return Err(fail(
                "channels",
                "is required and may not be empty: a transform serves only the channels it lists"
                    .to_owned(),
            ));
        }
        let max_items = match (&spec.split, spec.max_items) {
            (None, Some(_)) => {
                return Err(fail("max_items", "is set without `split`".to_owned()));
            }
            (None, None) => 1,
            (Some(_), max) => max.unwrap_or(DEFAULT_MAX_ITEMS),
        };
        if !(1..=MAX_MAX_ITEMS).contains(&max_items) {
            return Err(fail(
                "max_items",
                format!("must be between 1 and {MAX_MAX_ITEMS}"),
            ));
        }
        if spec.raw == RawMode::Item && spec.split.is_none() {
            return Err(fail(
                "raw",
                "\"item\" needs `split`; without it, write \"payload\"".to_owned(),
            ));
        }

        let env = Arc::new(Env::stdlib());
        let with_split = spec.split.is_some();
        let compile = |field: &'static str, source: &str, check: Option<Field>| {
            program(&env, source, with_split && field != "split", check)
                .map_err(|message| fail(field, message))
        };
        let split = spec
            .split
            .as_deref()
            .map(|source| compile("split", source, None))
            .transpose()?;
        let mut fields = vec![(Field::Title, compile("title", &spec.title, None)?)];
        for (field, source) in [
            (Field::Body, &spec.body),
            (Field::Link, &spec.link),
            (Field::ExternalId, &spec.external_id),
            (Field::GroupKey, &spec.group_key),
            (Field::Tags, &spec.tags),
            (Field::Priority, &spec.priority),
            (Field::Status, &spec.status),
            (Field::State, &spec.state),
            (Field::Ack, &spec.ack),
        ] {
            if let Some(source) = source {
                fields.push((field, compile(field.name(), source, Some(field))?));
            }
        }

        Ok(Self {
            compiled: Arc::new(Compiled {
                spec,
                env,
                split,
                max_items,
                fields,
            }),
            slots,
        })
    }

    /// The declaration it was compiled from.
    #[must_use]
    pub fn spec(&self) -> &TransformSpec {
        &self.compiled.spec
    }

    /// Evaluate on this thread, with the interpreter's own messages: for
    /// `transform try` on the operator's terminal, never for a request (T10).
    pub fn explain(&self, body: &[u8]) -> Result<Vec<Explained>, TransformError> {
        self.compiled.run(body, true)
    }
}

/// One element, as `transform try` prints it.
#[derive(Debug, Clone, PartialEq)]
pub struct Explained {
    pub mapped: Mapped,
    /// Each failed field with CEL's full message.
    pub errors: Vec<(&'static str, String)>,
}

impl Compiled {
    fn run(&self, body: &[u8], explain: bool) -> Result<Vec<Explained>, TransformError> {
        let json: serde_json::Value =
            serde_json::from_slice(body).map_err(|_| TransformError::NotJson)?;
        let value = cel::to_value(&json).map_err(|_| TransformError::NotJson)?;
        let payload = Arc::new(json);
        let mut root = Context::with_env(Arc::clone(&self.env));
        root.add_variable_from_value("body", value);

        let items: Vec<Option<Value>> = match &self.split {
            None => vec![None],
            Some(split) => {
                let Ok(Value::List(list)) = split.execute(&root) else {
                    return Err(TransformError::NotList);
                };
                if list.len() > self.max_items {
                    return Err(TransformError::TooMany {
                        max: self.max_items,
                        actual: list.len(),
                    });
                }
                list.iter().cloned().map(Some).collect()
            }
        };

        let mut out = Vec::with_capacity(items.len());
        for item in items {
            let mut scope = root.new_inner_scope();
            let element = match item {
                Some(item) => {
                    let element = item.json().unwrap_or(serde_json::Value::Null);
                    scope.add_variable_from_value("item", item);
                    element
                }
                None => payload.as_ref().clone(),
            };
            let mut explained = Explained {
                mapped: Mapped {
                    element,
                    payload: Arc::clone(&payload),
                    ..Mapped::default()
                },
                errors: Vec::new(),
            };
            for (field, program) in &self.fields {
                let outcome = match program.execute(&scope) {
                    Ok(value) => field.store(&mut explained.mapped, &value).map_err(|kind| {
                        (kind, format!("the result is not a valid {}", field.name()))
                    }),
                    Err(error) => Err((kind(&error), error.to_string())),
                };
                if let Err((kind, message)) = outcome {
                    explained.mapped.failed.push((field.name(), kind));
                    if explain {
                        explained.errors.push((field.name(), message));
                    }
                }
            }
            out.push(explained);
        }
        Ok(out)
    }
}

#[async_trait::async_trait]
impl Transform for CelTransform {
    fn id(&self) -> &str {
        &self.compiled.spec.id
    }

    fn channels(&self) -> &[ChannelId] {
        &self.compiled.spec.channels
    }

    fn update(&self) -> bool {
        self.compiled.spec.update
    }

    fn raw(&self) -> RawMode {
        self.compiled.spec.raw
    }

    fn lang(&self) -> FeedLang {
        self.compiled.spec.lang
    }

    fn title_fallback(&self) -> &str {
        self.compiled
            .spec
            .title_fallback
            .as_deref()
            .unwrap_or(&self.compiled.spec.id)
    }

    async fn evaluate(&self, body: Vec<u8>) -> Result<Vec<Mapped>, TransformError> {
        let permit = Arc::clone(&self.slots.0)
            .try_acquire_owned()
            .map_err(|_| TransformError::Busy)?;
        let compiled = Arc::clone(&self.compiled);
        // A blocking task cannot be cancelled: it keeps its slot until it ends, so
        // the cap on cores holds after a timeout too.
        let task = tokio::task::spawn_blocking(move || {
            let outcome = compiled.run(&body, false);
            drop(permit);
            outcome
        });
        match tokio::time::timeout(EVAL_TIMEOUT, task).await {
            Ok(Ok(outcome)) => {
                outcome.map(|elements| elements.into_iter().map(|e| e.mapped).collect())
            }
            Ok(Err(_panicked)) => Err(TransformError::Internal),
            Err(_elapsed) => Err(TransformError::Timeout),
        }
    }
}

/// Compile one program and run the §4.3 checks over its AST.
fn program(env: &Env, source: &str, item: bool, results: Option<Field>) -> Result<Program, String> {
    let hint = if source.contains(['"', '\'']) {
        ""
    } else {
        "\nhint: a literal string needs CEL quotes: '\"…\"'"
    };
    if source.chars().count() > MAX_SOURCE_CHARS {
        return Err(format!("is longer than {MAX_SOURCE_CHARS} characters"));
    }
    // Before the parser, which recurses per bracket: in a debug build it overflows an
    // 8 MiB stack at about 60 nested brackets, before the AST walk could refuse them.
    if bracket_depth(source) > MAX_DEPTH {
        return Err(format!(
            "nests brackets deeper than {MAX_DEPTH} levels, which evaluation cannot afford"
        ));
    }
    let program = env
        .compile(source)
        .map_err(|error| format!("does not compile: {error}{hint}"))?;
    let mut walk = Walk {
        item,
        references: 0,
        bound: Vec::new(),
        in_step: false,
        depth: 0,
    };
    walk.visit(program.expression())
        .map_err(|message| format!("{message}{hint}"))?;
    if let Some(field) = results {
        let mut literals = Vec::new();
        result_literals(program.expression(), &mut literals);
        if let Some(bad) = literals.into_iter().find(|text| !field.accepts(text)) {
            return Err(format!(
                "can result in \"{bad}\", which is not a {} value",
                field.name()
            ));
        }
    }
    Ok(program)
}

/// The deepest nesting of `(`, `[` and `{` outside string literals.
fn bracket_depth(source: &str) -> usize {
    let chars: Vec<char> = source.chars().collect();
    let (mut depth, mut deepest, mut at) = (0_usize, 0_usize, 0_usize);
    while let Some(&c) = chars.get(at) {
        match c {
            '"' | '\'' => at = literal_end(&chars, at),
            '(' | '[' | '{' => {
                depth = depth.saturating_add(1);
                deepest = deepest.max(depth);
                at = at.saturating_add(1);
            }
            ')' | ']' | '}' => {
                depth = depth.saturating_sub(1);
                at = at.saturating_add(1);
            }
            _ => at = at.saturating_add(1),
        }
    }
    deepest
}

/// Where the CEL string literal opening at `quote` ends: one past its closing
/// quote, or the end of the source. A triple-quoted literal ends only at three
/// quotes, and a raw one (`r` or `br` before the quote, any case) has no escapes:
/// `r"\"` is one backslash.
fn literal_end(chars: &[char], quote: usize) -> usize {
    let Some(&delimiter) = chars.get(quote) else {
        return chars.len();
    };
    // CEL's raw prefixes are `r` and `br`, any case, `b` first; and not the tail of
    // an identifier (`bar"…"` is not raw).
    let at = |offset: usize| quote.checked_sub(offset).and_then(|at| chars.get(at));
    let starts_token =
        |offset: usize| at(offset).is_none_or(|c| !(c.is_alphanumeric() || *c == '_'));
    let raw = matches!(at(1), Some('r' | 'R'))
        && (starts_token(2) || (matches!(at(2), Some('b' | 'B')) && starts_token(3)));
    let triple = chars.get(quote.saturating_add(1)) == Some(&delimiter)
        && chars.get(quote.saturating_add(2)) == Some(&delimiter);

    let mut at = quote.saturating_add(if triple { 3 } else { 1 });
    while let Some(&c) = chars.get(at) {
        if c == '\\' && !raw {
            at = at.saturating_add(2);
            continue;
        }
        if c == delimiter {
            if !triple {
                return at.saturating_add(1);
            }
            if chars.get(at.saturating_add(1)) == Some(&delimiter)
                && chars.get(at.saturating_add(2)) == Some(&delimiter)
            {
                return at.saturating_add(3);
            }
        }
        at = at.saturating_add(1);
    }
    chars.len()
}

/// The structural rules of RFC 0003 §4.3.
struct Walk<'a> {
    /// Whether `item` is in scope.
    item: bool,
    references: usize,
    /// Names a comprehension binds, innermost last.
    bound: Vec<&'a str>,
    /// Inside a comprehension's step, condition or result.
    in_step: bool,
    depth: usize,
}

impl<'a> Walk<'a> {
    fn visit(&mut self, node: &'a IdedExpr) -> Result<(), String> {
        self.depth = self.depth.saturating_add(1);
        if self.depth > MAX_DEPTH {
            return Err(format!(
                "nests deeper than {MAX_DEPTH} levels, which evaluation cannot afford"
            ));
        }
        let visited = self.visit_node(node);
        self.depth = self.depth.saturating_sub(1);
        visited
    }

    fn visit_node(&mut self, node: &'a IdedExpr) -> Result<(), String> {
        match &node.expr {
            Expr::Ident(name) => self.ident(name),
            Expr::Call(call) => {
                if call.func_name == "matches" {
                    if self.in_step {
                        return Err(
                            "calls matches() in a comprehension step, which compiles its \
                             pattern on every element"
                                .to_owned(),
                        );
                    }
                    let pattern = match call.args.last().map(|arg| &arg.expr) {
                        Some(Expr::Literal(LiteralValue::String(pattern))) => pattern.inner(),
                        _ => {
                            return Err(
                                "calls matches() with a pattern that is not a string literal"
                                    .to_owned(),
                            );
                        }
                    };
                    regex::Regex::new(pattern).map_err(|error| {
                        format!("has a matches() pattern that does not compile: {error}")
                    })?;
                }
                if let Some(target) = &call.target {
                    self.visit(target)?;
                }
                call.args.iter().try_for_each(|arg| self.visit(arg))
            }
            Expr::Comprehension(comprehension) => {
                if self.in_step {
                    return Err("nests a comprehension inside another".to_owned());
                }
                self.visit(&comprehension.iter_range)?;
                let depth = self.bound.len();
                self.bound.push(&comprehension.iter_var);
                if let Some(second) = &comprehension.iter_var2 {
                    self.bound.push(second);
                }
                self.bound.push(&comprehension.accu_var);
                self.in_step = true;
                let inner = [
                    &comprehension.accu_init,
                    &comprehension.loop_cond,
                    &comprehension.loop_step,
                    &comprehension.result,
                ]
                .into_iter()
                .try_for_each(|part| self.visit(part));
                self.in_step = false;
                self.bound.truncate(depth);
                inner
            }
            Expr::List(list) => list.elements.iter().try_for_each(|e| self.visit(e)),
            Expr::Map(map) => map.entries.iter().try_for_each(|entry| match &entry.expr {
                EntryExpr::MapEntry(entry) => {
                    self.visit(&entry.key)?;
                    self.visit(&entry.value)
                }
                EntryExpr::StructField(field) => self.visit(&field.value),
            }),
            Expr::Struct(_) => Err("builds a message, which has no type here".to_owned()),
            Expr::Select(select) => self.visit(&select.operand),
            Expr::Literal(_) | Expr::Unspecified => Ok(()),
        }
    }

    fn ident(&mut self, name: &str) -> Result<(), String> {
        if self.bound.contains(&name) {
            // A macro's own accumulator (`@result`) is not something the author wrote.
            return if name.starts_with('@') {
                Ok(())
            } else {
                self.count()
            };
        }
        if self.in_step {
            return Err(format!(
                "reads `{name}` in a comprehension step; a step may read only its own \
                 variable, or it is quadratic"
            ));
        }
        match name {
            "body" => {}
            "item" if self.item => {}
            "item" => {
                return Err("uses `item`, which exists only with `split`; write `body`".to_owned());
            }
            _ => {
                return Err(format!(
                    "reads `{name}`, which is not a variable: only `body` (and `item` with \
                     `split`) are in scope"
                ));
            }
        }
        self.count()
    }

    fn count(&mut self) -> Result<(), String> {
        self.references = self.references.saturating_add(1);
        if self.references > MAX_REFERENCES {
            return Err(format!(
                "reads `body`, `item` or a comprehension's variable more than \
                 {MAX_REFERENCES} times"
            ));
        }
        Ok(())
    }
}

/// The string literals `node` can evaluate to: itself, a ternary's branches, the
/// values of a map literal it indexes.
fn result_literals<'a>(node: &'a IdedExpr, out: &mut Vec<&'a str>) {
    match &node.expr {
        Expr::Literal(LiteralValue::String(text)) => out.push(text.inner()),
        Expr::Call(call) if call.func_name == "_?_:_" => {
            for branch in call.args.iter().skip(1) {
                result_literals(branch, out);
            }
        }
        Expr::Call(call) if call.func_name == "_[_]" => {
            if let Some(Expr::Map(map)) = call.args.first().map(|map| &map.expr) {
                for entry in &map.entries {
                    if let EntryExpr::MapEntry(entry) = &entry.expr {
                        result_literals(&entry.value, out);
                    }
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests;
