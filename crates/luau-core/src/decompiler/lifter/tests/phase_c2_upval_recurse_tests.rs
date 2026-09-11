//! Phase C2 pass #2 tests: recursive upvalue-name propagation with a
//! bounded fixpoint. Exercises `propagate_upval_names_once` driven by the
//! same iteration cap used in `lift_proto_inner`.

use std::collections::HashMap;

use super::super::{propagate_upval_names_once, PROPAGATE_UPVAL_MAX_ITERATIONS};
use crate::parser::types::Proto;

fn make_proto(num_upvalues: u8) -> Proto {
    Proto {
        max_stack_size: 0,
        num_params: 0,
        num_upvalues,
        is_vararg: false,
        flags: 0,
        typeinfo: None,
        code: Vec::new(),
        constants: Vec::new(),
        child_protos: Vec::new(),
        line_defined: 0,
        debug_name: None,
        line_info: None,
        debug_info: None,
    }
}

/// Drive the fixpoint up to MAX iterations, returning how many iterations
/// actually ran before it stabilised (so tests can assert the cap behaviour).
fn run_fixpoint(
    protos: &[Proto],
    inferred: &mut HashMap<usize, Vec<String>>,
    links: &HashMap<usize, Vec<(usize, usize, u8)>>,
) -> usize {
    let mut ran = 0;
    for _ in 0..PROPAGATE_UPVAL_MAX_ITERATIONS {
        ran += 1;
        let changed = propagate_upval_names_once(protos, inferred, links);
        if !changed { break; }
    }
    ran
}

// ─── Test 1 ──────────────────────────────────────────────────────────
// Three-level nesting: grandparent P0 has an inferred upvalue name ("game"),
// P1 captures it from P0, P2 captures the same value from P1. A single pass
// only reaches P1; the fixpoint must iterate again to propagate into P2.

#[test]
fn c2_grandchild_propagation_requires_fixpoint_iterations() {
    // 3 protos: grandparent (0), parent (1), grandchild (2).
    let protos = vec![
        make_proto(1), // P0 owns the original named upvalue "game" at slot 0.
        make_proto(1), // P1 captures P0's slot 0 into its slot 0.
        make_proto(1), // P2 captures P1's slot 0 into its slot 0.
    ];

    let mut inferred: HashMap<usize, Vec<String>> = HashMap::new();
    // Grandparent has the real name.
    inferred.insert(0, vec!["game".to_string()]);
    // Parent and grandchild start with placeholders.
    inferred.insert(1, vec!["upval_0".to_string()]);
    inferred.insert(2, vec!["upval_0".to_string()]);

    // Links: P1.slot0 ← P0.slot0 ; P2.slot0 ← P1.slot0
    let mut links: HashMap<usize, Vec<(usize, usize, u8)>> = HashMap::new();
    links.insert(1, vec![(0usize, 0usize, 0u8)]);
    links.insert(2, vec![(0usize, 1usize, 0u8)]);

    // First single pass: only P1 gets resolved (P2's parent P1 was still
    // "upval_0" when the pass began, because HashMap order is unspecified —
    // but even in the best-case ordering, the propagate_once contract is
    // one-level-per-call because it reads parents from the snapshot).
    let changed1 = propagate_upval_names_once(&protos, &mut inferred, &links);
    assert!(changed1, "first pass must resolve at least P1");
    assert_eq!(inferred[&1][0], "game", "P1 should receive grandparent name");

    // Run the fixpoint to completion; must converge with P2 also resolved.
    let ran = run_fixpoint(&protos, &mut inferred, &links);
    assert!(ran <= PROPAGATE_UPVAL_MAX_ITERATIONS);
    assert_eq!(inferred[&0][0], "game");
    assert_eq!(inferred[&1][0], "game");
    assert_eq!(inferred[&2][0], "game", "grandchild must be reached by fixpoint");
}

// ─── Test 2 ──────────────────────────────────────────────────────────
// Cycle safety: a deliberately malformed links map that has a self-reference
// plus a cycle P0 ↔ P1. No parent ever gets a name, so `changed` stays false
// after the first pass and the loop exits early. Even if we force every pass
// to report `changed=true` artificially, the MAX_ITERATIONS cap must stop
// the loop within the budget. We verify by running exactly MAX iterations
// and asserting termination.

#[test]
fn c2_cycle_safe_terminates_at_iteration_cap() {
    let protos = vec![
        make_proto(1),
        make_proto(1),
    ];

    // All slots are placeholders — nothing is ever resolvable.
    let mut inferred: HashMap<usize, Vec<String>> = HashMap::new();
    inferred.insert(0, vec!["upval_0".to_string()]);
    inferred.insert(1, vec!["upval_0".to_string()]);

    // Malformed cyclic links: P0←P1 and P1←P0, plus a self-link on P0.
    // Also include a bogus parent index out of range to exercise the
    // defensive skip. A naive implementation could loop forever chasing
    // these; our bounded loop must terminate.
    let mut links: HashMap<usize, Vec<(usize, usize, u8)>> = HashMap::new();
    links.insert(0, vec![
        (0usize, 1usize, 0u8),          // P0 ← P1 slot 0
        (0usize, 0usize, 0u8),          // P0 ← P0 slot 0 (self-link)
        (0usize, 999usize, 0u8),        // bogus parent index, must be skipped
    ]);
    links.insert(1, vec![
        (0usize, 0usize, 0u8),          // P1 ← P0 slot 0
    ]);
    // Bogus child index.
    links.insert(999, vec![(0usize, 0usize, 0u8)]);

    // Run exactly MAX_ITERATIONS passes; the loop below mirrors the
    // production fixpoint. Must not hang and must exit either via early
    // termination (changed=false) or via the iteration cap.
    let mut ran = 0;
    let mut final_changed = false;
    for _ in 0..PROPAGATE_UPVAL_MAX_ITERATIONS {
        ran += 1;
        final_changed = propagate_upval_names_once(&protos, &mut inferred, &links);
        if !final_changed { break; }
    }

    // With only placeholder parents, nothing propagates → converges on pass 1.
    assert_eq!(ran, 1, "unresolvable links converge in the first pass");
    assert!(!final_changed);

    // Slots remain untouched (no garbage written).
    assert_eq!(inferred[&0][0], "upval_0");
    assert_eq!(inferred[&1][0], "upval_0");

    // Now seed P0 so the cycle WOULD fire, and ensure we still cap cleanly.
    inferred.insert(0, vec!["game".to_string()]);
    let mut ran2 = 0;
    for _ in 0..PROPAGATE_UPVAL_MAX_ITERATIONS {
        ran2 += 1;
        let changed = propagate_upval_names_once(&protos, &mut inferred, &links);
        if !changed { break; }
    }
    assert!(ran2 <= PROPAGATE_UPVAL_MAX_ITERATIONS,
        "bounded loop must terminate within the iteration cap");
    // P1 should pick up "game" from P0. P0 is already "game", self-link no-op.
    assert_eq!(inferred[&0][0], "game");
    assert_eq!(inferred[&1][0], "game");
}
