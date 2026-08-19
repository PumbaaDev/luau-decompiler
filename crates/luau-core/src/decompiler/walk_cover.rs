//! Which instructions did the walk actually EXECUTE, per proto?
//!
//! Rotations 87 and 88 were both the same fault - a span whose walk stops at a
//! jump that leaves it while the instructions after that jump still belong to
//! the program - and both were found only after printing what a PRODUCER emits
//! instead of what a consumer receives. Every consumer-side instrument had
//! looked innocent, because every consumer was.
//!
//! This is the producer-side instrument for the remaining `calls_not_emitted`
//! class: it records each pc `lift_instruction_range` actually steps over, and
//! reports coverage against the proto's instruction count. A proto whose body
//! went missing because the walk quit early shows up as a coverage shortfall;
//! one that is genuinely short does not.
//!
//! Gated on `LUAU_WALK_COVER`; a no-op when unset.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

thread_local! {
    static SEEN: RefCell<BTreeMap<String, BTreeSet<usize>>> = RefCell::new(BTreeMap::new());
}

pub fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("LUAU_WALK_COVER").is_ok())
}

/// Proto-qualified on purpose. Rotation 86 spent four rotations on the wrong
/// one of two near-identical protos for want of exactly this field.
pub fn note(proto: &str, line_defined: u32, pc: usize) {
    if !enabled() {
        return;
    }
    let key = format!("{}@{}", proto, line_defined);
    SEEN.with(|s| {
        s.borrow_mut().entry(key).or_default().insert(pc);
    });
}

/// Report coverage for one proto and forget it.
pub fn report(proto: &str, line_defined: u32, code: &[u32]) {
    if !enabled() {
        return;
    }
    let key = format!("{}@{}", proto, line_defined);
    let n = SEEN.with(|s| s.borrow().get(&key).map(|v| v.len()).unwrap_or(0));
    // Normalised against the number of STEPS a complete linear walk would take,
    // not code.len(): an instruction carrying an AUX word occupies two slots but
    // is one step, so raw slot count makes every AUX-heavy proto look
    // half-walked. The first cut of this instrument did exactly that and
    // flagged ten healthy protos in 277_DragManager as shortfalls.
    let mut steps = 0usize;
    let mut pc = 0usize;
    while pc < code.len() {
        let op = crate::parser::opcodes::LuauOpcode::from_u8((code[pc] & 0xFF) as u8);
        steps += 1;
        pc += if op.has_aux() { 2 } else { 1 };
    }
    let pct = if steps == 0 { 100 } else { n * 100 / steps };
    eprintln!("COVER	{}	walked={}	steps={}	pct={}", key, n, steps, pct);

    // WHICH CALLS WENT MISSING.
    //
    // Rotation 90 measured coverage and adopted nothing, because walking more
    // instructions is not the same as emitting the missing calls. This asks the
    // question `calls_not_emitted` actually poses: of the CALL sites the
    // bytecode contains, which ones did the walk never reach? A missing call
    // whose pc WAS walked is a different defect from one whose pc was not, and
    // the two need different fixes.
    let walked = SEEN.with(|s| s.borrow().get(&key).cloned().unwrap_or_default());
    let mut missed = Vec::new();
    let mut total = 0usize;
    let mut pc = 0usize;
    while pc < code.len() {
        let op = crate::parser::opcodes::LuauOpcode::from_u8((code[pc] & 0xFF) as u8);
        if matches!(op, crate::parser::opcodes::LuauOpcode::Call) {
            total += 1;
            if !walked.contains(&pc) {
                missed.push(pc);
            }
        }
        pc += if op.has_aux() { 2 } else { 1 };
    }
    if !missed.is_empty() {
        eprintln!(
            "CALLGAP	{}	missed={}/{}	pcs={:?}",
            key,
            missed.len(),
            total,
            &missed[..missed.len().min(12)]
        );
    }
    SEEN.with(|s| {
        s.borrow_mut().remove(&key);
    });
}
