//! The plan: what the manager writes, validated for a small model and expanded by code.
//!
//! The smallest valid plan is an objective and a list of question strings. Everything else is
//! optional and defaulted here; the manager is never required to write a key that has a sensible
//! default. Validation errors are written for a 9b model to act on: at most three, each
//! `plan.<path>: <what is wrong>. <what to write>.`, the first followed by a minimal valid
//! example, none naming internal types.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::form::{EnumValue, Field, FieldType, Show, ShowAs, Who};
use crate::registry::{Registry, PHASE_MENU};

/// The keys a manager may write (anything else is rejected with this list).
pub const PLAN_KEYS: [&str; 10] =
    ["objective", "questions", "fields", "send", "title", "kind", "workspace", "deliverable", "phases", "limits"];

/// Most project-specific fields a plan may ask for.
pub const MAX_FIELDS: usize = 12;
/// Most questions in one plan (and in one continuation).
pub const MAX_QUESTIONS: usize = 8;
/// The plan field types a manager may name. An enum is a field that has `values`.
pub const PLAN_FIELD_TYPES: [&str; 8] = ["string", "text", "integer", "number", "bool", "date", "url", "list"];

const EXAMPLE: &str = r#"Example: {"objective":"Find the newest open-weight models from three labs and say what is confirmed.","questions":["Which models are newest?","What is rumoured but not released?"]}"#;

/// What happens to a project's scratch space when it finishes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Workspace {
    /// Nothing is removed.
    Keep,
    /// `work/` and raw fetch bytes go; stored text, journals, snapshots and reports stay.
    #[default]
    Compact,
    /// Everything but the report and the final state goes.
    Delete,
}

/// What the project hands back.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Deliverable {
    /// A cited report file.
    #[default]
    Report,
    /// A short answer in the conversation.
    Answer,
    /// Files the project made.
    Files,
}

/// A sub-question the project answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    /// `q1`, `q2`, ... (continuations carry on from the last number).
    pub id: String,
    /// The question, 10 to 200 characters.
    pub text: String,
}

/// A project-specific field the plan asks every source to report (a column of `records`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanField {
    /// snake_case, at most 32 characters.
    pub key: String,
    /// Words for the column (the key with spaces, capitalised, unless the manager wrote one).
    pub label: String,
    /// The type; `enum` when `values` is given.
    #[serde(rename = "type")]
    pub ty: FieldType,
    /// The allowed values of an enum (2 to 8).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<String>,
    /// The longest text the model may write here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_len: Option<u32>,
}

impl PlanField {
    /// The form column this plan field becomes: written by the model, nullable (the page may not
    /// say), capped, and drawn as a badge when it is an enum.
    pub fn to_field(&self) -> Field {
        let mut f = match self.ty {
            FieldType::Enum => {
                let mut f = Field::new(&self.key, FieldType::Enum, Who::Model);
                f.values = self.values.iter().map(EnumValue::plain).collect();
                f
            }
            FieldType::List => {
                let mut f = Field::new(&self.key, FieldType::List, Who::Model);
                f.of = Some(FieldType::String);
                f.max_items = Some(8);
                f.max_len = Some(80);
                f
            }
            FieldType::Text => {
                let mut f = Field::new(&self.key, FieldType::Text, Who::Model);
                f.max_len = self.max_len.or(Some(400));
                f
            }
            FieldType::Integer | FieldType::Number | FieldType::Bool => Field::new(&self.key, self.ty, Who::Model),
            // string, date and url: the model writes text. A url-typed column holds text the
            // model wrote, so code drops anything URL-shaped from it (design 4.9).
            _ => {
                let mut f = Field::new(&self.key, FieldType::String, Who::Model);
                f.max_len = self.max_len.or(Some(120));
                f
            }
        };
        f.label = self.label.clone();
        f.nullable = true;
        f.show = Some(Show::of(if self.ty == FieldType::Enum { ShowAs::Badge } else { ShowAs::Text }));
        f
    }
}

/// Caps on a project's work. A budget reached raises a checkin; it is never exceeded silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    /// Most distinct sources read (1 to 120).
    pub max_sources: u32,
    /// Most page fetches, at least `max_sources`.
    pub max_fetches: u32,
    /// Search angles per question (1 to 6).
    pub vectors_per_question: u32,
    /// Most plan rounds, the first included (1 to 5).
    pub max_rounds: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { max_sources: 40, max_fetches: 60, vectors_per_question: 5, max_rounds: 3 }
    }
}

/// A plan as the project holds it: what the manager wrote plus everything code derived.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    /// 1 for the first plan; a continuation is the previous number plus one.
    pub plan_no: u32,
    /// What the project is for, 20 to 500 characters.
    pub objective: String,
    /// A short name (the objective's first 60 characters unless written).
    pub title: String,
    /// The kind of project (a template in the registry): `research`.
    pub kind: String,
    /// The questions of this plan.
    pub questions: Vec<Question>,
    /// How many questions earlier plans of this project had (so ids carry on).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub question_offset: u32,
    /// The project-specific fields every source reports.
    pub fields: Vec<PlanField>,
    /// The field that identifies a record (the first field).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_field: Option<String>,
    /// Send the deliverable to the user when done.
    pub send: bool,
    /// What is handed back.
    pub deliverable: Deliverable,
    /// What happens to scratch space at the end.
    pub workspace: Workspace,
    /// The phases of this plan's rounds, from the menu.
    pub phases: Vec<String>,
    /// The caps.
    pub limits: Limits,
    /// When the plan was written.
    pub created_ms: u64,
    /// The plan this one continues.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_plan: Option<u32>,
    /// Item ids carried over from earlier rounds.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inherit: Vec<String>,
    /// Additional-source items to read first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub seed_sources: Vec<String>,
    /// The user's steering text (data, length-capped).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steer: Option<String>,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

impl Plan {
    /// The plan as the manager would have written it: the canonical input that [`parse_plan`]
    /// turns back into this plan. Used to revise a plan by changing keys.
    pub fn to_input(&self) -> Value {
        let fields: Vec<Value> = self
            .fields
            .iter()
            .map(|f| {
                let mut o = Map::new();
                o.insert("key".into(), json!(f.key));
                if f.ty != FieldType::String && f.ty != FieldType::Enum {
                    o.insert("type".into(), serde_json::to_value(f.ty).unwrap_or(Value::Null));
                }
                if !f.values.is_empty() {
                    o.insert("values".into(), json!(f.values));
                }
                Value::Object(o)
            })
            .collect();
        json!({
            "objective": self.objective,
            "questions": self.questions.iter().map(|q| q.text.clone()).collect::<Vec<_>>(),
            "fields": fields,
            "send": self.send,
            "title": self.title,
            "kind": self.kind,
            "workspace": self.workspace,
            "deliverable": self.deliverable,
            "phases": self.phases,
            "limits": self.limits,
        })
    }
}

/// Everything wrong with a plan, as the sentences the manager is told (at most three).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanErrors(pub Vec<String>);

impl fmt::Display for PlanErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.join("\n"))
    }
}

impl std::error::Error for PlanErrors {}

struct Errs(Vec<String>);

impl Errs {
    fn add(&mut self, path: &str, wrong: &str, fix: &str) {
        let at = if path.is_empty() { "plan".to_string() } else { format!("plan.{path}") };
        self.0.push(format!("{at}: {wrong}. {fix}."));
    }

    fn finish(mut self) -> PlanErrors {
        self.0.truncate(3);
        if let Some(first) = self.0.first_mut() {
            first.push(' ');
            first.push_str(EXAMPLE);
        }
        PlanErrors(self.0)
    }
}

fn clean(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect::<String>().trim().to_string()
}

fn kind_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "a list",
        Value::Object(_) => "an object",
    }
}

fn text_in(path: &str, v: &Value, lo: usize, hi: usize, what: &str, errs: &mut Errs) -> Option<String> {
    let Some(s) = v.as_str() else {
        errs.add(path, &format!("need a string; got {}", kind_of(v)), &format!("Write {what}, {lo} to {hi} characters"));
        return None;
    };
    let s = clean(s);
    let n = s.chars().count();
    if n < lo || n > hi {
        errs.add(path, &format!("need {lo} to {hi} characters; got {n}"), &format!("Write {what}"));
        return None;
    }
    Some(s)
}

/// Capitalised key with spaces: `model_name` becomes `Model name`.
pub fn label_of(key: &str) -> String {
    let s = key.replace('_', " ");
    let mut cs = s.chars();
    match cs.next() {
        Some(c) => c.to_uppercase().collect::<String>() + cs.as_str(),
        None => String::new(),
    }
}

fn parse_questions(v: &Value, first_no: u32, errs: &mut Errs) -> Vec<Question> {
    let Some(arr) = v.as_array() else {
        errs.add("questions", &format!("need a list of questions; got {}", kind_of(v)), r#"Write ["one question, 10 to 200 characters"]"#);
        return Vec::new();
    };
    if arr.is_empty() || arr.len() > MAX_QUESTIONS {
        errs.add(
            "questions",
            &format!("need 1 to {MAX_QUESTIONS} items; got {}", arr.len()),
            r#"Write each question as a string like "one question, 10 to 200 characters""#,
        );
        return Vec::new();
    }
    let mut out = Vec::new();
    for (i, q) in arr.iter().enumerate() {
        let path = format!("questions[{i}]");
        // `{"q": "..."}` is tolerated; nothing else is.
        let inner = match q {
            Value::Object(o) if o.len() == 1 && o.contains_key("q") => &o["q"],
            other => other,
        };
        if let Some(t) = text_in(&path, inner, 10, 200, "the question as one string", errs) {
            out.push(Question { id: format!("q{}", first_no + i as u32), text: t });
        }
    }
    out
}

fn parse_fields(v: &Value, errs: &mut Errs) -> Vec<PlanField> {
    let Some(arr) = v.as_array() else {
        errs.add("fields", &format!("need a list; got {}", kind_of(v)), r#"Write [{"key":"model_name"},{"key":"status","values":["released","rumoured"]}] or leave `fields` out"#);
        return Vec::new();
    };
    if arr.len() > MAX_FIELDS {
        errs.add("fields", &format!("need at most {MAX_FIELDS} fields; got {}", arr.len()), "Keep the few fields that matter, or leave `fields` out");
        return Vec::new();
    }
    let mut out: Vec<PlanField> = Vec::new();
    let mut seen = BTreeSet::new();
    for (i, f) in arr.iter().enumerate() {
        let path = format!("fields[{i}]");
        // A bare string is a key.
        let obj = match f {
            Value::String(s) => {
                let mut m = Map::new();
                m.insert("key".into(), json!(s));
                m
            }
            Value::Object(o) => o.clone(),
            other => {
                errs.add(&path, &format!("need an object; got {}", kind_of(other)), r#"Write {"key":"model_name"}"#);
                continue;
            }
        };
        for k in obj.keys() {
            // `label` and `key_field` are tolerated (a label is kept, `key_field` ignored).
            if !matches!(k.as_str(), "key" | "type" | "values" | "label" | "key_field") {
                errs.add(&format!("{path}.{k}"), "not a field key", "Use only key, type and values");
            }
        }
        let Some(key) = obj.get("key").and_then(Value::as_str).map(clean) else {
            errs.add(&format!("{path}.key"), "missing", r#"Write the field's name, like "model_name""#);
            continue;
        };
        let snake_ok = key.len() <= 32
            && key.chars().next().is_some_and(|c| c.is_ascii_lowercase())
            && key.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        if !snake_ok {
            errs.add(&format!("{path}.key"), &format!("`{key}` is not lower_snake_case of at most 32 characters"), r#"Write something like "release_date""#);
            continue;
        }
        if !seen.insert(key.clone()) {
            errs.add(&format!("{path}.key"), &format!("`{key}` is used twice"), "Give each field its own key");
            continue;
        }
        let values: Vec<String> = match obj.get("values") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(a)) if (2..=8).contains(&a.len()) && a.iter().all(|x| x.as_str().is_some_and(|s| !clean(s).is_empty() && clean(s).chars().count() <= 40)) => {
                a.iter().filter_map(Value::as_str).map(clean).collect()
            }
            Some(_) => {
                errs.add(&format!("{path}.values"), "need 2 to 8 short strings", r#"Write ["released","announced","rumoured"] or leave `values` out"#);
                continue;
            }
        };
        let ty = match obj.get("type").and_then(Value::as_str) {
            None => FieldType::String,
            Some("enum") if !values.is_empty() => FieldType::Enum,
            Some("enum") => {
                errs.add(&format!("{path}.type"), "an enum needs `values`", r#"Add "values":["a","b"] or use another type"#);
                continue;
            }
            Some(t) if PLAN_FIELD_TYPES.contains(&t) => serde_json::from_value(json!(t)).unwrap_or(FieldType::String),
            Some(t) => {
                errs.add(
                    &format!("{path}.type"),
                    &format!("\"{t}\" is not a field type"),
                    &format!("Use one of {}", PLAN_FIELD_TYPES.join(", ")),
                );
                continue;
            }
        };
        let ty = if !values.is_empty() && ty == FieldType::String { FieldType::Enum } else { ty };
        if !values.is_empty() && ty != FieldType::Enum {
            errs.add(&format!("{path}.values"), "`values` makes a field a choice between strings, which does not fit this type", "Drop `type` or drop `values`");
            continue;
        }
        let label = obj.get("label").and_then(Value::as_str).map(clean).filter(|l| !l.is_empty() && l.chars().count() <= 40).unwrap_or_else(|| label_of(&key));
        let max_len = match ty {
            FieldType::Text => Some(400),
            FieldType::String | FieldType::Date => Some(120),
            FieldType::Url => Some(200),
            _ => None,
        };
        out.push(PlanField { key, label, ty, values, max_len });
    }
    out
}

fn parse_limits(v: &Value, errs: &mut Errs) -> Limits {
    let mut l = Limits::default();
    let Some(o) = v.as_object() else {
        errs.add("limits", &format!("need an object; got {}", kind_of(v)), r#"Write {"max_sources":40} or leave `limits` out"#);
        return l;
    };
    let mut fetches_given = false;
    for (k, val) in o {
        let range = match k.as_str() {
            "max_sources" => (1, 120),
            "max_fetches" => (1, 400),
            "vectors_per_question" => (1, 6),
            "max_rounds" => (1, 5),
            _ => {
                errs.add(&format!("limits.{k}"), "not a limit", "Use max_sources, max_fetches, vectors_per_question or max_rounds");
                continue;
            }
        };
        let Some(n) = val.as_u64().filter(|n| (range.0..=range.1).contains(n)) else {
            errs.add(&format!("limits.{k}"), &format!("need a whole number from {} to {}", range.0, range.1), "Write the number or leave the limit out");
            continue;
        };
        let n = n as u32;
        match k.as_str() {
            "max_sources" => l.max_sources = n,
            "max_fetches" => {
                l.max_fetches = n;
                fetches_given = true;
            }
            "vectors_per_question" => l.vectors_per_question = n,
            _ => l.max_rounds = n,
        }
    }
    if !fetches_given {
        l.max_fetches = l.max_fetches.max(l.max_sources);
    } else if l.max_fetches < l.max_sources {
        errs.add("limits.max_fetches", &format!("{} is below max_sources {}", l.max_fetches, l.max_sources), "Make max_fetches at least max_sources");
    }
    l
}

fn one_of(path: &str, v: &Value, options: &[&str], errs: &mut Errs) -> Option<String> {
    match v.as_str() {
        Some(s) if options.contains(&s) => Some(s.to_string()),
        _ => {
            errs.add(path, &format!("{} is not allowed", v), &format!("Use one of {}", options.join(", ")));
            None
        }
    }
}

/// Validates the manager's plan JSON and expands it: question ids, field labels, the first field
/// as the record key, and a default for every key left out. At most three errors come back, each
/// with a sentence saying what to write, the first with a minimal valid example.
pub fn parse_plan(input: &Value, registry: &Registry, now_ms: u64) -> Result<Plan, PlanErrors> {
    let mut errs = Errs(Vec::new());
    let Some(obj) = input.as_object() else {
        errs.add("", &format!("need a JSON object; got {}", kind_of(input)), r#"Write {"objective":"...","questions":["..."]}"#);
        return Err(errs.finish());
    };
    for k in obj.keys() {
        if !PLAN_KEYS.contains(&k.as_str()) {
            errs.add(k, "not a plan key", &format!("Use only {}", PLAN_KEYS.join(", ")));
        }
    }
    let objective = match obj.get("objective") {
        Some(v) => text_in("objective", v, 20, 500, "what the project is for, in one or two sentences", &mut errs),
        None => {
            errs.add("objective", "missing", "Write what the project is for, 20 to 500 characters");
            None
        }
    };
    let questions = match obj.get("questions") {
        Some(v) => parse_questions(v, 1, &mut errs),
        None => {
            errs.add("questions", "missing", r#"Write 1 to 8 questions as strings like "one question, 10 to 200 characters""#);
            Vec::new()
        }
    };
    let fields = obj.get("fields").map(|v| parse_fields(v, &mut errs)).unwrap_or_default();
    let title = obj.get("title").and_then(|v| text_in("title", v, 3, 80, "a short name", &mut errs));
    let kind = match obj.get("kind") {
        Some(v) => one_of("kind", v, &registry.available_kinds(), &mut errs),
        None => Some("research".to_string()),
    };
    let workspace = obj.get("workspace").and_then(|v| one_of("workspace", v, &["keep", "compact", "delete"], &mut errs));
    let deliverable = obj.get("deliverable").and_then(|v| one_of("deliverable", v, &["report", "answer", "files"], &mut errs));
    let send = match obj.get("send") {
        None => false,
        Some(Value::Bool(b)) => *b,
        Some(v) => {
            errs.add("send", &format!("need true or false; got {}", kind_of(v)), "Write true to have the result sent to the user");
            false
        }
    };
    let phases = match obj.get("phases") {
        None => None,
        Some(Value::Array(a)) if !a.is_empty() && a.iter().all(|p| p.as_str().is_some_and(|p| PHASE_MENU.contains(&p))) => {
            Some(a.iter().filter_map(Value::as_str).map(String::from).collect::<Vec<_>>())
        }
        Some(_) => {
            errs.add("phases", "need a list of phase names from the menu", &format!("Use names from {} or leave `phases` out", PHASE_MENU.join(", ")));
            None
        }
    };
    let limits = obj.get("limits").map(|v| parse_limits(v, &mut errs)).unwrap_or_default();
    if !errs.0.is_empty() {
        return Err(errs.finish());
    }
    let objective = objective.unwrap_or_default();
    let kind = kind.unwrap_or_else(|| "research".into());
    let title = title.unwrap_or_else(|| objective.chars().take(60).collect::<String>().trim().to_string());
    let phases = phases.unwrap_or_else(|| registry.kind(&kind).map(|k| k.phases.clone()).unwrap_or_default());
    let key_field = fields.first().map(|f| f.key.clone());
    Ok(Plan {
        plan_no: 1,
        objective,
        title,
        kind,
        questions,
        question_offset: 0,
        fields,
        key_field,
        send,
        deliverable: serde_json::from_value(json!(deliverable.unwrap_or_else(|| "report".into()))).unwrap_or_default(),
        workspace: serde_json::from_value(json!(workspace.unwrap_or_else(|| "compact".into()))).unwrap_or_default(),
        phases,
        limits,
        created_ms: now_ms,
        parent_plan: None,
        inherit: Vec::new(),
        seed_sources: Vec::new(),
        steer: None,
    })
}

/// Applies changed keys to a plan and validates the result, for `revise` while a plan gate is
/// open. A key in `changes` replaces that key whole (`questions` is the full new list). The plan
/// number, creation time and continuation data stay.
pub fn revise_plan(old: &Plan, changes: &Value, registry: &Registry) -> Result<Plan, PlanErrors> {
    let Some(ch) = changes.as_object() else {
        return Err(Errs(vec![format!(r#"plan: need a JSON object with the keys to change. Write {{"questions":["..."]}}."#)]).finish());
    };
    let mut input = old.to_input();
    if let Some(o) = input.as_object_mut() {
        for (k, v) in ch {
            o.insert(k.clone(), v.clone());
        }
    }
    let mut p = parse_plan(&input, registry, old.created_ms)?;
    p.plan_no = old.plan_no;
    p.question_offset = old.question_offset;
    if old.question_offset > 0 || old.plan_no > 1 {
        for (i, q) in p.questions.iter_mut().enumerate() {
            q.id = format!("q{}", old.question_offset + 1 + i as u32);
        }
    }
    p.parent_plan = old.parent_plan;
    p.inherit = old.inherit.clone();
    p.seed_sources = old.seed_sources.clone();
    p.steer = old.steer.clone();
    Ok(p)
}

/// What a continuation plan adds to the plan it inherits from.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Continuation {
    /// The new questions (the lead's gaps, or the user's steer), 1 to 8.
    pub questions: Vec<String>,
    /// Item ids carried over.
    #[serde(default)]
    pub inherit: Vec<String>,
    /// Additional-source items to read first.
    #[serde(default)]
    pub seed_sources: Vec<String>,
    /// The user's steering text.
    #[serde(default)]
    pub steer: Option<String>,
}

/// A new plan that inherits `prev`: same objective, fields, deliverable, workspace and limits;
/// the new questions with ids that carry on from the last number; the next plan number. Refused
/// once `max_rounds` is reached.
pub fn continue_plan(prev: &Plan, spec: &Continuation, now_ms: u64) -> Result<Plan, PlanErrors> {
    let mut errs = Errs(Vec::new());
    if prev.plan_no >= prev.limits.max_rounds {
        errs.add("limits.max_rounds", &format!("{} rounds are used", prev.plan_no), "Report with what exists, or raise max_rounds up to 5");
    }
    let offset = prev.question_offset + prev.questions.len() as u32;
    let questions = parse_questions(&json!(spec.questions), offset + 1, &mut errs);
    if spec.seed_sources.len() > 40 || spec.seed_sources.iter().any(|s| s.chars().count() > 300) {
        errs.add("seed_sources", "need at most 40 items of at most 300 characters", "Pass fewer or shorter sources");
    }
    let steer = spec.steer.as_deref().map(clean).filter(|s| !s.is_empty());
    if steer.as_ref().is_some_and(|s| s.chars().count() > 1000) {
        errs.add("steer", "longer than 1000 characters", "Shorten the steering text");
    }
    if !errs.0.is_empty() {
        return Err(errs.finish());
    }
    let mut p = prev.clone();
    p.plan_no = prev.plan_no + 1;
    p.question_offset = offset;
    p.questions = questions;
    p.created_ms = now_ms;
    p.parent_plan = Some(prev.plan_no);
    p.inherit = spec.inherit.clone();
    p.seed_sources = spec.seed_sources.clone();
    p.steer = steer;
    Ok(p)
}

/// A plan with Matthew's models example (five project fields, three questions) for tests.
#[cfg(test)]
pub(crate) fn test_plan() -> Plan {
    let reg = Registry::builtin();
    let input = json!({
        "objective": "Find the newest and upcoming Kimi, DeepSeek and Qwen models and say what is confirmed and what is rumoured.",
        "questions": [
            "Which Kimi models (Moonshot AI) are newest or upcoming?",
            "Which DeepSeek models are newest or upcoming?",
            "Is Qwen4 released or announced, and what is known about it?"],
        "fields": [{"key": "model_name"}, {"key": "vendor"}, {"key": "status", "values": ["released", "announced", "rumoured", "unknown"]}, {"key": "release_date"}, {"key": "size"}],
        "send": true
    });
    parse_plan(&input, &reg, 1_000).expect("the example plan is valid")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg() -> Registry {
        Registry::builtin()
    }

    fn minimal() -> Value {
        json!({"objective": "Find out which small language models fit in 8 GB of memory.", "questions": ["Which 4B models are strongest?"]})
    }

    #[test]
    fn the_smallest_plan_is_an_objective_and_question_strings_and_code_fills_the_rest() {
        let p = parse_plan(&minimal(), &reg(), 5).unwrap();
        assert_eq!(p.plan_no, 1);
        assert_eq!(p.kind, "research");
        assert_eq!(p.questions[0].id, "q1");
        assert_eq!(p.title, "Find out which small language models fit in 8 GB of memory.");
        assert_eq!(p.deliverable, Deliverable::Report);
        assert_eq!(p.workspace, Workspace::Compact);
        assert!(!p.send);
        assert_eq!(p.limits, Limits::default());
        assert_eq!(p.phases, ["Plan", "DataCollection", "DataValidation", "Thinking", "Report", "Finish"]);
        assert!(p.fields.is_empty() && p.key_field.is_none());
        assert_eq!(p.created_ms, 5);
    }

    #[test]
    fn matthews_example_expands_labels_types_and_the_record_key() {
        let p = test_plan();
        assert_eq!(p.questions.len(), 3);
        assert_eq!(p.questions[2].id, "q3");
        assert_eq!(p.fields.len(), 5);
        assert_eq!(p.fields[0].label, "Model name");
        assert_eq!(p.fields[2].ty, FieldType::Enum);
        assert_eq!(p.fields[2].values.len(), 4);
        assert_eq!(p.fields[3].max_len, Some(120));
        assert_eq!(p.key_field.as_deref(), Some("model_name"));
        assert!(p.send);
    }

    #[test]
    fn question_objects_and_bare_field_names_are_tolerated() {
        let p = parse_plan(
            &json!({"objective": "Compare three ways to cache a prompt on a local model.", "questions": [{"q": "What does each cache keep?"}],
                    "fields": ["method", {"key": "cost", "key_field": true, "label": "Cost per call"}]}),
            &reg(),
            0,
        )
        .unwrap();
        assert_eq!(p.questions[0].text, "What does each cache keep?");
        assert_eq!(p.fields[0].key, "method");
        assert_eq!(p.fields[1].label, "Cost per call");
        assert_eq!(p.key_field.as_deref(), Some("method"));
    }

    #[test]
    fn each_bad_plan_gets_at_most_three_errors_each_with_a_fix_sentence() {
        let cases: Vec<(Value, &str)> = vec![
            (json!("a plan"), "need a JSON object"),
            (json!({"questions": ["Which models are strongest?"]}), "plan.objective: missing"),
            (json!({"objective": "too short", "questions": ["Which models are strongest?"]}), "plan.objective: need 20 to 500 characters; got 9"),
            (json!({"objective": "Find out which small language models fit in 8 GB.", "questions": []}), "plan.questions: need 1 to 8 items; got 0"),
            (json!({"objective": "Find out which small language models fit in 8 GB.", "questions": ["short"]}), "plan.questions[0]: need 10 to 200 characters; got 5"),
            (json!({"objective": "Find out which small language models fit in 8 GB.", "questions": [5]}), "plan.questions[0]: need a string; got a number"),
            (json!({"objective": "Find out which small language models fit in 8 GB.", "questions": ["Which models are strongest?"], "colour": 1}), "plan.colour: not a plan key"),
            (json!({"objective": "Find out which small language models fit in 8 GB.", "questions": ["Which models are strongest?"], "fields": [{"key": "d", "type": "datetime"}]}), "\"datetime\" is not a field type"),
            (json!({"objective": "Find out which small language models fit in 8 GB.", "questions": ["Which models are strongest?"], "fields": [{"key": "Model Name"}]}), "not lower_snake_case"),
            (json!({"objective": "Find out which small language models fit in 8 GB.", "questions": ["Which models are strongest?"], "phases": ["Plan", "Dream"]}), "plan.phases"),
            (json!({"objective": "Find out which small language models fit in 8 GB.", "questions": ["Which models are strongest?"], "kind": "poetry"}), "plan.kind"),
            (json!({"objective": "Find out which small language models fit in 8 GB.", "questions": ["Which models are strongest?"], "limits": {"max_sources": 500}}), "plan.limits.max_sources: need a whole number from 1 to 120"),
            (json!({"objective": "Find out which small language models fit in 8 GB.", "questions": ["Which models are strongest?"], "limits": {"max_sources": 100, "max_fetches": 50}}), "below max_sources"),
        ];
        for (input, expect) in cases {
            let e = parse_plan(&input, &reg(), 0).unwrap_err();
            let text = e.to_string();
            assert!(text.contains(expect), "expected `{expect}` in:\n{text}");
            assert!((1..=3).contains(&e.0.len()), "{text}");
            for line in &e.0 {
                assert!(line.starts_with("plan"), "{line}");
                assert!(line.contains(". ") || line.ends_with('.'), "{line}");
            }
            assert!(e.0[0].contains("Example: {"), "the first error carries an example: {text}");
        }
    }

    #[test]
    fn many_errors_are_cut_to_three() {
        let e = parse_plan(&json!({"a": 1, "b": 2, "c": 3, "d": 4, "e": 5}), &reg(), 0).unwrap_err();
        assert_eq!(e.0.len(), 3);
    }

    #[test]
    fn twelve_fields_pass_and_thirteen_fail() {
        let mk = |n: usize| {
            let fields: Vec<Value> = (0..n).map(|i| json!({"key": format!("f{i}")})).collect();
            json!({"objective": "Find out which small language models fit in 8 GB.", "questions": ["Which models are strongest?"], "fields": fields})
        };
        assert_eq!(parse_plan(&mk(12), &reg(), 0).unwrap().fields.len(), 12);
        let e = parse_plan(&mk(13), &reg(), 0).unwrap_err();
        assert!(e.to_string().contains("plan.fields: need at most 12 fields; got 13"), "{e}");
    }

    #[test]
    fn eight_questions_pass_and_nine_fail() {
        let mk = |n: usize| {
            let qs: Vec<String> = (0..n).map(|i| format!("Question number {i} about the topic?")).collect();
            json!({"objective": "Find out which small language models fit in 8 GB.", "questions": qs})
        };
        assert_eq!(parse_plan(&mk(8), &reg(), 0).unwrap().questions.len(), 8);
        assert!(parse_plan(&mk(9), &reg(), 0).is_err());
    }

    #[test]
    fn control_characters_are_replaced_and_lengths_count_characters() {
        let p = parse_plan(&json!({"objective": "Find out which small language\nmodels fit in 8 GB \u{1b}[2K of memory.", "questions": ["Which models are strongest 日本語 ok?"]}), &reg(), 0).unwrap();
        assert!(!p.objective.chars().any(char::is_control));
    }

    #[test]
    fn a_max_sources_above_the_default_fetches_raises_the_default_fetches() {
        let mut i = minimal();
        i["limits"] = json!({"max_sources": 100});
        let p = parse_plan(&i, &reg(), 0).unwrap();
        assert_eq!((p.limits.max_sources, p.limits.max_fetches), (100, 100));
    }

    #[test]
    fn engineering_is_designed_but_not_a_kind_a_plan_may_name_yet() {
        let mut i = minimal();
        i["kind"] = json!("engineering");
        let e = parse_plan(&i, &reg(), 0).unwrap_err();
        assert!(e.to_string().contains("Use one of research"), "{e}");
    }

    #[test]
    fn plan_round_trips_through_json_and_through_its_canonical_input() {
        let p = test_plan();
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(serde_json::from_str::<Plan>(&s).unwrap(), p);
        let again = parse_plan(&p.to_input(), &reg(), p.created_ms).unwrap();
        assert_eq!(again, p);
    }

    #[test]
    fn revise_changes_the_named_keys_and_keeps_the_rest() {
        let p = test_plan();
        let r = revise_plan(&p, &json!({"questions": ["Which Kimi models are newest or upcoming?", "Which Qwen models exist today?"], "send": false}), &reg()).unwrap();
        assert_eq!(r.questions.len(), 2);
        assert!(!r.send);
        assert_eq!(r.fields, p.fields);
        assert_eq!(r.objective, p.objective);
        assert!(revise_plan(&p, &json!({"questions": []}), &reg()).is_err());
        assert!(revise_plan(&p, &json!("x"), &reg()).is_err());
    }

    #[test]
    fn a_continuation_inherits_and_numbers_questions_on() {
        let p = test_plan();
        let c = continue_plan(
            &p,
            &Continuation {
                questions: vec!["What is Kimi K3's context length?".into()],
                inherit: vec!["i_00000001".into()],
                seed_sources: vec!["https://example.org/a".into()],
                steer: Some("focus on release dates".into()),
            },
            9,
        )
        .unwrap();
        assert_eq!(c.plan_no, 2);
        assert_eq!(c.parent_plan, Some(1));
        assert_eq!(c.questions[0].id, "q4");
        assert_eq!(c.objective, p.objective);
        assert_eq!(c.fields, p.fields);
        let c3 = continue_plan(&c, &Continuation { questions: vec!["Another question to chase down?".into()], ..Continuation::default() }, 10).unwrap();
        assert_eq!(c3.questions[0].id, "q5");
        // Round 3 of 3 is used: no fourth plan.
        let e = continue_plan(&c3, &Continuation { questions: vec!["Yet another question to ask?".into()], ..Continuation::default() }, 11).unwrap_err();
        assert!(e.to_string().contains("max_rounds"), "{e}");
    }

    #[test]
    fn plan_fields_become_nullable_model_columns() {
        let p = test_plan();
        let cols: Vec<Field> = p.fields.iter().map(PlanField::to_field).collect();
        assert!(cols.iter().all(|c| c.w == Who::Model && c.nullable));
        assert_eq!(cols[2].ty, FieldType::Enum);
        assert_eq!(cols[2].values.len(), 4);
        assert_eq!(cols[0].max_len, Some(120));
        assert_eq!(label_of("release_date"), "Release date");
    }
}
