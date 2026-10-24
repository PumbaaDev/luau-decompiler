//! A LOADNIL default that a conditional arm jumps OVER must not be constant-folded.
//!
//! `local h = self and self:FindFirstChild("Humanoid") or nil; if not h then return end; ...`
//! compiles to
//!
//! ```text
//! pc0: JUMPIFNOT R0 -> pc6      -- arm skipped: fall to the default
//! pc1: LOADK R3 K0              -- "Humanoid"
//! pc2: NAMECALL R1 R0 K1        -- FindFirstChild (AUX at pc3)
//! pc4: CALL R1 3 2
//! pc5: JUMPIF R1 -> pc7         -- value truthy: JUMP OVER the default load
//! pc6: LOADNIL R1               -- default
//! pc7: JUMPIF R1 -> pc9         -- the guard reads the MERGED register
//! pc8: RETURN
//! pc9: ... rest of the function ...
//! ```
//!
//! Real exhibit: Brookhaven CharacterUtil `removeFromSeat`. The LOADNIL parked a
//! constant nil in R1, so the guard on R1 became `if not nil then return end`,
//! i.e. an unconditional `return` that deleted every call after it.

use crate::decompiler::{decompile_proto, DecompileContext};
use crate::parser::types::{Chunk, Constant, Proto};

const OP_LOADNIL: u8 = 2;
const OP_LOADK: u8 = 5;
const OP_GETTABLEKS: u8 = 15;
const OP_NAMECALL: u8 = 20;
const OP_CALL: u8 = 21;
const OP_RETURN: u8 = 22;
const OP_JUMPIF: u8 = 25;
const OP_JUMPIFNOT: u8 = 26;

fn insn_ad(op: u8, a: u8, d: i16) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((d as u16 as u32) << 16)
}

fn insn_abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
}

fn build_chunk() -> Chunk {
    let code = vec![
        insn_ad(OP_JUMPIFNOT, 0, 5),      // pc0 -> pc6
        insn_ad(OP_LOADK, 3, 0),          // pc1 R3 = "Humanoid"
        insn_abc(OP_NAMECALL, 1, 0, 0),   // pc2
        1,                                // pc3 AUX: K1 "FindFirstChild"
        insn_abc(OP_CALL, 1, 3, 2),       // pc4
        insn_ad(OP_JUMPIF, 1, 1),         // pc5 -> pc7 (over the default)
        insn_abc(OP_LOADNIL, 1, 0, 0),    // pc6 default
        insn_ad(OP_JUMPIF, 1, 1),         // pc7 -> pc9
        insn_abc(OP_RETURN, 0, 1, 0),     // pc8 guard return
        insn_abc(OP_GETTABLEKS, 2, 1, 0), // pc9
        2,                                // pc10 AUX: K2 "Parent"
        insn_abc(OP_RETURN, 2, 2, 0),     // pc11
    ];
    let proto = Proto {
        max_stack_size: 8,
        num_params: 1,
        num_upvalues: 0,
        is_vararg: false,
        flags: 0,
        typeinfo: None,
        code,
        constants: vec![
            Constant::String("Humanoid".to_string()),
            Constant::String("FindFirstChild".to_string()),
            Constant::String("Parent".to_string()),
        ],
        child_protos: Vec::new(),
        line_defined: 1,
        debug_name: Some("removeFromSeat".to_string()),
        line_info: None,
        debug_info: None,
    };
    Chunk {
        version: 6,
        types_version: 0,
        strings: Vec::new(),
        protos: vec![proto],
        main_proto: 0,
    }
}

fn decompile(chunk: &Chunk) -> String {
    let mut ctx = DecompileContext::new(chunk);
    decompile_proto(&mut ctx, &chunk.protos[0], 0, 0)
}

#[test]
fn loadnil_default_jumped_over_by_arm_keeps_guard_and_rest() {
    let out = decompile(&build_chunk());
    eprintln!("=== emitted ===\n{out}\n===============");
    assert!(out.contains("FindFirstChild"), "the call was dropped:\n{out}");
    assert!(
        out.contains("Parent"),
        "the guard folded to an unconditional return and deleted the rest:\n{out}"
    );
}

#[test]
fn loadnil_default_merge_binding_is_in_scope_of_the_guard() {
    let out = decompile(&build_chunk());
    // The guard must read a name declared at the scope it is read in:
    // `local h = self and self:M(..) or nil`, or `local h = nil; if .. then h = .. end`.
    // A declaration that exists only inside the `if` arm is the defect.
    let lines: Vec<&str> = out.lines().collect();
    let indent = |l: &str| l.len() - l.trim_start().len();
    let decl = lines
        .iter()
        .position(|l| l.trim_start().starts_with("local ") && l.contains("FindFirstChild"));
    let Some(decl) = decl else {
        // Declared earlier with a nil seed and assigned in the arm.
        assert!(
            lines.iter().any(|l| l.trim_start().starts_with("local ") && l.contains("nil")),
            "no declaration of the call result:\n{out}"
        );
        return;
    };
    let guard = lines.iter().position(|l| l.contains("return")).expect("return in output");
    assert!(
        indent(lines[decl]) <= indent(lines[guard]),
        "declaration is nested deeper than the guard that reads it:\n{out}"
    );
}
