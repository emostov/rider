//! `rider.toml` under the Rider home: the role table and limits, set over the built-in ones.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::cli::{PROMPTS, PROMPTS_DIR};
use crate::core::{ATTEMPT_MILLIS_MAX, ROLES_MAX};
use crate::roles::{
    AGENTS_MAX, BUILDERS_DEFAULT, Harness, PROFILE_LEN_MAX, ROLES, RepoRoles, RoleSpec,
};
use crate::run_task::{ATTEMPTS_DEFAULT, ATTEMPTS_MAX};

/// The file's name under the Rider home.
pub const FILE_NAME: &str = "rider.toml";
/// Upper bound on the file's size.
pub const FILE_BYTES_MAX: u64 = 64 * 1024;
/// Upper bound on, and default of, the time one attempt may run, in minutes.
pub const ATTEMPT_MINUTES_MAX: u64 = ATTEMPT_MILLIS_MAX / 60_000;
const _: () =
    assert!(ATTEMPT_MINUTES_MAX > 0 && ATTEMPT_MINUTES_MAX * 60_000 <= ATTEMPT_MILLIS_MAX);
const PROMPT_SUFFIX: &str = ".md";
const MODEL_NAME_LEN_MAX: usize = 32;
const MODEL_LEN_MAX: usize = 128;
const SLOT_DIGITS_MAX: usize = 2;
const POOL_SLOTS_NEW_ROLE: u8 = 2;
/// Upper bound on the repositories the file overrides roles for.
pub const REPOS_MAX: usize = 64;
/// Upper bound on the skills one role runs.
pub const SKILLS_MAX: usize = 8;
const SKILL_LEN_MAX: usize = 64;
const MISSING: &str = "is missing for a role that is not built in";
const _: () = assert!(AGENTS_MAX < 100);

/// The limits the daemon runs under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Agents running at once, across every role.
    pub agents: u8,
    /// Agents of building roles running at once.
    pub builders_at_once: u8,
    pub attempt_millis: u64,
    /// Failed attempts after which a task is blocked instead of run again.
    pub attempts: u8,
}

/// The effective role table and limits, and the TOML that states them.
#[derive(Debug)]
pub struct Settings {
    pub roles: Vec<RoleSpec>,
    pub repos: Vec<RepoSettings>,
    pub limits: Limits,
    pub toml: String,
}

/// One repository's overridden roles, and the TOML of the whole table as it sees it.
#[derive(Debug)]
pub struct RepoSettings {
    pub roles: RepoRoles,
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
    #[serde(default)]
    repo: BTreeMap<String, RepoShape>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepoShape {
    #[serde(default)]
    roles: BTreeMap<String, BTreeMap<String, toml::Value>>,
}

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LimitsShape {
    #[serde(skip_serializing_if = "Option::is_none")]
    agents: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    builders_at_once: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    attempt_minutes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    attempts: Option<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum HarnessName {
    Claude,
    Codex,
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
    needs_of: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    builds: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    slots: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    skills: Option<Vec<String>>,
}

impl RoleShape {
    fn built_in(spec: &RoleSpec) -> RoleShape {
        let harness = match spec.harness {
            Harness::Claude => HarnessName::Claude,
            Harness::Codex => HarnessName::Codex,
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
            needs_of: Some(spec.needs_of),
            builds: Some(spec.builds),
            slots: None,
            skills: Some(spec.skills.iter().map(|skill| skill.to_string()).collect()),
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
            needs_of,
            builds,
            slots,
            skills,
        } = over;
        self.harness = harness.or(self.harness);
        self.model = model.or(self.model.take());
        self.title = title.or(self.title.take());
        self.model_name = model_name.or(self.model_name.take());
        self.model_slug = model_slug.or(self.model_slug.take());
        self.prompt = prompt.or(self.prompt.take());
        self.commits = commits.or(self.commits);
        self.needs_of = needs_of.or(self.needs_of);
        self.builds = builds.or(self.builds);
        self.slots = slots.or(self.slots);
        self.skills = skills.or(self.skills.take());
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
        .map(|(label, role)| role_spec(&path, home, "", label, role, limits.agents))
        .collect::<Result<Vec<RoleSpec>, ConfigFileError>>()?;
    let toml = render(&path, &limits, &roles)?;
    let repos = repo_settings(&path, home, &limits, &roles, shape.repo)?;
    assert!(!specs.is_empty() && specs.len() <= ROLES_MAX);
    assert!(
        specs
            .iter()
            .all(|spec| spec.slots > 0 && spec.slots <= AGENTS_MAX)
    );
    Ok(Settings {
        roles: specs,
        repos,
        limits,
        toml,
    })
}

fn repo_settings(
    path: &Path,
    home: &Path,
    limits: &Limits,
    roles: &[(String, RoleShape)],
    repos: BTreeMap<String, RepoShape>,
) -> Result<Vec<RepoSettings>, ConfigFileError> {
    if repos.len() > REPOS_MAX {
        let problem = format!("names more than the {REPOS_MAX} repositories allowed");
        return Err(field_error(path, "repo".to_string(), problem));
    }
    let mut out: Vec<RepoSettings> = Vec::with_capacity(repos.len());
    for (key, repo) in repos {
        let prefix = format!("repo.{key:?}.");
        let written = PathBuf::from(&key);
        if !written.is_absolute() {
            let problem = "must be an absolute path".to_string();
            return Err(field_error(path, format!("repo.{key:?}"), problem));
        }
        let canonical = written.canonicalize().unwrap_or(written);
        if out.iter().any(|seen| seen.roles.repo == canonical) {
            let problem = "names a repository another key already names".to_string();
            return Err(field_error(path, format!("repo.{key:?}"), problem));
        }
        let mut merged = roles.to_vec();
        let mut specs = Vec::with_capacity(repo.roles.len());
        for (label, mut keys) in repo.roles {
            let field = format!("{prefix}roles.{label}");
            let Some((_, role)) = merged.iter_mut().find(|(known, _)| *known == label) else {
                return Err(field_error(path, field, "is not a role".to_string()));
            };
            let skills = keys.remove("skills");
            if let Some(key) = keys.into_keys().next() {
                let problem =
                    format!("cannot be set in [{field}]: only `skills` may be set per repository");
                return Err(field_error(path, format!("{field}.{key}"), problem));
            }
            let Some(skills) = skills else {
                continue;
            };
            let skills = skills.try_into::<Vec<String>>().map_err(|_| {
                let problem = "must be a list of skill names".to_string();
                field_error(path, format!("{field}.skills"), problem)
            })?;
            role.skills = Some(skills);
            specs.push(role_spec(path, home, &prefix, &label, role, limits.agents)?);
        }
        let toml = render(path, limits, &merged)?;
        let roles = RepoRoles {
            repo: canonical,
            roles: specs,
        };
        out.push(RepoSettings { roles, toml });
    }
    assert!(out.len() <= REPOS_MAX);
    Ok(out)
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
    let builders_at_once = shape.builders_at_once.unwrap_or(BUILDERS_DEFAULT);
    if !(1..=AGENTS_MAX).contains(&builders_at_once) {
        let problem = format!("must be from 1 to {AGENTS_MAX}, not {builders_at_once}");
        let field = "limits.builders_at_once".to_string();
        return Err(field_error(path, field, problem));
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
    let attempts = shape.attempts.unwrap_or(ATTEMPTS_DEFAULT);
    if !(1..=ATTEMPTS_MAX).contains(&attempts) {
        let problem = format!("must be from 1 to {ATTEMPTS_MAX}, not {attempts}");
        return Err(field_error(path, "limits.attempts".to_string(), problem));
    }
    let limits = Limits {
        agents,
        builders_at_once,
        attempt_millis: minutes * 60_000,
        attempts,
    };
    assert!(limits.attempt_millis > 0 && limits.attempt_millis <= ATTEMPT_MILLIS_MAX);
    assert!(limits.builders_at_once > 0 && limits.builders_at_once <= AGENTS_MAX);
    Ok(limits)
}

fn is_name(text: &str) -> bool {
    text.starts_with(|c: char| c.is_ascii_lowercase())
        && text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn text_fits(text: &str, len_max: usize) -> bool {
    !text.trim().is_empty() && text.len() <= len_max && !text.contains(char::is_control)
}

// The runtime role from a merged shape; every field must be set and valid.
fn role_spec(
    path: &Path,
    home: &Path,
    prefix: &str,
    label: &str,
    role: &RoleShape,
    agents: u8,
) -> Result<RoleSpec, ConfigFileError> {
    let error = |name: &str, problem: String| {
        field_error(path, format!("{prefix}roles.{label}.{name}"), problem)
    };
    let missing = |name: &str| error(name, MISSING.to_string());
    if !is_name(label) {
        let problem = "must be lowercase letters, digits and '-', starting with a letter";
        return Err(field_error(
            path,
            format!("{prefix}roles.{label}"),
            problem.into(),
        ));
    }
    let harness = match role.harness.ok_or_else(|| missing("harness"))? {
        HarnessName::Claude => Harness::Claude,
        HarnessName::Codex => Harness::Codex,
    };
    let model = role.model.clone().ok_or_else(|| missing("model"))?;
    let spaced = model.contains(char::is_whitespace);
    if spaced || !text_fits(&model, MODEL_LEN_MAX) || model.starts_with('-') {
        return Err(error("model", format!("{model:?} is not a model name")));
    }
    let title = role.title.clone().ok_or_else(|| missing("title"))?;
    if !title.eq_ignore_ascii_case(label) {
        return Err(error("title", format!("must be {label:?} in any case")));
    }
    let model_name = role.model_name.clone();
    let model_name = model_name.ok_or_else(|| missing("model_name"))?;
    if !text_fits(&model_name, MODEL_NAME_LEN_MAX) {
        let problem = format!("must be 1 to {MODEL_NAME_LEN_MAX} bytes, no control characters");
        return Err(error("model_name", problem));
    }
    let model_slug = role.model_slug.clone();
    let model_slug = model_slug.ok_or_else(|| missing("model_slug"))?;
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
    let needs_of = role.needs_of.unwrap_or(!commits);
    if !commits && !needs_of {
        let problem = "must be true for a role that does not commit".to_string();
        return Err(error("needs_of", problem));
    }
    let (slots, pool_slots) =
        slot_counts(label, role.slots, agents).map_err(|problem| error("slots", problem))?;
    let skills = role.skills.as_deref().unwrap_or_default();
    let skills = skills_for(harness, skills).map_err(|problem| error("skills", problem))?;
    // Validated once here; the table lives for the rest of the process.
    Ok(RoleSpec {
        label: String::leak(label.to_string()),
        harness,
        model: String::leak(model),
        commits_expected: commits,
        needs_of,
        builds: role.builds.unwrap_or(true),
        title: String::leak(title),
        model_name: String::leak(model_name),
        model_slug: String::leak(model_slug),
        slots,
        pool_slots,
        prompt_file: String::leak(prompt_file),
        skills,
    })
}

fn skills_for(harness: Harness, skills: &[String]) -> Result<&'static [&'static str], String> {
    if skills.len() > SKILLS_MAX {
        return Err(format!("lists more than the {SKILLS_MAX} skills allowed"));
    }
    let mut out: Vec<&'static str> = Vec::with_capacity(skills.len());
    for skill in skills {
        let named = skill.starts_with(|c: char| c.is_ascii_alphanumeric())
            && skill.len() <= SKILL_LEN_MAX
            && skill
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_:".contains(&b));
        if !named {
            return Err(format!(
                "{skill:?} is not a skill name: letters, digits, '-', '_' and ':', starting \
                 with a letter or digit, at most {SKILL_LEN_MAX} bytes"
            ));
        }
        if !harness.runs_skills() {
            let harness = match harness {
                Harness::Claude => "claude",
                Harness::Codex => "codex",
            };
            return Err(format!(
                "names the skill {skill:?}, which the {harness} harness cannot run"
            ));
        }
        out.push(String::leak(skill.clone()));
    }
    assert!(out.len() == skills.len() && out.len() <= SKILLS_MAX);
    Ok(Vec::leak(out))
}

fn slot_counts(label: &str, set: Option<u8>, agents: u8) -> Result<(u8, u8), String> {
    assert!(agents > 0 && agents <= AGENTS_MAX);
    let slots = set.unwrap_or(agents);
    if !(1..=AGENTS_MAX).contains(&slots) {
        return Err(format!("must be from 1 to {AGENTS_MAX}, not {slots}"));
    }
    let built_in = ROLES.iter().find(|spec| spec.label == label);
    let pool_slots = match (set, built_in) {
        (Some(set), _) => set,
        (None, Some(spec)) => spec.pool_slots.min(slots),
        (None, None) => POOL_SLOTS_NEW_ROLE.min(slots),
    };
    assert!(pool_slots > 0 && pool_slots <= slots);
    Ok((slots, pool_slots))
}

// A built-in prompt's file name, or the absolute path of the user's own prompt file, which
// must not be one of the built-in files Rider rewrites under the prompts directory.
fn prompt_file(home: &Path, prompt: &str) -> Result<String, String> {
    let built_in = format!("{prompt}{PROMPT_SUFFIX}");
    if ROLES.iter().any(|spec| spec.prompt_file == built_in) {
        return Ok(built_in);
    }
    let path = home.join(prompt);
    let Some(found) = path.canonicalize().ok().filter(|found| found.is_file()) else {
        let names: Vec<&str> = ROLES
            .iter()
            .filter_map(|spec| spec.prompt_file.strip_suffix(PROMPT_SUFFIX))
            .collect();
        return Err(format!(
            "{} is not a file, and {prompt:?} is not a built-in prompt ({})",
            path.display(),
            names.join(", ")
        ));
    };
    let prompts = home.join(PROMPTS_DIR);
    let prompts = prompts.canonicalize().unwrap_or(prompts);
    if let Some((name, _)) = PROMPTS.iter().find(|(name, _)| found == prompts.join(name)) {
        return Err(format!(
            "{} is the built-in prompt {name} that Rider rewrites; copy it to another path",
            path.display()
        ));
    }
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
        builders_at_once: Some(limits.builders_at_once),
        attempt_minutes: Some(limits.attempt_millis / 60_000),
        attempts: Some(limits.attempts),
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
