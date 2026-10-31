//! A guarded-tail OR chain nested inside a non-loop structured run.
//!
//! `structure_control_flow` claims `Region::GuardedTailIf` only at top level.
//! Inside an `if` arm the arm is lifted by `lift_structured_run`, whose run
//! structurer (`structure_numeric_for_body`) leaves non-loop control flow to the
//! flat lifter. For Shrine `stopCutscene` (31 Oct 2026) the arm holds
//!
//! ```text
//!   if p == true or cam ~= v.Camera or cam.CameraType ~= Scriptable
//!       or not v97 or (v97 - cam.CFrame.Position).Magnitude > 140 then
//!       <release camera> ... return
//!   end
//!   u8 = { ... }            -- the out-of-line tail, laid out after the release
//!   return
//! ```
//!
//! and the flat lifter kept only the last link: the first four tests vanished
//! and the release ran whenever the magnitude test passed.
//!
//! This finds such a chain inside the run and splits the run around it:
//! `[start, entry)`, the chain (lifted by `lift_guarded_tail_if`),
//! `[merge, tail)`, `[tail_end, end)`. It is refused unless every piece keeps
//! its jumps inside itself, nothing outside the chain jumps into it except into
//! its entry, and the run holds no loop, so no block is lifted twice and no
//! edge is cut.

use std::collections::BTreeSet;

use crate::analysis::cfg::ControlFlowGraph;
use crate::analysis::structuring::try_match_guarded_tail_if;

pub(super) struct GuardedTailSplit {
    pub guards: Vec<(usize, bool)>,
    pub tail: Vec<usize>,
    pub merge: usize,
    pub entry_start: usize,
    pub merge_start: usize,
    pub tail_start: usize,
    pub tail_end: usize,
}

pub(super) fn guarded_tail_split(
    code: &[u32],
    cfg: &ControlFlowGraph,
    loop_headers: &BTreeSet<usize>,
    start: usize,
    end: usize,
) -> Option<GuardedTailSplit> {
    let in_run: Vec<usize> = cfg
        .blocks
        .values()
        .filter(|b| b.id == b.start && b.start >= start && b.end <= end)
        .map(|b| b.id)
        .collect();
    if in_run.iter().any(|id| loop_headers.contains(id)) {
        return None;
    }
    let none = BTreeSet::new();
    for &entry in &in_run {
        let Some((guards, tail, merge)) =
            try_match_guarded_tail_if(cfg, code, entry, &none, loop_headers)
        else {
            continue;
        };
        if let Some(split) = check_split(cfg, &in_run, start, end, guards, tail, merge) {
            return Some(split);
        }
    }
    None
}

fn check_split(
    cfg: &ControlFlowGraph,
    in_run: &[usize],
    start: usize,
    end: usize,
    guards: Vec<(usize, bool)>,
    tail: Vec<usize>,
    merge: usize,
) -> Option<GuardedTailSplit> {
    let entry_start = cfg.blocks.get(&guards.first()?.0)?.start;
    let merge_start = cfg.blocks.get(&merge)?.start;
    let tail_blk = cfg.blocks.get(tail.first()?)?;
    let (tail_start, tail_end) = (tail_blk.start, tail_blk.end);
    if entry_start < start || merge_start >= end || tail_end > end || tail_start <= merge_start {
        return None;
    }
    let guard_ids: BTreeSet<usize> = guards.iter().map(|g| g.0).collect();
    let chain: BTreeSet<usize> = guard_ids.iter().chain(tail.iter()).copied().collect();
    if guards.iter().any(|g| !in_run.contains(&g.0)) || !in_run.contains(&tail[0]) {
        return None;
    }
    // Later links are entered only from earlier links.
    for &(g, _) in guards.iter().skip(1) {
        let blk = cfg.blocks.get(&g)?;
        if blk.predecessors.iter().any(|p| !guard_ids.contains(p)) {
            return None;
        }
    }
    // Each piece outside the chain keeps its jumps inside itself.
    for &id in in_run {
        if chain.contains(&id) {
            continue;
        }
        let blk = cfg.blocks.get(&id)?;
        let ok = if blk.start < entry_start {
            blk.successors.iter().all(|&s| s <= entry_start && s >= start)
        } else if blk.start >= merge_start && blk.start < tail_start {
            blk.successors.iter().all(|&s| s >= merge_start && s < tail_start)
        } else if blk.start >= tail_end {
            blk.successors.iter().all(|&s| s >= tail_end && s <= end)
        } else {
            false // a block between the chain's links and the merge
        };
        if !ok {
            return None;
        }
    }
    Some(GuardedTailSplit { guards, tail, merge, entry_start, merge_start, tail_start, tail_end })
}
