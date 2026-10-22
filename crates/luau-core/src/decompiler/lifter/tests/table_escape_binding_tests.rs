//! A pending table that stores ITSELF must be bound, not cloned.
//!
//! The module idiom `local C = {}; C.__index = C` compiles to
//! `SETTABLEKS R2."__index" R2`. `table_needs_binding` used to see no read in
//! that store, so the constructor stayed parked and the SETTABLEKS fold took a
//! snapshot of the (empty) literal: the output was `{ __index = {} }` and every
//! instance method lookup failed.

use crate::decompiler::lifter::opcode_handlers::table_needs_binding;

const OP_CALL: u8 = 21;
const OP_NEWTABLE: u8 = 53;
const OP_SETTABLEKS: u8 = 16;

fn abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
}

#[test]
fn self_store_binds_the_table() {
    // NEWTABLE R2; SETTABLEKS R2."__index" R2
    let code = [abc(OP_NEWTABLE, 2, 0, 0), 0, abc(OP_SETTABLEKS, 2, 2, 0), 0];
    assert!(table_needs_binding(&code, 0, 2));
}

#[test]
fn store_into_another_table_still_inlines() {
    // `M.sub = {}` with no other use stays an inline literal.
    let code = [abc(OP_NEWTABLE, 3, 0, 0), 0, abc(OP_SETTABLEKS, 3, 0, 0), 0];
    assert!(!table_needs_binding(&code, 0, 3));
}

#[test]
fn single_call_argument_still_inlines() {
    // `print({})`: one consumer, no binding.
    let code = [abc(OP_NEWTABLE, 1, 0, 0), 0, abc(OP_CALL, 0, 2, 1)];
    assert!(!table_needs_binding(&code, 0, 1));
}
