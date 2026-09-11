//! Phase 8: real CFG-level control flow flattening.
//!
//! For each proto, this pass:
//!   1. Parses its bytecode into logical instructions (Closure + N×ClosureUpval
//!      treated as one fat instruction so block boundaries never split them).
//!   2. Identifies basic blocks. Block leaders = PC 0, jump targets, the
//!      instruction immediately after any block-terminator.
//!   3. Numbers blocks 0..N and emits a state-machine dispatch:
//!      ```text
//!      state := 0
//!      loop:
//!        if state == EXIT then return 0 end
//!        if state == 0 then <block 0 body>; state := next; jump loop end
//!        if state == 1 then <block 1 body>; state := next; jump loop end
//!        ...
//!      ```
//!   4. Rewrites each block's terminator into a `state := ...` assignment
//!      followed by `jump loop`.
//!
//! **Block terminators:** `Jump`, `JumpIfFalse`, `JumpIfTrue`, `Return`. The
//! short-circuit-`and`/`or` jumps (`JumpIfFalseKeep`/`JumpIfTrueKeep`) are
//! **not** treated as terminators — they're intra-expression, their targets
//! always land within the same logical expression, and treating them as
//! block-breakers would split stack values across block boundaries.

use std::collections::HashMap;

use crate::vm::opcodes::{BinSubOp, Op};
use crate::vm::{
    emit_instr, patch_a, Const, ConstKey, Module, StringState, INSTR_WIDTH,
};
use crate::ProtectOptions;

pub fn flatten(mut module: Module, opts: &ProtectOptions) -> Module {
    let _ = opts;
    // Rebuild the const pool index once so per-proto interning shares entries.
    let mut const_pool: HashMap<ConstKey, u16> = HashMap::new();
    for (i, c) in module.constants.iter().enumerate() {
        const_pool.insert(c.as_key(), i as u16);
    }
    for proto_idx in 0..module.protos.len() {
        flatten_proto(&mut module, proto_idx, &mut const_pool);
    }
    module
}

fn flatten_proto(
    module: &mut Module,
    proto_idx: usize,
    const_pool: &mut HashMap<ConstKey, u16>,
) {
    let original_code = module.protos[proto_idx].code.clone();
    if original_code.is_empty() {
        return;
    }

    let instrs = match parse_logical_instrs(&original_code) {
        Some(v) => v,
        None => return, // malformed; leave proto alone
    };
    if instrs.len() < 4 {
        // Single-block (or near-single) proto — flattening adds dispatch
        // overhead with no obfuscation gain.
        return;
    }

    let pc_to_index: HashMap<usize, usize> = instrs
        .iter()
        .enumerate()
        .map(|(i, instr)| (instr.pc, i))
        .collect();

    // Mark block leaders.
    let mut is_leader = vec![false; instrs.len()];
    is_leader[0] = true;
    for (i, instr) in instrs.iter().enumerate() {
        match instr.op {
            Op::Jump | Op::JumpIfFalse | Op::JumpIfTrue => {
                let after = instr.pc + instr.byte_size;
                let target = after as i32 + (instr.a as i32) * INSTR_WIDTH as i32;
                if target >= 0 {
                    if let Some(&j) = pc_to_index.get(&(target as usize)) {
                        is_leader[j] = true;
                    } else {
                        // Jump target lands somewhere unparseable (mid-Closure?).
                        // Bail out and leave this proto alone.
                        return;
                    }
                }
                if i + 1 < instrs.len() {
                    is_leader[i + 1] = true;
                }
            }
            Op::Return => {
                if i + 1 < instrs.len() {
                    is_leader[i + 1] = true;
                }
            }
            _ => {}
        }
    }

    // Slice into blocks.
    let mut blocks: Vec<Vec<LogicalInstr>> = Vec::new();
    let mut current: Vec<LogicalInstr> = Vec::new();
    for (i, instr) in instrs.iter().enumerate() {
        if is_leader[i] && !current.is_empty() {
            blocks.push(std::mem::take(&mut current));
        }
        current.push(instr.clone());
    }
    if !current.is_empty() {
        blocks.push(current);
    }
    if blocks.len() < 2 {
        return;
    }

    // Map original PC → block id.
    let pc_to_block: HashMap<usize, usize> = blocks
        .iter()
        .enumerate()
        .map(|(i, b)| (b[0].pc, i))
        .collect();

    let n_blocks = blocks.len();
    let exit_state = n_blocks;

    // Intern numeric state-ID constants. We use `i + 1` so 0 is reserved
    // (and so the dispatcher's `state == k` checks don't have to special-case
    // zero against fresh-uninitialized stack values).
    let mut state_const_idx: Vec<u16> = Vec::with_capacity(n_blocks + 1);
    for i in 0..=n_blocks {
        let idx = intern_number(module, const_pool, (i + 1) as f64);
        state_const_idx.push(idx);
    }

    // Reserve state slot.
    let state_slot = module.protos[proto_idx].num_locals;
    module.protos[proto_idx].num_locals = state_slot + 1;

    // Emit the new bytecode.
    let mut new_code: Vec<u8> = Vec::new();

    // Initial state := block 0 id.
    emit_instr(&mut new_code, Op::PushConst, state_const_idx[0] as i16, 0);
    emit_instr(&mut new_code, Op::StoreLocal, state_slot as i16, 0);

    let dispatch_top = new_code.len();

    // Exit check (early in the dispatcher so we return out of the loop fast).
    emit_instr(&mut new_code, Op::LoadLocal, state_slot as i16, 0);
    emit_instr(
        &mut new_code,
        Op::PushConst,
        state_const_idx[exit_state] as i16,
        0,
    );
    emit_instr(&mut new_code, Op::BinOp, BinSubOp::Eq as i16, 0);
    let exit_skip = new_code.len();
    emit_instr(&mut new_code, Op::JumpIfFalse, 0, 0);
    emit_instr(&mut new_code, Op::Return, 0, 0);
    let after_exit_check = new_code.len();
    patch_jump(&mut new_code, exit_skip, after_exit_check);

    let mut to_dispatch: Vec<usize> = Vec::new();

    for (block_id, block) in blocks.iter().enumerate() {
        // if state == block_id then ... else (skip to next test) end
        emit_instr(&mut new_code, Op::LoadLocal, state_slot as i16, 0);
        emit_instr(
            &mut new_code,
            Op::PushConst,
            state_const_idx[block_id] as i16,
            0,
        );
        emit_instr(&mut new_code, Op::BinOp, BinSubOp::Eq as i16, 0);
        let skip = new_code.len();
        emit_instr(&mut new_code, Op::JumpIfFalse, 0, 0);

        // Block content (all but terminator) — bytes copied verbatim. Any
        // intra-block short-circuit jumps (JumpIfFalseKeep / JumpIfTrueKeep)
        // are inside this block and their relative deltas are preserved.
        let terminator = block.last().expect("non-empty block");
        let content = &block[..block.len() - 1];
        for instr in content {
            new_code.extend_from_slice(&instr.bytes);
        }

        let fallthrough_block = if block_id + 1 < n_blocks {
            block_id + 1
        } else {
            exit_state
        };

        match terminator.op {
            Op::Return => {
                // Keep Return verbatim — function exits.
                new_code.extend_from_slice(&terminator.bytes);
            }
            Op::Jump => {
                let after = terminator.pc + terminator.byte_size;
                let target =
                    (after as i32 + (terminator.a as i32) * INSTR_WIDTH as i32) as usize;
                let target_block = *pc_to_block.get(&target).unwrap_or(&exit_state);
                emit_state_transition(
                    &mut new_code,
                    state_const_idx[target_block],
                    state_slot,
                    &mut to_dispatch,
                );
            }
            Op::JumpIfFalse | Op::JumpIfTrue => {
                let after = terminator.pc + terminator.byte_size;
                let target =
                    (after as i32 + (terminator.a as i32) * INSTR_WIDTH as i32) as usize;
                let target_block = *pc_to_block.get(&target).unwrap_or(&exit_state);

                // Whichever original op, the BYTECODE-LEVEL semantics are:
                // "pop cond; if (op-says-take-jump) jump to target; else fall through".
                // We emit JumpIfFalse over a 'kept' path, so:
                //   - For Op::JumpIfFalse: cond falsy → jump to target;
                //                          truthy → fall through.
                //     => false-state = target_block, true-state = fallthrough.
                //   - For Op::JumpIfTrue:  cond truthy → jump to target;
                //                          falsy → fall through.
                //     => true-state = target_block, false-state = fallthrough.
                let (true_block, false_block) = if matches!(terminator.op, Op::JumpIfFalse)
                {
                    (fallthrough_block, target_block)
                } else {
                    (target_block, fallthrough_block)
                };

                // JumpIfFalse pops the cond. Truthy → next 3 instrs run.
                // Falsy → skip the truthy path's 3 instrs.
                let cond_jump = new_code.len();
                emit_instr(&mut new_code, Op::JumpIfFalse, 0, 0);

                // Truthy path
                emit_state_transition(
                    &mut new_code,
                    state_const_idx[true_block],
                    state_slot,
                    &mut to_dispatch,
                );

                // Falsy path
                let false_path_start = new_code.len();
                patch_jump(&mut new_code, cond_jump, false_path_start);
                emit_state_transition(
                    &mut new_code,
                    state_const_idx[false_block],
                    state_slot,
                    &mut to_dispatch,
                );
            }
            // Anything else as a block "terminator" means the encoder ended
            // the proto without an explicit branch — just fall through to the
            // next block via state assignment.
            _ => {
                new_code.extend_from_slice(&terminator.bytes);
                emit_state_transition(
                    &mut new_code,
                    state_const_idx[fallthrough_block],
                    state_slot,
                    &mut to_dispatch,
                );
            }
        }

        let block_end = new_code.len();
        patch_jump(&mut new_code, skip, block_end);
    }

    for j in to_dispatch {
        patch_jump(&mut new_code, j, dispatch_top);
    }

    // Safety net — if state ever lands on an unknown value, exit cleanly.
    emit_instr(&mut new_code, Op::Return, 0, 0);

    // Sanity: every i16 jump operand we patched must fit. patch_jump panics
    // otherwise (caught early during development).
    module.protos[proto_idx].code = new_code;
}

fn emit_state_transition(
    out: &mut Vec<u8>,
    state_const: u16,
    state_slot: u16,
    to_dispatch: &mut Vec<usize>,
) {
    emit_instr(out, Op::PushConst, state_const as i16, 0);
    emit_instr(out, Op::StoreLocal, state_slot as i16, 0);
    let j = out.len();
    emit_instr(out, Op::Jump, 0, 0);
    to_dispatch.push(j);
}

#[derive(Clone)]
struct LogicalInstr {
    pc: usize,
    op: Op,
    a: i16,
    /// Operand B (kept for Closure's upval-count read).
    #[allow(dead_code)]
    b: i16,
    /// Total bytes this logical instruction occupies (5 for normal ops,
    /// 5 * (1 + n_upvals) for Closure).
    byte_size: usize,
    bytes: Vec<u8>,
}

fn parse_logical_instrs(code: &[u8]) -> Option<Vec<LogicalInstr>> {
    let mut out = Vec::new();
    let mut pc = 0;
    while pc < code.len() {
        if pc + INSTR_WIDTH > code.len() {
            return None;
        }
        let op_byte = code[pc];
        let a = i16::from_le_bytes([code[pc + 1], code[pc + 2]]);
        let b = i16::from_le_bytes([code[pc + 3], code[pc + 4]]);
        let op = match canonical_op(op_byte) {
            Some(op) => op,
            None => return None,
        };
        let byte_size = if matches!(op, Op::Closure) {
            // Closure pulls in B ClosureUpval instructions.
            let upvals = if b >= 0 { b as usize } else { 0 };
            INSTR_WIDTH * (1 + upvals)
        } else {
            INSTR_WIDTH
        };
        if pc + byte_size > code.len() {
            return None;
        }
        out.push(LogicalInstr {
            pc,
            op,
            a,
            b,
            byte_size,
            bytes: code[pc..pc + byte_size].to_vec(),
        });
        pc += byte_size;
    }
    Some(out)
}

fn canonical_op(b: u8) -> Option<Op> {
    if (b as usize) >= Op::COUNT as usize {
        return None;
    }
    // SAFETY: Op is `#[repr(u8)]` with contiguous discriminants 0..COUNT-1.
    // We just bounds-checked.
    Some(unsafe { std::mem::transmute::<u8, Op>(b) })
}

fn patch_jump(buf: &mut [u8], at: usize, target: usize) {
    let next_pc = (at + INSTR_WIDTH) as i32;
    let delta = target as i32 - next_pc;
    let delta_instrs = delta / INSTR_WIDTH as i32;
    if !(i16::MIN as i32..=i16::MAX as i32).contains(&delta_instrs) {
        panic!("flatten: jump out of i16 range (delta={delta_instrs})");
    }
    patch_a(buf, at, delta_instrs as i16);
}

fn intern_number(
    module: &mut Module,
    pool: &mut HashMap<ConstKey, u16>,
    val: f64,
) -> u16 {
    let c = Const::Number(val);
    let key = c.as_key();
    if let Some(&i) = pool.get(&key) {
        return i;
    }
    let i = module.constants.len() as u16;
    module.constants.push(c);
    module.const_states.push(StringState::Plain);
    pool.insert(key, i);
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_op_round_trip() {
        for b in 0..Op::COUNT {
            let op = canonical_op(b).expect("known op");
            assert_eq!(op as u8, b);
        }
        for b in Op::COUNT..=255u8 {
            assert!(canonical_op(b).is_none(), "byte {b} should be invalid");
        }
    }
}
