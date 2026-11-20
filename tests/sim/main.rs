//! A deterministic simulator for the daemon's core: a seed picks the order of events and the
//! result of every action, and invariants are checked after every step.
//!
//! Replay one seed with `SIM_SEED=<n> cargo test --test sim`.

mod world;

use world::World;

/// The fixed seeds every run covers, each replayed.
const SEEDS: u64 = 64;
/// Further seeds every run covers once each.
const MORE_SEEDS: u64 = 2000;
/// How many of the last steps a failure prints.
const TRACE_SHOWN: usize = 40;

fn seeds() -> Vec<u64> {
    match std::env::var("SIM_SEED") {
        Ok(text) => match text.parse::<u64>() {
            Ok(seed) => vec![seed],
            Err(_) => panic!("SIM_SEED must be a number, got {text:?}"),
        },
        Err(_) => (0..SEEDS).collect(),
    }
}

#[test]
fn more_seeds_hold_every_invariant() {
    if std::env::var("SIM_SEED").is_ok() {
        return;
    }
    for seed in SEEDS..SEEDS + MORE_SEEDS {
        assert!(!run(seed).is_empty());
    }
}

fn run(seed: u64) -> Vec<String> {
    let mut world = World::new(seed);
    match world.run() {
        Ok(()) => world.trace,
        Err(failure) => {
            let start = world.trace.len().saturating_sub(TRACE_SHOWN);
            for line in &world.trace[start..] {
                eprintln!("{line}");
            }
            panic!(
                "seed {seed} failed: {failure}\nreplay with SIM_SEED={seed} cargo test --test sim"
            )
        }
    }
}

#[test]
fn seeds_hold_every_invariant() {
    for seed in seeds() {
        let first = run(seed);
        let second = run(seed);
        assert!(!first.is_empty());
        assert!(
            first == second,
            "seed {seed} failed: replay gave a different sequence of actions\n\
             replay with SIM_SEED={seed} cargo test --test sim"
        );
    }
}
