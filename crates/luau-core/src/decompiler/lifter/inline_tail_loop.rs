//! `repeat BODY until c` nested in a loop body whose BODY opens with a
//! generic-for.
//!
//! The bytecode is the loop body, then a forward exit test over a JUMPBACK:
//!
//! ```text
//!   header:  ... FORGPREP .. FORGLOOP ...
//!   cp:      JUMPIFNOT R3 -> exit      (the exit test)
//!   latch:   JUMPBACK -> header
//!   exit:
//! ```
//!
//! The nested-body scan met the for first, consumed it as a `GenericFor`
//! region, and only then reached the latch: the header was already behind
//! `linear_start`, so the whole body (for included) was lifted a second time as
//! flat code - bare 3-value `a, b, c = pairs(t)` assigns, loop variables never
//! bound - inside a `while c do` that re-ran the flat copy.
//!
//! The structurer now claims the loop at its header and hands the body span
//! `[header, cp)` over recursively; this lifter emits it as `repeat .. until`.

use super::*;

/// Lift `Region::InlineLoopInLoop` when its exit test is at the TAIL
/// (`cond_pc >= body_start`), as `repeat BODY until not <fall-through cond>`.
#[allow(clippy::too_many_arguments)]
pub(super) fn lift_inline_tail_loop(
    ctx: &mut DecompileContext,
    proto: &Proto,
    proto_index: usize,
    cfg: &ControlFlowGraph,
    depth: usize,
    header_start: usize,
    cond_pc: usize,
    latch_pc: usize,
    body: &[Region],
    regs: &mut Vec<RegVal>,
    locals: &mut LocalTracker,
    stmts: &mut Vec<Stat>,
) {
    // Carried registers must be real variables before the body reads them.
    {
        let mut writes = collect_body_writes(&proto.code, header_start, latch_pc);
        writes.sort_unstable();
        writes.dedup();
        let skip: Vec<usize> = writes
            .iter()
            .copied()
            .filter(|&r| reg_dead_on_entry(&proto.code, header_start, latch_pc, r))
            .collect();
        premateralize_loop_carried(
            ctx, proto, regs, locals, stmts,
            &writes, &skip, header_start,
            Some((header_start, latch_pc + 1)),
        );
    }

    let snap = locals.snapshot();
    let mut body_stmts = Vec::new();
    let prev_loop_end = ctx.current_loop_end;
    let prev_loop_body = ctx.current_loop_body.take();
    ctx.emitted_latches.push(crate::decompiler::EmittedLoop {
        latch: Some(latch_pc),
        trailing_recompute: false,
    });
    ctx.current_loop_end = Some(latch_pc);
    for sub_region in body {
        match sub_region {
            Region::Linear { start, end } => {
                lift_instruction_range(
                    ctx, proto, proto_index, depth, *start, *end, regs, locals, &mut body_stmts,
                    true,
                );
            }
            _ => lift_region(
                ctx, proto, proto_index, cfg, sub_region, regs, locals, depth + 1, &mut body_stmts,
            ),
        }
    }
    ctx.emitted_latches.pop();
    ctx.current_loop_end = prev_loop_end;
    ctx.current_loop_body = prev_loop_body;

    // The test sits after the body: the loop REPEATS while the fall-through
    // condition holds, so `until` is its negation.
    let repeat_while = extract_branch_condition(ctx, proto, cond_pc, regs);
    hoist_loop_locals(locals, &snap, stmts, &mut body_stmts);
    stmts.push(Stat::Repeat {
        body: body_stmts,
        condition: negate_condition(&repeat_while),
    });
}
