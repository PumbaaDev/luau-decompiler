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

    /// A short-circuit `and`-chain of guard blocks that together gate a `tail`
    /// the compiler laid out AFTER the fall-through continuation.
    ///
    /// The shape (measured on `lv_0124 ClientStatCache::Update`):
    ///
    /// ```text
    ///   B0: GETUPVAL R2 U0 ; JUMPXEQKNIL U0 == nil  -> M   ; U0 == nil exits
    ///   B1:                  JUMPXEQKNIL arg ~= nil  -> T   ; both set  goes to T
    ///   M : <continuation — the fall-through path>
    ///   ...
    ///   T : <guarded body, ends in RETURN>
    /// ```
    ///
    /// Every guard's "exit" edge lands on the shared `merge` block `M`; its
    /// "advance" edge moves to the next guard, and the last guard's advance edge
    /// reaches `tail` (`T`). Because `T` sits PAST `M` in PC order and is reached
    /// only through the whole chain, the plain `IfThenElse` structurer collapses
    /// each guard to a flat (empty) then-block and orphans `T` as a bare `Linear`
    /// region — which, emitted before the continuation and ending in its own
    /// `return`, makes the live continuation look dead and get truncated by DCE
    /// (`calls_not_emitted`). This variant keeps the guard so the lifter emits
    /// `if <g0 and g1 and ...> then <tail> end` ahead of the continuation.
    GuardedTailIf {
        /// Guard blocks in evaluation order. The bool is `true` when the edge
        /// that ADVANCES the chain toward the tail is the guard's branch (taken)
        /// edge, `false` when it is the fall-through edge — this selects whether
        /// the lifter conjoins the branch's fall-through condition or its
        /// negation.
        guards: Vec<(usize, bool)>,
        /// The guarded tail blocks, ascending by PC. Closed: every exit either
        /// returns or stays inside the set.
        tail: Vec<usize>,
        /// The shared continuation every guard's exit edge lands on. NOT a
        /// member here — it keeps its own region and is lifted exactly once.
        merge: usize,
    },

    /// `if a or b ... then <body> end` where `<body>` ends in RETURN, sits in
    /// PC order BEFORE the continuation, and is reached ONLY from the chain.
    ///
    /// The shapes `find_or_chain_return_guard` cannot take: a link whose test
    /// needs `#x` (`if not mods or #mods <= 0 then return true end`), or a body
    /// that is more than one block (`if not ready or not id then apply(nil)
    /// return end`, where `apply` was inlined with its own early return). The
    /// plain if/else fallback reports the body as the MERGE of the first link,
    /// so the body is emitted after the continuation's own trailing `return`
    /// and dropped as dead, and the second link's jump to the continuation is
    /// out of range and dropped too: the guard vanishes.
    OrGuardIf {
        /// Chain links in evaluation order. The bool is `true` when the edge
        /// to the body is the link's branch (taken) edge.
        links: Vec<(usize, bool)>,
        /// The guarded body blocks, ascending. Closed and acyclic: every exit
        /// stays inside the set or returns.
        body: Vec<usize>,
        /// The continuation the last link falls to. NOT a member.
        cont: usize,
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

        // A short-circuit `and`-chain of guards gating an out-of-line tail must
        // be matched before the plain if/else fallback: that fallback would
        // flatten the inner guard(s) to nothing and orphan the tail as a bare
        // Linear region emitted before the continuation, which DCE then treats
        // as dead after its own return (the `calls_not_emitted` shape on
        // `lv_0124 Update`). See `Region::GuardedTailIf`.
        if block.successors.len() == 2 {
            if let Some((guards, tail, merge)) =
                try_match_guarded_tail_if(cfg, code, block_id, &handled, &loop_headers_all)
            {
                for &(g, _) in &guards {
                    handled.insert(g);
                }
                for &t in &tail {
                    handled.insert(t);
                }
                // `merge` is intentionally left unhandled: it is the region's
                // continuation, not part of it.
                regions.push(Region::GuardedTailIf { guards, tail, merge });
                continue;
            }
        }

        // OR-CHAIN RETURN GUARD.
        //
        // `if a or b or c then return end` compiles to a chain of conditional
        // blocks that all branch to ONE shared terminal RETURN block, the last
        // falling through to the continuation. `find_merge_point` reports that
        // shared terminal as the merge (it is trivially reachable from every
        // arm), so the return block is parked as the "merge", emitted last in
        // RPO — after the function's real trailing `return` — and then dropped
        // as unreachable. The whole guard vanishes. Measured on lv_0919
        // `CheckOcclusion`: the three-raycast occlusion guard was silently gone.
        //
        // Recognising the chain and emitting each link as its own guard
        // (`if a then return end  if b then return end  ...`) is semantically
        // exact and keeps the shared return in every link. Gated tight — a
        // chain of at least two links, every non-entry link a BARE conditional
        // or a sole-predecessor link with a PURE preamble (lifted once, in
        // order), sharing one TERMINAL return block,
        // with a distinct continuation — so it never touches the both-arm-write
        // merge shape (neither arm is directly terminal there) or a genuine
        // two-return if (both arms terminal).
        if block.successors.len() == 2 {
            if let Some((chain, terminal, cont)) =
                find_or_chain_return_guard(cfg, code, block_id, &handled)
            {
                for &bid in &chain {
                    let Some(b) = cfg.blocks.get(&bid) else { continue };
                    // `terminal` is the shared return; its polarity decides which
                    // arm carries it. successors[0] is the fall-through, [1] the
                    // jump target. When the terminal is the jump target we emit
                    // an empty then + return else, and the DCE phase-4 negate/swap
                    // turns `if not c then else return end` into `if c then return
                    // end`. When it is the fall-through we emit it directly.
                    let terminal_is_fallthrough = b.successors.first() == Some(&terminal);
                    let (then_region, else_region) = if terminal_is_fallthrough {
                        (vec![terminal], vec![])
                    } else {
                        (vec![], vec![terminal])
                    };
                    regions.push(Region::IfThenElse {
                        cond_pc: bid,
                        then_region,
                        else_region,
                        // No merge: each link is a self-contained guard whose
                        // continuation is the next sibling region, not a join.
                        merge_pc: None,
                    });
                    handled.insert(bid);
                }
                handled.insert(terminal);
                // `cont` is intentionally left unhandled — the continuation is a
                // sibling region, lifted on its own.
                let _ = cont;
                continue;
            }
        }

        // OR-CHAIN GUARD WITH A COMPUTED LINK OR A MULTI-BLOCK BODY. Runs only
        // when `find_or_chain_return_guard` declined, so every shape it takes
        // is untouched. See `Region::OrGuardIf`.
        if block.successors.len() == 2 {
            if let Some((links, body, cont)) =
                find_or_guard_body(cfg, code, block_id, &handled, &loop_headers_all)
            {
                for &(l, _) in &links {
                    handled.insert(l);
                }
                for &b in &body {
                    handled.insert(b);
                }
                regions.push(Region::OrGuardIf { links, body, cont });
                continue;
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

/// Opcodes that a nil/value guard may compute BEFORE its branch: they only park
/// a value in a register, so folding them into a short-circuit condition is
/// faithful (Lua evaluates the right operand of `and` only when the left held).
/// Deliberately excludes CALL, NAMECALL, every SET*, closure/upvalue capture,
/// table allocation and RETURN — anything with an observable effect.
fn is_pure_guard_setup_op(op: LuauOpcode) -> bool {
    matches!(
        op,
        LuauOpcode::Nop
            | LuauOpcode::LoadNil
            | LuauOpcode::LoadB
            | LuauOpcode::LoadN
            | LuauOpcode::LoadK
            | LuauOpcode::LoadKX
            | LuauOpcode::Move
            | LuauOpcode::GetUpval
            | LuauOpcode::GetGlobal
            | LuauOpcode::GetImport
            | LuauOpcode::GetTable
            | LuauOpcode::GetTableKS
            | LuauOpcode::GetTableN
    )
}

/// A conditional branch whose fall-through condition `extract_branch_condition`
/// can reconstruct. Unconditional `Jump`/`JumpBack` yield no usable predicate.
fn is_conditional_branch_op(op: LuauOpcode) -> bool {
    matches!(
        op,
        LuauOpcode::JumpIf
            | LuauOpcode::JumpIfNot
            | LuauOpcode::JumpIfEq
            | LuauOpcode::JumpIfNotEq
            | LuauOpcode::JumpIfLE
            | LuauOpcode::JumpIfNotLE
            | LuauOpcode::JumpIfLT
            | LuauOpcode::JumpIfNotLT
            | LuauOpcode::JumpXEqKNil
            | LuauOpcode::JumpXEqKB
            | LuauOpcode::JumpXEqKN
            | LuauOpcode::JumpXEqKS
    )
}

/// Is `block` a pure guard — every instruction before its terminator only parks
/// a value, and the terminator is a reconstructable conditional branch?
fn is_pure_guard_block(code: &[u32], block: &BasicBlock) -> bool {
    let branch_pc = match block.branch_pc {
        Some(p) => p,
        None => return false,
    };
    if branch_pc >= code.len() {
        return false;
    }
    let mut i = block.start;
    while i < branch_pc && i < code.len() {
        let op = LuauOpcode::from_u8(insn_op(code[i]));
        if !is_pure_guard_setup_op(op) {
            return false;
        }
        i += if op.has_aux() { 2 } else { 1 };
    }
    is_conditional_branch_op(LuauOpcode::from_u8(insn_op(code[branch_pc])))
}

/// Does `block`'s last instruction return?
fn block_ends_in_return(code: &[u32], block: &BasicBlock) -> bool {
    let mut last = block.start;
    let mut i = block.start;
    while i < block.end && i < code.len() {
        last = i;
        i += if LuauOpcode::from_u8(insn_op(code[i])).has_aux() {
            2
        } else {
            1
        };
    }
    last < code.len() && LuauOpcode::from_u8(insn_op(code[last])) == LuauOpcode::Return
}

/// Detect a short-circuit `and`-chain of guard blocks gating a `tail` that the
/// compiler placed AFTER the shared continuation. See `Region::GuardedTailIf`.
///
/// Returns `(guards, tail_blocks, merge)` on a match. The chain must be at least
/// two guards long — a single guard branching to an out-of-line tail is already
/// structured correctly as `if/else` (both arms present, nothing orphaned); it
/// is only when the FIRST guard's advance edge lands on a SECOND guard that the
/// plain structurer flattens the inner guard to nothing and orphans the tail.
fn try_match_guarded_tail_if(
    cfg: &ControlFlowGraph,
    code: &[u32],
    entry: usize,
    handled: &BTreeSet<usize>,
    loop_headers: &BTreeSet<usize>,
) -> Option<(Vec<(usize, bool)>, Vec<usize>, usize)> {
    const MAX_GUARDS: usize = 4;
    let trace = std::env::var("LUAU_GTI_TRACE").is_ok();
    let eb = cfg.blocks.get(&entry)?;
    if trace {
        eprintln!(
            "GTI entry={} succ={:?} preds={:?}",
            entry, eb.successors, eb.predecessors
        );
    }
    if eb.successors.len() != 2 || handled.contains(&entry) || loop_headers.contains(&entry) {
        return None;
    }

    // The merge `M` is the block every guard's exit edge lands on. It is one of
    // the entry guard's two successors; try each.
    for &m_try in &[eb.successors[1], eb.successors[0]] {
        let m_start = match cfg.blocks.get(&m_try) {
            Some(b) => b.start,
            None => continue,
        };
        let mut guards: Vec<(usize, bool)> = Vec::new();
        let mut cur = entry;
        let mut tail: Option<usize> = None;
        while guards.len() < MAX_GUARDS {
            let b = match cfg.blocks.get(&cur) {
                Some(b) => b,
                None => break,
            };
            // A guard: two successors, precedes the merge, pure test, unclaimed.
            if b.successors.len() != 2
                || b.start >= m_start
                || handled.contains(&cur)
                || loop_headers.contains(&cur)
                || !is_pure_guard_block(code, b)
            {
                break;
            }
            let s0 = b.successors[0]; // fall-through
            let s1 = b.successors[1]; // branch (taken)
            let (advance, advance_is_taken) = if s0 == m_try && s1 != m_try {
                (s1, true)
            } else if s1 == m_try && s0 != m_try {
                (s0, false)
            } else {
                break; // neither, or both, exit to M — not this shape
            };
            guards.push((cur, advance_is_taken));

            // The advance target is another guard (before M) or the tail (after
            // M). Guards precede the merge; the tail sits past it. That PC split
            // is what makes the tail unmistakable and the walk terminating.
            let ab = match cfg.blocks.get(&advance) {
                Some(b) => b,
                None => break,
            };
            let is_next_guard = ab.start < m_start
                && ab.successors.len() == 2
                && !handled.contains(&advance)
                && !loop_headers.contains(&advance)
                && is_pure_guard_block(code, ab)
                && ab.successors.iter().any(|&s| s == m_try);
            if is_next_guard {
                cur = advance;
                continue;
            }
            if ab.start > m_start {
                tail = Some(advance);
            }
            break;
        }

        if trace {
            eprintln!(
                "GTI  m_try={} m_start={} guards={:?} tail={:?}",
                m_try, m_start, guards, tail
            );
        }
        let tail = match tail {
            Some(t) if guards.len() >= 2 => t,
            _ => continue,
        };
        if handled.contains(&tail) || loop_headers.contains(&tail) {
            continue;
        }
        let tb = match cfg.blocks.get(&tail) {
            Some(b) => b,
            None => continue,
        };
        // The tail is reached ONLY through the chain: its sole predecessor is
        // the last guard. Otherwise nesting it under the guard would be unsound.
        let last_guard = guards.last().unwrap().0;
        if trace {
            eprintln!(
                "GTI  tail={} preds={:?} last_guard={}",
                tail, tb.predecessors, last_guard
            );
        }
        if tb.predecessors.len() != 1 || tb.predecessors[0] != last_guard {
            continue;
        }
        // Collect the tail's forward region and require it CLOSED: every block
        // after the merge, none a guard or the merge, no loop header, and every
        // successor stays inside the set (so it cannot leak back into the
        // continuation). A closed acyclic region necessarily ends in RETURN.
        let tail_blocks = collect_region_blocks(cfg, tail, None, handled);
        if tail_blocks.is_empty() {
            continue;
        }
        let tail_set: BTreeSet<usize> = tail_blocks.iter().copied().collect();
        let guard_ids: BTreeSet<usize> = guards.iter().map(|g| g.0).collect();
        let closed = tail_blocks.iter().all(|&t| {
            let blk = match cfg.blocks.get(&t) {
                Some(b) => b,
                None => return false,
            };
            blk.start > m_start
                && t != m_try
                && !guard_ids.contains(&t)
                && !loop_headers.contains(&t)
                && blk.successors.iter().all(|s| tail_set.contains(s))
        });
        if !closed {
            continue;
        }
        // The furthest-PC exit must actually return.
        let has_return_exit = tail_blocks.iter().any(|t| {
            cfg.blocks
                .get(t)
                .map_or(false, |b| b.successors.is_empty() && block_ends_in_return(code, b))
        });
        if !has_return_exit {
            continue;
        }
        // SOUNDNESS NARROWING: accept only a SINGLE-basic-block tail.
        //
        // A multi-block tail carries internal control flow, and re-lifting it
        // out-of-line through `lift_branch_region` can mis-structure that flow
        // and silently drop live calls: measured on `rc_0054 drawSpotlight`,
        // where a branchy tail lost 4 `UDim2.fromOffset` calls (ast 19 -> 15
        // against bc 19) while the plain structurer emitted it correctly. That
        // loss is invisible to the file-level `calls_not_emitted` checker
        // because fastcalled builtins elsewhere in the file inflate its total,
        // so it has to be excluded structurally, not caught after the fact.
        //
        // A single straight-line block has no structuring decision to get wrong,
        // so its lift is lossless by construction — which is exactly the shape
        // that motivated this region: `lv_0124 Update`'s out-of-line tail is the
        // one-block `ClientCall("RetrievePlayerStat", arg2); self:Set(...);
        // return` continuation, while the big guarded body is the `merge` the
        // normal structurer already lifts in full. Widening past one block needs
        // a lossless multi-block tail lift, not a wider match here.
        if tail_blocks.len() != 1 {
            continue;
        }
        return Some((guards, tail_blocks, m_try));
    }
    None
}

/// Is `block_id` a TERMINAL block whose last real instruction is a RETURN?
///
/// Zero successors in the CFG already implies a RETURN (only `Return` leaves a
/// block without successors), but the RETURN opcode is checked explicitly so a
/// malformed empty block never counts.
fn block_is_terminal_return(
    cfg: &ControlFlowGraph,
    code: &[u32],
    block_id: usize,
) -> bool {
    let Some(block) = cfg.blocks.get(&block_id) else { return false };
    if !block.successors.is_empty() {
        return false;
    }
    match block.branch_pc {
        Some(pc) if pc < code.len() => {
            matches!(LuauOpcode::from_u8(insn_op(code[pc])), LuauOpcode::Return)
        }
        _ => false,
    }
}

/// Is `block_id` a BARE conditional block — one whose only real instruction is
/// its two-way conditional branch (no preamble to lift, so no side effect can
/// be lost when it is emitted as a guard condition)?
fn block_is_bare_conditional(cfg: &ControlFlowGraph, block_id: usize) -> bool {
    let Some(block) = cfg.blocks.get(&block_id) else { return false };
    block.successors.len() == 2 && block.branch_pc == Some(block.start)
}

/// Is `block_id` an interior OR-chain link whose test needs a PURE preamble —
/// e.g. `if item == nil or not t[item.id] then ... return end`, where the second
/// link computes `t[item.id]` (GETTABLEKS + GETTABLE) before its branch?
///
/// Same whitelist `is_pure_guard_block` already applies to `GuardedTailIf`'s
/// interior guards: only value parks (no CALL/NAMECALL/SET*/closure). The link is
/// still lifted ONCE, in program order, after the previous link's guard has
/// returned, so the preamble runs exactly when the bytecode runs it (only when
/// every earlier link fell through) and nothing is duplicated or hoisted.
///
/// Additionally the link must be reached ONLY from the previous link: a block
/// with a preamble that some other edge also enters is a join, and emitting it
/// as a sibling guard after the previous one would misplace that edge. A bare
/// link keeps its original (unrestricted) acceptance via
/// `block_is_bare_conditional`, so every chain that matched before still does.
///
/// Measured shape (AdoptMe `ToppingsHelper`-style, reproduced from canonical
/// `luau-compile -O1` of the source above): without this the chain stops at one
/// link, the plain if/else structurer turns the guard into
/// `if item ~= nil then local x = t[item.id] end return true`, and the shared
/// `hint(...); return false` body is emitted dead after `return true` and removed
/// by DCE — the `calls_not_emitted` class.
fn is_pure_preamble_chain_link(
    cfg: &ControlFlowGraph,
    code: &[u32],
    block_id: usize,
    prev_link: usize,
) -> bool {
    let Some(block) = cfg.blocks.get(&block_id) else { return false };
    block.successors.len() == 2
        && block.predecessors.len() == 1
        && block.predecessors[0] == prev_link
        && is_pure_or_link_block(code, block)
}

/// Recognise an `if a or b or c then return end` chain rooted at `entry`.
///
/// Shape: a chain of conditional blocks `[entry, B1, .., Bk]` (k >= 1, so at
/// least two links total) where every link has ONE shared TERMINAL return block
/// `T` as a successor, each link's OTHER successor is the next link, and the
/// last link's other successor is the continuation `C` (which is not `T`). Every
/// link after `entry` must be a BARE conditional, or a single-predecessor link
/// whose preamble is pure value setup (`is_pure_preamble_chain_link`); the entry
/// may carry any preamble, e.g. the calls whose results the chain tests. No link
/// may already be handled.
///
/// Returns `(chain, terminal, continuation)`. The narrow gate — exactly one
/// terminal successor per link, chain length >= 2, bare interior links — keeps
/// it clear of the both-arm-write merge (neither arm terminal) and the genuine
/// two-return `if` (both arms terminal).
fn find_or_chain_return_guard(
    cfg: &ControlFlowGraph,
    code: &[u32],
    entry: usize,
    handled: &BTreeSet<usize>,
) -> Option<(Vec<usize>, usize, usize)> {
    // A chain that needs a pure-preamble link must own the terminal. When another
    // edge (the ladder's last fall-through) also reaches it, retry with bare links
    // only: that is the chain an entry reached by several edges has always got, and
    // the rest of the ladder then starts its own chain (StealAnEgg
    // FullscreenPresentation `Open`).
    or_chain_return_guard_with(cfg, code, entry, handled, true)
        .or_else(|| or_chain_return_guard_with(cfg, code, entry, handled, false))
}

fn or_chain_return_guard_with(
    cfg: &ControlFlowGraph,
    code: &[u32],
    entry: usize,
    handled: &BTreeSet<usize>,
    allow_pure_links: bool,
) -> Option<(Vec<usize>, usize, usize)> {
    if handled.contains(&entry) {
        return None;
    }
    let eb = cfg.blocks.get(&entry)?;
    if eb.successors.len() != 2 {
        return None;
    }
    // Exactly one of the entry's successors must be a terminal return block —
    // that is the shared guard body `T`.
    let s0 = eb.successors[0];
    let s1 = eb.successors[1];
    let t0 = block_is_terminal_return(cfg, code, s0);
    let t1 = block_is_terminal_return(cfg, code, s1);
    let terminal = match (t0, t1) {
        (true, false) => s0,
        (false, true) => s1,
        _ => return None, // both or neither terminal: not this shape
    };
    let mut chain = vec![entry];
    let mut cur = if terminal == s0 { s1 } else { s0 };
    let mut used_pure_link = false;
    // Walk the chain. Each interior link must be a bare conditional that also
    // branches to the same `T`; its other successor is the next link.
    loop {
        if handled.contains(&cur) || chain.contains(&cur) {
            break;
        }
        let is_bare = block_is_bare_conditional(cfg, cur);
        if !is_bare {
            // A pure-preamble link is accepted only behind a STRAIGHT-LINE
            // entry (at most one predecessor). An entry that is itself a join
            // tests a register whose value differs per incoming edge, and the
            // register file at a join does not always carry that merge: on
            // StealAnEgg `ContentCreatorsAdminPanel` (`for .. do if id == v then
            // server = s break end end; if not server or not server.Enabled
            // then notice(..) return end`) the found-in-loop value is lost, the
            // entry tests the fall-through `LOADNIL` and folds to `not nil`, and
            // the guard would become an unconditional `notice(..); return` that
            // kills the whole function. Bare-link chains keep their original
            // (unrestricted) acceptance, so nothing that matched before changes.
            if !allow_pure_links
                || eb.predecessors.len() > 1
                || !is_pure_preamble_chain_link(cfg, code, cur, *chain.last()?)
            {
                break;
            }
        }
        let cb = cfg.blocks.get(&cur)?;
        let cs0 = cb.successors[0];
        let cs1 = cb.successors[1];
        let next = if cs0 == terminal {
            cs1
        } else if cs1 == terminal {
            cs0
        } else {
            // This link does not target the shared return — the chain ended at
            // the previous link, and `cur` is the continuation.
            break;
        };
        used_pure_link |= !is_bare;
        chain.push(cur);
        cur = next;
    }
    // `cur` is the continuation `C`. Require a real chain (>= 2 links) and a
    // continuation distinct from the terminal body.
    if chain.len() < 2 || cur == terminal || chain.contains(&cur) {
        return None;
    }
    // A chain that relies on a pure-preamble link must OWN the terminal: every
    // edge into `T` comes from a chain link. `T` is claimed (handled) by this
    // region, so an edge from anywhere else would lose its target. That happens
    // when the walk stops at a link it cannot accept but which still branches to
    // `T` — AdoptMe `DropBabyHelper.refresh_button_visibility` (`if not a or b ==
    // nil or #b == 0 then hide() return end show()`): the third link's LENGTH
    // preamble is not whitelisted, so it became the continuation and its
    // `#b == 0 -> hide()` arm silently vanished. Bare-only chains are unchanged.
    if used_pure_link {
        let owns_terminal = cfg
            .blocks
            .get(&terminal)
            .map_or(false, |t| t.predecessors.iter().all(|p| chain.contains(p)));
        if !owns_terminal {
            return None;
        }
    }
    // THE TERMINAL MUST NOT BE THE FUNCTION'S OWN LAST BLOCK.
    //
    // This is the line between the bug and a correctly-structured guard. When
    // the shared terminal is an EARLY return that sits before more code (a real
    // trailing `return`, a loop, another guard), RPO emits it last — after that
    // later code's own return — where it is unreachable and dropped, and the
    // whole guard vanishes (lv_0919: `if a or b or c then return false end`
    // before the occlusion loop). When the terminal IS the function's natural
    // end (an `if a and b then body end` whose falsy arms fall through to the
    // trailing `return nil`), emitting it last is correct and the generic
    // if/else already nests it properly — reshaping it here would DROP those
    // nil-guards (measured on lv_0463 `find`). So fire only when a block starts
    // AFTER the terminal, i.e. the terminal is stranded rather than final.
    if !cfg.blocks.keys().any(|&b| b > terminal) {
        return None;
    }
    Some((chain, terminal, cur))
}

/// Does `block` end in a `for` loop's own back edge (FORNLOOP / FORGLOOP)?
fn block_ends_in_for_loop_back_edge(code: &[u32], block: &BasicBlock) -> bool {
    let Some(pc) = block.branch_pc else { return false };
    pc < code.len()
        && matches!(
            LuauOpcode::from_u8(insn_op(code[pc])),
            LuauOpcode::ForNLoop | LuauOpcode::ForGLoop
        )
}

/// Is `b` the body-start block of a counted or generic `for` loop?
///
/// The loop header the CFG reports for a `for` is its body start, entered from
/// the FORNPREP/FORGPREP block above and from the FOR*LOOP back edge. Such a
/// header is part of a closed region as long as the back edge is the loop's
/// own (see `block_ends_in_for_loop_back_edge`).
fn is_for_loop_body_head(cfg: &ControlFlowGraph, code: &[u32], b: usize) -> bool {
    let Some(blk) = cfg.blocks.get(&b) else { return false };
    let has_prep_pred = blk.predecessors.iter().any(|p| {
        cfg.blocks.get(p).map_or(false, |pb| {
            pb.branch_pc.map_or(false, |pc| {
                pc < code.len()
                    && matches!(
                        LuauOpcode::from_u8(insn_op(code[pc])),
                        LuauOpcode::ForNPrep | LuauOpcode::ForGPrep
                    )
            })
        })
    });
    let has_loop_pred = blk.predecessors.iter().any(|p| {
        *p >= b
            && cfg
                .blocks
                .get(p)
                .map_or(false, |pb| block_ends_in_for_loop_back_edge(code, pb))
    });
    has_prep_pred && has_loop_pred
}

/// Is `b` entered only from a `for` loop's prep block and its own back edge
/// (FORNPREP / FORGPREP and FORNLOOP / FORGLOOP), in any pc order?
fn is_for_loop_entry_join(cfg: &ControlFlowGraph, code: &[u32], b: usize) -> bool {
    let Some(blk) = cfg.blocks.get(&b) else { return false };
    let is_prep = |p: &usize| {
        cfg.blocks.get(p).map_or(false, |pb| {
            pb.branch_pc.map_or(false, |pc| {
                pc < code.len()
                    && matches!(
                        LuauOpcode::from_u8(insn_op(code[pc])),
                        LuauOpcode::ForNPrep | LuauOpcode::ForGPrep
                    )
            })
        })
    };
    let is_back = |p: &usize| {
        cfg.blocks.get(p).map_or(false, |pb| block_ends_in_for_loop_back_edge(code, pb))
    };
    blk.predecessors.iter().any(is_prep)
        && blk.predecessors.iter().all(|p| is_prep(p) || is_back(p))
}

/// Do two or more predecessors of `entry` each write the register `entry`'s
/// branch tests, one of them through a multi-edge default-write block?
///
/// Then the tested value is a JOIN of per-path writes (`a and a:IsA("T") and
/// a.Value` parked into one register), which the guard lifter cannot see: the
/// register reads as `nil`, the guard is constant true, and its body swallows
/// the continuation (SAE EnchantedPond overWater).
fn tested_reg_is_value_join(cfg: &ControlFlowGraph, code: &[u32], entry: usize) -> bool {
    let Some(eb) = cfg.blocks.get(&entry) else { return true };
    let Some(bpc) = eb.branch_pc else { return true };
    if bpc >= code.len() {
        return true;
    }
    let reg = insn_a(code[bpc]);
    let writers: Vec<usize> = eb.predecessors.iter().copied().filter(|p| {
        let Some(pb) = cfg.blocks.get(p) else { return true };
        let mut i = pb.start;
        while i < pb.end.min(code.len()) {
            let op = LuauOpcode::from_u8(insn_op(code[i]));
            if insn_a(code[i]) == reg
                && matches!(
                    op,
                    LuauOpcode::LoadNil
                        | LuauOpcode::LoadB
                        | LuauOpcode::LoadN
                        | LuauOpcode::LoadK
                        | LuauOpcode::Move
                        | LuauOpcode::GetUpval
                        | LuauOpcode::GetGlobal
                        | LuauOpcode::GetImport
                        | LuauOpcode::GetTable
                        | LuauOpcode::GetTableKS
                        | LuauOpcode::GetTableN
                        | LuauOpcode::NameCall
                        | LuauOpcode::Call
                        | LuauOpcode::NewTable
                        | LuauOpcode::DupTable
                        | LuauOpcode::NewClosure
                        | LuauOpcode::Not
                        | LuauOpcode::Minus
                        | LuauOpcode::Length
                )
            {
                return true;
            }
            i += if op.has_aux() { 2 } else { 1 };
        }
        false
    }).collect();
    // A single `if/else` diamond writing the register is materialised as a
    // named local by the lifter (SAE ManorGateTimer `Hitbox2`) and the guard
    // reads it fine. A CHAIN (`a and a:IsA(T) and a.Value`: several tests
    // jumping to one default-write block) is not.
    writers.len() >= 2
        && writers
            .iter()
            .any(|w| cfg.blocks.get(w).map_or(true, |b| b.predecessors.len() >= 2))
}

/// A non-entry OR-guard link whose only effect beyond pure value parking is
/// ONE call whose result is the very register the link's branch tests
/// (`x:IsA("T")` in `if not (a and a:IsA("T")) then ... end`).
///
/// The call runs only when every earlier link fell through, exactly when the
/// bytecode reaches it, so folding it into the `or` operand keeps both the
/// order and the count of evaluations. The lifter folds the call into the
/// condition (see `fold_preamble_into_cond`); a link it cannot fold splits the
/// chain, which lifts the body once per group.
fn is_call_link_block(code: &[u32], block: &BasicBlock) -> bool {
    let Some(branch_pc) = block.branch_pc else { return false };
    if branch_pc >= code.len() {
        return false;
    }
    let bop = LuauOpcode::from_u8(insn_op(code[branch_pc]));
    if !matches!(bop, LuauOpcode::JumpIf | LuauOpcode::JumpIfNot) {
        return false;
    }
    let tested = insn_a(code[branch_pc]) as usize;
    let mut calls = 0usize;
    let mut last_call_result = None;
    let mut i = block.start;
    while i < branch_pc && i < code.len() {
        let op = LuauOpcode::from_u8(insn_op(code[i]));
        match op {
            LuauOpcode::NameCall => {}
            LuauOpcode::Call => {
                calls += 1;
                // C == 2: exactly one result.
                if insn_c(code[i]) != 2 {
                    return false;
                }
                last_call_result = Some(insn_a(code[i]) as usize);
            }
            _ if is_pure_guard_setup_op(op) => {}
            _ => return false,
        }
        i += if op.has_aux() { 2 } else { 1 };
    }
    calls == 1 && last_call_result == Some(tested)
}

/// A non-entry OR-guard link: a pure guard block (`is_pure_guard_block`) whose
/// preamble may also take a length (`#x`). LENGTH only parks a value like the
/// table reads already whitelisted; the link is folded into the `or` condition,
/// which evaluates it exactly when every earlier link fell through, as the
/// bytecode does.
fn is_pure_or_link_block(code: &[u32], block: &BasicBlock) -> bool {
    let Some(branch_pc) = block.branch_pc else { return false };
    if branch_pc >= code.len() {
        return false;
    }
    let mut i = block.start;
    while i < branch_pc && i < code.len() {
        let op = LuauOpcode::from_u8(insn_op(code[i]));
        if !(is_pure_guard_setup_op(op) || op == LuauOpcode::Length) {
            return false;
        }
        i += if op.has_aux() { 2 } else { 1 };
    }
    is_conditional_branch_op(LuauOpcode::from_u8(insn_op(code[branch_pc])))
}

/// Upper bounds that keep `find_or_guard_body` a local pattern, not a search.
const OR_GUARD_MAX_LINKS: usize = 8;
const OR_GUARD_MAX_BODY_BLOCKS: usize = 16;

/// Match `Region::OrGuardIf` at `entry`: returns `(links, body, cont)`.
///
/// Gate, all required:
/// * `entry` is a conditional block (it may be a loop-body head or a join);
/// * every later link is a pure (`is_pure_or_link_block`) conditional whose
///   ONLY predecessor is the previous link, and one of its successors is the
///   shared body head `T`; at least two links;
/// * every edge into `T` comes from a link;
/// * the body (everything reachable from `T`) is closed, unclaimed, entered
///   only through `T`, and every exit returns; its only back edges are `for`
///   loops' own FORNLOOP / FORGLOOP edges;
/// * the body lies BEFORE the continuation in PC order (stranded). A body
///   after the continuation is the natural tail the if/else fallback already
///   nests correctly.
fn find_or_guard_body(
    cfg: &ControlFlowGraph,
    code: &[u32],
    entry: usize,
    handled: &BTreeSet<usize>,
    loop_headers: &BTreeSet<usize>,
) -> Option<(Vec<(usize, bool)>, Vec<usize>, usize)> {
    let eb = cfg.blocks.get(&entry)?;
    if eb.successors.len() != 2
        || handled.contains(&entry)
        || loop_headers.contains(&entry)
    {
        return None;
    }
    // A join entry (several predecessors that are not a `for` body head) tests
    // a register fed by a value join the lifter cannot see here: it reads as
    // `nil`, the guard is constant true, and its body swallows the
    // continuation (SAE EnchantedPond overWater). Leave those to the general
    // structurer.
    let join_entry = eb.predecessors.len() > 1
        && !is_for_loop_entry_join(cfg, code, entry)
        && tested_reg_is_value_join(cfg, code, entry);
    for &head in &[eb.successors[1], eb.successors[0]] {
        if let Some(found) = or_guard_with_head(cfg, code, entry, head, handled, loop_headers) {
            if join_entry {
                if std::env::var("LUAU_ORGUARD_TRACE").is_ok() {
                    eprintln!(
                        "ORGUARD-JOINDECLINE entry={} len={} links={:?} body={:?} cont={}",
                        entry, code.len(), found.0, found.1, found.2
                    );
                }
                return None;
            }
            return Some(found);
        }
    }
    None
}

/// `find_or_guard_body` for one choice of body head.
fn or_guard_with_head(
    cfg: &ControlFlowGraph,
    code: &[u32],
    entry: usize,
    head: usize,
    handled: &BTreeSet<usize>,
    loop_headers: &BTreeSet<usize>,
) -> Option<(Vec<(usize, bool)>, Vec<usize>, usize)> {
    let eb = cfg.blocks.get(&entry)?;
    let other = |succ: &[usize]| if succ[0] == head { succ[1] } else { succ[0] };
    let mut links = vec![(entry, eb.successors[1] == head)];
    let mut cur = other(&eb.successors);
    loop {
        let prev = links.last()?.0;
        let Some(cb) = cfg.blocks.get(&cur) else { break };
        let is_link = links.len() < OR_GUARD_MAX_LINKS
            && cb.successors.len() == 2
            && cb.successors.contains(&head)
            && cb.successors[0] != cb.successors[1]
            && cb.predecessors.len() == 1
            && cb.predecessors[0] == prev
            && !handled.contains(&cur)
            && !loop_headers.contains(&cur)
            && (is_pure_guard_block(code, cb) || is_call_link_block(code, cb));
        if !is_link {
            break;
        }
        links.push((cur, cb.successors[1] == head));
        cur = other(&cb.successors);
    }
    let cont = cur;
    let link_ids: BTreeSet<usize> = links.iter().map(|l| l.0).collect();
    if links.len() < 2 || cont <= head || link_ids.contains(&cont) {
        return None;
    }
    let hb = cfg.blocks.get(&head)?;
    if hb.predecessors.is_empty() || !hb.predecessors.iter().all(|p| link_ids.contains(p)) {
        return None;
    }
    let body = closed_return_body(cfg, code, head, cont, &link_ids, handled, loop_headers)?;
    if std::env::var("LUAU_ORGUARD_TRACE").is_ok() {
        let conds = body
            .iter()
            .filter(|b| cfg.blocks.get(b).map_or(false, |x| x.successors.len() == 2))
            .count();
        eprintln!(
            "ORGUARD entry={} links={:?} body={:?} conds={} cont={}",
            entry, links, body, conds, cont
        );
    }
    Some((links, body, cont))
}

/// Everything reachable from `head`, if it is a closed, forward-only,
/// unclaimed region entered only through `head` whose every exit returns.
fn closed_return_body(
    cfg: &ControlFlowGraph,
    code: &[u32],
    head: usize,
    cont: usize,
    link_ids: &BTreeSet<usize>,
    handled: &BTreeSet<usize>,
    loop_headers: &BTreeSet<usize>,
) -> Option<Vec<usize>> {
    let mut body: BTreeSet<usize> = BTreeSet::new();
    let mut work = vec![head];
    while let Some(b) = work.pop() {
        if !body.insert(b) {
            continue;
        }
        if body.len() > OR_GUARD_MAX_BODY_BLOCKS
            || b == cont
            || link_ids.contains(&b)
            || handled.contains(&b)
            || (loop_headers.contains(&b) && !is_for_loop_body_head(cfg, code, b))
        {
            return None;
        }
        let blk = cfg.blocks.get(&b)?;
        if blk.successors.is_empty() {
            if !block_ends_in_return(code, blk) {
                return None;
            }
            continue;
        }
        // A backward edge is allowed only as a counted/generic `for` loop's own
        // back edge (FORNLOOP / FORGLOOP): the body then stays closed, because
        // the loop's single exit is its fall-through successor.
        if blk.successors.iter().any(|&s| s <= b) && !block_ends_in_for_loop_back_edge(code, blk) {
            return None; // a backward edge: not a forward-only body
        }
        work.extend(blk.successors.iter().copied());
    }
    let entered_only_at_head = body.iter().filter(|&&b| b != head).all(|b| {
        cfg.blocks
            .get(b)
            .map_or(false, |blk| blk.predecessors.iter().all(|p| body.contains(p)))
    });
    if !entered_only_at_head {
        return None;
    }
    Some(body.into_iter().collect())
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

/// Longest loop (in instructions) Shape C0 claims. Every ordinary loop measured on
/// the 25k-file gate is under 330; the only ones past 3,000 are the dispatch loops
/// of obfuscated bytecode VMs (hundreds of arms), where claiming the loop at its
/// header makes the arms' pending-register state leak across joins and surfaces a
/// `declared_never_assigned` that the old double-lift hid. Those keep the old path.
const MAX_C0_LOOP_SPAN: usize = 2000;

/// The last JUMPBACK in `(pc, end)` whose target is exactly `pc`, if any.
fn back_edge_latch_to(code: &[u32], pc: usize, end: usize) -> Option<usize> {
    let end = end.min(code.len());
    let mut w = pc + 1;
    let mut found = None;
    while w < end {
        let op = LuauOpcode::from_u8(insn_op(code[w]));
        if matches!(op, LuauOpcode::JumpBack) && w as i64 + insn_d(code[w]) as i64 + 1 == pc as i64 {
            found = Some(w);
        }
        w += if op.has_aux() { 2 } else { 1 };
    }
    found
}

/// Does `[start, end)` hold a complete numeric or generic for prep->loop pair?
/// (`range_contains_nested_for` also answers true for a bare back edge.)
fn range_has_for_pair(code: &[u32], start: usize, end: usize) -> bool {
    let end = end.min(code.len());
    let mut pc = start;
    while pc < end {
        let op = LuauOpcode::from_u8(insn_op(code[pc]));
        let d = insn_d(code[pc]) as i64;
        let (target, loop_ops): (i64, &[LuauOpcode]) = match op {
            LuauOpcode::ForNPrep => (pc as i64 + d, &[LuauOpcode::ForNLoop]),
            LuauOpcode::ForGPrep | LuauOpcode::ForGPrepINext | LuauOpcode::ForGPrepNext => {
                (pc as i64 + d + 1, &[LuauOpcode::ForGLoop, LuauOpcode::Deprecated61])
            }
            _ => (-1, &[]),
        };
        if target > pc as i64 && (target as usize) < end {
            let top = LuauOpcode::from_u8(insn_op(code[target as usize]));
            if loop_ops.contains(&top) {
                return true;
            }
        }
        pc += if op.has_aux() { 2 } else { 1 };
    }
    false
}

/// Does `[start, end)` hold a complete GENERIC for pair and no numeric for at
/// all? Obfuscated VM dispatch loops are built from numeric fors with dozens of
/// forward JUMPs; the nested-generic-for repairs must not restructure them (the
/// structured copy adopts bindings of the peeled first iteration).
pub(crate) fn range_is_plain_generic_for(code: &[u32], start: usize, end: usize) -> bool {
    let end = end.min(code.len());
    let mut pc = start;
    let mut generic = false;
    while pc < end {
        let op = LuauOpcode::from_u8(insn_op(code[pc]));
        if matches!(op, LuauOpcode::ForNPrep | LuauOpcode::ForNLoop) {
            return false;
        }
        pc += if op.has_aux() { 2 } else { 1 };
    }
    pc = start;
    while pc < end {
        let op = LuauOpcode::from_u8(insn_op(code[pc]));
        if matches!(
            op,
            LuauOpcode::ForGPrep | LuauOpcode::ForGPrepINext | LuauOpcode::ForGPrepNext
        ) {
            generic = true;
        }
        pc += if op.has_aux() { 2 } else { 1 };
    }
    generic && range_has_for_pair(code, start, end)
}

/// Does the generic for sit at the very top of the loop: the first FORGPREP is
/// within a few instructions of the header and nothing before it branches? Shape
/// C0 targets `while true do for .. end; ..` where the for opens the body. A loop
/// whose for follows conditional code (a `continue` skipping the exit test, a
/// guard that decides whether the for runs) is not claimed: its flat lift keeps
/// the tail tests and yields that C0's condition-free rebuild loses.
fn for_prep_opens_loop_body(code: &[u32], header: usize, latch: usize) -> bool {
    const MAX_PREFIX: usize = 12;
    let end = latch.min(code.len());
    let mut w = header;
    while w < end && w <= header + MAX_PREFIX {
        let op = LuauOpcode::from_u8(insn_op(code[w]));
        if matches!(
            op,
            LuauOpcode::ForGPrep | LuauOpcode::ForGPrepINext | LuauOpcode::ForGPrepNext
        ) {
            return true;
        }
        if forward_conditional_jump(op, w, code[w]).is_some() {
            return false;
        }
        w += if op.has_aux() { 2 } else { 1 };
    }
    false
}

/// Is the instruction run just before `latch` a forward conditional jump to
/// `latch + 1` (a `repeat .. until c` / `while c` tail test whose back edge is the
/// fall-through)? Shape C0 targets `if done then break end; step()`, where a
/// statement sits between the exit test and the back edge.
fn has_tail_exit_guard(code: &[u32], latch: usize) -> bool {
    (latch.saturating_sub(2)..latch).any(|w| {
        let op = LuauOpcode::from_u8(insn_op(code[w]));
        forward_conditional_jump(op, w, code[w])
            .map(|(next_pc, t)| next_pc == latch && t == latch + 1)
            .unwrap_or(false)
    })
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

        // ── Shape C0: a `while true` loop whose header is reached BEFORE a
        //    nested for-loop that sits directly in its body ─────────────────
        //
        // Shape C below keys off the JUMPBACK latch (or a header guard), so for
        // `while true do for .. end; if done then break end; step() end` the
        // scan met the nested for first, consumed it as a GenericFor region,
        // and only then reached the latch: the loop's header was already behind
        // `linear_start`, so the whole body (for included) was lifted a second
        // time as flat code (loop variable never bound). Claim the loop here,
        // at its header, as a condition-free loop: its body is structured
        // recursively and the for is found there. A header that is itself the
        // guard (`while c do`) is left to Shape C.
        let c0_latch = if std::env::var("LUAU_NO_NFG_C").is_ok() {
            None
        } else {
            back_edge_latch_to(code, pc, body_end)
        };
        if let Some(latch_pc) = c0_latch {
            let guard_at_header = forward_conditional_jump(op, pc, insn)
                .map(|(next_pc, t)| t > next_pc && t == latch_pc + 1)
                .unwrap_or(false);
            if !guard_at_header
                && latch_pc - pc <= MAX_C0_LOOP_SPAN
                && range_is_plain_generic_for(code, pc, latch_pc)
                && !has_tail_exit_guard(code, latch_pc)
                && for_prep_opens_loop_body(code, pc, latch_pc)
            {
                if std::env::var("LUAU_NFG_TRACE").is_ok() {
                    eprintln!("NFG_C0 header={} latch={} span={}", pc, latch_pc, latch_pc - pc);
                }
                if linear_start < pc {
                    regions.push(Region::Linear { start: linear_start, end: pc });
                }
                let nested_body = structure_numeric_for_body(code, pc, latch_pc, cfg, loop_headers);
                regions.push(Region::InlineLoopInLoop {
                    header_start: pc,
                    cond_pc: None,
                    body_start: pc,
                    latch_pc,
                    body: nested_body,
                });
                pc = latch_pc + 1;
                linear_start = pc;
                continue;
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
                        // Arms the flat lift mangles (numeric loop, several loops, or
                        // a branch outside a loop body) would be newly emitted
                        // half-lifted (HazardView `v23`, LemonSignal `v10`, a
                        // duplicated ClickToMove block), so keep the old split.
                        let nested = arm_needs_old_split(
                            code, cfg, then_blocks.iter().chain(else_blocks.iter()));
                        let overwrites = arm_overwrites_prefix_value(
                            code, cfg, cond_id, pc, merge, &then_blocks, &else_blocks);
                        let cut = if (nested || overwrites)
                            && !prefix_has_callfb_chain(code, cfg, cond_id, pc) { pc }
                            else { cond_prefix_cut(code, cfg, cond_id, pc) };
                        if linear_start < cut { regions.push(Region::Linear { start: linear_start, end: cut }); }
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
pub(crate) fn arm_terminates(code: &[u32], start: usize, end: usize) -> bool {
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

/// True when the condition block's prefix (`block.start..pc`) holds an
/// `a and f()` chain recognised only because its last operand is a CALLFB pair.
/// The CFG keeps such a chain whole, so the prefix can run far back (getIfCanUse,
/// GiveItem). The old double-lift split would emit that prefix, calls and all,
/// twice; there the prefix must be lifted once, whatever the arms look like.
fn prefix_has_callfb_chain(
    code: &[u32],
    cfg: &ControlFlowGraph,
    cond_id: usize,
    pc: usize,
) -> bool {
    let start = match cfg.blocks.get(&cond_id) {
        Some(b) => b.start.min(pc),
        None => return false,
    };
    let mut w = start;
    while w < pc.min(code.len()) {
        let op = LuauOpcode::from_u8(insn_op(code[w]));
        if matches!(op, LuauOpcode::JumpIf | LuauOpcode::JumpIfNot)
            && crate::analysis::bool_idiom::recognize_or_and_chain(code, w)
                .is_some_and(|c| c.has_callfb_operand)
        {
            return true;
        }
        w += if op.has_aux() { 2 } else { 1 };
    }
    false
}

/// Where the `Linear` region before a conditional-wrapping-a-loop must stop.
///
/// `Region::IfThenElse` lifts its own condition block (`block.start..branch`),
/// so a `Linear` that runs up to the branch pc lifts that prefix a SECOND time.
/// When the prefix contains a call, the first lift consumes the call's argument
/// registers and the second finds them `Unknown` and mints undeclared `vN`
/// temporaries (`EasyVisuals.new(...)(v6, v7, ...)`). Cutting the `Linear` at the
/// condition block's start lifts the prefix once.
///
/// Limited to prefixes that hold a call: for call-free prefixes the double lift
/// has no side effects and the long-standing output is kept byte-for-byte.
pub(crate) fn cond_prefix_cut(
    code: &[u32],
    cfg: &ControlFlowGraph,
    cond_id: usize,
    pc: usize,
) -> usize {
    let start = match cfg.blocks.get(&cond_id) {
        Some(b) => b.start.min(pc),
        None => return pc,
    };
    let has_call = code.get(start..pc.min(code.len())).is_some_and(|w| {
        w.iter().any(|&i| {
            matches!(
                LuauOpcode::from_u8(insn_op(i)),
                LuauOpcode::Call | LuauOpcode::CallFb
            )
        })
    });
    if has_call { start } else { pc }
}

/// True when a register is written by the condition-block prefix AND by an arm,
/// and the first touch after the merge READS it (`local c = base; if g then
/// for .. do c = v.Color break end end; return c`, measured on
/// DrScrambleTradeInController `themeForRow`).
///
/// Cutting the `Linear` at the block start lifts the prefix once, which leaves
/// the register as an already-declared local; `premateralize_branch_escapes_spans`
/// excludes such registers, so the arm write is parked and annihilated at the
/// join and the post-merge read silently sees the pre-branch value. The old
/// split (double lift) re-parks the prefix value, which that pass pins, so the
/// write survives. Such shapes keep the old split.
fn arm_overwrites_prefix_value<'a>(
    code: &[u32],
    cfg: &ControlFlowGraph,
    cond_id: usize,
    pc: usize,
    merge: usize,
    then_blocks: &'a [usize],
    else_blocks: &'a [usize],
) -> bool {
    use crate::decompiler::lifter::{
        collect_body_writes, merge_first_touch_is_read, reg_read_before_write,
    };
    let Some(start) = cfg.blocks.get(&cond_id).map(|b| b.start.min(pc)) else {
        return false;
    };
    // Only a value written BEFORE the prefix's last call is flushed into a
    // declared local by that call; a value written after it stays a parked
    // expression the premat pass pins (Recorder `ClassPropsByName = nil`).
    let Some(last_call) = (start..pc.min(code.len())).rev().find(|&i| {
        matches!(LuauOpcode::from_u8(insn_op(code[i])), LuauOpcode::Call | LuauOpcode::CallFb)
    }) else {
        return false;
    };
    let after_call = collect_body_writes(code, last_call + 1, pc);
    let prefix_writes: Vec<usize> = collect_body_writes(code, start, last_call)
        .into_iter()
        .filter(|r| !after_call.contains(r))
        .collect();
    if prefix_writes.is_empty() {
        return false;
    }
    let writes_of = |ids: &[usize]| -> BTreeSet<usize> {
        let mut set = BTreeSet::new();
        for id in ids {
            if let Some(b) = cfg.blocks.get(id) {
                set.extend(collect_body_writes(code, b.start, b.end.min(code.len())));
            }
        }
        set
    };
    let then_writes = writes_of(then_blocks);
    let else_writes = writes_of(else_blocks);
    // A register both arms write never carries the prefix value to the merge
    // (a call temp such as a NAMECALL self slot); only a register some path
    // leaves untouched can lose a write to the declared-local exclusion.
    let both_arms = !then_blocks.is_empty() && !else_blocks.is_empty();
    prefix_writes.iter().any(|r| {
        let in_then = then_writes.contains(r);
        let in_else = else_writes.contains(r);
        // An arm that reads the register before writing it is a carried
        // identity (`ClassPropsByName = ClassPropsByName or {}`); the premat
        // pass pins those, so the write already survives.
        let carried = [then_blocks, else_blocks].iter().any(|ids| {
            let span = ids.iter().filter_map(|id| cfg.blocks.get(id)).fold(None, |acc, b| {
                Some(acc.map_or((b.start, b.end), |(s, e): (usize, usize)| (s.min(b.start), e.max(b.end))))
            });
            span.is_some_and(|(s, e)| reg_read_before_write(code, s, e.min(code.len()), *r))
        });
        !carried
            && (in_then || in_else)
            && !(both_arms && in_then && in_else)
            && merge_first_touch_is_read(code, merge, code.len(), *r)
    })
}

/// True when the arm is not safe to emit through the flat arm lift: it holds a
/// numeric loop, two or more loops, or a forward branch of its own outside any
/// generic-loop body. The flat lift handles an arm that is a straight run or a
/// single generic loop, and mangles these shapes, so they keep the long-standing
/// split instead of being newly emitted half-lifted.
fn arm_needs_old_split<'a>(
    code: &[u32],
    cfg: &ControlFlowGraph,
    blocks: impl Iterator<Item = &'a usize>,
) -> bool {
    let mut spans: Vec<(usize, usize)> = blocks
        .filter_map(|id| cfg.blocks.get(id))
        .map(|b| (b.start, b.end.min(code.len())))
        .collect();
    spans.sort_unstable();
    // Pass 1: generic-loop pc ranges [prep, loop_pc + aux]; a numeric loop is
    // dropped by the flat lift (measured: ReactElementValidator
    // `validateChildKeys`), so it disqualifies the arm outright.
    let mut loop_ranges: Vec<(usize, usize)> = Vec::new();
    let mut branches: Vec<usize> = Vec::new();
    for &(start, end) in &spans {
        let mut p = start;
        while p < end {
            let op = LuauOpcode::from_u8(insn_op(code[p]));
            match op {
                LuauOpcode::ForNPrep => return true,
                LuauOpcode::ForGPrep | LuauOpcode::ForGPrepINext | LuauOpcode::ForGPrepNext => {
                    let loop_pc = (p as i64 + insn_d(code[p]) as i64 + 1).max(p as i64) as usize;
                    loop_ranges.push((p, loop_pc + 2));
                }
                _ => {
                    if forward_conditional_jump(op, p, code[p]).is_some() {
                        branches.push(p);
                    }
                }
            }
            p += if op.has_aux() { 2 } else { 1 };
        }
    }
    if loop_ranges.len() >= 2 {
        return true;
    }
    // A branch of the arm's own, outside any loop body, is the shape the flat
    // lift mangles (loops lost, whole sub-diamonds emitted twice).
    branches
        .iter()
        .any(|&q| !loop_ranges.iter().any(|&(lo, hi)| q > lo && q < hi))
}

#[cfg(test)]
mod cond_prefix_tests {
    use super::*;

    const OP_LOADNIL: u32 = 2;
    const OP_CALL: u32 = 21;
    const OP_RETURN: u32 = 22;
    const OP_JUMPIFNOT: u32 = 26;
    const OP_JUMP: u32 = 23;
    const OP_FORGPREP: u32 = 58;
    const OP_FORGLOOP: u32 = 59;

    fn ad(op: u32, a: u32, d: i16) -> u32 {
        op | (a << 8) | ((d as u16 as u32) << 16)
    }

    fn abc(op: u32, a: u32, b: u32, c: u32) -> u32 {
        op | (a << 8) | (b << 16) | (c << 24)
    }

    fn cfg_of(code: Vec<u32>) -> (Vec<u32>, ControlFlowGraph) {
        let proto = Proto {
            max_stack_size: 16,
            num_params: 0,
            num_upvalues: 0,
            is_vararg: false,
            flags: 0,
            typeinfo: None,
            code: code.clone(),
            constants: Vec::new(),
            child_protos: Vec::new(),
            line_defined: 1,
            debug_name: None,
            line_info: None,
            debug_info: None,
        };
        (code, ControlFlowGraph::build(&proto))
    }

    #[test]
    fn cuts_at_block_start_when_the_condition_block_prefix_holds_a_call() {
        // 0: LOADNIL 1: CALL 2: JUMPIFNOT -> 4 3: LOADNIL 4: RETURN
        let (code, cfg) = cfg_of(vec![
            abc(OP_LOADNIL, 1, 0, 0),
            abc(OP_CALL, 1, 1, 1),
            ad(OP_JUMPIFNOT, 2, 1),
            abc(OP_LOADNIL, 3, 0, 0),
            abc(OP_RETURN, 0, 1, 0),
        ]);
        assert_eq!(cond_prefix_cut(&code, &cfg, 0, 2), 0);
    }

    #[test]
    fn keeps_the_branch_pc_when_the_prefix_has_no_call() {
        let (code, cfg) = cfg_of(vec![
            abc(OP_LOADNIL, 1, 0, 0),
            abc(OP_LOADNIL, 4, 0, 0),
            ad(OP_JUMPIFNOT, 2, 1),
            abc(OP_LOADNIL, 3, 0, 0),
            abc(OP_RETURN, 0, 1, 0),
        ]);
        assert_eq!(cond_prefix_cut(&code, &cfg, 0, 2), 2);
    }

    fn loop_code(with_leading_branch: bool) -> Vec<u32> {
        let lead = if with_leading_branch {
            ad(OP_JUMPIFNOT, 2, 5)
        } else {
            abc(OP_LOADNIL, 2, 0, 0)
        };
        vec![
            lead,
            abc(OP_LOADNIL, 5, 0, 0),
            ad(OP_FORGPREP, 5, 1),
            abc(OP_LOADNIL, 9, 0, 0),
            ad(OP_FORGLOOP, 5, -2),
            2,
            abc(OP_RETURN, 0, 1, 0),
        ]
    }

    #[test]
    fn flags_a_generic_for_nested_under_an_arm_branch() {
        let (code, cfg) = cfg_of(loop_code(true));
        let ids: Vec<usize> = cfg.blocks.keys().copied().collect();
        assert!(arm_needs_old_split(&code, &cfg, ids.iter()));
    }

    #[test]
    fn flags_an_arm_holding_two_loops() {
        // 0: LOADNIL 1: FORGPREP -> 3 2: LOADNIL 3: FORGLOOP (+AUX)
        // 5: FORGPREP -> 7 6: LOADNIL 7: FORGLOOP (+AUX) 9: RETURN
        let (code, cfg) = cfg_of(vec![
            abc(OP_LOADNIL, 5, 0, 0),
            ad(OP_FORGPREP, 5, 1),
            abc(OP_LOADNIL, 9, 0, 0),
            ad(OP_FORGLOOP, 5, -2),
            2,
            ad(OP_FORGPREP, 5, 1),
            abc(OP_LOADNIL, 9, 0, 0),
            ad(OP_FORGLOOP, 5, -2),
            2,
            abc(OP_RETURN, 0, 1, 0),
        ]);
        let ids: Vec<usize> = cfg.blocks.keys().copied().collect();
        assert!(arm_needs_old_split(&code, &cfg, ids.iter()));
    }

    #[test]
    fn allows_a_single_generic_loop_whose_only_branch_is_inside_its_body() {
        let (code, cfg) = cfg_of(vec![
            abc(OP_LOADNIL, 5, 0, 0),
            ad(OP_FORGPREP, 5, 3),
            ad(OP_JUMPIFNOT, 9, 1),
            abc(OP_LOADNIL, 9, 0, 0),
            abc(OP_LOADNIL, 10, 0, 0),
            ad(OP_FORGLOOP, 5, -4),
            2,
            abc(OP_RETURN, 0, 1, 0),
        ]);
        let ids: Vec<usize> = cfg.blocks.keys().copied().collect();
        assert!(!arm_needs_old_split(&code, &cfg, ids.iter()));
    }

    #[test]
    fn flags_a_loop_free_arm_with_its_own_branch() {
        let (code, cfg) = cfg_of(vec![
            ad(OP_JUMPIFNOT, 2, 1),
            abc(OP_LOADNIL, 3, 0, 0),
            abc(OP_RETURN, 0, 1, 0),
        ]);
        let ids: Vec<usize> = cfg.blocks.keys().copied().collect();
        assert!(arm_needs_old_split(&code, &cfg, ids.iter()));
    }

    #[test]
    fn ignores_a_generic_for_with_no_branch_before_it() {
        let (code, cfg) = cfg_of(loop_code(false));
        let ids: Vec<usize> = cfg.blocks.keys().copied().collect();
        assert!(!arm_needs_old_split(&code, &cfg, ids.iter()));
    }

    // themeForRow: `local c = base; local g = x:Find(); if g then for .. do
    // if cond then c = v.Color break end end end; return c`. R2 is written in
    // the call-holding condition prefix AND inside the loop arm, and read after
    // the merge. Cutting the Linear at the block start drops the prefix's
    // parked R2, the lifter names it, and the arm write is then lost.
    fn theme_code(merge_reads_r2: bool) -> Vec<u32> {
        vec![
            abc(OP_LOADNIL, 2, 0, 0),          // 0 R2 = base (prefix write)
            abc(OP_CALL, 3, 1, 1),             // 1 call in the prefix
            ad(OP_JUMPIFNOT, 3, 7),            // 2 -> 10
            abc(OP_LOADNIL, 5, 0, 0),          // 3
            ad(OP_FORGPREP, 5, 3),             // 4 -> loop at 8
            ad(OP_JUMPIFNOT, 9, 1),            // 5 -> 7
            abc(OP_LOADNIL, 2, 0, 0),          // 6 R2 = v.Color (arm write)
            abc(OP_LOADNIL, 10, 0, 0),         // 7
            ad(OP_FORGLOOP, 5, -4),            // 8
            2,                                 // 9 aux
            if merge_reads_r2 { abc(OP_RETURN, 2, 2, 0) } else { abc(OP_RETURN, 0, 1, 0) }, // 10
        ]
    }

    fn first_linear_end(regions: &[Region]) -> Option<usize> {
        match regions.first() {
            Some(Region::Linear { end, .. }) => Some(*end),
            _ => None,
        }
    }

    #[test]
    fn keeps_the_branch_split_when_an_arm_overwrites_a_prefix_value_read_after_the_merge() {
        let (code, cfg) = cfg_of(theme_code(true));
        let regions =
            structure_numeric_for_body(&code, 0, code.len(), &cfg, &BTreeSet::new());
        assert_eq!(first_linear_end(&regions), Some(2), "{:?}", regions);
    }

    // `local t = x:M(); if g then R2 = a else R2/other = b end; return R2`.
    fn two_arm_code(else_reg: u32) -> Vec<u32> {
        vec![
            abc(OP_LOADNIL, 2, 0, 0),    // 0 prefix write R2
            abc(OP_CALL, 3, 1, 1),       // 1
            ad(OP_JUMPIFNOT, 3, 2),      // 2 -> 5
            abc(OP_LOADNIL, 2, 0, 0),    // 3 then: R2
            ad(OP_JUMP, 0, 1),           // 4 -> 6
            abc(OP_LOADNIL, else_reg, 0, 0), // 5 else
            abc(OP_RETURN, 2, 2, 0),     // 6 merge reads R2
        ]
    }

    fn overwrites_for(else_reg: u32) -> bool {
        let (code, cfg) = cfg_of(two_arm_code(else_reg));
        let ids = |lo: usize, hi: usize| -> Vec<usize> {
            cfg.blocks.iter().filter(|(_, b)| b.start >= lo && b.start < hi)
                .map(|(id, _)| *id).collect()
        };
        arm_overwrites_prefix_value(&code, &cfg, 0, 2, 6, &ids(3, 5), &ids(5, 6))
    }

    #[test]
    fn ignores_a_prefix_value_written_after_the_last_call() {
        let code = vec![
            abc(OP_CALL, 3, 1, 1),       // 0 call first
            abc(OP_LOADNIL, 2, 0, 0),    // 1 R2 = nil AFTER the call (stays parked)
            ad(OP_JUMPIFNOT, 3, 2),      // 2 -> 5
            abc(OP_LOADNIL, 2, 0, 0),    // 3 then: R2
            ad(OP_JUMP, 0, 1),           // 4 -> 6
            abc(OP_LOADNIL, 7, 0, 0),    // 5 else leaves R2
            abc(OP_RETURN, 2, 2, 0),     // 6 merge reads R2
        ];
        let (code, cfg) = cfg_of(code);
        let ids = |lo: usize, hi: usize| -> Vec<usize> {
            cfg.blocks.iter().filter(|(_, b)| b.start >= lo && b.start < hi)
                .map(|(id, _)| *id).collect()
        };
        assert!(!arm_overwrites_prefix_value(&code, &cfg, 0, 2, 6, &ids(3, 5), &ids(5, 6)));
    }

    #[test]
    fn ignores_a_carried_identity_the_arm_reads_before_writing() {
        let code = vec![
            abc(OP_LOADNIL, 2, 0, 0),    // 0 prefix write R2
            abc(OP_CALL, 3, 1, 1),       // 1
            ad(OP_JUMPIFNOT, 3, 3),      // 2 -> 6
            abc(6, 9, 2, 0),             // 3 then: MOVE R9 R2 (read first)
            abc(OP_LOADNIL, 2, 0, 0),    // 4 then: R2 written
            ad(OP_JUMP, 0, 1),           // 5 -> 7
            abc(OP_LOADNIL, 7, 0, 0),    // 6 else
            abc(OP_RETURN, 2, 2, 0),     // 7 merge reads R2
        ];
        let (code, cfg) = cfg_of(code);
        let ids = |lo: usize, hi: usize| -> Vec<usize> {
            cfg.blocks.iter().filter(|(_, b)| b.start >= lo && b.start < hi)
                .map(|(id, _)| *id).collect()
        };
        assert!(!arm_overwrites_prefix_value(&code, &cfg, 0, 2, 7, &ids(3, 6), &ids(6, 7)));
    }

    #[test]
    fn ignores_a_register_that_both_arms_overwrite() {
        assert!(!overwrites_for(2));
    }

    #[test]
    fn flags_a_register_only_one_arm_overwrites() {
        assert!(overwrites_for(7));
    }

    #[test]
    fn still_cuts_at_the_block_start_when_the_merge_never_reads_the_overwritten_value() {
        let (code, cfg) = cfg_of(theme_code(false));
        let regions =
            structure_numeric_for_body(&code, 0, code.len(), &cfg, &BTreeSet::new());
        assert_eq!(first_linear_end(&regions), None, "{:?}", regions);
    }
}
