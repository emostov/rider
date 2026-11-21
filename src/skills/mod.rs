mod install;

use std::fmt;
use std::path::PathBuf;

pub use install::{InstallDir, Outcome, Report, install, resolve, update_recorded};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const SKILLS: [&str; 3] = ["review-queue", "ride-swarm", "ship-swarm"];
pub const FILES: [(&str, &str); 8] = [
    (
        "review-queue/SKILL.md",
        include_str!("../../skills/review-queue/SKILL.md"),
    ),
    (
        "ride-swarm/SKILL.md",
        include_str!("../../skills/ride-swarm/SKILL.md"),
    ),
    (
        "ride-swarm/lead.md",
        include_str!("../../skills/ride-swarm/lead.md"),
    ),
    (
        "ride-swarm/schemas/rider-draft@1.json",
        include_str!("../../skills/ride-swarm/schemas/rider-draft@1.json"),
    ),
    (
        "ride-swarm/schemas/rider-finding.json",
        include_str!("../../skills/ride-swarm/schemas/rider-finding.json"),
    ),
    (
        "ride-swarm/schemas/rider-result@2.json",
        include_str!("../../skills/ride-swarm/schemas/rider-result@2.json"),
    ),
    (
        "ship-swarm/SKILL.md",
        include_str!("../../skills/ship-swarm/SKILL.md"),
    ),
    (
        "ship-swarm/settings.md",
        include_str!("../../skills/ship-swarm/settings.md"),
    ),
];
const SKILL_FILE: &str = "SKILL.md";

#[derive(Debug, Clone)]
pub enum SkillsError {
    Io {
        action: &'static str,
        path: PathBuf,
        message: String,
    },
    Manifest {
        path: PathBuf,
        why: String,
    },
    TooLarge(PathBuf),
    NotRegular(PathBuf),
    TooManyFiles(PathBuf),
    TooManyInstalls(PathBuf),
    BadRecord(PathBuf),
    BadInto(PathBuf),
    UnknownSkill(String),
}

impl fmt::Display for SkillsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SkillsError::Io {
                action,
                path,
                message,
            } => write!(f, "could not {action} {}: {message}", path.display()),
            SkillsError::Manifest { path, why } => {
                write!(
                    f,
                    "{} is not a rider install manifest: {why}",
                    path.display()
                )
            }
            SkillsError::TooLarge(path) => write!(f, "{} is too large", path.display()),
            SkillsError::NotRegular(path) => {
                write!(f, "{} is not a regular file", path.display())
            }
            SkillsError::TooManyFiles(path) => {
                write!(f, "{} holds too many files", path.display())
            }
            SkillsError::TooManyInstalls(path) => {
                write!(f, "{} records too many install directories", path.display())
            }
            SkillsError::BadRecord(path) => {
                write!(
                    f,
                    "{} holds a line that is not an absolute path",
                    path.display()
                )
            }
            SkillsError::BadInto(path) => {
                let path = path.display();
                write!(
                    f,
                    "--into {path} must be UTF-8 with no .. and no line break"
                )
            }
            SkillsError::UnknownSkill(name) => {
                write!(
                    f,
                    "no skill is named {name:?}; rider skills list names them"
                )
            }
        }
    }
}

pub fn skill_files(skill: &str) -> impl Iterator<Item = (&'static str, &'static str)> + '_ {
    FILES.iter().filter_map(move |(path, text)| {
        let relative = path.strip_prefix(skill)?.strip_prefix('/')?;
        Some((relative, *text))
    })
}

pub fn show(name: &str) -> Result<&'static str, SkillsError> {
    let text = skill_files(name)
        .find(|(path, _)| *path == SKILL_FILE)
        .map(|(_, text)| text)
        .ok_or_else(|| SkillsError::UnknownSkill(name.to_string()))?;
    assert!(SKILLS.contains(&name) && text.starts_with("---\n"));
    Ok(text)
}

pub fn description(name: &str) -> Result<&'static str, SkillsError> {
    let text = show(name)?;
    let description = text
        .lines()
        .skip(1)
        .take_while(|line| *line != "---")
        .find_map(|line| line.strip_prefix("description:"))
        .map(str::trim)
        .unwrap_or_default();
    assert!(description.len() < text.len());
    Ok(description)
}
