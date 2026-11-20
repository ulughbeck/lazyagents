use std::fmt;

use anyhow::Result;
use inquire::list_option::ListOption;
use inquire::validator::Validation;
use inquire::{InquireError, MultiSelect, Select, Text};
use serde_json::{Map, Value};

use super::output::{compact_line, display_width, print_colored};
use crate::acp::{Choice, Elicitation, ElicitationAnswer, Field, Input};

/// Asks the user to fill in a form. Esc cancels the request.
/// Ctrl-C returns `InquireError::OperationInterrupted`, so the turn stops.
pub(super) fn ask(request: &Elicitation) -> Result<ElicitationAnswer> {
    let message = crate::markdown::sanitize(&request.message);
    print_colored(
        true,
        "33",
        &format!("? {}\n", message.trim_end().replace('\n', "\n  ")),
    );
    match form(&request.fields) {
        Err(InquireError::OperationCanceled) => Ok(ElicitationAnswer::Cancel),
        answer => Ok(answer?),
    }
}

/// Asks each field in order, then lets the user review and change the answers.
fn form(fields: &[Field]) -> Result<ElicitationAnswer, InquireError> {
    let mut answers = fields.iter().map(Field::default_value).collect::<Vec<_>>();
    for (field, answer) in fields.iter().zip(&mut answers) {
        *answer = ask_field(field, answer.as_ref())?;
    }
    loop {
        let mut choices = vec![Pick::new("submit", Review::Submit)];
        choices.extend(
            fields
                .iter()
                .zip(&answers)
                .enumerate()
                .map(|(index, (field, answer))| {
                    let value = answer
                        .as_ref()
                        .map_or_else(|| "skipped".to_owned(), |value| field.display(value));
                    Pick::new(
                        &format!("{}: {value}", field.label().to_lowercase()),
                        Review::Edit(index),
                    )
                }),
        );
        choices.push(Pick::new("decline", Review::Decline));
        let rows = crossterm::terminal::size().map_or(24, |(_, rows)| usize::from(rows));
        let page_size = choices.len().min(rows.saturating_sub(4).max(3));
        let review = crate::prompts::select(
            Select::new("send answers?", choices)
                .with_page_size(page_size)
                .with_render_config(answered_theme())
                .with_help_message("select a field to change it"),
        )?;
        match review.value {
            Review::Submit => break,
            Review::Decline => return Ok(ElicitationAnswer::Decline),
            Review::Edit(index) => {
                answers[index] = ask_field(&fields[index], answers[index].as_ref())?
            }
        }
    }
    let content = fields
        .iter()
        .zip(answers)
        .filter_map(|(field, answer)| Some((field.name.clone(), answer?)))
        .collect::<Map<_, _>>();
    Ok(ElicitationAnswer::Accept(content))
}

#[derive(Clone, Copy)]
enum Review {
    Submit,
    Edit(usize),
    Decline,
}

/// A menu entry with its own label.
#[derive(Clone)]
struct Pick<T> {
    label: String,
    value: T,
}

impl<T> Pick<T> {
    fn new(label: &str, value: T) -> Self {
        Self {
            label: compact_line(label, display_width()),
            value,
        }
    }
}

impl<T> fmt::Display for Pick<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.label)
    }
}

/// Asks one field, starting from `current`. Returns `None` for a skipped field.
fn ask_field(field: &Field, current: Option<&Value>) -> Result<Option<Value>, InquireError> {
    let label = format!(
        "{}:",
        compact_line(field.label(), display_width()).to_lowercase()
    );
    let mut help = field
        .description
        .iter()
        .map(|description| compact_line(description, display_width()))
        .collect::<Vec<_>>();
    if !field.required {
        help.push("optional".into());
    }
    let skip = (!field.required).then(|| Pick::new("skip", None));
    match &field.input {
        Input::Text { .. } | Input::Integer { .. } | Input::Number { .. } => {
            if !field.required {
                help.pop();
                help.push("optional, leave empty to skip".into());
            }
            let help = help.join(" · ");
            let initial = current
                .map(|value| match value {
                    Value::String(text) => crate::markdown::sanitize(text),
                    value => value.to_string(),
                })
                .unwrap_or_default();
            let mut prompt = Text::new(&label)
                .with_render_config(answered_theme())
                .with_initial_value(&initial)
                .with_formatter(&skipped_if_empty)
                .with_validator(|text: &str| {
                    Ok(match field.parse(text) {
                        Ok(_) => Validation::Valid,
                        Err(error) => Validation::Invalid(error.into()),
                    })
                });
            if !help.is_empty() {
                prompt = prompt.with_help_message(&help);
            }
            let text = prompt.prompt()?;
            Ok(field.parse(&text).ok().flatten())
        }
        Input::Boolean { .. } => {
            let mut choices = vec![
                Pick::new("yes", Some(Value::Bool(true))),
                Pick::new("no", Some(Value::Bool(false))),
            ];
            choices.extend(skip);
            select(field, &label, choices, current, &help)
        }
        Input::Select { options, .. } => {
            let mut choices = options
                .iter()
                .map(|option| {
                    Pick::new(
                        &choice_label(option),
                        Some(Value::from(option.value.clone())),
                    )
                })
                .collect::<Vec<_>>();
            choices.extend(skip);
            select(field, &label, choices, current, &help)
        }
        Input::MultiSelect { options, .. } => {
            help.push("space to select".into());
            let help = help.join(" · ");
            let selected = current
                .and_then(Value::as_array)
                .map(|values| {
                    options
                        .iter()
                        .enumerate()
                        .filter(|(_, option)| values.iter().any(|value| value == &option.value))
                        .map(|(index, _)| index)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let choices = options
                .iter()
                .map(|option| Pick::new(&choice_label(option), option.value.clone()))
                .collect();
            let check = field.clone();
            let picked = crate::prompts::multi_select(
                MultiSelect::new(&label, choices)
                    .with_render_config(answered_theme())
                    .with_default(&selected)
                    .with_help_message(&help)
                    .with_formatter(&|picked| {
                        let values = picked.iter().map(|pick| pick.value.value.clone());
                        field.display(&values.collect())
                    })
                    .with_validator(move |picked: &[ListOption<&Pick<String>>]| {
                        Ok(match check.check_selection(picked.len()) {
                            Ok(()) => Validation::Valid,
                            Err(error) => Validation::Invalid(error.into()),
                        })
                    }),
            )?;
            if picked.is_empty() && !field.required {
                return Ok(None);
            }
            Ok(Some(picked.into_iter().map(|pick| pick.value).collect()))
        }
    }
}

/// One answer from a menu. The cursor starts at the current answer.
fn select(
    field: &Field,
    label: &str,
    choices: Vec<Pick<Option<Value>>>,
    current: Option<&Value>,
    help: &[String],
) -> Result<Option<Value>, InquireError> {
    let cursor = choices
        .iter()
        .position(|choice| choice.value.as_ref() == current)
        .unwrap_or_default();
    let help = help.join(" · ");
    let answer = |choice: ListOption<&Pick<Option<Value>>>| {
        let value = choice.value.value.as_ref();
        value.map_or_else(|| "skipped".into(), |value| field.display(value))
    };
    let mut prompt = Select::new(label, choices)
        .with_render_config(answered_theme())
        .with_starting_cursor(cursor)
        .with_formatter(&answer);
    if !help.is_empty() {
        prompt = prompt.with_help_message(&help);
    }
    crate::prompts::select(prompt).map(|choice| choice.value)
}

fn skipped_if_empty(text: &str) -> String {
    match text.trim() {
        "" => "skipped".into(),
        _ => text.into(),
    }
}

fn choice_label(option: &Choice) -> String {
    match &option.description {
        Some(description) => format!("{} · {description}", option.label()),
        None => option.label().to_owned(),
    }
}

fn answered_theme() -> inquire::ui::RenderConfig<'static> {
    crate::prompts::theme().with_answered_prompt_prefix(inquire::ui::Styled::new(">"))
}
