//! The table of agent roles: which harness and model runs each, and how many at once.

use std::sync::OnceLock;

/// The program that runs an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Harness {
    Claude,
    Codex,
}

/// One role an agent can take on a task.
#[derive(Debug)]
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
    },
    RoleSpec {
        label: "polish",
        harness: Harness::Claude,
        model: "claude-sonnet-5-5",
        commits_expected: true,
        needs_of: false,
        builds: true,
        title: "Polish",
        model_name: "Sonnet 5.5",
        model_slug: "sonnet-5-5",
        slots: AGENTS_MAX,
        pool_slots: 4,
        prompt_file: "polish.md",
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
        index += 1;
    }
};

static TABLE: OnceLock<&'static [RoleSpec]> = OnceLock::new();

/// Makes `table` the role table for the rest of the process; it can be installed only once.
pub fn install(table: Vec<RoleSpec>) -> Result<&'static [RoleSpec], InstallError> {
    assert!(!table.is_empty());
    assert!(
        table
            .iter()
            .all(|role| role.slots > 0 && role.slots <= AGENTS_MAX)
    );
    let leaked: &'static [RoleSpec] = Vec::leak(table);
    TABLE.set(leaked).map_err(|_| InstallError)?;
    Ok(leaked)
}

/// A role table was already installed in this process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstallError;

/// The installed role table, or the built-in one when none was installed.
pub fn table() -> &'static [RoleSpec] {
    let table = TABLE.get().copied().unwrap_or(&ROLES);
    assert!(!table.is_empty());
    table
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

/// The Board profile of one agent, `<label>-<slot>-<model_slug>`, such as
/// `implement-1-opus-5-5`. `slot` counts from 1.
pub fn profile_name(spec: &RoleSpec, slot: u8) -> String {
    assert!(
        slot >= 1 && slot <= spec.slots,
        "slot {slot} outside 1..={}",
        spec.slots
    );
    let profile = format!("{}-{slot}-{}", spec.label, spec.model_slug);
    assert!(
        profile.len() <= PROFILE_LEN_MAX,
        "profile {profile} is too long"
    );
    assert!(profile.starts_with(|c: char| c.is_ascii_alphabetic()));
    assert!(
        profile
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    );
    profile
}

/// The name of one agent for people, `<title> <slot> (<model_name>)`, such as
/// `Implement 1 (Opus 5.5)`. `slot` counts from 1.
pub fn display_name(spec: &RoleSpec, slot: u8) -> String {
    assert!(
        slot >= 1 && slot <= spec.slots,
        "slot {slot} outside 1..={}",
        spec.slots
    );
    assert!(spec.title.eq_ignore_ascii_case(spec.label));
    let name = format!("{} {slot} ({})", spec.title, spec.model_name);
    assert!(name.len() > spec.title.len() + spec.model_name.len());
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_table_matches_the_spec() {
        let r = role_from_label("review").unwrap();
        assert_eq!(r.model, "gpt-6.1-sol");
        assert!(matches!(r.harness, Harness::Codex));
        assert!(!r.commits_expected);
        assert_eq!(
            role_from_label("implement").unwrap().model,
            "claude-opus-5-5"
        );
        assert!(role_from_label("implement").unwrap().commits_expected);
        assert_eq!(
            role_from_label("polish").unwrap().model,
            "claude-sonnet-5-5"
        );
        assert_eq!(
            role_from_label("security").unwrap().model,
            "claude-mythos-5-2"
        );
        let light = role_from_label("light").unwrap();
        assert_eq!(light.model, "claude-sonnet-5-5");
        assert!(matches!(light.harness, Harness::Claude));
        assert!(light.commits_expected);
        assert_eq!(light.slots, AGENTS_MAX);
        assert_eq!(light.prompt_file, "implement.md");
        let qa = role_from_label("qa").unwrap();
        assert_eq!(qa.model, "claude-opus-5-5");
        assert!(matches!(qa.harness, Harness::Claude));
        assert!(!qa.commits_expected);
        assert_eq!(qa.slots, AGENTS_MAX);
        assert_eq!(qa.prompt_file, "qa.md");
        assert!(role_from_label("lead").is_none());
        assert!(role_from_label("").is_none());
    }

    #[test]
    fn names_each_agent_by_role_slot_and_model() {
        let implement = role_from_label("implement").unwrap();
        assert_eq!(profile_name(implement, 2), "implement-2-opus-5-5");
        let review = role_from_label("review").unwrap();
        assert_eq!(display_name(review, 1), "Review 1 (Sol 6.1)");
        let security = role_from_label("security").unwrap();
        assert_eq!(profile_name(security, 1), "security-1-mythos-5-2");
        assert_eq!(display_name(security, 1), "Security 1 (Mythos 5.2)");
        let light = role_from_label("light").unwrap();
        assert_eq!(profile_name(light, 1), "light-1-sonnet-5-5");
        assert_eq!(display_name(light, 2), "Light 2 (Sonnet 5.5)");
        let qa = role_from_label("qa").unwrap();
        assert_eq!(profile_name(qa, 1), "qa-1-opus-5-5");
        assert_eq!(display_name(qa, 1), "QA 1 (Opus 5.5)");
    }

    #[test]
    fn every_profile_is_one_the_board_accepts() {
        for role in ROLES.iter() {
            for slot in 1..=role.slots {
                let profile = profile_name(role, slot);
                assert!(profile.len() <= PROFILE_LEN_MAX);
                assert!(profile.starts_with(|c: char| c.is_ascii_alphabetic()));
                assert!(
                    profile
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-')
                );
            }
        }
    }
}
