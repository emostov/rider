//! The table of agent roles: which harness and model runs each, and how many at once.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The program that runs an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Harness {
    Claude,
    Codex,
    CodexReview,
}

/// One role an agent can take on a task.
#[derive(Debug, Clone)]
pub struct RoleSpec {
    pub label: &'static str,
    pub harness: Harness,
    pub model: &'static str,
    pub commits_expected: bool,
    /// A task of this role must name, in its `of` label, the task whose work it checks.
    pub needs_of: bool,
    /// An agent of this role builds and tests, so it counts against the builders limit.
    pub builds: bool,
    /// The role's name for people, such as "Implement" or "QA".
    pub title: &'static str,
    /// The model's name for people, such as "Opus 5.5".
    pub model_name: &'static str,
    /// The model's name inside a Board profile, such as "opus-5-5".
    pub model_slug: &'static str,
    pub slots: u8,
    pub pool_slots: u8,
    /// A file name under `prompts/`, or an absolute path to the user's own prompt.
    pub prompt_file: &'static str,
    pub skills: &'static [&'static str],
}

/// The roles one repository overrides, each merged over the role of the same label.
#[derive(Debug)]
pub struct RepoRoles {
    pub repo: PathBuf,
    pub roles: Vec<RoleSpec>,
}

/// Upper bound on agents running at once, across every role, and its default.
pub const AGENTS_MAX: u8 = 32;
/// Agents of building roles running at once, unless the configuration sets it.
pub const BUILDERS_DEFAULT: u8 = 4;
const _: () = assert!(BUILDERS_DEFAULT > 0 && BUILDERS_DEFAULT <= AGENTS_MAX);
/// The longest profile name the board accepts.
pub const PROFILE_LEN_MAX: usize = 32;

pub const ROLES: [RoleSpec; 6] = [
    RoleSpec {
        label: "implement",
        harness: Harness::Claude,
        model: "claude-opus-5-5",
        commits_expected: true,
        needs_of: false,
        builds: true,
        title: "Implement",
        model_name: "Opus 5.5",
        model_slug: "opus-5-5",
        slots: AGENTS_MAX,
        pool_slots: 12,
        prompt_file: "implement.md",
        skills: &[],
    },
    RoleSpec {
        label: "light",
        harness: Harness::Claude,
        model: "claude-sonnet-5-5",
        commits_expected: true,
        needs_of: false,
        builds: true,
        title: "Light",
        model_name: "Sonnet 5.5",
        model_slug: "sonnet-5-5",
        slots: AGENTS_MAX,
        pool_slots: 12,
        prompt_file: "implement.md",
        skills: &[],
    },
    RoleSpec {
        label: "review",
        harness: Harness::Codex,
        model: "gpt-6.1-sol",
        commits_expected: false,
        needs_of: true,
        builds: false,
        title: "Review",
        model_name: "Sol 6.1",
        model_slug: "sol-6-1",
        slots: AGENTS_MAX,
        pool_slots: 8,
        prompt_file: "review.md",
        skills: &[],
    },
    RoleSpec {
        label: "native-review",
        harness: Harness::CodexReview,
        model: "gpt-6.2-sol",
        commits_expected: false,
        needs_of: true,
        builds: false,
        title: "Native-review",
        model_name: "Sol 6.2",
        model_slug: "sol-6-2",
        slots: AGENTS_MAX,
        pool_slots: 8,
        prompt_file: "native-review.md",
        skills: &[],
    },
    RoleSpec {
        label: "security",
        harness: Harness::Claude,
        model: "claude-mythos-5-2",
        commits_expected: false,
        needs_of: true,
        builds: false,
        title: "Security",
        model_name: "Mythos 5.2",
        model_slug: "mythos-5-2",
        slots: AGENTS_MAX,
        pool_slots: 2,
        prompt_file: "security.md",
        skills: &[],
    },
    RoleSpec {
        label: "qa",
        harness: Harness::Claude,
        model: "claude-opus-5-5",
        commits_expected: false,
        needs_of: true,
        builds: true,
        title: "QA",
        model_name: "Opus 5.5",
        model_slug: "opus-5-5",
        slots: AGENTS_MAX,
        pool_slots: 2,
        prompt_file: "qa.md",
        skills: &[],
    },
];

// The built-in table is checked when the crate compiles: every role has a slot, and a role
// without commits works in the branch of the task its `of` names.
const _: () = {
    let mut index = 0;
    while index < ROLES.len() {
        assert!(ROLES[index].slots > 0);
        assert!(ROLES[index].slots <= AGENTS_MAX);
        assert!(ROLES[index].pool_slots > 0);
        assert!(ROLES[index].pool_slots <= ROLES[index].slots);
        assert!(ROLES[index].commits_expected || ROLES[index].needs_of);
        assert!(ROLES[index].skills.is_empty());
        index += 1;
    }
};

struct Installed {
    roles: &'static [RoleSpec],
    repos: &'static [RepoRoles],
}

static TABLE: OnceLock<Installed> = OnceLock::new();

/// Makes `table` the role table, and `repos` the repositories' overrides of it, for the rest
/// of the process; they can be installed only once.
pub fn install(
    table: Vec<RoleSpec>,
    repos: Vec<RepoRoles>,
) -> Result<&'static [RoleSpec], InstallError> {
    assert!(!table.is_empty());
    assert!(
        table
            .iter()
            .all(|role| role.slots > 0 && role.slots <= AGENTS_MAX)
    );
    assert!(repos.iter().all(|repo| {
        repo.repo.is_absolute()
            && repo
                .roles
                .iter()
                .all(|role| table.iter().any(|base| base.label == role.label))
    }));
    let roles: &'static [RoleSpec] = Vec::leak(table);
    let repos: &'static [RepoRoles] = Vec::leak(repos);
    TABLE
        .set(Installed { roles, repos })
        .map_err(|_| InstallError)?;
    Ok(roles)
}

/// A role table was already installed in this process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstallError;

/// The installed role table, or the built-in one when none was installed.
pub fn table() -> &'static [RoleSpec] {
    let table = TABLE.get().map_or(&ROLES[..], |installed| installed.roles);
    assert!(!table.is_empty());
    table
}

/// `role` as the repository at `repo` overrides it, or `role` itself when it does not.
pub fn role_in_repo(role: &'static RoleSpec, repo: &Path) -> &'static RoleSpec {
    assert!(repo.is_absolute());
    let repos = TABLE.get().map_or(&[][..], |installed| installed.repos);
    let canonical = repo.canonicalize().ok();
    let found = repos
        .iter()
        .filter(|entry| entry.repo == repo || canonical.as_deref() == Some(&entry.repo))
        .flat_map(|entry| entry.roles.iter())
        .find(|over| over.label == role.label);
    let found = found.unwrap_or(role);
    assert_eq!(found.label, role.label);
    found
}

/// The role with this label, or `None` when no role has it.
pub fn role_from_label(label: &str) -> Option<&'static RoleSpec> {
    // The label comes from board data, so an empty or unknown one is simply no role.
    let table = table();
    assert!(table.iter().all(|role| !role.label.is_empty()));
    let role = table.iter().find(|role| role.label == label);
    if let Some(found) = role {
        assert_eq!(found.label, label);
        assert!(found.slots > 0);
    }
    role
}
