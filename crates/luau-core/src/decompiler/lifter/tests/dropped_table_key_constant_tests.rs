//! dropped_table_key_constant (26 Sep, campaign3): an empty-string field key
//! is a legitimate `Constant::String("")`, and the Luau VM's own
//! GETTABLEKS/SETTABLEKS resolution (`VM_KV(aux)`) reads it exactly like any
//! other string constant. `resolve_aux_string_canonical` rejected it with a
//! `!s.is_empty()` guard left over from the tier's introduction (`deb5149`),
//! so `get_table_string_from_aux` fell back to a synthetic `field_{aux}`
//! placeholder even though the real key was sitting right there in
//! `proto.constants[aux]`.
//!
//! Confirmed against raw bytecode with a scratch `examples/inspect_proto.rs`
//! dump (not checked in): RIVALS
//! `Players__superjepy_PlayerScripts_Controllers_MusicController.luac` proto 32
//! K[28] and proto 0 K[5] are both `Constant::String("")`, and
//! DungeonQuestReborn `JoinRequestClient.luac` proto 25 K[18] likewise sits
//! directly beside the sibling key K[19]="playersNotReady" that resolved
//! fine. These tests pin `resolve_aux_string_canonical`'s answer directly so
//! a future re-introduction of the `is_empty()` guard fails loudly.

use super::super::resolve_aux_string_canonical;
use crate::parser::types::{Constant, Proto};

fn proto_with_constants(constants: Vec<Constant>) -> Proto {
    Proto {
        max_stack_size: 8,
        num_params: 0,
        num_upvalues: 0,
        is_vararg: false,
        flags: 0,
        typeinfo: None,
        code: Vec::new(),
        constants,
        child_protos: Vec::new(),
        line_defined: 1,
        debug_name: Some("dropped_table_key_constant".to_string()),
        line_info: None,
        debug_info: None,
    }
}

#[test]
fn resolves_an_empty_string_constant_as_a_key() {
    // K[5] = "" — exactly the RIVALS MusicController `_new` closure's shape.
    let proto = proto_with_constants(vec![
        Constant::Nil,
        Constant::String("_volume".to_string()),
        Constant::Nil,
        Constant::Nil,
        Constant::Nil,
        Constant::String(String::new()),
    ]);
    assert_eq!(
        resolve_aux_string_canonical(&proto, 5),
        Some(String::new()),
        "an empty-string constant is a real VM_KV(aux) key, not a resolution failure"
    );
}

#[test]
fn still_rejects_a_missing_constant_index() {
    let proto = proto_with_constants(vec![Constant::String("a".to_string())]);
    assert_eq!(
        resolve_aux_string_canonical(&proto, 99),
        None,
        "an out-of-range aux must still fall through to the placeholder tiers"
    );
}

#[test]
fn still_rejects_a_non_string_constant() {
    let proto = proto_with_constants(vec![Constant::Number(1.0)]);
    assert_eq!(
        resolve_aux_string_canonical(&proto, 0),
        None,
        "a non-string constant at this index is not a valid field key"
    );
}

#[test]
fn still_rejects_an_overlong_string() {
    let long = "x".repeat(201);
    let proto = proto_with_constants(vec![Constant::String(long)]);
    assert_eq!(
        resolve_aux_string_canonical(&proto, 0),
        None,
        "the existing <=200 sanity cap must be unaffected by dropping is_empty()"
    );
}

#[test]
fn resolves_an_ordinary_non_empty_key_unchanged() {
    let proto = proto_with_constants(vec![Constant::String("playersNotReady".to_string())]);
    assert_eq!(
        resolve_aux_string_canonical(&proto, 0),
        Some("playersNotReady".to_string())
    );
}
