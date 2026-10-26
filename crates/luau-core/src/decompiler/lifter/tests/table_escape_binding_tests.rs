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

const OP_LENGTH: u8 = 52;
const OP_MOVE: u8 = 6;
const OP_RETURN: u8 = 22;
const OP_CAPTURE: u8 = 70;

#[test]
fn store_as_value_then_length_binds_the_table() {
    // `local t = {..}; M.T = t; M.N = #t`: the store is a use of the table,
    // so the length read is its second use. Without binding, the constructor
    // is re-emitted for each consumer and the exported table stays a copy.
    let code = [
        abc(OP_NEWTABLE, 2, 0, 0),
        0,
        abc(OP_SETTABLEKS, 2, 0, 0), // R0."T" = R2 (A is the stored value)
        0,
        abc(OP_LENGTH, 3, 2, 0),
    ];
    assert!(table_needs_binding(&code, 0, 2));
}

#[test]
fn store_as_value_then_call_argument_binds_the_table() {
    // `t = {}; M.T = t; f(t)`
    let code = [
        abc(OP_NEWTABLE, 2, 0, 0),
        0,
        abc(OP_SETTABLEKS, 2, 0, 0),
        0,
        abc(OP_MOVE, 3, 2, 0),
        abc(OP_CALL, 3, 2, 1),
    ];
    assert!(table_needs_binding(&code, 0, 2));
}

#[test]
fn store_as_value_then_return_binds_the_table() {
    // `t = {}; M.T = t; return t`
    let code = [
        abc(OP_NEWTABLE, 2, 0, 0),
        0,
        abc(OP_SETTABLEKS, 2, 0, 0),
        0,
        abc(OP_RETURN, 2, 2, 0),
    ];
    assert!(table_needs_binding(&code, 0, 2));
}

#[test]
fn captured_then_read_binds_the_table() {
    // CAPTURE VAL R2 is a use of the table; a later MOVE is the second.
    let code = [
        abc(OP_NEWTABLE, 2, 0, 0),
        0,
        abc(OP_CAPTURE, 0, 2, 0),
        abc(OP_MOVE, 3, 2, 0),
    ];
    assert!(table_needs_binding(&code, 0, 2));
}

#[test]
fn single_return_still_inlines() {
    // `return {..}` is one consumer.
    let code = [abc(OP_NEWTABLE, 2, 0, 0), 0, abc(OP_RETURN, 2, 2, 0)];
    assert!(!table_needs_binding(&code, 0, 2));
}

#[test]
fn upvalue_capture_index_is_not_a_register_use() {
    // CAPTURE upval 1 names upvalue #1, not register 1, so two of them
    // beside a table in R1 are not uses of that table.
    let code = [
        abc(OP_NEWTABLE, 1, 0, 0),
        0,
        abc(OP_CAPTURE, 2, 1, 0),
        abc(OP_CAPTURE, 2, 1, 0),
    ];
    assert!(!table_needs_binding(&code, 0, 1));
}

#[test]
fn two_captures_alone_do_not_force_a_binding() {
    // Closures capturing the same table share the one binding the first
    // capture declares, so a second CAPTURE is not a second use.
    let code = [
        abc(OP_NEWTABLE, 2, 0, 0),
        0,
        abc(OP_CAPTURE, 0, 2, 0),
        abc(OP_CAPTURE, 0, 2, 0),
    ];
    assert!(!table_needs_binding(&code, 0, 2));
}

#[test]
fn store_as_value_then_capture_binds_the_table() {
    // `M.Cache = t; function M.Get() return t end`
    let code = [
        abc(OP_NEWTABLE, 2, 0, 0),
        0,
        abc(OP_SETTABLEKS, 2, 0, 0),
        0,
        abc(OP_CAPTURE, 0, 2, 0),
    ];
    assert!(table_needs_binding(&code, 0, 2));
}

#[test]
fn store_then_capture_far_apart_still_binds_the_table() {
    // `M.INDEX = t` early, a closure capturing `t` more than 1,024
    // instructions later: one object, so it must be bound.
    let mut code = vec![abc(OP_NEWTABLE, 2, 0, 0), 0, abc(OP_SETTABLEKS, 2, 0, 0), 0];
    code.extend(std::iter::repeat(abc(OP_MOVE, 5, 6, 0)).take(1500));
    code.push(abc(OP_CAPTURE, 0, 2, 0));
    assert!(table_needs_binding(&code, 0, 2));
}
