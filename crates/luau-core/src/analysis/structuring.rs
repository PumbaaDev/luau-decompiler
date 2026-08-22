use std::collections::BTreeSet;

use super::cfg::{BasicBlock, ControlFlowGraph, NaturalLoop};
use crate::parser::opcodes::LuauOpcode;
use crate::parser::types::*;

/// High-level region types detected in the CFG.
/// These feed directly into the decompiler's AST generation.
#[derive(Debug, Clone)]
pub enum Region {
    /// A straight-line sequence of instructions
    Linear { start: usize, end: usize },

    /// if <cond> then <then_blocks> [else <else_blocks>] end
    IfThenElse {
        /// PC of the conditional jump instruction
        cond_pc: usize,
        /// PCs of blocks in the then-branch
        then_region: Vec<usize>,
        /// PCs of blocks in the else-branch (empty if no else)
        else_region: Vec<usize>,
        /// PC where control flow merges after the if
        merge_pc: Option<usize>,
    },

    /// A branch region whose only result is the value some register carries
    /// where control flow reconverges — `a and b`, `a or b`, `a and b or c`,
    /// and any `if` that merely selects between values.
    ///
    /// Unlike `IfThenElse` the arms are NOT required to be disjoint, which is
    /// what lets it represent a short-circuit ladder whose fallback block is
    /// shared between two arms. `join` is deliberately excluded from the
    /// region so it keeps its own region and is lifted exactly once.
    ValueJoin {
        entry: usize,
        arms: Vec<usize>,
        join: usize,
    },

    /// while true do ... end (loop with break-based exit)
    WhileTrue {
        header: usize,
        body_blocks: Vec<usize>,
    },

    /// while <cond> do ... end
    WhileDo {
        header: usize,
        body_blocks: Vec<usize>,
    },

    /// repeat ... until <cond>
    RepeatUntil {
        header: usize,
        body_blocks: Vec<usize>,
        cond_pc: usize,
    },

    /// for i = start, stop, step do ... end
    NumericFor {
        prep_pc: usize,
        loop_pc: usize,
        body_start: usize,
        body_end: usize,
        /// Phase B0.4: nested structured regions inside `[body_start, body_end)`.
        ///
        /// Populated by `structure_numeric_for_body` when
        /// `structure_control_flow` matches a ForNPrep. Each element is either
        /// a `Region::Linear` (straight-line body code) or a nested
        /// `Region::NumericFor` (recursively structured).
        ///
        /// The lifter iterates this vector instead of walking `[body_start,
        /// body_end)` linearly, which is what lets nested for-loops render
        /// recursively. If this vector is empty the lifter falls back to the
        /// pre-B0.4 linear behavior (used for edge cases where the scan
        /// produces no regions, e.g. an empty body).
        body: Vec<Region>,
    },

    /// A `while <cond> do` / `while true do` loop nested inside a for-loop body,
    /// carried as PC ranges rather than CFG block ids.
    ///
    /// `structure_control_flow` claims every block inside a matched for-body as
    /// `handled` before its natural-loop check runs, so an inner loop's header
    /// is skipped and no `WhileDo`/`WhileTrue` region is ever produced for it.
    /// `structure_numeric_for_body` — which structures for-bodies instead — had
    /// no back-edge recognizer at all, so the inner loop degenerated into a
    /// forward `if` plus a `continue` and its body ran once per outer iteration.
    ///
    /// This variant is emitted by that scan when it finds a JUMPBACK whose
    /// target lies wholly inside the body range being scanned.
    InlineLoopInLoop {
        /// Back-edge target: the loop header, re-executed every iteration.
        header_start: usize,
        /// PC of the exit conditional (`while c do`), or `None` (`while true do`).
        cond_pc: Option<usize>,
        /// First instruction of the loop body proper.
        body_start: usize,
        /// PC of the JUMPBACK latch.
        latch_pc: usize,
        /// Structured body regions.
        body: Vec<Region>,
    },

    /// for k, v in iterator do ... end
    GenericFor {
        prep_pc: usize,
        loop_pc: usize,
        body_start: usize,
        body_end: usize,
        /// Phase B0.6: nested structured regions inside `[body_start, body_end)`.
        ///
        /// Mirrors the `Region::NumericFor::body` mechanism from B0.4 for the
        /// generic-for case. Populated by `structure_control_flow` (and
        /// recursively by `structure_numeric_for_body`) when a GenericFor
        /// region is emitted. Each element is a `Region::Linear`,
        /// `Region::NumericFor`, `Region::GenericFor`, or
        /// `Region::InlineIfThenInLoop`.
        ///
        /// When this vector is empty the lifter falls back to the pre-B0.6
        /// linear body lift (via `lift_instruction_range`).
        body: Vec<Region>,
    },

    /// Phase B0.5: `if <cond> then <body> end` wrapping nested for-loop
    /// regions inside a numeric-for body range.
    ///
    /// Emitted ONLY by `structure_numeric_for_body` when a forward
    /// conditional jump's target range contains a recognized nested
    /// NumericFor or GenericFor. The lifter unpacks this by extracting the
    /// condition from `cond_pc` via `extract_branch_condition` and iterating
    /// `body` the same way it iterates `Region::NumericFor::body`
    /// (Linear → `lift_instruction_range`, nested Region → `lift_region`).
    ///
    /// Scope: This variant is intentionally narrow. It exists so that
    /// `if X then for j = ... end end` inside a numeric-for body does not
    /// tear the JumpIf* target across a nested for-loop extraction and
    /// produce a spurious `if cond then break end` (the Phase B0.4 failure
    /// mode on this shape). A broader recursive if-structurer for loop
    /// bodies is out of scope for Phase B0.5.
    InlineIfThenInLoop {
        /// PC of the JumpIf* / JumpIfEq* / JumpXEqK* branch instruction.
        /// The `extract_branch_condition` helper in the lifter reads this
        /// PC and derives the fall-through (i.e. then-body) condition.
        cond_pc: usize,
        /// Nested regions lifted inside the if-then body range
        /// `[jump_next_pc, merge_pc)`. May contain Linear, NumericFor,
        /// GenericFor, or (rarely) further InlineIfThenInLoop variants.
        body: Vec<Region>,
    },
}

/// Analyze CFG and produce ordered regions

/// Structure a BLOCK SUBSET with if/else matching. GATED on LUAU_BRANCH_ARMS.
///
/// Rebuilt for attribution (rotation 80): rotation 74 measured this at
/// -45 clean / +92 defects and rotation 76's narrowing at -40 / +93, and
/// rotation 76 falsified the merge explanation for that cost. The cost is
/// real, reproducible and unexplained, so this exists to be measured per file
/// rather than adopted.
pub(crate) fn structure_block_subset(
    cfg: &ControlFlowGraph,
    proto: &Proto,
    subset: &[usize],
) -> Vec<Region> {
    let _ = proto;
    let member: BTreeSet<usize> = subset.iter().copied().collect();
    let mut regions = Vec::new();
    let mut handled: BTreeSet<usize> = BTreeSet::new();
    for &block_id in subset {
        if handled.contains(&block_id) {
            continue;
        }
        let Some(block) = cfg.blocks.get(&block_id) else { continue };
        if block.successors.len() == 2 {
            let fallthrough = block.successors[0];
            let branch = block.successors[1];
            let merge = cfg.find_merge_point(fallthrough, branch);
            let then_blocks = collect_region_blocks(cfg, fallthrough, merge, &handled);
            let else_blocks = collect_region_blocks(cfg, branch, merge, &handled);
            let ok = !then_blocks.is_empty()
                && then_blocks.iter().all(|b| member.contains(b))
                && else_blocks.iter().all(|b| member.contains(b))
                && then_blocks.len() < subset.len()
                && else_blocks.len() < subset.len();
            if ok {
                let has_else =
                    !else_blocks.is_empty() && merge.map_or(true, |m| branch != m);
                regions.push(Region::IfThenElse {
                    cond_pc: block_id,
                    then_region: then_blocks.iter().copied().collect(),
                    else_region: if has_else {
                        else_blocks.iter().copied().collect()
                    } else {
                        vec![]
                    },
                    merge_pc: merge,
                });
                handled.insert(block_id);
                for &b in &then_blocks {
                    handled.insert(b);
                }
                if has_else {
                    for &b in &else_blocks {
                        handled.insert(b);
                    }
                }
                continue;
            }
        }
        regions.push(Region::Linear { start: block.start, end: block.end });
        handled.insert(block_id);
    }
    regions
}

pub fn structure_control_flow(
    cfg: &ControlFlowGraph,
    proto: &Proto,
) -> Vec<Region> {
    let code = &proto.code;
    let loops = cfg.find_loops();
    let loop_headers_all: BTreeSet<usize> = loops.iter().map(|l| l.header).collect();
    let mut regions = Vec::new();
    let mut handled = BTreeSet::new();
    let rpo = cfg.reverse_postorder();
    // LUAU_RPO_TRACE reports blocks the structurer will never visit. A block
    // absent from the reverse postorder is UNREACHABLE from the entry in the
    // CFG, so it gets no region and its code is silently dropped - which is
    // what a vanished function body looks like from here.
    if std::env::var("LUAU_RPO_TRACE").is_ok() {
        let seen: BTreeSet<usize> = rpo.iter().copied().collect();
        let missing: Vec<usize> = cfg.blocks.keys().copied().filter(|b| !seen.contains(b)).collect();
        if !missing.is_empty() {
            // Is each orphan genuine dead code, or a MISSING EDGE? If some
            // block lists it as a successor, the CFG knows the edge exists and
            // reachability lost it - a real defect that silently deletes live
            // code. If nothing points at it, the compiler emitted dead code and
            // dropping it is correct.
            let orphan_with_pred: Vec<usize> = missing
                .iter()
                .copied()
                .filter(|m| cfg.blocks.values().any(|b| b.successors.contains(m)))
                .collect();
            eprintln!(
                "RPOPRED proto={}@{} orphans={} with_predecessor={:?}",
                proto.debug_name.as_deref().unwrap_or("?"),
                proto.line_defined,
                missing.len(),
                orphan_with_pred
            );
            eprintln!(
                "RPO proto={}@{} blocks={} reachable={} UNREACHABLE={:?}",
                proto.debug_name.as_deref().unwrap_or("?"),
                proto.line_defined,
                cfg.blocks.len(),
                rpo.len(),
                missing
            );
        }
    }

    for &block_id in &rpo {
        if handled.contains(&block_id) {
            continue;
        }

        let block = match cfg.blocks.get(&block_id) {
            Some(b) => b,
            None => continue,
        };

        // Check if this block is a for-loop prep
        if let Some(region) = try_match_for_loop(code, block_id, &loops) {
            match &region {
                Region::NumericFor { body_start, body_end, .. }
                | Region::GenericFor { body_start, body_end, .. } => {
                    // Mark all blocks in the loop body as handled
                    // Block IDs are instruction PCs and may not be contiguous,
                    // so we check if a block's start falls within [body_start, body_end)
                    // OR if the block's range overlaps the body range
                    for (&bid, blk) in &cfg.blocks {
                        if blk.start >= *body_start && blk.start < *body_end {
                            handled.insert(bid);
                        }
                    }
                    // Also mark the loop instruction itself
                    handled.insert(*body_end);
                }
                _ => {}
            }
            handled.insert(block_id);
            // Phase B0.4: attach a nested region body to Region::NumericFor so
            // the lifter can recursively render inner for-loops. Without this,
            // an inner ForNPrep inside the outer body range was lifted linearly
            // (as raw opcodes) because the outer match marked every block in
            // `[body_start, body_end)` as handled BEFORE try_match_for_loop
            // ever saw the inner ForNPrep block. The scan below fixes that by
            // walking the body pc range directly, independent of CFG blocks,
            // and producing a Vec<Region> that mirrors the nested structure.
            //
            // Phase B0.6: extended to GenericFor — same rationale. An inner
            // for-loop inside a generic-for body was silently dropped by
            // `lift_instruction_range`'s empty for-opcode match arm; now we
            // pre-structure the body the same way and the lifter's new
            // GenericFor.body iterator dispatches nested regions recursively.
            let final_region = match region {
                Region::NumericFor { prep_pc, loop_pc, body_start, body_end, body: _ } => {
                    let nested = structure_numeric_for_body(code, body_start, body_end, cfg, &loop_headers_all);
                    Region::NumericFor {
                        prep_pc,
                        loop_pc,
                        body_start,
                        body_end,
                        body: nested,
                    }
                }
                Region::GenericFor { prep_pc, loop_pc, body_start, body_end, body: _ } => {
                    let nested = structure_numeric_for_body(code, body_start, body_end, cfg, &loop_headers_all);
                    Region::GenericFor {
                        prep_pc,
                        loop_pc,
                        body_start,
                        body_end,
                        body: nested,
                    }
                }
                other => other,
            };
            regions.push(final_region);
            continue;
        }

        // Check if this block is a loop header
        if let Some(lp) = loops.iter().find(|l| l.header == block_id) {
            // Validate that this is a REAL loop by checking if the back-edge
            // source actually has a backward jump instruction targeting the
            // header. False loops arise when the dominator tree creates
            // spurious back-edges from forward-only code.
            if !is_real_loop(code, cfg, lp) {
                // Not a real loop — let it fall through to if/else or linear
                // detection below instead of being misclassified as a loop.
            } else {
                let body_vec: Vec<usize> = lp.body.iter().copied().collect();

                // Determine loop kind from the header's branching
                if block.successors.len() == 2 {
                    // Header has a conditional branch. It's a while-do only if
                    // exactly one successor exits the loop (the other stays in
                    // the body). If both successors are in the loop body, this
                    // is actually a while-true with an if-statement at the top.
                    let s0_in_loop = lp.body.contains(&block.successors[0]);
                    let s1_in_loop = lp.body.contains(&block.successors[1]);
                    if s0_in_loop && !s1_in_loop || !s0_in_loop && s1_in_loop {
                        // Exactly one successor exits — genuine while-do
                        regions.push(Region::WhileDo {
                            header: block_id,
                            body_blocks: body_vec.clone(),
                        });
                    } else {
                        // Both in loop or both out (unusual) — treat as while-true
                        regions.push(Region::WhileTrue {
                            header: block_id,
                            body_blocks: body_vec.clone(),
                        });
                    }
                } else if block.successors.len() == 1 {
                    // Check if back-edge source has a condition (repeat-until)
                    let back_block = cfg.blocks.get(&lp.back_edge_source);
                    if back_block.map(|b| b.successors.len()) == Some(2) {
                        regions.push(Region::RepeatUntil {
                            header: block_id,
                            body_blocks: body_vec.clone(),
                            cond_pc: lp.back_edge_source,
                        });
                    } else {
                        regions.push(Region::WhileTrue {
                            header: block_id,
                            body_blocks: body_vec.clone(),
                        });
                    }
                } else {
                    regions.push(Region::WhileTrue {
                        header: block_id,
                        body_blocks: body_vec.clone(),
                    });
                }

                for &b in &lp.body {
                    handled.insert(b);
                }
                continue;
            }
        }

        // A branch that only selects a VALUE has to be matched before the
        // if/then/else fallback. `find_merge_point` is a first-common-node
        // search, not a post-dominator: given a short-circuit ladder it reports
        // one of the arms as the merge, because that arm is trivially reachable
        // from the other. Everything downstream then inherits the wrong join.
        if block.successors.len() == 2 {
            if let Some(vj) = crate::analysis::value_region::find_value_join(
                cfg,
                code,
                block_id,
                &loop_headers_all,
            ) {
                // Decline if an enclosing region already claimed any member —
                // re-lifting a claimed block would duplicate it.
                let free = !handled.contains(&vj.join)
                    && vj.arms.iter().all(|b| !handled.contains(b));
                if free {
                    handled.insert(block_id);
                    for &b in &vj.arms {
                        handled.insert(b);
                    }
                    // `join` is intentionally left unhandled: it is the region's
                    // continuation, not part of it.
                    regions.push(Region::ValueJoin {
                        entry: vj.entry,
                        arms: vj.arms,
                        join: vj.join,
                    });
                    continue;
                }
            }
        }

        // Check if this is a conditional branch (if-then-else)
        if block.successors.len() == 2 {
            let fallthrough = block.successors[0]; // usually the "then" path
            let branch = block.successors[1];       // the jump target

            // Find where branches merge
            let merge = cfg.find_merge_point(fallthrough, branch);

            // Collect blocks in each branch
            let then_blocks = collect_region_blocks(cfg, fallthrough, merge, &handled);
            let else_blocks = collect_region_blocks(cfg, branch, merge, &handled);

            // Determine if this is if-then or if-then-else
            let has_else = !else_blocks.is_empty()
                && merge.map_or(true, |m| branch != m);

            regions.push(Region::IfThenElse {
                cond_pc: block_id,
                then_region: then_blocks.iter().copied().collect(),
                else_region: if has_else {
                    else_blocks.iter().copied().collect()
                } else {
                    vec![]
                },
                merge_pc: merge,
            });

            if std::env::var("LUAU_IF_TRACE").is_ok() {
                // How much of the proto does ONE if/else absorb? An arm that
                // runs to the end of the function marks every remaining block
                // handled, so nothing after it ever receives a region - which
                // is what a vanished body looks like from here.
                eprintln!(
                    "IF proto={}@{} cond_block={} then={} else={} merge={:?} has_else={}",
                    proto.debug_name.as_deref().unwrap_or("?"),
                    proto.line_defined,
                    block_id,
                    then_blocks.len(),
                    else_blocks.len(),
                    merge,
                    has_else
                );
            }
            handled.insert(block_id);
            for &b in &then_blocks {
                handled.insert(b);
            }
            if has_else {
                for &b in &else_blocks {
                    handled.insert(b);
                }
            }
            continue;
        }

        // Default: linear block
        regions.push(Region::Linear {
            start: block.start,
            end: block.end,
        });
        handled.insert(block_id);
    }

    if std::env::var("LUAU_REGION_TRACE").is_ok() {
        // How much of the proto did structuring actually claim? A body that
        // vanishes with every block REACHABLE (rotation 62) must be losing its
        // code between region production and region lifting, and this is the
        // first of those two numbers.
        eprintln!(
            "REGIONS proto={}@{} blocks={} regions={}",
            proto.debug_name.as_deref().unwrap_or("?"),
            proto.line_defined,
            cfg.blocks.len(),
            regions.len()
        );
        // Which regions, and how far each reaches. `blocks >> regions` means
        // structuring claimed the proto with a few coarse spans instead of
        // nesting it, and this shows WHICH span swallowed the rest.
        for (i, r) in regions.iter().enumerate() {
            let d = format!("{:?}", r);
            let head = d.chars().take(1600).collect::<String>();
            eprintln!("  REGION[{}] {}", i, head);
        }
    }

    regions
}

/// Try to match a for-loop at the given block.
///
/// Validates that the D offset from a FORNPREP/FORGPREP instruction actually
/// points to a matching FORNLOOP/FORGLOOP instruction. Without this check,
/// misidentified opcodes (common with Roblox's shuffled bytecode) can cause
/// arbitrary code ranges to be wrapped in bogus for-loop structures.
fn try_match_for_loop(
    code: &[u32],
    block_start: usize,
    _loops: &[NaturalLoop],
) -> Option<Region> {
    if block_start >= code.len() {
        return None;
    }

    let insn = code[block_start];
    let op = LuauOpcode::from_u8(insn_op(insn));
    let d = insn_d(insn);

    // FORNPREP and FORGPREP have DIFFERENT jump-offset semantics:
    //
    //   FORNPREP: VM `pc++; if skip_loop { pc += D; }`. The skip branch lands
    //             PAST the FORNLOOP (at skip_target = prep_pc + 1 + D). The
    //             FORNLOOP sits one slot BEFORE that — loop_pc = prep_pc + D.
    //
    //   FORGPREP: VM `pc++; pc += D;` (unconditional). Lands directly AT the
    //             FORGLOOP — loop_pc = prep_pc + 1 + D.
    //
    // Getting this wrong blocks structural recognition of numeric-for entirely:
    // the `code[loop_end_pc] == FORNLOOP` check fails, try_match_for_loop returns
    // None, and the lifter emits raw jump spaghetti instead of a `for` statement.
    match op {
        LuauOpcode::ForNPrep => {
            // FORNPREP → loop_pc = prep_pc + D (NO +1).
            let target_i = block_start as i64 + d as i64;
            if target_i < 0 || target_i as usize >= code.len() {
                return None;
            }
            let loop_end_pc = target_i as usize;
            if loop_end_pc <= block_start {
                return None;
            }
            // Validate: the instruction at loop_end_pc must be FORNLOOP
            let target_op = LuauOpcode::from_u8(insn_op(code[loop_end_pc]));
            if target_op != LuauOpcode::ForNLoop {
                return None;
            }
            let body_start = block_start + 1;
            Some(Region::NumericFor {
                prep_pc: block_start,
                loop_pc: loop_end_pc,
                body_start,
                body_end: loop_end_pc,
                // Phase B0.4: the caller (`structure_control_flow` or
                // `structure_numeric_for_body`) is responsible for populating
                // the nested body by calling `structure_numeric_for_body`
                // after this match returns. Leaving this empty here keeps
                // `try_match_for_loop` a pure pattern-match over a single pc.
                body: Vec::new(),
            })
        }
        LuauOpcode::ForGPrep | LuauOpcode::ForGPrepINext | LuauOpcode::ForGPrepNext => {
            // FORGPREP → loop_pc = prep_pc + D + 1 (unconditional jump to FORGLOOP).
            let target_i = block_start as i64 + d as i64 + 1;
            if target_i < 0 || target_i as usize >= code.len() {
                return None;
            }
            let loop_end_pc = target_i as usize;
            if loop_end_pc <= block_start {
                return None;
            }
            // Validate: the instruction at loop_end_pc must be FORGLOOP or FORGLOOPINEXT.
            // FORGLOOPINEXT (Deprecated61) is the specialized ipairs loop-back used by
            // Roblox's Luau compiler alongside FORGPREP_INEXT.
            let target_op = LuauOpcode::from_u8(insn_op(code[loop_end_pc]));
            if target_op != LuauOpcode::ForGLoop && target_op != LuauOpcode::Deprecated61 {
                return None;
            }
            let body_start = block_start + 1;
            Some(Region::GenericFor {
                prep_pc: block_start,
                loop_pc: loop_end_pc,
                body_start,
                body_end: loop_end_pc,
                // Phase B0.6: the caller populates `body` via
                // `structure_numeric_for_body` (same pattern as NumericFor).
                body: Vec::new(),
            })
        }
        _ => None,
    }
}

/// Collect blocks reachable from `start` up to (but not including) `boundary`
fn collect_region_blocks(
    cfg: &ControlFlowGraph,
    start: usize,
    boundary: Option<usize>,
    already_handled: &BTreeSet<usize>,
) -> Vec<usize> {
    let mut result = Vec::new();
    let mut visited = BTreeSet::new();
    let mut queue = std::collections::VecDeque::new();

    if already_handled.contains(&start) {
        return result;
    }
    if boundary == Some(start) {
        return result;
    }

    visited.insert(start);
    queue.push_back(start);

    while let Some(node) = queue.pop_front() {
        result.push(node);
        if let Some(block) = cfg.blocks.get(&node) {
            for &succ in &block.successors {
                // Don't cross the merge point — and don't step OVER it either.
                // A branch arm ends where control flow reconverges, so an edge
                // landing at or beyond the merge belongs to the continuation,
                // not to the arm. Without the `>=` test a short-circuit ladder
                // (whose then-block also has an edge past the merge, straight
                // to the real join) drags the entire rest of the function into
                // the arm, and everything after it stops executing.
                if boundary.map_or(false, |b| succ >= b) {
                    continue;
                }
                if visited.insert(succ) && !already_handled.contains(&succ) {
                    queue.push_back(succ);
                }
            }
        }
    }
    // Sort by block start PC so instructions are lifted in program order
    result.sort_unstable();
    result
}

/// Check if a detected natural loop is a REAL loop by verifying that the
/// back-edge source block actually contains a backward jump instruction
/// (JumpBack, ForNLoop, ForGLoop, or a conditional/unconditional Jump
/// whose target is at or before the header). False positives arise when
/// the dominator analysis creates spurious back-edges in purely
/// forward-branching code (common with shuffled/unknown opcodes).
fn is_real_loop(
    code: &[u32],
    cfg: &ControlFlowGraph,
    lp: &NaturalLoop,
) -> bool {
    // Check if ANY block in the loop body has a backward jump to the header
    // (or to any block at/before the header that might eventually reach it).
    let header = lp.header;

    for &block_id in &lp.body {
        let block = match cfg.blocks.get(&block_id) {
            Some(b) => b,
            None => continue,
        };

        // Check the last instruction of this block
        let last_pc = last_real_pc(code, block);
        if last_pc >= code.len() {
            continue;
        }

        let insn = code[last_pc];
        let op = LuauOpcode::from_u8(insn_op(insn));
        let d = insn_d(insn);
        let e = insn_e(insn);

        match op {
            // Explicit backward jump opcodes — these are always real loops
            LuauOpcode::JumpBack => return true,
            LuauOpcode::ForNLoop | LuauOpcode::ForGLoop | LuauOpcode::Deprecated61 => return true,

            // Unconditional forward Jump that targets the header or earlier
            LuauOpcode::Jump => {
                let target = (last_pc as i32 + d as i32 + 1) as usize;
                if target <= header {
                    return true;
                }
            }
            LuauOpcode::JumpX => {
                let target = (last_pc as i32 + e + 1) as usize;
                if target <= header {
                    return true;
                }
            }

            // Conditional jumps that target the header or earlier
            LuauOpcode::JumpIf | LuauOpcode::JumpIfNot
            | LuauOpcode::JumpIfEq | LuauOpcode::JumpIfLE | LuauOpcode::JumpIfLT
            | LuauOpcode::JumpIfNotEq | LuauOpcode::JumpIfNotLE | LuauOpcode::JumpIfNotLT
            | LuauOpcode::JumpXEqKNil | LuauOpcode::JumpXEqKB
            | LuauOpcode::JumpXEqKN | LuauOpcode::JumpXEqKS => {
                let target = (last_pc as i32 + d as i32 + 1) as usize;
                if target <= header {
                    return true;
                }
            }

            _ => {}
        }
    }

    false
}

/// Phase B0.4 + B0.5: build the nested region tree for a numeric-for body range.
///
/// Scans `[body_start, body_end)` linearly, producing:
///   - `Region::NumericFor { ..., body: <recursive> }` for each nested
///     ForNPrep whose matching ForNLoop lies within the range (B0.4).
///   - `Region::GenericFor { body_start, body_end }` for each nested
///     FORGPREP/FORGPREP_NEXT/FORGPREP_INEXT whose matching FORGLOOP lies
///     within the range (B0.5, Shape A).
///   - `Region::InlineIfThenInLoop { cond_pc, body }` for each forward
///     conditional jump whose target range contains one of the above
///     nested for-loops (B0.5, Shape B — prevents the JumpIf*'s target
///     from being misread as an out-of-range forward jump = spurious
///     `if cond then break end`).
///   - `Region::Linear { start, end }` for the surrounding straight-line code.
///
/// This is intentionally narrower than `structure_control_flow`: it does NOT
/// re-run loop / if-then-else / while-true detection. Running the full
/// structurer recursively on a for-loop body would misinterpret the outer
/// for-loop's own back-edge (via the inner ForNLoop's header-dominance) as a
/// separate while-do region, which breaks nested-for rendering.
///
/// Non-for control flow inside a for-loop body (if/while/repeat that does
/// NOT wrap a nested for-loop) continues to lift via `lift_instruction_range`
/// inside the `Region::Linear` segments, preserving the pre-B0.5 behavior
/// for those shapes. Phase B0.5 only upgrades the specific
/// conditional-wrapping-a-for case.
///
/// Recursion: called recursively for NumericFor and GenericFor bodies (where
/// the nested body may itself contain further nested for-loops), and for
/// InlineIfThenInLoop bodies (where the if-arm may contain one or more
/// nested for-loops).
pub(crate) fn structure_numeric_for_body(
    code: &[u32],
    body_start: usize,
    body_end: usize,
    cfg: &ControlFlowGraph,
    loop_headers: &BTreeSet<usize>,
) -> Vec<Region> {
    let mut regions: Vec<Region> = Vec::new();
    if body_end <= body_start || body_end > code.len() {
        return regions;
    }

    let mut linear_start = body_start;
    let mut pc = body_start;

    // Rotation 113: what does the scan SEE, per pc? Two generic-for bodies in
    // the same proto (`59_LocalCubs::?@40`) get opposite results - 27..45
    // structures into ValueJoin+Linear, 55..162 stays one flat Linear - and the
    // difference between them has never been printed.
    let shape_trace = std::env::var("LUAU_SHAPE_TRACE")
        .ok()
        .and_then(|v| {
            let (a, b) = v.split_once('-')?;
            Some((a.trim().parse::<usize>().ok()?, b.trim().parse::<usize>().ok()?))
        })
        .filter(|&(a, b)| a == body_start && b == body_end);
    if shape_trace.is_some() {
        eprintln!("SHAPE	scan body={}..{}", body_start, body_end);
    }

    while pc < body_end {
        let insn = code[pc];
        let op = LuauOpcode::from_u8(insn_op(insn));
        if shape_trace.is_some() {
            let fj = forward_conditional_jump(op, pc, insn);
            eprintln!(
                "SHAPE	  pc={}	op={:?}	fwd_cond={:?}	linear_start={}",
                pc, op, fj, linear_start
            );
        }

        // ── Value join inside a loop body ────────────────────────────────
        //
        // `local row = rows[i]  print(row and row.n or -1)` is the same shape
        // whether it sits at the top level or inside a `for`. Without this the
        // body is lifted as raw instructions, the inline-if path discards the
        // branch's register state wholesale, and the selected value degenerates
        // into whichever arm happened to be laid out last.
        //
        // Matched before every loop shape below: those all key off a for-prep
        // or a back edge, and a value join contains neither.
        // The enclosing loop's own header is the first block of its body, and
        // it has already been claimed by the region that called us — so unlike
        // at top level it is a legitimate value-join entry here. Headers of
        // loops NESTED in this body still need protecting.
        let headers_here: BTreeSet<usize> = loop_headers
            .iter()
            .copied()
            .filter(|&h| h != body_start)
            .collect();
        if cfg.blocks.contains_key(&pc) {
            if let Some(vj) = crate::analysis::value_region::find_value_join(cfg, code, pc, &headers_here) {
                let join_start = cfg.blocks.get(&vj.join).map(|b| b.start).unwrap_or(body_end);
                // Must be wholly inside the range being scanned, or the region
                // would claim instructions this call is not responsible for.
                if join_start <= body_end {
                    if linear_start < pc {
                        regions.push(Region::Linear { start: linear_start, end: pc });
                    }
                    regions.push(Region::ValueJoin {
                        entry: vj.entry,
                        arms: vj.arms,
                        join: vj.join,
                    });
                    pc = join_start;
                    linear_start = pc;
                    continue;
                }
            }
        }

        if let Some((then_start, target)) = forward_conditional_jump(op, pc, insn) {
            let cond_block = cfg.blocks.iter()
                .find(|(_, b)| b.start <= pc && pc < b.end).map(|(id, _)| *id);
            if let Some(cond_id) = cond_block {
                if target > then_start && target <= body_end && then_start < body_end {
                    let mut last_pc = then_start; let mut w = then_start; let mut spans_loop = false;
                    while w < target && w < code.len() {
                        last_pc = w;
                        let wop = LuauOpcode::from_u8(insn_op(code[w]));
                        if matches!(wop, LuauOpcode::ForNPrep | LuauOpcode::ForGPrep
                            | LuauOpcode::ForGPrepINext | LuauOpcode::ForGPrepNext
                            | LuauOpcode::JumpBack) { spans_loop = true; }
                        w += if wop.has_aux() { 2 } else { 1 };
                    }
                    let is_loop_cond = last_pc < code.len()
                        && matches!(LuauOpcode::from_u8(insn_op(code[last_pc])), LuauOpcode::JumpBack)
                        && (last_pc as i64 + insn_d(code[last_pc]) as i64 + 1) <= pc as i64;
                    let mut merge = target;
                    if last_pc < code.len()
                        && matches!(LuauOpcode::from_u8(insn_op(code[last_pc])), LuauOpcode::Jump) {
                        let d = insn_d(code[last_pc]);
                        if d > 0 { let m = (last_pc as i64 + d as i64 + 1) as usize;
                            if m > target && m <= body_end { merge = m; } }
                    }
                    let then_blocks: Vec<usize> = cfg.blocks.iter()
                        .filter(|(_, b)| b.start >= then_start && b.start < target)
                        .map(|(id, _)| *id).collect();
                    let else_blocks: Vec<usize> = cfg.blocks.iter()
                        .filter(|(_, b)| b.start >= target && b.start < merge)
                        .map(|(id, _)| *id).collect();
                    let mut spans: Vec<(usize, usize)> = then_blocks.iter().chain(else_blocks.iter())
                        .filter_map(|id| cfg.blocks.get(id).map(|b| (b.start, b.end))).collect();
                    spans.sort_unstable();
                    let tiles = !spans.is_empty() && spans[0].0 == then_start
                        && spans.last().map(|s| s.1) == Some(merge)
                        && spans.windows(2).all(|w| w[0].1 == w[1].0);
                    if spans_loop && tiles && !is_loop_cond && !then_blocks.is_empty()
                        && !then_blocks.contains(&cond_id) {
                        if linear_start < pc { regions.push(Region::Linear { start: linear_start, end: pc }); }
                        regions.push(Region::IfThenElse { cond_pc: cond_id,
                            then_region: then_blocks, else_region: else_blocks, merge_pc: Some(merge) });
                        pc = merge; linear_start = pc; continue;
                    }
                }
            }
        }

        // ── Phase B0.5, Shape A: nested ForGPrep → ForGLoop pair ─────────
        //
        // FORGPREP target formula: loop_pc = prep_pc + D + 1 (unconditional
        // jump to the matching FORGLOOP, which is at the END of the loop).
        // FORGLOOP has an AUX word (holds the variable count in the low 24
        // bits and the inext flag in bit 31), so we must advance by 2 when
        // resuming the scan past it.
        if matches!(
            op,
            LuauOpcode::ForGPrep | LuauOpcode::ForGPrepINext | LuauOpcode::ForGPrepNext
        ) {
            let d = insn_d(insn);
            let target_i = pc as i64 + d as i64 + 1;
            if target_i > pc as i64 && target_i < body_end as i64 {
                let loop_end_pc = target_i as usize;
                let target_op = LuauOpcode::from_u8(insn_op(code[loop_end_pc]));
                let is_forgloopinext = target_op == LuauOpcode::Deprecated61;
                if target_op == LuauOpcode::ForGLoop || is_forgloopinext {
                    if linear_start < pc {
                        regions.push(Region::Linear {
                            start: linear_start,
                            end: pc,
                        });
                    }
                    let inner_body_start = pc + 1;
                    let inner_body_end = loop_end_pc;
                    let nested_body =
                        structure_numeric_for_body(code, inner_body_start, inner_body_end, cfg, loop_headers);
                    regions.push(Region::GenericFor {
                        prep_pc: pc,
                        loop_pc: loop_end_pc,
                        body_start: inner_body_start,
                        body_end: inner_body_end,
                        body: nested_body,
                    });
                    // FORGLOOP has an AUX word → advance by 2.
                    // FORGLOOPINEXT (Deprecated61) has no AUX word → advance by 1.
                    pc = loop_end_pc + if is_forgloopinext { 1 } else { 2 };
                    linear_start = pc;
                    continue;
                }
            }
        }

        // ── Phase B0.4: nested ForNPrep → ForNLoop pair ──────────────────
        if matches!(op, LuauOpcode::ForNPrep) {
            let d = insn_d(insn);
            let target_i = pc as i64 + d as i64;
            if target_i > pc as i64 && target_i < body_end as i64 {
                let loop_end_pc = target_i as usize;
                let target_op = LuauOpcode::from_u8(insn_op(code[loop_end_pc]));
                if target_op == LuauOpcode::ForNLoop {
                    if linear_start < pc {
                        regions.push(Region::Linear {
                            start: linear_start,
                            end: pc,
                        });
                    }

                    let inner_body_start = pc + 1;
                    let inner_body_end = loop_end_pc;
                    let nested_body =
                        structure_numeric_for_body(code, inner_body_start, inner_body_end, cfg, loop_headers);

                    regions.push(Region::NumericFor {
                        prep_pc: pc,
                        loop_pc: loop_end_pc,
                        body_start: inner_body_start,
                        body_end: inner_body_end,
                        body: nested_body,
                    });

                    // Advance past the inner ForNLoop instruction. ForNLoop
                    // has no AUX word.
                    pc = loop_end_pc + 1;
                    linear_start = pc;
                    continue;
                }
            }
        }

        // ── Shape C: a `while` / `while true` loop nested in this body ───
        //
        // Must be tried BEFORE Shape B: a while-guard and an if-guard are the
        // same forward conditional jump instruction, and they differ only by the
        // JUMPBACK latch that closes the range. Letting Shape B win turns the
        // loop into `if cond then ... continue end`, so the body runs once per
        // outer iteration instead of looping.
        // Two triggers, because the latch is not always the first thing the scan
        // reaches. `while c do BODY end` leads with its guard, and Shape B below
        // would claim the whole range at that guard and never look at the latch.
        //
        //   (1) a JUMPBACK latch, reached directly (`while true do`, or a guard
        //       shape this scan did not recognise);
        //   (2) a forward conditional whose target is preceded by a JUMPBACK
        //       landing at or before the conditional — the classic
        //       `while c do BODY end` layout.
        let shape_c = match op {
            LuauOpcode::JumpBack if insn_d(insn) < 0 => {
                let t = pc as i64 + 1 + insn_d(insn) as i64;
                // Strict containment: the back edge must lie wholly inside the
                // range being scanned, so this can never claim the enclosing
                // for-loop's own back edge or an outer while's latch.
                if t >= body_start as i64 && t <= pc as i64 {
                    Some((t as usize, None, pc))
                } else {
                    None
                }
            }
            _ => match forward_conditional_jump(op, pc, insn) {
                Some((next_pc, target)) if target > next_pc && target <= body_end => {
                    // The latch must be the LAST instruction before the target,
                    // found by walking instruction boundaries so an AUX word is
                    // never mistaken for a JUMPBACK.
                    let mut last = None;
                    let mut w = next_pc;
                    while w < target {
                        last = Some(w);
                        let wop = LuauOpcode::from_u8(insn_op(code[w]));
                        w += if wop.has_aux() { 2 } else { 1 };
                    }
                    last.and_then(|latch| {
                        let lop = LuauOpcode::from_u8(insn_op(code[latch]));
                        if lop != LuauOpcode::JumpBack || insn_d(code[latch]) >= 0 {
                            return None;
                        }
                        let h = latch as i64 + 1 + insn_d(code[latch]) as i64;
                        if h >= body_start as i64 && h <= pc as i64 {
                            Some((h as usize, Some(pc), latch))
                        } else {
                            None
                        }
                    })
                }
                _ => None,
            },
        };

        if let Some((header_start, mut cond_pc, latch_pc)) = shape_c {
            let after_latch = latch_pc + 1;
            let mut inner_body_start = header_start;
            if let Some(cp) = cond_pc {
                inner_body_start = forward_conditional_jump(
                    LuauOpcode::from_u8(insn_op(code[cp])),
                    cp,
                    code[cp],
                )
                .map(|(next_pc, _)| next_pc)
                .unwrap_or(header_start);
            } else {
                // Reached via the latch: look for an exit test in the header
                // whose target is the instruction right after the latch.
                let mut h = header_start;
                while h < latch_pc {
                    let hop = LuauOpcode::from_u8(insn_op(code[h]));
                    if let Some((next_pc, t)) = forward_conditional_jump(hop, h, code[h]) {
                        if t == after_latch {
                            cond_pc = Some(h);
                            inner_body_start = next_pc;
                            break;
                        }
                    }
                    h += if hop.has_aux() { 2 } else { 1 };
                }
            }
            if linear_start < header_start {
                regions.push(Region::Linear {
                    start: linear_start,
                    end: header_start,
                });
            }
            let nested_body = structure_numeric_for_body(code, inner_body_start, latch_pc, cfg, loop_headers);
            regions.push(Region::InlineLoopInLoop {
                header_start,
                cond_pc,
                body_start: inner_body_start,
                latch_pc,
                body: nested_body,
            });
            // JUMPBACK has no AUX word.
            pc = after_latch;
            linear_start = pc;
            continue;
        }

        // ── Phase B0.5, Shape B: forward conditional jump whose target
        //    range contains a nested for-loop ───────────────────────────
        //
        // Without this recognition, the inner for-loop would still be
        // extracted (by the ForNPrep/ForGPrep arms above), but the Linear
        // segment preceding it would end at pc_of_prep. The conditional's
        // target (past the inner for-loop) would then be treated as a
        // forward-jump-beyond-range by `lift_instruction_range` and emit a
        // spurious `if cond then break end`. We pre-recognize the wrapping
        // if-arm and emit a `Region::InlineIfThenInLoop` that carries the
        // condition PC and a recursively-structured then-body.
        if let Some((jump_next_pc, target)) = forward_conditional_jump(op, pc, insn) {
            // Target must stay within this body range.
            if target > jump_next_pc && target <= body_end {
                // THE ARM MAY END BEFORE THE TARGET.
                //
                // `jump_next_pc..target` is the fall-through side, but an arm
                // that exits early with its OWN unconditional jump to the same
                // place does not own the instructions between that jump and
                // the target. Measured on `533_NPCs::AddOverrideEvent`:
                //
                //     129: JUMPIF     R2 -> 183      `if not quest`
                //     130..138       error(...)
                //     139: JUMP      -> 183          <-- the arm ends HERE
                //     140..182       the rest of the function
                //
                // Taking the arm as 130..183 swallowed 140..182 into it, and
                // the flat lift of the swallowed span stopped at the pc-139
                // jump - so `149: GETTABLEKS R3 R1."StartReqs"` was never
                // lifted and its `JUMPIFNOT` at 151 rendered as a bare `v3`
                // that nothing assigns.
                //
                // Only a jump that leaves the WHOLE arm counts; a nested if's
                // jump targets something inside it.
                //
                // MEASURED (rotation 87), ADOPTED:
                //     clean 1,104 -> 1,105   defects 39 -> 38
                //     fixes 533_NPCs; NO new findings anywhere
                //     11 files changed; calls lost 0, loops lost 0
                //     99_BeequipTypes gains +21 calls, +2 loops, +13 ifs
                //     87_BeeBoosts turns nested ifs into guard clauses and
                //     declares `v3` it previously left bare
                let mut arm_end = target;
                {
                    let mut scan = jump_next_pc;
                    while scan < target && scan < code.len() {
                        let op2 = LuauOpcode::from_u8(insn_op(code[scan]));
                        if matches!(op2, LuauOpcode::Jump) {
                            let t2 = scan as i64 + insn_d(code[scan]) as i64 + 1;
                            // A jump to the target is only an ARM EXIT if what
                            // follows it is unreachable from inside the arm.
                            // An if/else INSIDE the arm also jumps to the arm's
                            // end to skip its else, and truncating there
                            // deletes the rest of the arm - measured on
                            // `555_DapperBearInit`, which lost a whole dialogue
                            // branch and three `table.insert` calls.
                            let after = scan + 1;
                            let reachable_from_arm = cfg
                                .blocks
                                .get(&after)
                                .map(|b| {
                                    b.predecessors
                                        .iter()
                                        .any(|&p| p >= jump_next_pc && p < target)
                                })
                                .unwrap_or(true);
                            if t2 >= target as i64 && !reachable_from_arm {
                                arm_end = scan;
                                break;
                            }
                        }
                        scan += if op2.has_aux() { 2 } else { 1 };
                    }
                }
                // Do not wrap if the range is trivial (no nested for inside).
                // Tested against the TRUE arm: a nested for that lives past the
                // arm's exit is not the arm's, and wrapping for its sake is
                // what swallowed the tail above.
                if range_contains_nested_for(code, jump_next_pc, arm_end) {
                    if linear_start < pc {
                        regions.push(Region::Linear {
                            start: linear_start,
                            end: pc,
                        });
                    }
                    let then_body =
                        structure_numeric_for_body(code, jump_next_pc, arm_end, cfg, loop_headers);
                    regions.push(Region::InlineIfThenInLoop {
                        cond_pc: pc,
                        body: then_body,
                    });
                    // Resume AFTER the arm's exit jump when there was one, so
                    // the tail it used to swallow is structured on its own.
                    pc = if arm_end < target { arm_end + 1 } else { target };
                    linear_start = pc;
                    continue;
                }
            }
        }

        // ── Shape C: an early-exit GUARD ────────────────────────────────
        //
        // `if <cond> then return end` followed by the rest of the function.
        // The arm terminates, so it has no merge and `structure_control_flow`
        // hands the ENTIRE remainder to the else side; the arm is then lifted
        // as one flat span and `lift_instruction_range` stops at the arm's
        // RETURN, losing everything after it.
        //
        // Measured on `603_Orbiter::Update@182` - `RUNREG run=5..61 Linear
        // 5..61`, one region for the whole body, and pcs 17..58 (four of the
        // function's five CALL sites) never emitted:
        //
        //      7: JUMPIFNOT R2 -> 11    `if not self.Root`
        //     10: JUMPIF    R3 -> 17     `or not ...Parent then`
        //     14: CALL      Destroy          self:Destroy(false)
        //     16: RETURN                     return false end
        //     17: ...                    <-- belongs AFTER the guard
        //
        // Same treatment as Shape B, keyed on the arm TERMINATING rather than
        // on it containing a nested for: emit the guard as its own region and
        // continue at the target, so the tail becomes a SIBLING.
        //
        // Appending the tail inside the arm instead is what `LUAU_RET_RESUME`
        // did, and DCE phase 2 correctly deleted it (rotation 92) - the tail
        // has to be structurally outside the `if`, not textually after the
        // `return`.
        // MEASURED (rotation 93), ADOPTED:
        //     clean 1,106 -> 1,123   defects 37 -> 22
        //     total emitted calls across the corpus 38,177 -> 38,501
        //     187 files changed; files losing a loop: 0
        //     KNOWN RESIDUAL, recorded not suppressed: 9 files lose calls and
        //     6 new findings appear in 3 files (381_Eggs, 396_TadpoleTailInit,
        //     351_PassiveTile). See the journal, rotation 93.
        //
        // `LUAU_NO_GUARD_ARM` turns Shape C OFF. Kept as a permanent bisect
        // switch: the residual above is diagnosed by diffing the region lists
        // with it on and off, and rotation 94 wasted two attempts writing
        // predicates from a MODEL of the arm instead of from that diff.
        // `LUAU_GUARD_SKIP_PC=<pc>[,<pc>]` suppresses individual wraps, so the
        // culprit among several in one file can be BISECTED instead of
        // guessed. Rotation 96: three predicates in a row failed to exclude the
        // six wraps in `351_PassiveTile`, so the next step is to find which one
        // matters rather than invent a fourth rule.
        let skip_this = std::env::var("LUAU_GUARD_SKIP_PC")
            .ok()
            .map(|v| v.split(',').any(|x| x.trim().parse::<usize>() == Ok(pc)))
            .unwrap_or(false);

        // DO NOT FRAGMENT A RANGE A LOOP SPANS.
        //
        // Rotation 96 bisected `351_PassiveTile`: suppressing any ONE of its
        // five wraps changes nothing, suppressing all five restores every lost
        // call. No wrap is bad; the SET is. Each wrap ends `pc = target;
        // linear_start = pc`, so five of them split the body into six pieces
        // and a loop spanning several is no longer inside any one region.
        //
        // So the gate is a property of the RANGE, not of the wrap or its arm -
        // which is why the three arm-shape predicates in rotations 94-96 all
        // failed to exclude wraps that were individually blameless.
        let loop_in_rest = loop_headers
            .iter()
            .any(|&h| h > pc && h < body_end);

        if !loop_in_rest && !skip_this && std::env::var("LUAU_NO_GUARD_ARM").is_err() {
            if let Some((jump_next_pc, target)) = forward_conditional_jump(op, pc, insn) {
                if target > jump_next_pc && target <= body_end && arm_terminates(code, jump_next_pc, target)
                {
                    if linear_start < pc {
                        regions.push(Region::Linear { start: linear_start, end: pc });
                    }
                    // Rotation 95: report every wrap. `LUAU_RUNREG_TRACE` only
                    // covers `lift_structured_run`, and Shape C also fires from
                    // recursive calls it never prints - measured on
                    // `351_PassiveTile`, whose region lists are IDENTICAL with
                    // Shape C on and off while its output loses ten calls.
                    if std::env::var("LUAU_GUARD_TRACE").is_ok() {
                        eprintln!(
                            // `code.len()` stands in for proto identity:
                            // this function receives only the code slice, and a
                            // trace without SOME identifier has now misled this
                            // work three times (r47, r86, r92).
                            // `headers` is the count of loop headers the CFG knows about. It
                            // is here because the loop-header gate (below) did NOT fire for
                            // `351_PassiveTile` and the next question is whether that set is
                            // empty for the proto being structured - which would mean `cfg` and
                            // `code` describe different protos.
                            // `blocks` and `maxblk` decide the r97 question: if the CFG's
                            // highest block start lies beyond this proto's code, `cfg` belongs to
                            // a DIFFERENT proto than `code` - they are threaded as separate
                            // parameters and nothing checks they agree.
                            "GUARD	codelen={}	blocks={}	maxblk={}	loops={}	headers={}	cond_pc={}	op={:?}	arm={}..{}	arm_op={:?}	body_end={}",
                            code.len(),
                            cfg.blocks.len(),
                            cfg.blocks.keys().max().copied().unwrap_or(0),
                            cfg.find_loops().len(),
                            loop_headers.len(),
                            pc,
                            op,
                            jump_next_pc,
                            target,
                            LuauOpcode::from_u8(insn_op(code[jump_next_pc])),
                            body_end
                        );
                    }
                    let then_body =
                        structure_numeric_for_body(code, jump_next_pc, target, cfg, loop_headers);
                    regions.push(Region::InlineIfThenInLoop { cond_pc: pc, body: then_body });
                    pc = target;
                    linear_start = pc;
                    continue;
                }
            }
        }

        // Advance by 1 or 2 depending on AUX presence (mirrors the walk in
        // `ControlFlowGraph::build` and `lift_instruction_range`).
        let step = if op.has_aux() { 2 } else { 1 };
        pc += step;
    }

    // Flush any trailing linear range.
    if linear_start < body_end {
        regions.push(Region::Linear {
            start: linear_start,
            end: body_end,
        });
    }

    regions
}

/// Phase B0.5: decode a forward conditional jump into `(next_pc_after_header,
/// target_pc)`, or `None` if `op` at `pc` is not a recognized forward
/// conditional jump.
///
/// `next_pc_after_header` accounts for the AUX word on comparison jumps
/// (JumpIfEq/NotEq/LT/LE/NotLT/NotLE and JumpXEqK*). `JumpIf` / `JumpIfNot`
/// are AD-format with no AUX.
///
/// Returns `None` for backward jumps, unconditional jumps, and non-jump
/// opcodes.
fn forward_conditional_jump(op: LuauOpcode, pc: usize, insn: u32) -> Option<(usize, usize)> {
    let d = insn_d(insn);
    if d <= 0 {
        return None;
    }
    match op {
        LuauOpcode::JumpIf
        | LuauOpcode::JumpIfNot => {
            let target = (pc as i64 + d as i64 + 1) as usize;
            Some((pc + 1, target))
        }
        LuauOpcode::JumpIfEq
        | LuauOpcode::JumpIfNotEq
        | LuauOpcode::JumpIfLE
        | LuauOpcode::JumpIfNotLE
        | LuauOpcode::JumpIfLT
        | LuauOpcode::JumpIfNotLT
        | LuauOpcode::JumpXEqKNil
        | LuauOpcode::JumpXEqKB
        | LuauOpcode::JumpXEqKN
        | LuauOpcode::JumpXEqKS => {
            let target = (pc as i64 + d as i64 + 1) as usize;
            Some((pc + 2, target))
        }
        _ => None,
    }
}

/// Phase B0.5: does `[start, end)` contain a recognized nested for-loop
/// prep-loop pair whose loop end lies within the range?
///
/// Used to decide whether an if-arm should be wrapped in an
/// `InlineIfThenInLoop` region. Only positive cases (nested for-loop found)
/// trigger the wrap — cases without any nested for-loop continue to lift
/// via the Linear segment and the lifter's in-line if-then handling in
/// `lift_instruction_range`.
pub(crate) fn range_contains_nested_for(code: &[u32], start: usize, end: usize) -> bool {
    if end <= start || end > code.len() {
        return false;
    }
    let mut pc = start;
    while pc < end {
        let insn = code[pc];
        let op = LuauOpcode::from_u8(insn_op(insn));
        if matches!(op, LuauOpcode::JumpBack) {
            let back = pc as i64 + insn_d(insn) as i64 + 1;
            if back >= start as i64 && back < pc as i64 { return true; }
        }
        // Nested NumericFor?
        if matches!(op, LuauOpcode::ForNPrep) {
            let d = insn_d(insn);
            let target_i = pc as i64 + d as i64;
            if target_i > pc as i64 && (target_i as usize) < end && (target_i as usize) < code.len() {
                let target_op = LuauOpcode::from_u8(insn_op(code[target_i as usize]));
                if target_op == LuauOpcode::ForNLoop {
                    return true;
                }
            }
        }
        // Nested GenericFor?
        if matches!(
            op,
            LuauOpcode::ForGPrep | LuauOpcode::ForGPrepINext | LuauOpcode::ForGPrepNext
        ) {
            let d = insn_d(insn);
            let target_i = pc as i64 + d as i64 + 1;
            if target_i > pc as i64 && (target_i as usize) < end && (target_i as usize) < code.len() {
                let target_op = LuauOpcode::from_u8(insn_op(code[target_i as usize]));
                if target_op == LuauOpcode::ForGLoop || target_op == LuauOpcode::Deprecated61 {
                    return true;
                }
            }
        }
        let step = if op.has_aux() { 2 } else { 1 };
        pc += step;
    }
    false
}

/// Does `[start, end)` end in a RETURN, making it an early-exit guard arm?
///
/// The LAST instruction specifically: an arm containing a return somewhere in
/// the middle has its own internal structure and is not a guard.
fn arm_terminates(code: &[u32], start: usize, end: usize) -> bool {
    if end <= start || end > code.len() {
        return false;
    }
    // A "straight-line arms only" variant was built and MEASURED WORSE on every
    // axis (rotation 93): 1,120/22 vs 1,123/22, files losing calls 19 vs 9,
    // files losing loops 3 vs 0, total emitted calls +153 vs +324. Structuring
    // is order-dependent - declining to wrap one site lets a worse wrap happen
    // later - so a narrower predicate is not automatically a safer one. Kept
    // permissive; the note is here so it is not re-narrowed on intuition.
    let mut pc = start;
    let mut last = start;
    while pc < end {
        last = pc;
        let op = LuauOpcode::from_u8(insn_op(code[pc]));
        pc += if op.has_aux() { 2 } else { 1 };
    }
    matches!(LuauOpcode::from_u8(insn_op(code[last])), LuauOpcode::Return)
}


/// Does this range hold a complete nested CONDITIONAL - a forward-branching
/// test whose target also lies inside the range?
///
/// The sibling `range_contains_nested_for` exists because an arm lifted as a
/// flat span drops its loops. Arms drop their nested BRANCHES the same way and
/// for the same reason, but only the loop case was ever wired up.
///
/// `533_NPCs::AddEvent`: the then-arm of the `type(...) == "string"` test is
/// blocks 123..209, ONE contiguous range, so `ranges.len() > 1` is false and
/// neither arm trigger in `lift_branch_region` can fire. Inside it the
/// `if data.StartReqs` at pc 176 is emitted with its condition register never
/// assigned, and the pc-139 block that should precede it comes out after.
///
/// Backward targets are excluded: those are loops, already handled.
pub(crate) fn range_contains_nested_cond(code: &[u32], start: usize, end: usize) -> bool {
    if end <= start || end > code.len() {
        return false;
    }
    let mut pc = start;
    while pc < end {
        let insn = code[pc];
        let op = LuauOpcode::from_u8(insn_op(insn));
        let step = if op.has_aux() { 2 } else { 1 };
        if matches!(
            op,
            LuauOpcode::JumpIf
                | LuauOpcode::JumpIfNot
                | LuauOpcode::JumpIfEq
                | LuauOpcode::JumpIfNotEq
                | LuauOpcode::JumpIfNotLE
                | LuauOpcode::JumpIfNotLT
        ) {
            let target = pc as i64 + insn_d(insn) as i64 + 1;
            // STRICTLY inside. A test whose target is exactly `end` is the
            // arm's OWN guard, not a nested branch: structuring the range then
            // re-reads that guard as the region boundary and drops it.
            // Measured - with `<= end`, `1125_Supreme_Star_Amulet_Generator`
            // lost `if 490000000000 <= value2.Honey then` and ran the purchase
            // body unconditionally, which no check in the suite can see.
            if target > pc as i64 && (target as usize) < end {
                return true;
            }
        }
        pc += step;
    }
    false
}

/// Find the last real instruction PC in a basic block (skipping AUX words).
fn last_real_pc(code: &[u32], block: &BasicBlock) -> usize {
    if block.end == 0 || block.end <= block.start {
        return block.start;
    }
    let mut last_pc = block.end - 1;
    if last_pc > block.start {
        let check_pc = last_pc - 1;
        if check_pc < code.len() {
            let check_insn = code[check_pc];
            let check_op = LuauOpcode::from_u8(insn_op(check_insn));
            if check_op.has_aux() {
                last_pc = check_pc;
            }
        }
    }
    last_pc
}
