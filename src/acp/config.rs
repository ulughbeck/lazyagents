use std::collections::BTreeMap;
use std::fmt;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::Client;

#[derive(Clone, Copy)]
pub enum ConfigKind {
    Model,
    Thinking,
}

/// A config option value as the agent sends it and `agent.json` stores it.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum ConfigValue {
    Boolean(bool),
    Select(String),
}

impl fmt::Display for ConfigValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Boolean(true) => formatter.write_str("on"),
            Self::Boolean(false) => formatter.write_str("off"),
            Self::Select(value) => formatter.write_str(value),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ConfigChoice {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
}

impl fmt::Display for ConfigChoice {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = crate::markdown::sanitize(&self.label);
        match &self.description {
            Some(description) if !description.is_empty() => {
                let description = crate::markdown::sanitize(description);
                write!(formatter, "{label} - {description}")
            }
            _ => formatter.write_str(&label),
        }
    }
}

/// A session config option of a type this client supports.
#[derive(Clone, Debug)]
pub enum ConfigOption {
    Select(SelectConfigOption),
    Boolean(BooleanConfigOption),
}

#[derive(Clone, Debug)]
pub struct SelectConfigOption {
    category: Option<String>,
    pub id: String,
    pub name: String,
    pub current_value: String,
    pub choices: Vec<ConfigChoice>,
}

#[derive(Clone, Debug)]
pub struct BooleanConfigOption {
    category: Option<String>,
    pub id: String,
    pub name: String,
    pub current_value: bool,
}

impl SelectConfigOption {
    /// The index of the current choice, for a picker's starting position.
    pub fn cursor(&self) -> usize {
        self.choices
            .iter()
            .position(|choice| choice.value == self.current_value)
            .unwrap_or_default()
    }

    fn current_label(&self) -> &str {
        self.choices
            .iter()
            .find(|choice| choice.value == self.current_value)
            .map_or(&self.current_value, |choice| &choice.label)
    }
}

impl ConfigOption {
    pub fn id(&self) -> &str {
        match self {
            Self::Select(option) => &option.id,
            Self::Boolean(option) => &option.id,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Select(option) => &option.name,
            Self::Boolean(option) => &option.name,
        }
    }

    fn category(&self) -> Option<&str> {
        match self {
            Self::Select(option) => option.category.as_deref(),
            Self::Boolean(option) => option.category.as_deref(),
        }
    }

    pub fn value(&self) -> ConfigValue {
        match self {
            Self::Select(option) => ConfigValue::Select(option.current_value.clone()),
            Self::Boolean(option) => ConfigValue::Boolean(option.current_value),
        }
    }

    /// The current value as the agent names it.
    pub fn current_label(&self) -> String {
        match self {
            Self::Select(option) => option.current_label().to_owned(),
            Self::Boolean(option) => ConfigValue::Boolean(option.current_value).to_string(),
        }
    }

    fn accepts(&self, value: &ConfigValue) -> bool {
        match (self, value) {
            (Self::Select(option), ConfigValue::Select(value)) => {
                option.choices.iter().any(|choice| &choice.value == value)
            }
            (Self::Boolean(_), ConfigValue::Boolean(_)) => true,
            _ => false,
        }
    }

    fn set_current(&mut self, value: &ConfigValue) {
        match (self, value) {
            (Self::Select(option), ConfigValue::Select(value)) => {
                option.current_value.clone_from(value);
            }
            (Self::Boolean(option), ConfigValue::Boolean(value)) => option.current_value = *value,
            _ => {}
        }
    }
}

impl Client {
    pub fn select_config_option(&self, kind: ConfigKind) -> Option<&SelectConfigOption> {
        self.config_options.iter().find_map(|option| match option {
            ConfigOption::Select(option)
                if matches_kind(kind, &option.id, option.category.as_deref()) =>
            {
                Some(option)
            }
            _ => None,
        })
    }

    /// Options in the `model_config` category, in the agent's order. The model
    /// and reasoning options are excluded because `/model` asks for them first.
    pub fn model_config_options(&self) -> Vec<&ConfigOption> {
        let excluded = [ConfigKind::Model, ConfigKind::Thinking].map(|kind| {
            self.select_config_option(kind)
                .map(|option| option.id.as_str())
        });
        self.config_options
            .iter()
            .filter(|option| option.category() == Some("model_config"))
            .filter(|option| !excluded.contains(&Some(option.id())))
            .collect()
    }

    /// The current `model_config` values, as `agent.json` stores them.
    pub fn model_config_values(&self) -> BTreeMap<String, ConfigValue> {
        self.model_config_options()
            .into_iter()
            .map(|option| (option.id().to_owned(), option.value()))
            .collect()
    }

    pub async fn set_config_option(&mut self, id: &str, value: ConfigValue) -> Result<()> {
        let mut params = json!({"sessionId": self.session_id, "configId": id});
        match &value {
            ConfigValue::Boolean(value) => {
                params["type"] = "boolean".into();
                params["value"] = (*value).into();
            }
            ConfigValue::Select(value) => params["value"] = value.as_str().into(),
        }
        let response = self
            .request("session/set_config_option", params)
            .await
            .with_context(|| format!("could not set ACP option {id:?}"))?;
        if let Some(options) = response.get("configOptions").and_then(Value::as_array) {
            self.set_config_options(options);
        } else if let Some(option) = self
            .config_options
            .iter_mut()
            .find(|option| option.id() == id)
        {
            option.set_current(&value);
        }
        Ok(())
    }

    pub async fn set_config(&mut self, kind: ConfigKind, value: &str) -> Result<bool> {
        let Some(option) = self.select_config_option(kind) else {
            return Ok(false);
        };
        if option.current_value == value {
            return Ok(true);
        }
        if !option.choices.iter().any(|choice| choice.value == value) {
            return Ok(false);
        }
        let id = option.id.clone();
        self.set_config_option(&id, ConfigValue::Select(value.to_owned()))
            .await?;
        Ok(true)
    }

    /// Applies a saved `model_config` value. Returns false when the agent no
    /// longer offers the option or the value.
    pub async fn set_model_config(&mut self, id: &str, value: &ConfigValue) -> Result<bool> {
        let Some(option) = self
            .model_config_options()
            .into_iter()
            .find(|option| option.id() == id)
        else {
            return Ok(false);
        };
        if !option.accepts(value) {
            return Ok(false);
        }
        if &option.value() != value {
            self.set_config_option(id, value.clone()).await?;
        }
        Ok(true)
    }

    pub fn current_config_label(&self, kind: ConfigKind) -> Option<&str> {
        self.select_config_option(kind)
            .map(SelectConfigOption::current_label)
    }

    /// Replaces the options with a complete list from the agent.
    pub(super) fn set_config_options(&mut self, options: &[Value]) {
        self.config_options = options.iter().filter_map(parse_config_option).collect();
    }
}

fn matches_kind(kind: ConfigKind, id: &str, category: Option<&str>) -> bool {
    match kind {
        ConfigKind::Model => category == Some("model") || id == "model",
        ConfigKind::Thinking => {
            category == Some("thought_level")
                || ["reasoning", "thinking", "effort"]
                    .iter()
                    .any(|hint| id.contains(hint))
        }
    }
}

/// A config option as the agent sends it. Other types fail to parse and are ignored.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RawOption {
    Select(RawSelect),
    Boolean(RawBoolean),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSelect {
    id: String,
    name: Option<String>,
    category: Option<String>,
    current_value: String,
    options: Vec<RawChoice>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawBoolean {
    id: String,
    name: Option<String>,
    category: Option<String>,
    current_value: bool,
}

/// A select value or a group of values. Malformed entries are skipped.
#[derive(Deserialize)]
#[serde(untagged)]
enum RawChoice {
    Group {
        options: Vec<RawChoice>,
    },
    Value {
        value: String,
        name: Option<String>,
        description: Option<String>,
    },
    Other(serde::de::IgnoredAny),
}

impl RawChoice {
    fn flatten(self, choices: &mut Vec<ConfigChoice>) {
        match self {
            Self::Group { options } => {
                for option in options {
                    option.flatten(choices);
                }
            }
            Self::Value {
                value,
                name,
                description,
            } => choices.push(ConfigChoice {
                label: name.unwrap_or_else(|| value.clone()),
                value,
                description,
            }),
            Self::Other(_) => {}
        }
    }
}

fn parse_config_option(option: &Value) -> Option<ConfigOption> {
    match RawOption::deserialize(option).ok()? {
        RawOption::Select(option) => {
            let mut choices = Vec::new();
            for choice in option.options {
                choice.flatten(&mut choices);
            }
            if choices.is_empty() {
                return None;
            }
            // Model names are shown in lowercase everywhere.
            if matches_kind(ConfigKind::Model, &option.id, option.category.as_deref()) {
                for choice in &mut choices {
                    choice.label = choice.label.to_lowercase();
                }
            }
            Some(ConfigOption::Select(SelectConfigOption {
                name: option.name.unwrap_or_else(|| option.id.clone()),
                id: option.id,
                category: option.category,
                current_value: option.current_value,
                choices,
            }))
        }
        RawOption::Boolean(option) => Some(ConfigOption::Boolean(BooleanConfigOption {
            name: option.name.unwrap_or_else(|| option.id.clone()),
            id: option.id,
            category: option.category,
            current_value: option.current_value,
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_grouped_select_and_boolean_options_and_ignores_unknown_types() {
        let options = [
            json!({"id": "model", "name": "Model", "type": "select", "currentValue": "deep",
            "options": [{"group": "models", "name": "Models", "options": [
                {"value": "fast", "name": "Fast"},
                {"value": "deep", "name": "Deep", "description": "More capable"},
                {"name": "No value"}
            ]}]}),
            json!({"id": "fast-mode", "name": "Fast mode", "type": "boolean",
                "category": "model_config", "currentValue": true}),
            json!({"id": "budget", "name": "Budget", "type": "number", "currentValue": 3}),
            json!({"id": "broken", "type": "boolean", "currentValue": "yes"}),
        ];
        let parsed: Vec<_> = options.iter().filter_map(parse_config_option).collect();
        assert_eq!(parsed.len(), 2);
        let ConfigOption::Select(model) = &parsed[0] else {
            panic!("model is not a select option");
        };
        assert_eq!(model.choices.len(), 2);
        assert_eq!(model.choices[1].label, "deep");
        assert_eq!(
            model.choices[1].description.as_deref(),
            Some("More capable")
        );
        assert_eq!(model.current_label(), "deep");
        assert_eq!(parsed[1].value(), ConfigValue::Boolean(true));
        assert_eq!(parsed[1].current_label(), "on");
        assert!(!parsed[1].accepts(&ConfigValue::Select("true".into())));
    }

    #[test]
    fn recognizes_standard_model_and_thinking_options() {
        assert!(matches_kind(ConfigKind::Model, "model", Some("model")));
        assert!(matches_kind(
            ConfigKind::Thinking,
            "reasoning_effort",
            Some("thought_level")
        ));
        assert!(matches_kind(ConfigKind::Thinking, "effort", None));
        assert!(!matches_kind(ConfigKind::Model, "mode", None));
    }

    #[test]
    fn stores_values_in_their_json_types() {
        let values = BTreeMap::from([
            ("context".to_owned(), ConfigValue::Select("1m".into())),
            ("fast-mode".to_owned(), ConfigValue::Boolean(false)),
        ]);
        let stored = serde_json::to_value(&values).unwrap();
        assert_eq!(stored, json!({"context": "1m", "fast-mode": false}));
        assert_eq!(
            serde_json::from_value::<BTreeMap<String, ConfigValue>>(stored).unwrap(),
            values
        );
    }
}
