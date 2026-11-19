//! The fixed table of agent roles: which harness and model runs each, and how many at once.

/// The program that runs an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Harness {
    Claude,
    Codex,
    Tact,
}

/// One role an agent can take on a task.
#[derive(Debug)]
pub struct RoleSpec {
    pub label: &'static str,
    pub harness: Harness,
    pub model: &'static str,
    pub commits_expected: bool,
    /// The model's name for people, such as "Opus 5.5".
    pub model_name: &'static str,
    /// The model's name inside a Board profile, such as "opus-5-5".
    pub model_slug: &'static str,
    pub slots: u8,
    /// A file name under `prompts/`.
    pub prompt_file: &'static str,
}

/// Upper bound on agents running at once, across every role.
pub const AGENTS_MAX: u8 = 4;
/// The longest profile name the board accepts.
pub const PROFILE_LEN_MAX: usize = 32;

pub const ROLES: [RoleSpec; 5] = [
    RoleSpec {
        label: "implement",
        harness: Harness::Claude,
        model: "claude-opus-5-5",
        commits_expected: true,
        model_name: "Opus 5.5",
        model_slug: "opus-5-5",
        slots: 2,
        prompt_file: "implement.md",
    },
    RoleSpec {
        label: "light",
        harness: Harness::Claude,
        model: "claude-sonnet-5-5",
        commits_expected: true,
        model_name: "Sonnet 5.5",
        model_slug: "sonnet-5-5",
        slots: 2,
        prompt_file: "implement.md",
    },
    RoleSpec {
        label: "review",
        harness: Harness::Codex,
        model: "gpt-6.1-sol",
        commits_expected: false,
        model_name: "Sol 6.1",
        model_slug: "sol-6-1",
        slots: 2,
        prompt_file: "review.md",
    },
    RoleSpec {
        label: "polish",
        harness: Harness::Claude,
        model: "claude-sonnet-5-5",
        commits_expected: true,
        model_name: "Sonnet 5.5",
        model_slug: "sonnet-5-5",
        slots: 1,
        prompt_file: "polish.md",
    },
    RoleSpec {
        label: "security",
        harness: Harness::Claude,
        model: "claude-mythos-5-2",
        commits_expected: false,
        model_name: "Mythos 5.2",
        model_slug: "mythos-5-2",
        slots: 1,
        prompt_file: "security.md",
    },
];

// The table is checked when the crate compiles: every role has a slot, and the slots
// together can fill every agent.
const _: () = {
    let mut slots_total: u8 = 0;
    let mut index = 0;
    while index < ROLES.len() {
        assert!(ROLES[index].slots > 0);
        assert!(ROLES[index].slots <= AGENTS_MAX);
        slots_total += ROLES[index].slots;
        index += 1;
    }
    assert!(slots_total >= AGENTS_MAX);
};

/// The role with this label, or `None` when no role has it.
pub fn role_from_label(label: &str) -> Option<&'static RoleSpec> {
    // The label comes from board data, so an empty or unknown one is simply no role.
    assert!(ROLES.iter().all(|role| !role.label.is_empty()));
    let role = ROLES.iter().find(|role| role.label == label);
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

/// The name of one agent for people, `<Label> <slot> (<model_name>)`, such as
/// `Implement 1 (Opus 5.5)`. `slot` counts from 1.
pub fn display_name(spec: &RoleSpec, slot: u8) -> String {
    assert!(
        slot >= 1 && slot <= spec.slots,
        "slot {slot} outside 1..={}",
        spec.slots
    );
    assert!(spec.label.is_ascii() && !spec.label.is_empty());
    let (first, rest) = spec.label.split_at(1);
    let name = format!(
        "{}{rest} {slot} ({})",
        first.to_ascii_uppercase(),
        spec.model_name
    );
    assert!(name.len() > spec.label.len() + spec.model_name.len());
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
        assert_eq!(light.slots, 2);
        assert_eq!(light.prompt_file, "implement.md");
        assert!(role_from_label("lead").is_none());
        assert!(role_from_label("").is_none());
        assert!(ROLES.iter().map(|r| r.slots).sum::<u8>() >= AGENTS_MAX);
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
