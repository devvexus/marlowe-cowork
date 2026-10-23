//! Forms: the one definition that is both the JSON Schema an employee is forced into and the
//! description of how each field is shown.
//!
//! A [`Form`] is a field list. Every [`Field`] says who writes it ([`Who`]), what it holds
//! ([`FieldType`]), where a reader finds it ([`Field::source`]) and how it is drawn
//! ([`Show`], the display-hint vocabulary). Forms are data: the registry loads them from JSON, a
//! project state embeds the ones it uses, and nothing downstream (view-model, painter, overview)
//! knows what a "source" or an "experiment" is.
//!
//! Two pure functions come from a form: [`model_schema`] (only the `Who::Model` fields; display
//! hints and code fields never reach the model) and [`data_schema`] (every stored field, for
//! checking a finished item).

use std::collections::BTreeSet;
use std::fmt;

use serde::de::{self, Deserializer};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::plan::Plan;

/// Who writes a field.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Who {
    /// The model, forced by the grammar (`"M"`).
    #[serde(rename = "M")]
    Model,
    /// Code, after the model returns (`"C"`). The default: a field nobody names never reaches a model.
    #[default]
    #[serde(rename = "C")]
    Code,
    /// The plan: the manager or the user (`"P"`).
    #[serde(rename = "P")]
    Plan,
}

/// What a field holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldType {
    /// A short string.
    String,
    /// Multi-line text.
    Text,
    /// A whole number.
    Integer,
    /// A real number.
    Number,
    /// true or false.
    Bool,
    /// One of the field's `values`.
    Enum,
    /// An ISO date string.
    Date,
    /// A link (code-written only: a model never writes a URL).
    Url,
    /// Milliseconds since the epoch.
    Timestamp,
    /// A span in milliseconds.
    Duration,
    /// A list of scalars (`of`).
    List,
    /// A list of objects whose keys are `columns`.
    Table,
    /// The ids of the agents assigned to the item.
    Agents,
    /// Anything; code-written only.
    Json,
}

impl FieldType {
    /// A type a list may hold.
    pub fn is_scalar(self) -> bool {
        matches!(
            self,
            FieldType::String | FieldType::Text | FieldType::Integer | FieldType::Number | FieldType::Bool | FieldType::Enum | FieldType::Date
        )
    }

    /// The display hint used when a field carries none.
    pub fn default_show(self) -> ShowAs {
        match self {
            FieldType::Enum => ShowAs::Badge,
            FieldType::Url => ShowAs::Link,
            FieldType::Table => ShowAs::Table,
            FieldType::Timestamp => ShowAs::Time,
            FieldType::Duration => ShowAs::Duration,
            FieldType::Agents => ShowAs::AgentRef,
            FieldType::Json => ShowAs::Json,
            _ => ShowAs::Text,
        }
    }
}

/// The colour names a form may use. A form never carries a colour value: the painter maps each
/// tone to the locked palette ([`Tone::rgb`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tone {
    /// Green `#8fd4a8`.
    Ok,
    /// Amber `#d6a95f`.
    Warn,
    /// Red `#dd7f85`.
    Bad,
    /// Blue `#7fa8dd`.
    Info,
    /// The accent `#9B7EDE`.
    Accent,
    /// The dim weight `#6a6480`.
    Dim,
}

impl Tone {
    /// The palette colour this tone draws in. These are the locked constants from
    /// `marlowe-surface/src/theme.rs` and the project instructions; a test pins every one.
    pub fn rgb(self) -> (u8, u8, u8) {
        match self {
            Tone::Ok => (0x8f, 0xd4, 0xa8),
            Tone::Warn => (0xd6, 0xa9, 0x5f),
            Tone::Bad => (0xdd, 0x7f, 0x85),
            Tone::Info => (0x7f, 0xa8, 0xdd),
            Tone::Accent => (0x9B, 0x7E, 0xDE),
            Tone::Dim => (0x6a, 0x64, 0x80),
        }
    }

    /// The lower-case name.
    pub fn as_str(self) -> &'static str {
        match self {
            Tone::Ok => "ok",
            Tone::Warn => "warn",
            Tone::Bad => "bad",
            Tone::Info => "info",
            Tone::Accent => "accent",
            Tone::Dim => "dim",
        }
    }
}

/// The display-hint vocabulary (`show.as`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShowAs {
    /// Plain text, wrapped, clipped with `...`.
    #[default]
    Text,
    /// A glyph and colour from the enum value's `tone` and `glyph`.
    Badge,
    /// A clickable `label ↗` that asks the host to open the browser.
    Link,
    /// A table with the schema-named columns.
    Table,
    /// `n/m` and a bar.
    Progress,
    /// A bar against a range with warn and crit colouring; without a `max` just the value.
    Meter,
    /// `00:42:10`, live while the item runs.
    Duration,
    /// A relative time ("3 min ago").
    Time,
    /// Agent chips; activating one opens that agent.
    AgentRef,
    /// Collapsed raw JSON.
    Json,
    /// Not shown (still in the JSON).
    Hidden,
}

impl ShowAs {
    /// Whether this hint can draw a field of type `ty`.
    pub fn fits(self, ty: FieldType) -> bool {
        match self {
            ShowAs::Text => !matches!(ty, FieldType::Table | FieldType::Agents | FieldType::Json),
            ShowAs::Badge => matches!(ty, FieldType::Enum | FieldType::Bool),
            ShowAs::Link => ty == FieldType::Url,
            ShowAs::Table => ty == FieldType::Table,
            ShowAs::Progress => matches!(ty, FieldType::Number | FieldType::Integer | FieldType::Json),
            ShowAs::Meter => matches!(ty, FieldType::Number | FieldType::Integer),
            ShowAs::Duration => ty == FieldType::Duration,
            ShowAs::Time => ty == FieldType::Timestamp,
            ShowAs::AgentRef => ty == FieldType::Agents,
            ShowAs::Json | ShowAs::Hidden => true,
        }
    }
}

/// How a field is drawn: the hint and its options.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Show {
    /// The hint.
    #[serde(rename = "as")]
    pub as_: ShowAs,
    /// `text`: lines to show before clipping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<u32>,
    /// `link`: the key of a sibling field that holds the link's words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_from: Option<String>,
    /// `table`: which columns to show, in order (default: every column not hidden).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub columns: Vec<String>,
    /// `table`: the column to sort by.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort: Option<String>,
    /// `meter`, `progress`: the unit (`%`, `bytes`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// `meter`: the low end of the range (default 0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    /// `meter`: the high end; absent or null shows the value without a bar (a size).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    /// `meter`: from this value the bar is amber.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warn_at: Option<f64>,
    /// `meter`: from this value the bar is red.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crit_at: Option<f64>,
}

impl Show {
    /// A bare hint with no options.
    pub fn of(as_: ShowAs) -> Show {
        Show { as_, ..Show::default() }
    }
}

/// One allowed value of an enum field, with the words the model is shown and how it is drawn.
#[derive(Debug, Clone, PartialEq)]
pub struct EnumValue {
    /// The value itself.
    pub v: String,
    /// The definition the model sees (with the test it applies), if any.
    pub def: Option<String>,
    /// A weight a score is computed from, if any.
    pub weight: Option<f64>,
    /// The tone a badge draws in.
    pub tone: Option<Tone>,
    /// The glyph a badge draws.
    pub glyph: Option<String>,
}

impl EnumValue {
    /// A value with nothing else said about it.
    pub fn plain(v: impl Into<String>) -> EnumValue {
        EnumValue { v: v.into(), def: None, weight: None, tone: None, glyph: None }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum EnumValueRepr {
    Plain(String),
    Full {
        v: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        def: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        weight: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tone: Option<Tone>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        glyph: Option<String>,
    },
}

impl Serialize for EnumValue {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let repr = if self.def.is_none() && self.weight.is_none() && self.tone.is_none() && self.glyph.is_none() {
            EnumValueRepr::Plain(self.v.clone())
        } else {
            EnumValueRepr::Full { v: self.v.clone(), def: self.def.clone(), weight: self.weight, tone: self.tone, glyph: self.glyph.clone() }
        };
        repr.serialize(s)
    }
}

impl<'de> Deserialize<'de> for EnumValue {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(match EnumValueRepr::deserialize(d).map_err(de::Error::custom)? {
            EnumValueRepr::Plain(v) => EnumValue::plain(v),
            EnumValueRepr::Full { v, def, weight, tone, glyph } => EnumValue { v, def, weight, tone, glyph },
        })
    }
}

/// Where a reader finds a field's value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldSource<'a> {
    /// `data.<key>`: stored in the item's `data` (the default, with the field's own key).
    Data(&'a str),
    /// `live.<key>`: a code-filled live slot (cpu, gpu, size, runtime). Never stored in `data`.
    Live(&'a str),
    /// The item's assigned agents.
    Agents,
}

/// One field of a form (or one column of a table field).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Field {
    /// The key (snake_case).
    pub key: String,
    /// Words for a column header or a label; empty means the key.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    /// What the field holds.
    #[serde(rename = "type")]
    pub ty: FieldType,
    /// Who writes it.
    #[serde(default)]
    pub w: Who,
    /// Whether a finished item must have it. Default: yes for a model field, no otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
    /// The value may be null (the page did not say).
    #[serde(default, skip_serializing_if = "is_false")]
    pub nullable: bool,
    /// `data.<key>` (default), `live.<key>` or `agents`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// The longest string (for a list, the longest item).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_len: Option<u32>,
    /// The most items of a list or table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_items: Option<u32>,
    /// The lowest number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    /// The highest number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    /// The allowed values of an enum.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<EnumValue>,
    /// What a list holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub of: Option<FieldType>,
    /// The columns of a table.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub columns: Vec<Field>,
    /// `plan.fields`: the columns are the plan's project-specific fields (the `records` table).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub columns_from: Option<String>,
    /// How it is drawn; absent means [`FieldType::default_show`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show: Option<Show>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// The `columns_from` value that means "the plan's own fields".
pub const COLUMNS_FROM_PLAN: &str = "plan.fields";

impl Field {
    /// A field with the given key, type and writer; everything else defaulted.
    pub fn new(key: &str, ty: FieldType, w: Who) -> Field {
        Field {
            key: key.to_string(),
            label: String::new(),
            ty,
            w,
            required: None,
            nullable: false,
            from: None,
            max_len: None,
            max_items: None,
            min: None,
            max: None,
            values: Vec::new(),
            of: None,
            columns: Vec::new(),
            columns_from: None,
            show: None,
        }
    }

    /// The label, or the key with underscores as spaces when there is none.
    pub fn label_or_key(&self) -> String {
        if self.label.is_empty() {
            self.key.replace('_', " ")
        } else {
            self.label.clone()
        }
    }

    /// Whether a finished item must carry this field.
    pub fn is_required(&self) -> bool {
        self.required.unwrap_or(self.w == Who::Model)
    }

    /// The display hint in force.
    pub fn show_as(&self) -> ShowAs {
        self.show.as_ref().map(|s| s.as_).unwrap_or_else(|| self.ty.default_show())
    }

    /// Where the value is read from.
    pub fn source(&self) -> Result<FieldSource<'_>, String> {
        match self.from.as_deref() {
            None => Ok(FieldSource::Data(&self.key)),
            Some("agents") => Ok(FieldSource::Agents),
            Some(s) => match s.split_once('.') {
                Some(("data", k)) if !k.is_empty() => Ok(FieldSource::Data(k)),
                Some(("live", k)) if !k.is_empty() => Ok(FieldSource::Live(k)),
                _ => Err(format!("`from` is `{s}`; use data.<key>, live.<key> or agents")),
            },
        }
    }

    /// The enum value record for `v`.
    pub fn enum_value(&self, v: &str) -> Option<&EnumValue> {
        self.values.iter().find(|e| e.v == v)
    }
}

/// A form: the schema of one kind of item and how to draw it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Form {
    /// The form's name (snake_case).
    pub form: String,
    /// The version; the id is `<form>@<version>`.
    pub version: u32,
    /// A title for the item kind.
    #[serde(default)]
    pub title: String,
    /// The title line template: `{data.index}. {data.title}`, `{status}`, `{label}`, `{id}`, `{kind}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_label: Option<String>,
    /// The fields, in the order they are drawn and (for model fields) asked for.
    pub fields: Vec<Field>,
    /// Named code checks run after the grammar (design 5.4).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<String>,
}

impl Form {
    /// `<form>@<version>`.
    pub fn id(&self) -> String {
        format!("{}@{}", self.form, self.version)
    }

    /// The field with this key.
    pub fn field(&self, key: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.key == key)
    }

    /// Everything wrong with the form itself (an empty list when it is sound): bad names, a model
    /// field with no length cap, a hint that cannot draw its type, a colour value. A test runs it
    /// over every registered form, so a broken form never ships.
    pub fn validate(&self) -> Vec<String> {
        let mut errs = Vec::new();
        let id = self.id();
        if !is_snake(&self.form) {
            errs.push(format!("{id}: form name must be snake_case"));
        }
        if self.version == 0 {
            errs.push(format!("{id}: version starts at 1"));
        }
        let mut seen = BTreeSet::new();
        for f in &self.fields {
            if !seen.insert(f.key.as_str()) {
                errs.push(format!("{id}: field `{}` appears twice", f.key));
            }
            check_field(&id, &f.key, f, false, self, &mut errs);
        }
        if let Some(t) = &self.item_label {
            check_template(&id, t, self, &mut errs);
        }
        if let Ok(v) = serde_json::to_value(self) {
            if let Some(s) = find_hex_colour(&v) {
                errs.push(format!("{id}: `{s}` is a colour value; a form names tones, the painter owns the palette"));
            }
        }
        errs
    }
}

impl fmt::Display for Form {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.id())
    }
}

fn is_snake(s: &str) -> bool {
    let mut cs = s.chars();
    matches!(cs.next(), Some('a'..='z')) && s.len() <= 32 && cs.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn check_field(form: &str, path: &str, f: &Field, in_table: bool, root: &Form, errs: &mut Vec<String>) {
    let at = |m: &str| format!("{form}: field `{path}`: {m}");
    if !is_snake(&f.key) {
        errs.push(at("key must be snake_case, at most 32 characters"));
    }
    match f.source() {
        Err(e) => errs.push(at(&e)),
        Ok(src) => {
            if matches!(src, FieldSource::Live(_)) && f.w != Who::Code {
                errs.push(at("a live field is written by code (w \"C\")"));
            }
            if in_table && !matches!(src, FieldSource::Data(_)) {
                errs.push(at("a table column reads its own row; `from` is not allowed"));
            }
            if matches!(src, FieldSource::Agents) != (f.ty == FieldType::Agents) {
                errs.push(at("type `agents` and `from: agents` go together"));
            }
            if f.w == Who::Model && !matches!(src, FieldSource::Data(_)) {
                errs.push(at("a model field is stored in data"));
            }
        }
    }
    if f.w == Who::Model {
        match f.ty {
            FieldType::Url | FieldType::Timestamp | FieldType::Duration | FieldType::Agents | FieldType::Json => {
                errs.push(at("a model never writes a url, a timestamp, a duration, agents or free JSON"));
            }
            FieldType::String | FieldType::Text if f.max_len.is_none() => errs.push(at("model-written text needs `max_len`")),
            FieldType::List => {
                if f.max_items.is_none() {
                    errs.push(at("a model-written list needs `max_items`"));
                }
                if matches!(f.of, None | Some(FieldType::String | FieldType::Text)) && f.max_len.is_none() {
                    errs.push(at("a model-written list of text needs `max_len` for its items"));
                }
            }
            FieldType::Table if f.max_items.is_none() => errs.push(at("a model-written table needs `max_items`")),
            _ => {}
        }
    }
    if f.ty == FieldType::Enum && f.values.is_empty() {
        errs.push(at("an enum needs `values`"));
    }
    if f.ty != FieldType::Enum && !f.values.is_empty() {
        errs.push(at("`values` belongs to an enum"));
    }
    let mut vs = BTreeSet::new();
    for e in &f.values {
        if e.v.is_empty() || !vs.insert(e.v.as_str()) {
            errs.push(at(&format!("enum value `{}` is empty or repeated", e.v)));
        }
    }
    if f.ty == FieldType::List {
        match f.of {
            Some(t) if t.is_scalar() => {}
            _ => errs.push(at("a list needs `of` naming a scalar type")),
        }
    }
    if f.ty == FieldType::Table {
        if in_table {
            errs.push(at("tables do not nest"));
        }
        match (&f.columns_from, f.columns.is_empty()) {
            (None, true) => errs.push(at("a table needs `columns` or `columns_from`")),
            (Some(c), _) if c != COLUMNS_FROM_PLAN => errs.push(at("`columns_from` can only be `plan.fields`")),
            (Some(_), false) => errs.push(at("give `columns` or `columns_from`, not both")),
            _ => {}
        }
        let mut seen = BTreeSet::new();
        for c in &f.columns {
            if !seen.insert(c.key.as_str()) {
                errs.push(at(&format!("column `{}` appears twice", c.key)));
            }
            check_field(form, &format!("{path}.{}", c.key), c, true, root, errs);
        }
    } else if !f.columns.is_empty() || f.columns_from.is_some() {
        errs.push(at("`columns` belongs to a table"));
    }
    if let (Some(a), Some(b)) = (f.min, f.max) {
        if a > b {
            errs.push(at("`min` is above `max`"));
        }
    }
    if let Some(s) = &f.show {
        if !s.as_.fits(f.ty) {
            errs.push(at(&format!("show `{:?}` cannot draw type `{:?}`", s.as_, f.ty)));
        }
        if let Some(l) = &s.label_from {
            if !in_table && root.field(l).is_none() {
                errs.push(at(&format!("`label_from` names `{l}`, which is not a field")));
            }
        }
        if !s.columns.is_empty() && f.columns_from.is_none() {
            for c in &s.columns {
                if !f.columns.iter().any(|x| &x.key == c) {
                    errs.push(at(&format!("`show.columns` names `{c}`, which is not a column")));
                }
            }
        }
        if let (Some(a), Some(b)) = (s.min, s.max) {
            if a >= b {
                errs.push(at("meter `min` must be below `max`"));
            }
        }
    }
}

fn check_template(form: &str, t: &str, root: &Form, errs: &mut Vec<String>) {
    let mut rest = t;
    while let Some(i) = rest.find('{') {
        let Some(j) = rest[i..].find('}') else {
            errs.push(format!("{form}: item_label has an unclosed `{{`"));
            return;
        };
        let name = &rest[i + 1..i + j];
        let ok = match name.split_once('.') {
            Some(("data", k)) => {
                let first = k.split('.').next().unwrap_or(k);
                root.fields.iter().any(|f| matches!(f.source(), Ok(FieldSource::Data(d)) if d == first))
            }
            Some(("live", k)) => !k.is_empty(),
            None => matches!(name, "status" | "kind" | "id" | "label"),
            _ => false,
        };
        if !ok {
            errs.push(format!("{form}: item_label names `{{{name}}}`, which no field or item slot provides"));
        }
        rest = &rest[i + j + 1..];
    }
}

/// The first string in `v` that looks like a hex colour (`#` and 3, 4, 6 or 8 hex digits not
/// followed by a letter or digit), if any.
pub fn find_hex_colour(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => hex_colour_in(s),
        Value::Array(a) => a.iter().find_map(find_hex_colour),
        Value::Object(o) => o.values().find_map(find_hex_colour),
        _ => None,
    }
}

fn hex_colour_in(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'#' {
            let mut j = i + 1;
            while j < b.len() && b[j].is_ascii_hexdigit() {
                j += 1;
            }
            let n = j - i - 1;
            if matches!(n, 3 | 4 | 6 | 8) && !(j < b.len() && b[j].is_ascii_alphanumeric()) {
                return Some(s[i..j].to_string());
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    None
}

/// Fills `{name}` placeholders in `template` from `lookup`; a name `lookup` does not know becomes
/// empty. Text outside braces is kept as written.
pub fn fill_template(template: &str, lookup: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::new();
    let mut rest = template;
    while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        match rest[i..].find('}') {
            Some(j) => {
                out.push_str(&lookup(&rest[i + 1..i + j]).unwrap_or_default());
                rest = &rest[i + j + 1..];
            }
            None => {
                out.push_str(&rest[i..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// The columns of a table field: its own, or the plan's project-specific fields when it says
/// `columns_from: plan.fields`.
pub fn resolved_columns(field: &Field, plan: Option<&Plan>) -> Vec<Field> {
    match (&field.columns_from, plan) {
        (Some(_), Some(p)) => p.fields.iter().map(|pf| pf.to_field()).collect(),
        (Some(_), None) => Vec::new(),
        (None, _) => field.columns.clone(),
    }
}

fn scalar_schema(ty: FieldType, f: &Field, nullable: bool) -> Value {
    let mut o = Map::new();
    let base = match ty {
        FieldType::Integer | FieldType::Timestamp | FieldType::Duration => "integer",
        FieldType::Number => "number",
        FieldType::Bool => "boolean",
        _ => "string",
    };
    o.insert("type".into(), if nullable { json!([base, "null"]) } else { json!(base) });
    match ty {
        FieldType::String | FieldType::Text | FieldType::Date | FieldType::Url => {
            if let Some(n) = f.max_len {
                o.insert("maxLength".into(), json!(n));
            }
        }
        FieldType::Integer | FieldType::Number | FieldType::Timestamp | FieldType::Duration => {
            if let Some(n) = f.min {
                o.insert("minimum".into(), json!(n));
            }
            if let Some(n) = f.max {
                o.insert("maximum".into(), json!(n));
            }
        }
        FieldType::Enum => {
            let mut vals: Vec<Value> = f.values.iter().map(|e| json!(e.v)).collect();
            if nullable {
                vals.push(Value::Null);
            }
            o.insert("enum".into(), Value::Array(vals));
        }
        _ => {}
    }
    Value::Object(o)
}

fn field_schema(f: &Field, plan: Option<&Plan>, only_model: bool) -> Value {
    match f.ty {
        FieldType::List => {
            let mut o = Map::new();
            o.insert("type".into(), if f.nullable { json!(["array", "null"]) } else { json!("array") });
            o.insert("items".into(), scalar_schema(f.of.unwrap_or(FieldType::String), f, false));
            if let Some(n) = f.max_items {
                o.insert("maxItems".into(), json!(n));
            }
            Value::Object(o)
        }
        FieldType::Table => {
            let cols = resolved_columns(f, plan);
            let mut o = Map::new();
            o.insert("type".into(), if f.nullable { json!(["array", "null"]) } else { json!("array") });
            o.insert("items".into(), object_schema(&cols, plan, only_model));
            if let Some(n) = f.max_items {
                o.insert("maxItems".into(), json!(n));
            }
            Value::Object(o)
        }
        FieldType::Json | FieldType::Agents => json!({}),
        ty => scalar_schema(ty, f, f.nullable),
    }
}

fn object_schema(fields: &[Field], plan: Option<&Plan>, only_model: bool) -> Value {
    let mut props = Map::new();
    let mut required = Vec::new();
    for f in fields {
        if only_model && f.w != Who::Model {
            continue;
        }
        if !matches!(f.source(), Ok(FieldSource::Data(_))) {
            continue;
        }
        props.insert(f.key.clone(), field_schema(f, plan, only_model));
        // The grammar needs every model field present (a nullable one may say null); for stored
        // data only a field the form marks required must be there.
        if (only_model && f.w == Who::Model) || (!only_model && f.is_required()) {
            required.push(json!(f.key));
        }
    }
    json!({"type": "object", "properties": props, "required": required, "additionalProperties": false})
}

/// The JSON Schema an employee's answer is forced into: only the `Who::Model` fields, in form
/// order, every one required, `additionalProperties: false`, with `maxLength`, `maxItems`, `enum`,
/// `minimum` and `maximum`. A table with `columns_from: plan.fields` gets the plan's
/// project-specific fields as its columns. Display hints, labels, definitions and code fields
/// never appear in it.
pub fn model_schema(form: &Form, plan: &Plan) -> Value {
    object_schema(&form.fields, Some(plan), true)
}

/// The schema a finished item's `data` is checked against: every field stored in `data` (model,
/// code or plan written), required only where [`Field::is_required`]. Live and agent fields are
/// not part of `data`.
pub fn data_schema(form: &Form, plan: &Plan) -> Value {
    object_schema(&form.fields, Some(plan), false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(key: &str, w: Who) -> Field {
        let mut f = Field::new(key, FieldType::String, w);
        f.max_len = Some(40);
        f
    }

    fn form(fields: Vec<Field>) -> Form {
        Form { form: "x".into(), version: 1, title: String::new(), item_label: None, fields, rules: vec![] }
    }

    #[test]
    fn tones_map_to_the_locked_palette() {
        assert_eq!(Tone::Ok.rgb(), (0x8f, 0xd4, 0xa8));
        assert_eq!(Tone::Warn.rgb(), (0xd6, 0xa9, 0x5f));
        assert_eq!(Tone::Bad.rgb(), (0xdd, 0x7f, 0x85));
        assert_eq!(Tone::Info.rgb(), (0x7f, 0xa8, 0xdd));
        assert_eq!(Tone::Accent.rgb(), (0x9B, 0x7E, 0xDE));
        assert_eq!(Tone::Dim.rgb(), (0x6a, 0x64, 0x80));
    }

    #[test]
    fn enum_values_read_plain_or_full_and_write_the_shortest_form() {
        let plain: EnumValue = serde_json::from_str("\"released\"").unwrap();
        assert_eq!(plain, EnumValue::plain("released"));
        assert_eq!(serde_json::to_string(&plain).unwrap(), "\"released\"");
        let full: EnumValue = serde_json::from_str(r#"{"v":"High","weight":0.7,"tone":"ok","glyph":"●"}"#).unwrap();
        assert_eq!(full.weight, Some(0.7));
        assert_eq!(full.tone, Some(Tone::Ok));
        assert_eq!(serde_json::from_str::<EnumValue>(&serde_json::to_string(&full).unwrap()).unwrap(), full);
    }

    #[test]
    fn hex_colours_are_found_and_issue_numbers_are_not() {
        assert_eq!(hex_colour_in("fg #9B7EDE"), Some("#9B7EDE".into()));
        assert_eq!(hex_colour_in("#abc"), Some("#abc".into()));
        assert_eq!(hex_colour_in("Experiment #{data.n}"), None);
        assert_eq!(hex_colour_in("Experiment #1"), None);
        assert_eq!(hex_colour_in("#abcdefg"), None);
    }

    #[test]
    fn templates_fill_and_leave_unknowns_empty() {
        let lookup = |k: &str| match k {
            "status" => Some("running".to_string()),
            _ => None,
        };
        assert_eq!(fill_template("E [{status}] {data.x}!", &lookup), "E [running] !");
        assert_eq!(fill_template("no braces", &lookup), "no braces");
        assert_eq!(fill_template("open {brace", &lookup), "open {brace");
    }

    #[test]
    fn a_sound_form_validates_and_a_broken_one_says_why() {
        let mut f = form(vec![text("body", Who::Model)]);
        f.item_label = Some("{data.body}".into());
        assert_eq!(f.validate(), Vec::<String>::new());
        // A model string with no cap, a url written by the model, a hint on the wrong type.
        f.fields.push(Field::new("free", FieldType::String, Who::Model));
        f.fields.push(Field::new("link", FieldType::Url, Who::Model));
        let mut bad_hint = Field::new("n", FieldType::String, Who::Code);
        bad_hint.show = Some(Show::of(ShowAs::Meter));
        f.fields.push(bad_hint);
        let errs = f.validate().join("\n");
        assert!(errs.contains("needs `max_len`"), "{errs}");
        assert!(errs.contains("never writes a url"), "{errs}");
        assert!(errs.contains("cannot draw type"), "{errs}");
    }

    #[test]
    fn a_template_naming_nothing_is_refused() {
        let mut f = form(vec![text("body", Who::Model)]);
        f.item_label = Some("{data.nope} {status}".into());
        let errs = f.validate();
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("data.nope"));
    }

    #[test]
    fn a_form_with_a_colour_value_is_refused() {
        let mut f = form(vec![text("body", Who::Model)]);
        f.title = "paint it #9B7EDE".into();
        assert!(f.validate().iter().any(|e| e.contains("colour value")));
    }

    #[test]
    fn live_fields_must_be_code_written_and_never_reach_the_model() {
        let mut live = Field::new("cpu_pct", FieldType::Number, Who::Code);
        live.from = Some("live.cpu_pct".into());
        let f = form(vec![text("a", Who::Model), live.clone()]);
        assert!(f.validate().is_empty(), "{:?}", f.validate());
        let plan = crate::plan::test_plan();
        assert!(model_schema(&f, &plan)["properties"].get("cpu_pct").is_none());
        assert!(data_schema(&f, &plan)["properties"].get("cpu_pct").is_none());
        live.w = Who::Model;
        let bad = form(vec![live]);
        assert!(bad.validate().iter().any(|e| e.contains("written by code")));
    }

    #[test]
    fn model_schema_keeps_only_model_fields_in_order_with_their_caps() {
        let mut level = Field::new("level", FieldType::Enum, Who::Model);
        level.values = vec![EnumValue::plain("high"), EnumValue::plain("low")];
        let mut tags = Field::new("tags", FieldType::List, Who::Model);
        tags.of = Some(FieldType::String);
        tags.max_items = Some(3);
        tags.max_len = Some(12);
        let mut shown = text("shown", Who::Code);
        shown.show = Some(Show::of(ShowAs::Hidden));
        let f = form(vec![text("zeta", Who::Model), shown, level, tags]);
        let plan = crate::plan::test_plan();
        let s = model_schema(&f, &plan);
        let keys: Vec<&String> = s["properties"].as_object().unwrap().keys().collect();
        assert_eq!(keys, ["zeta", "level", "tags"]);
        assert_eq!(s["properties"]["zeta"]["maxLength"], 40);
        assert_eq!(s["properties"]["level"]["enum"], json!(["high", "low"]));
        assert_eq!(s["properties"]["tags"]["maxItems"], 3);
        assert_eq!(s["properties"]["tags"]["items"]["maxLength"], 12);
        assert_eq!(s["additionalProperties"], false);
        assert_eq!(s["required"], json!(["zeta", "level", "tags"]));
        marlowe_contract::schema::check_shape(&s).unwrap();
        let text = s.to_string();
        for banned in ["show", "\"w\"", "label", "tone", "glyph", "weight"] {
            assert!(!text.contains(banned), "{banned} reached the model schema: {text}");
        }
    }

    #[test]
    fn plan_fields_become_the_records_columns_and_nullable_columns_accept_null() {
        let mut records = Field::new("records", FieldType::Table, Who::Model);
        records.max_items = Some(10);
        records.columns_from = Some(COLUMNS_FROM_PLAN.into());
        let f = form(vec![records]);
        let plan = crate::plan::test_plan();
        let s = model_schema(&f, &plan);
        let cols = &s["properties"]["records"]["items"]["properties"];
        assert_eq!(cols.as_object().unwrap().len(), plan.fields.len());
        assert_eq!(cols["model_name"]["type"], json!(["string", "null"]));
        assert_eq!(cols["status"]["enum"], json!(["released", "announced", "rumoured", "unknown", null]));
        marlowe_contract::schema::check_shape(&s).unwrap();
        let ok = json!({"records": [{"model_name": "Kimi K3", "vendor": null, "status": "rumoured", "release_date": null, "size": null}]});
        assert!(marlowe_contract::schema::validate(&ok, &s).is_empty());
        let bad = json!({"records": [{"model_name": "x", "vendor": null, "status": "maybe", "release_date": null, "size": null}]});
        assert!(!marlowe_contract::schema::validate(&bad, &s).is_empty());
    }
}
