use std::fmt;

use anyhow::Result;
use serde::de::{IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::value::RawValue;
use serde_json::{json, Map, Value};

use super::{Client, Handler, StreamEvent, Updates};

/// A form-mode `elicitation/create` request that the user can answer.
#[derive(Clone, Debug, PartialEq)]
pub struct Elicitation {
    pub message: String,
    pub fields: Vec<Field>,
}

/// What the user did.
#[derive(Clone, Debug, PartialEq)]
pub enum ElicitationAnswer {
    /// Holds only the fields that the user filled in.
    Accept(Map<String, Value>),
    Decline,
    Cancel,
}

/// What the session log keeps. The answers are not stored.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ElicitationOutcome {
    Answered,
    Declined,
    Cancelled,
    /// Declined without asking, because the client cannot show the request.
    Unsupported,
}

/// One form field, in schema order.
#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    pub name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub required: bool,
    pub input: Input,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Input {
    Text {
        default: Option<String>,
        min_length: Option<usize>,
        max_length: Option<usize>,
        format: Option<Format>,
    },
    Integer {
        default: Option<i64>,
        minimum: Option<i64>,
        maximum: Option<i64>,
    },
    Number {
        default: Option<f64>,
        minimum: Option<f64>,
        maximum: Option<f64>,
    },
    Boolean {
        default: Option<bool>,
    },
    Select {
        options: Vec<Choice>,
        default: Option<String>,
    },
    MultiSelect {
        options: Vec<Choice>,
        default: Vec<String>,
        min_items: Option<usize>,
        max_items: Option<usize>,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Format {
    Email,
    Uri,
    Date,
    DateTime,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Choice {
    pub value: String,
    pub title: Option<String>,
    pub description: Option<String>,
}

impl Choice {
    pub fn label(&self) -> &str {
        self.title.as_deref().unwrap_or(&self.value)
    }
}

impl Field {
    pub fn label(&self) -> &str {
        self.title.as_deref().unwrap_or(&self.name)
    }

    /// The answer before the user changes it.
    pub fn default_value(&self) -> Option<Value> {
        match &self.input {
            Input::Text { default, .. } => default.clone().map(Value::from),
            Input::Integer { default, .. } => default.map(Value::from),
            Input::Number { default, .. } => default.map(Value::from),
            Input::Boolean { default } => default.map(Value::from),
            Input::Select { default, .. } => default.clone().map(Value::from),
            Input::MultiSelect { default, .. } if default.is_empty() => None,
            Input::MultiSelect { default, .. } => Some(default.clone().into()),
        }
    }

    /// Checks typed input. Returns `None` for an empty optional field.
    pub fn parse(&self, text: &str) -> Result<Option<Value>, String> {
        if text.trim().is_empty() {
            return if self.required {
                Err("a value is required".into())
            } else {
                Ok(None)
            };
        }
        match &self.input {
            Input::Text {
                min_length,
                max_length,
                format,
                ..
            } => {
                let length = text.chars().count();
                if let Some(minimum) = min_length.filter(|minimum| length < *minimum) {
                    return Err(format!("enter at least {minimum} characters"));
                }
                if let Some(maximum) = max_length.filter(|maximum| length > *maximum) {
                    return Err(format!("enter at most {maximum} characters"));
                }
                if let Some(format) = format {
                    check_format(*format, text)?;
                }
                Ok(Some(text.into()))
            }
            Input::Integer {
                minimum, maximum, ..
            } => {
                let value = text
                    .trim()
                    .parse::<i64>()
                    .map_err(|_| "enter a whole number".to_owned())?;
                check_range(value, *minimum, *maximum)?;
                Ok(Some(value.into()))
            }
            Input::Number {
                minimum, maximum, ..
            } => {
                let value = text
                    .trim()
                    .parse::<f64>()
                    .ok()
                    .filter(|value| value.is_finite())
                    .ok_or_else(|| "enter a number".to_owned())?;
                check_range(value, *minimum, *maximum)?;
                Ok(Some(value.into()))
            }
            Input::Boolean { .. } | Input::Select { .. } | Input::MultiSelect { .. } => {
                Err("choose an option".into())
            }
        }
    }

    /// Checks the number of selected options.
    pub fn check_selection(&self, count: usize) -> Result<(), String> {
        let Input::MultiSelect {
            min_items,
            max_items,
            ..
        } = &self.input
        else {
            return Ok(());
        };
        if count == 0 && !self.required {
            return Ok(());
        }
        if let Some(minimum) = min_items.filter(|minimum| count < *minimum) {
            return Err(format!("select at least {minimum}"));
        }
        if let Some(maximum) = max_items.filter(|maximum| count > *maximum) {
            return Err(format!("select at most {maximum}"));
        }
        Ok(())
    }

    /// Shows an answer with option titles.
    pub fn display(&self, value: &Value) -> String {
        let title = |value: &str| match &self.input {
            Input::Select { options, .. } | Input::MultiSelect { options, .. } => options
                .iter()
                .find(|option| option.value == value)
                .map_or(value, Choice::label)
                .to_owned(),
            _ => value.to_owned(),
        };
        match value {
            Value::Bool(true) => "yes".into(),
            Value::Bool(false) => "no".into(),
            Value::String(text) => title(text),
            Value::Array(items) => items
                .iter()
                .filter_map(Value::as_str)
                .map(title)
                .collect::<Vec<_>>()
                .join(", "),
            value => value.to_string(),
        }
    }
}

fn check_range<T: PartialOrd + fmt::Display + Copy>(
    value: T,
    minimum: Option<T>,
    maximum: Option<T>,
) -> Result<(), String> {
    if let Some(minimum) = minimum.filter(|minimum| value < *minimum) {
        return Err(format!("enter {minimum} or more"));
    }
    if let Some(maximum) = maximum.filter(|maximum| value > *maximum) {
        return Err(format!("enter {maximum} or less"));
    }
    Ok(())
}

fn check_format(format: Format, text: &str) -> Result<(), String> {
    let valid = match format {
        Format::Email => text.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
                && !text.contains(char::is_whitespace)
                && !domain.contains('@')
        }),
        Format::Uri => text.split_once(':').is_some_and(|(scheme, _)| {
            scheme.starts_with(|character: char| character.is_ascii_alphabetic())
                && scheme
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || "+-.".contains(character))
                && !text.contains(char::is_whitespace)
        }),
        Format::Date => {
            text.len() == 10 && chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d").is_ok()
        }
        Format::DateTime => chrono::DateTime::parse_from_rfc3339(text).is_ok(),
    };
    if valid {
        return Ok(());
    }
    Err(match format {
        Format::Email => "enter an email address",
        Format::Uri => "enter a URI, for example https://example.com",
        Format::Date => "enter a date as YYYY-MM-DD",
        Format::DateTime => "enter a date and time, for example 2026-01-31T09:00:00Z",
    }
    .into())
}

/// The request params. Omitted and `null` fields are the same.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateParams {
    mode: String,
    message: String,
    #[serde(default)]
    requested_schema: Option<Schema>,
}

#[derive(Deserialize)]
struct Schema {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    properties: Option<Properties>,
    #[serde(default)]
    required: Option<Vec<String>>,
}

/// Properties in the order of the request.
struct Properties(Vec<(String, Value)>);

impl<'de> Deserialize<'de> for Properties {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Ordered;
        impl<'de> Visitor<'de> for Ordered {
            type Value = Properties;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("an object")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Properties, A::Error> {
                let mut properties = Vec::new();
                while let Some(entry) = map.next_entry()? {
                    properties.push(entry);
                }
                Ok(Properties(properties))
            }
        }
        deserializer.deserialize_map(Ordered)
    }
}

/// The property schemas of the stable ACP schema. Other keys are not supported.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum PropertySchema {
    String(StringSchema),
    Number(NumberSchema<f64>),
    Integer(NumberSchema<i64>),
    Boolean(BooleanSchema),
    Array(ArraySchema),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct StringSchema {
    title: Option<String>,
    description: Option<String>,
    default: Option<String>,
    #[serde(rename = "enum")]
    values: Option<Vec<String>>,
    /// Titles for `enum` in older MCP schemas.
    enum_names: Option<Vec<String>>,
    one_of: Option<Vec<EnumOption>>,
    format: Option<Format>,
    min_length: Option<usize>,
    max_length: Option<usize>,
    #[serde(rename = "_meta")]
    _meta: Option<IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NumberSchema<T> {
    title: Option<String>,
    description: Option<String>,
    default: Option<T>,
    minimum: Option<T>,
    maximum: Option<T>,
    #[serde(rename = "_meta")]
    _meta: Option<IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BooleanSchema {
    title: Option<String>,
    description: Option<String>,
    default: Option<bool>,
    #[serde(rename = "_meta")]
    _meta: Option<IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ArraySchema {
    title: Option<String>,
    description: Option<String>,
    default: Option<Vec<String>>,
    items: ItemsSchema,
    min_items: Option<usize>,
    max_items: Option<usize>,
    #[serde(rename = "_meta")]
    _meta: Option<IgnoredAny>,
}

/// `{"type": "string", "enum": [...]}` or the titled `{"anyOf": [...]}`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ItemsSchema {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(rename = "enum")]
    values: Option<Vec<String>>,
    any_of: Option<Vec<EnumOption>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnumOption {
    #[serde(rename = "const")]
    value: String,
    title: String,
    description: Option<String>,
    #[serde(rename = "_meta")]
    _meta: Option<IgnoredAny>,
}

impl From<EnumOption> for Choice {
    fn from(option: EnumOption) -> Self {
        Self {
            value: option.value,
            title: Some(option.title),
            description: option.description,
        }
    }
}

fn untitled(values: Vec<String>) -> Vec<Choice> {
    values
        .into_iter()
        .map(|value| Choice {
            value,
            title: None,
            description: None,
        })
        .collect()
}

/// A request that the client understands or declines without asking.
enum Parsed {
    Ask(Elicitation),
    Unsupported(String),
}

/// Returns `None` for invalid params or a mode that the client did not advertise.
fn parse(params: &RawValue, advertised: bool) -> Option<Parsed> {
    if !advertised {
        return None;
    }
    let params = serde_json::from_str::<CreateParams>(params.get()).ok()?;
    let message = params.message;
    match params.mode.as_str() {
        "form" => {
            let schema = params.requested_schema?;
            Some(match form_fields(schema) {
                Some(fields) => Parsed::Ask(Elicitation { message, fields }),
                None => Parsed::Unsupported(message),
            })
        }
        _ => None,
    }
}

/// Returns `None` when the form uses a construct this client cannot show.
fn form_fields(schema: Schema) -> Option<Vec<Field>> {
    if schema.kind.as_deref().is_some_and(|kind| kind != "object") {
        return None;
    }
    let properties = schema
        .properties
        .map_or_else(Vec::new, |properties| properties.0);
    let required = schema.required.unwrap_or_default();
    if !required
        .iter()
        .all(|name| properties.iter().any(|(key, _)| key == name))
    {
        return None;
    }
    let mut fields: Vec<Field> = Vec::new();
    for (name, schema) in properties {
        if fields.iter().any(|field| field.name == name) {
            return None;
        }
        let required = required.contains(&name);
        fields.push(field(
            name,
            required,
            PropertySchema::deserialize(schema).ok()?,
        )?);
    }
    Some(fields)
}

fn field(name: String, required: bool, schema: PropertySchema) -> Option<Field> {
    let (title, description, input) = match schema {
        PropertySchema::String(schema) => {
            let options = match (schema.one_of, schema.values, schema.enum_names) {
                (Some(options), None, None) => {
                    Some(options.into_iter().map(Choice::from).collect::<Vec<_>>())
                }
                (None, Some(values), None) => Some(untitled(values)),
                (None, Some(values), Some(names)) if names.len() == values.len() => Some(
                    values
                        .into_iter()
                        .zip(names)
                        .map(|(value, title)| Choice {
                            value,
                            title: Some(title),
                            description: None,
                        })
                        .collect(),
                ),
                (None, None, None) => None,
                _ => return None,
            };
            let input = match options {
                Some(options) => {
                    if options.is_empty()
                        || schema.format.is_some()
                        || schema.min_length.is_some()
                        || schema.max_length.is_some()
                    {
                        return None;
                    }
                    Input::Select {
                        default: schema
                            .default
                            .filter(|value| options.iter().any(|option| &option.value == value)),
                        options,
                    }
                }
                None => Input::Text {
                    default: schema.default,
                    min_length: schema.min_length,
                    max_length: schema.max_length,
                    format: schema.format,
                },
            };
            (schema.title, schema.description, input)
        }
        PropertySchema::Number(schema) => (
            schema.title,
            schema.description,
            Input::Number {
                default: schema.default,
                minimum: schema.minimum,
                maximum: schema.maximum,
            },
        ),
        PropertySchema::Integer(schema) => (
            schema.title,
            schema.description,
            Input::Integer {
                default: schema.default,
                minimum: schema.minimum,
                maximum: schema.maximum,
            },
        ),
        PropertySchema::Boolean(schema) => (
            schema.title,
            schema.description,
            Input::Boolean {
                default: schema.default,
            },
        ),
        PropertySchema::Array(schema) => {
            let options = match (
                schema.items.kind.as_deref(),
                schema.items.values,
                schema.items.any_of,
            ) {
                (Some("string"), Some(values), None) => untitled(values),
                (None, None, Some(options)) => options.into_iter().map(Choice::from).collect(),
                _ => return None,
            };
            if options.is_empty() {
                return None;
            }
            let default = schema
                .default
                .unwrap_or_default()
                .into_iter()
                .filter(|value| options.iter().any(|option| &option.value == value))
                .collect();
            (
                schema.title,
                schema.description,
                Input::MultiSelect {
                    options,
                    default,
                    min_items: schema.min_items,
                    max_items: schema.max_items,
                },
            )
        }
    };
    Some(Field {
        name,
        title,
        description,
        required,
        input,
    })
}

impl Client {
    pub(super) async fn handle_elicitation<H: Handler>(
        &mut self,
        id: Value,
        params: &RawValue,
        updates: &mut Updates<H>,
        cancelled: bool,
    ) -> Result<()> {
        let Some(parsed) = parse(params, self.elicitation) else {
            return self
                .write(&json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32602, "message": "Invalid elicitation/create params or unsupported mode"}
                }))
                .await;
        };
        let request = match parsed {
            Parsed::Ask(request) => request,
            Parsed::Unsupported(message) => {
                self.write(&json!({"jsonrpc": "2.0", "id": id, "result": {"action": "decline"}}))
                    .await?;
                return updates.event(StreamEvent::Elicitation {
                    message,
                    outcome: ElicitationOutcome::Unsupported,
                });
            }
        };
        let answer = if cancelled {
            Ok(ElicitationAnswer::Cancel)
        } else {
            updates.elicitation(&request)
        };
        let (result, outcome) = match &answer {
            Ok(ElicitationAnswer::Accept(content)) => (
                json!({"action": "accept", "content": content}),
                ElicitationOutcome::Answered,
            ),
            Ok(ElicitationAnswer::Decline) => {
                (json!({"action": "decline"}), ElicitationOutcome::Declined)
            }
            Ok(ElicitationAnswer::Cancel) | Err(_) => {
                (json!({"action": "cancel"}), ElicitationOutcome::Cancelled)
            }
        };
        // Every request needs an answer, also when the prompt failed.
        self.write(&json!({"jsonrpc": "2.0", "id": id, "result": result}))
            .await?;
        updates.event(StreamEvent::Elicitation {
            message: request.message,
            outcome,
        })?;
        answer.map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Takes the schema as text, because `Value` sorts object keys.
    fn fields(schema: &str) -> Option<Vec<Field>> {
        let params = format!(r#"{{"mode":"form","message":"m","requestedSchema":{schema}}}"#);
        match parse(&RawValue::from_string(params).unwrap(), true)? {
            Parsed::Ask(Elicitation { fields, .. }) => Some(fields),
            Parsed::Unsupported(_) => None,
        }
    }

    #[test]
    fn keeps_property_order_and_reads_every_supported_field() {
        let fields = fields(
            r#"{
            "type": "object",
            "properties": {
                "zeta": {"type": "string", "title": "Name", "minLength": 2, "default": "Ada",
                    "_meta": {"x": 1}},
                "alpha": {"type": "integer", "minimum": 1, "maximum": 9, "default": 3},
                "ratio": {"type": "number", "minimum": 0.5},
                "agree": {"type": "boolean", "default": true},
                "plain": {"type": "string", "enum": ["a", "b"], "default": "b"},
                "titled": {"type": "string", "oneOf": [
                    {"const": "x", "title": "Ex", "description": "First"}]},
                "legacy": {"type": "string", "enum": ["l"], "enumNames": ["Legacy"]},
                "tags": {"type": "array", "items": {"type": "string", "enum": ["t", "u"]},
                    "default": ["u", "missing"], "maxItems": 1},
                "picks": {"type": "array", "items": {"anyOf": [{"const": "p", "title": "P"}]}}
            },
            "required": ["zeta", "tags"]
        }"#,
        )
        .unwrap();
        let names = fields
            .iter()
            .map(|field| field.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            ["zeta", "alpha", "ratio", "agree", "plain", "titled", "legacy", "tags", "picks"]
        );
        assert_eq!(fields[0].label(), "Name");
        assert!(fields[0].required && !fields[1].required && fields[7].required);
        assert_eq!(fields[0].default_value(), Some(json!("Ada")));
        assert_eq!(fields[4].default_value(), Some(json!("b")));
        assert_eq!(fields[7].default_value(), Some(json!(["u"])));
        assert_eq!(fields[5].display(&json!("x")), "Ex");
        assert_eq!(fields[6].display(&json!("l")), "Legacy");
        assert_eq!(fields[8].display(&json!(["p"])), "P");
        assert!(matches!(
            fields[2].input,
            Input::Number {
                minimum: Some(0.5),
                ..
            }
        ));
    }

    #[test]
    fn declines_constructs_outside_the_supported_subset() {
        for property in [
            json!({"type": "string", "pattern": "^a$"}),
            json!({"type": "string", "format": "hostname"}),
            json!({"type": "integer", "minimum": 1.5}),
            json!({"type": "number", "exclusiveMinimum": 0}),
            json!({"type": "object", "properties": {}}),
            json!({"type": "array", "items": {"type": "number"}}),
            json!({"type": "string", "enum": []}),
            json!({"type": "string", "enum": ["a"], "oneOf": [{"const": "a", "title": "A"}]}),
            json!({"type": ["string", "null"]}),
            json!({"$ref": "#/$defs/name"}),
        ] {
            let schema = json!({"properties": {"x": property}}).to_string();
            assert!(fields(&schema).is_none(), "{property}");
        }
        assert!(fields(r#"{"type": "array"}"#).is_none());
        assert!(fields(r#"{"properties": {}, "required": ["missing"]}"#).is_none());
        assert!(
            fields(r#"{"properties": {"a": {"type": "boolean"}, "a": {"type": "boolean"}}}"#)
                .is_none()
        );
        assert_eq!(fields("{}"), Some(Vec::new()));
    }

    #[test]
    fn validates_typed_answers() {
        let fields = fields(
            r#"{"properties": {
            "name": {"type": "string", "minLength": 2, "maxLength": 3},
            "mail": {"type": "string", "format": "email"},
            "day": {"type": "string", "format": "date"},
            "count": {"type": "integer", "minimum": 1, "maximum": 5},
            "ratio": {"type": "number"},
            "tags": {"type": "array", "items": {"type": "string", "enum": ["a", "b"]},
                "minItems": 1, "maxItems": 1}
        }, "required": ["name", "count", "tags"]}"#,
        )
        .unwrap();
        assert!(fields[0].parse("").is_err());
        assert!(fields[0].parse("a").is_err());
        assert!(fields[0].parse("abcd").is_err());
        assert_eq!(fields[0].parse("界界"), Ok(Some(json!("界界"))));
        assert_eq!(fields[1].parse(""), Ok(None));
        assert!(fields[1].parse("not mail").is_err());
        assert!(fields[1].parse("a@example.com").is_ok());
        assert!(fields[2].parse("2026-02-30").is_err());
        assert!(fields[2].parse("2026-02-28").is_ok());
        assert!(fields[3].parse("6").is_err());
        assert!(fields[3].parse("2.5").is_err());
        assert_eq!(fields[3].parse(" 4 "), Ok(Some(json!(4))));
        assert!(fields[4].parse("NaN").is_err());
        assert_eq!(fields[4].parse("0.25"), Ok(Some(json!(0.25))));
        assert!(fields[5].check_selection(0).is_err());
        assert!(fields[5].check_selection(1).is_ok());
        assert!(fields[5].check_selection(2).is_err());
    }

    #[test]
    fn rejects_unadvertised_modes_and_invalid_params() {
        let raw = |value: Value| serde_json::value::to_raw_value(&value).unwrap();
        let form = raw(json!({"mode": "form", "message": "m", "requestedSchema": {}}));
        assert!(parse(&form, false).is_none());
        assert!(parse(&form, true).is_some());
        for params in [
            json!({"mode": "form", "message": "m"}),
            json!({"mode": "url", "message": "m", "url": "https://a.example",
                "elicitationId": "e"}),
            json!({"mode": "_custom", "message": "m"}),
            json!({"mode": "form", "requestedSchema": {}}),
            json!({"mode": "form", "message": "m", "requestedSchema": {"properties": []}}),
        ] {
            assert!(parse(&raw(params.clone()), true).is_none(), "{params}");
        }
    }
}
