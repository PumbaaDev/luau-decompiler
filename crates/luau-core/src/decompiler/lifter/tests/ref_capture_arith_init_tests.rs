//! A register whose FIRST value is an arithmetic result and which is then
//! captured BY REFERENCE (`ADDK R0 R1 + K; NEWCLOSURE; CAPTURE ref R0`) must
//! have that value emitted as the captured local's initializer. Parking it in
//! the register file (a leaf-leaf BinOp is "inlinable") leaves the closure
//! reading a never-assigned local. Shape from PetSimulator99 HugePartyUtil.

use crate::decompiler::{decompile_proto, DecompileContext};
use crate::parser::types::{Chunk, Constant, Proto};

const OP_GETUPVAL: u8 = 9;
const OP_GETIMPORT: u8 = 12;
const OP_NEWCLOSURE: u8 = 19;
const OP_CALL: u8 = 21;
const OP_RETURN: u8 = 22;
const OP_CLOSEUPVALS: u8 = 11;
const OP_JUMPIFNOT: u8 = 26;
const OP_LOADB: u8 = 3;
const OP_ADDK: u8 = 39;
const OP_CAPTURE: u8 = 70;

fn insn_ad(op: u8, a: u8, d: i16) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((d as u16 as u32) << 16)
}

fn insn_abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
}

fn proto(code: Vec<u32>, constants: Vec<Constant>, num_upvalues: u8, name: &str) -> Proto {
    Proto {
        max_stack_size: 16,
        num_params: 0,
        num_upvalues,
        is_vararg: false,
        flags: 0,
        typeinfo: None,
        code,
        constants,
        child_protos: Vec::new(),
        line_defined: 1,
        debug_name: Some(name.to_string()),
        line_info: None,
        debug_info: None,
    }
}

fn chunk_with(_unused: bool) -> Chunk {
    // 0 LOADB R7 false
    // 1 GETIMPORT R9 ; 3 CALL R9      (now = getTime())
    // 4 ADDK R8 R9 + 90               (the captured variable's first value)
    // 5 GETIMPORT R9 ; 7 CALL R9      (R9 reused: flushes `now` to a local)
    // 8 JUMPIFNOT R7 -> 11
    // 9 NEWCLOSURE R10 ; 10 CAPTURE ref R8   (capture INSIDE a conditional arm)
    // 11 NEWCLOSURE R11 ; 12 CAPTURE ref R8
    // 13 CLOSEUPVALS R8 ; 14 RETURN R11
    let import_val = (1u32 << 30) | (0 << 20);
    let code = vec![
        insn_abc(OP_LOADB, 7, 0, 0),
        insn_ad(OP_GETIMPORT, 9, 0),
        import_val,
        insn_abc(OP_CALL, 9, 1, 2),
        insn_abc(OP_ADDK, 8, 9, 1),
        insn_ad(OP_GETIMPORT, 9, 0),
        import_val,
        insn_abc(OP_CALL, 9, 1, 2),
        insn_ad(OP_JUMPIFNOT, 7, 2),
        insn_ad(OP_NEWCLOSURE, 10, 0),
        insn_abc(OP_CAPTURE, 1, 8, 0),
        insn_ad(OP_NEWCLOSURE, 11, 0),
        insn_abc(OP_CAPTURE, 1, 8, 0),
        insn_abc(OP_CLOSEUPVALS, 8, 0, 0),
        insn_abc(OP_RETURN, 11, 2, 0),
    ];
    let consts = vec![Constant::Import(import_val), Constant::Number(90.0), ];
    let child_code = vec![insn_abc(OP_GETUPVAL, 0, 0, 0), insn_abc(OP_RETURN, 0, 2, 0)];
    let mut parent = proto(code, consts, 0, "arith_parent");
    parent.child_protos = vec![1];
    let child = proto(child_code, Vec::new(), 1, "child");
    Chunk {
        version: 6,
        types_version: 0,
        strings: vec!["getTime".to_string()],
        protos: vec![parent, child],
        main_proto: 0,
    }
}

#[test]
fn arithmetic_initializer_of_ref_captured_register_is_emitted() {
    let chunk = chunk_with(false);
    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);
    assert!(
        out.lines().any(|l| l.contains("+ 90") && l.contains('=')),
        "the `+ 90` initializer of the captured local must be emitted:\n{out}"
    );
}
