//! `rider.toml` under the Rider home: the role table and limits, set over the built-in ones.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::core::{ATTEMPT_MILLIS_MAX, ROLES_MAX};
use crate::roles::{AGENTS_MAX, Harness, PROFILE_LEN_MAX, ROLES, RoleSpec};

/// The file's name under the Rider home.
pub const FILE_NAME: &str = "rider.toml";
/// Upper bound on the file's size.
pub const FILE_BYTES_MAX: u64 = 64 * 1024;
/// Upper bound on, and default of, the time one attempt may run, in minutes.
pub const ATTEMPT_MINUTES_MAX: u64 = ATTEMPT_MILLIS_MAX / 60_000;
const _: () =
    assert!(ATTEMPT_MINUTES_MAX > 0 && ATTEMPT_MINUTES_MAX * 60_000 <= ATTEMPT_MILLIS_MAX);
const PROMPT_SUFFIX: &str = ".md";
const SLOT_DIGITS_MAX: usize = 2;
const _: () = assert!(AGENTS_MAX < 100);

/// The limits the daemon and pool run under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Agents running at once, across every role.
    pub agents: u8,
    pub attempt_millis: u64,
}

/// The effective role table and limits, and the TOML that states them.
#[derive(Debug)]
pub struct Settings {
    pub roles: Vec<RoleSpec>,
    pub limits: Limits,
    pub toml: String,
}

/// A `rider.toml` that cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigFileError {
    Read {
        path: PathBuf,
        message: String,
    },
    TooLarge {
        path: PathBuf,
    },
    Parse {
        path: PathBuf,
        message: String,
    },
    Field {
        path: PathBuf,
        field: String,
        problem: String,
    },
}

impl fmt::Display for ConfigFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigFileError::Read { path, message } => {
                write!(f, "could not read {}: {message}", path.display())
            }
            ConfigFileError::TooLarge { path } => {
                write!(
                    f,
                    "{} is larger than {FILE_BYTES_MAX} bytes",
                    path.display()
                )
            }
            ConfigFileError::Parse { path, message } => {
                write!(f, "{}: {}", path.display(), message.trim_end())
            }
            ConfigFileError::Field {
                path,
                field,
                problem,
            } => write!(f, "{}: {field} {problem}", path.display()),
        }
    }
}

impl std::error::Error for ConfigFileError {}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileShape {
    #[serde(default)]
    limits: LimitsShape,
    #[serde(default)]
    roles: BTreeMap<String, RoleShape>,
}

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LimitsShape {
    #[serde(skip_serializing_if = "Option::is_none")]
    agents: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    attempt_minutes: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum HarnessName {
    Claude,
    Codex,
    #[serde(skip_deserializing)]
    Tact,
}

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RoleShape {
    #[serde(skip_serializing_if = "Option::is_none")]
    harness: Option<HarnessName>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model_slug: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    commits: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    slots: Option<u8>,
}

impl RoleShape {
    fn built_in(spec: &RoleSpec) -> RoleShape {
        let harness = match spec.harness {
            Harness::Claude => HarnessName::Claude,
            Harness::Codex => HarnessName::Codex,
            Harness::Tact => HarnessName::Tact,
        };
        let prompt = spec.prompt_file.strip_suffix(PROMPT_SUFFIX);
        assert!(prompt.is_some(), "built-in prompts are .md files");
        RoleShape {
            harness: Some(harness),
            model: Some(spec.model.to_string()),
            title: Some(spec.title.to_string()),
            model_name: Some(spec.model_name.to_string()),
            model_slug: Some(spec.model_slug.to_string()),
            prompt: prompt.map(str::to_string),
            commits: Some(spec.commits_expected),
            slots: None,
        }
    }

    fn overlay(&mut self, over: RoleShape) {
        let RoleShape {
            harness,
            model,
            title,
            model_name,
            model_slug,
            prompt,
            commits,
            slots,
        } = over;
        self.harness = harness.or(self.harness);
        self.model = model.or(self.model.take());
        self.title = title.or(self.title.take());
        self.model_name = model_name.or(self.model_name.take());
        self.model_slug = model_slug.or(self.model_slug.take());
        self.prompt = prompt.or(self.prompt.take());
        self.commits = commits.or(self.commits);
        self.slots = slots.or(self.slots);
    }
}

/// The settings from `<home>/rider.toml` over the built-in ones; the built-in ones alone when
/// there is no file.
pub fn load(home: &Path) -> Result<Settings, ConfigFileError> {
    assert!(home.is_absolute());
    let path = home.join(FILE_NAME);
    let shape = match read(&path)? {
        None => FileShape::default(),
        Some(text) => {
            toml::from_str::<FileShape>(&text).map_err(|error| ConfigFileError::Parse {
                path: path.clone(),
                message: error.to_string(),
            })?
        }
    };
    let mut roles: Vec<(String, RoleShape)> = ROLES
        .iter()
        .map(|spec| (spec.label.to_string(), RoleShape::built_in(spec)))
        .collect();
    for (label, over) in shape.roles {
        let count = roles.len();
        match roles.iter_mut().find(|(known, _)| *known == label) {
            Some((_, role)) => role.overlay(over),
            None if count >= ROLES_MAX => {
                let problem = format!("is one role more than the {ROLES_MAX} allowed");
                return Err(field_error(&path, format!("roles.{label}"), problem));
            }
            None => roles.push((label, over)),
        }
    }
    let limits = limits(&path, &shape.limits)?;
    let specs = roles
        .iter()
        .map(|(label, role)| role_spec(&path, home, label, role, limits.agents))
        .collect::<Result<Vec<RoleSpec>, ConfigFileError>>()?;
    let toml = render(&path, &limits, &roles)?;
    assert!(!specs.is_empty() && specs.len() <= ROLES_MAX);
    assert!(
        specs
            .iter()
            .all(|spec| spec.slots > 0 && spec.slots <= AGENTS_MAX)
    );
    Ok(Settings {
        roles: specs,
        limits,
        toml,
    })
}

fn read(path: &Path) -> Result<Option<String>, ConfigFileError> {
    let read_error = |error: std::io::Error| ConfigFileError::Read {
        path: path.to_path_buf(),
        message: error.to_string(),
    };
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(read_error(error)),
    };
    let mut text = String::new();
    let bound = FILE_BYTES_MAX.saturating_add(1);
    file.take(bound)
        .read_to_string(&mut text)
        .map_err(read_error)?;
    if u64::try_from(text.len()).map_or(true, |len| len > FILE_BYTES_MAX) {
        return Err(ConfigFileError::TooLarge {
            path: path.to_path_buf(),
        });
    }
    Ok(Some(text))
}

fn field_error(path: &Path, field: String, problem: String) -> ConfigFileError {
    assert!(!field.is_empty() && !problem.is_empty());
    ConfigFileError::Field {
        path: path.to_path_buf(),
        field,
        problem,
    }
}

fn limits(path: &Path, shape: &LimitsShape) -> Result<Limits, ConfigFileError> {
    let agents = shape.agents.unwrap_or(AGENTS_MAX);
    if !(1..=AGENTS_MAX).contains(&agents) {
        let problem = format!("must be from 1 to {AGENTS_MAX}, not {agents}");
        return Err(field_error(path, "limits.agents".to_string(), problem));
    }
    let minutes = shape.attempt_minutes.unwrap_or(ATTEMPT_MINUTES_MAX);
    if !(1..=ATTEMPT_MINUTES_MAX).contains(&minutes) {
        let problem = format!("must be from 1 to {ATTEMPT_MINUTES_MAX}, not {minutes}");
        return Err(field_error(
            path,
            "limits.attempt_minutes".to_string(),
            problem,
        ));
    }
    let limits = Limits {
        agents,
        attempt_millis: minutes * 60_000,
    };
    assert!(limits.attempt_millis > 0 && limits.attempt_millis <= ATTEMPT_MILLIS_MAX);
    Ok(limits)
}

fn is_name(text: &str) -> bool {
    text.starts_with(|c: char| c.is_ascii_lowercase())
        && text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

// The runtime role from a merged shape; every field must be set and valid.
fn role_spec(
    path: &Path,
    home: &Path,
    label: &str,
    role: &RoleShape,
    agents: u8,
) -> Result<RoleSpec, ConfigFileError> {
    let error =
        |name: &str, problem: String| field_error(path, format!("roles.{label}.{name}"), problem);
    let missing = |name: &str| {
        error(
            name,
            "is missing for a role that is not built in".to_string(),
        )
    };
    if !is_name(label) {
        let problem = "must be lowercase letters, digits and '-', starting with a letter";
        return Err(field_error(
            path,
            format!("roles.{label}"),
            problem.to_string(),
        ));
    }
    let harness = match role.harness.ok_or_else(|| missing("harness"))? {
        HarnessName::Claude => Harness::Claude,
        HarnessName::Codex => Harness::Codex,
        HarnessName::Tact => Harness::Tact,
    };
    let model = role.model.clone().ok_or_else(|| missing("model"))?;
    if model.is_empty() || model.starts_with('-') || model.contains(char::is_whitespace) {
        return Err(error("model", format!("{model:?} is not a model name")));
    }
    let title = role.title.clone().ok_or_else(|| missing("title"))?;
    if !title.eq_ignore_ascii_case(label) {
        return Err(error("title", format!("must be {label:?} in any case")));
    }
    let model_name = role
        .model_name
        .clone()
        .ok_or_else(|| missing("model_name"))?;
    if model_name.trim().is_empty() {
        return Err(error("model_name", "must not be empty".to_string()));
    }
    let model_slug = role
        .model_slug
        .clone()
        .ok_or_else(|| missing("model_slug"))?;
    let profile_len = label.len() + model_slug.len() + 2 + SLOT_DIGITS_MAX;
    if !is_name(&model_slug) || profile_len > PROFILE_LEN_MAX {
        let problem = format!(
            "must be lowercase letters, digits and '-', and leave the profile \
             <role>-<slot>-<model_slug> at most {PROFILE_LEN_MAX} characters"
        );
        return Err(error("model_slug", problem));
    }
    let prompt = role.prompt.as_deref().ok_or_else(|| missing("prompt"))?;
    let prompt_file = prompt_file(home, prompt).map_err(|problem| error("prompt", problem))?;
    let commits = role.commits.ok_or_else(|| missing("commits"))?;
    let slots = role.slots.unwrap_or(agents);
    if !(1..=AGENTS_MAX).contains(&slots) {
        return Err(error(
            "slots",
            format!("must be from 1 to {AGENTS_MAX}, not {slots}"),
        ));
    }
    // Validated once here; the table lives for the rest of the process.
    Ok(RoleSpec {
        label: String::leak(label.to_string()),
        harness,
        model: String::leak(model),
        commits_expected: commits,
        title: String::leak(title),
        model_name: String::leak(model_name),
        model_slug: String::leak(model_slug),
        slots,
        prompt_file: String::leak(prompt_file),
    })
}

// A built-in prompt's file name, or the absolute path of the user's own prompt file.
fn prompt_file(home: &Path, prompt: &str) -> Result<String, String> {
    let built_in = format!("{prompt}{PROMPT_SUFFIX}");
    if ROLES.iter().any(|spec| spec.prompt_file == built_in) {
        return Ok(built_in);
    }
    if !prompt.contains('/') {
        let names: Vec<&str> = ROLES
            .iter()
            .filter_map(|spec| spec.prompt_file.strip_suffix(PROMPT_SUFFIX))
            .collect();
        return Err(format!(
            "{prompt:?} is neither a built-in prompt ({}) nor a path",
            names.join(", ")
        ));
    }
    let path = home.join(prompt);
    let found = path.canonicalize().ok().filter(|found| found.is_file());
    let found = found.ok_or_else(|| format!("{} is not a file", path.display()))?;
    let found = found.to_str().map(str::to_string);
    let found = found.ok_or_else(|| format!("{} is not UTF-8", path.display()))?;
    assert!(Path::new(&found).is_absolute());
    Ok(found)
}

fn render(
    path: &Path,
    limits: &Limits,
    roles: &[(String, RoleShape)],
) -> Result<String, ConfigFileError> {
    let shape = LimitsShape {
        agents: Some(limits.agents),
        attempt_minutes: Some(limits.attempt_millis / 60_000),
    };
    let mut out = format!("[limits]\n{}", to_toml(path, &shape)?);
    for (label, role) in roles {
        out.push_str(&format!("\n[roles.{label}]\n{}", to_toml(path, role)?));
    }
    assert!(out.starts_with("[limits]"));
    Ok(out)
}

fn to_toml(path: &Path, value: &impl Serialize) -> Result<String, ConfigFileError> {
    toml::to_string(value).map_err(|error| ConfigFileError::Parse {
        path: path.to_path_buf(),
        message: error.to_string(),
    })
}
