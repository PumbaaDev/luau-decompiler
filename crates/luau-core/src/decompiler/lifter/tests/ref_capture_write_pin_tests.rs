//! A register captured by REFERENCE is the same variable for every write
//! between its CAPTURE and the CLOSEUPVALS that releases it. Writes in that
//! span must be emitted as assignments to the captured binding — never
//! copy-propagated away, never shadow-declared under a fresh name.
//!
//! Found via the 218-defect census (docs/DEFECT_CENSUS_218.md, mechanism 1b).
//! `279_ReplicatedStorage_LocalFX_JellyBeanToss`:
//!
//! ```text
//!  78: LOADNIL    R12
//!  80: NEWCLOSURE R14 P0
//!  83: CAPTURE    ref 12          ; child U2 — child does U2:Disconnect()
//! ...
//! 102: CALL       R16 ...         ; RenderStepped:Connect(closure)
//! 103: MOVE       R12 R16         ; conn = <result>   ← THE WRITE
//! 104: CLOSEUPVALS R8
//! ```
//!
//! The capture guard correctly declared `local result3` and the child read
//! `result3:Disconnect()` — but the MOVE at 103 copy-propagated the call
//! result past the binding (`regs[12] = Name("RenderStepped")`, no statement),
//! so `result3` was never assigned: the connection could never disconnect,
//! and the semantic checker flagged `declared_never_assigned`.
//!
//! The fix pins the register to its bound name for the [CAPTURE, CLOSEUPVALS)
//! span via `ctx.pin_reg_name`, which the MOVE handler's `pinned_dest` arm and
//! `store_complex`'s pinned-loop-carried arm already honor by materializing
//! the write.

use crate::decompiler::{decompile_proto, DecompileContext};
use crate::parser::types::{Chunk, Constant, Proto};

// Canonical Luau opcode bytes (identity opmap, Chunk.version = 6).
const OP_LOADNIL: u8 = 2;
const OP_MOVE: u8 = 6;
const OP_GETUPVAL: u8 = 9;
const OP_GETIMPORT: u8 = 12;
const OP_NEWCLOSURE: u8 = 19;
const OP_NAMECALL: u8 = 20;
const OP_CALL: u8 = 21;
const OP_RETURN: u8 = 22;
const OP_CLOSEUPVALS: u8 = 11;
const OP_CAPTURE: u8 = 70;

fn insn_ad(op: u8, a: u8, d: i16) -> u32 {
    let du = d as u16 as u32;
    (op as u32) | ((a as u32) << 8) | (du << 16)
}

fn insn_abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
}

fn pack_import(ids: &[u32]) -> u32 {
    let count = (ids.len() as u32) & 0x3;
    let mut v = count << 30;
    if !ids.is_empty() {
        v |= (ids[0] & 0x3FF) << 20;
    }
    if ids.len() >= 2 {
        v |= (ids[1] & 0x3FF) << 10;
    }
    if ids.len() >= 3 {
        v |= ids[2] & 0x3FF;
    }
    v
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

/// The connection idiom in miniature:
///
/// ```text
/// parent:
///   0: LOADNIL    R0                    ; local conn
///   1: NEWCLOSURE R1, child#0
///   2: CAPTURE    ref R0               ; child U0
///   3: GETIMPORT  R2, K0 ("Signal")    ; AUX
///   5: NAMECALL   R2, R2 K1 "Connect"  ; AUX     R2:Connect(R4=closure)
///   7: MOVE       R4, R1
///   8: CALL       R2, args=2, results=1 → 1 value
///   9: MOVE       R0, R2               ; conn = <Connect result>  ← must EMIT
///  10: CLOSEUPVALS R0
///  11: RETURN     R0, 1
/// child (1 upvalue):
///   0: GETUPVAL  R0, U0
///   1: RETURN    R0, 2                 ; return conn
/// ```
fn connection_chunk() -> Chunk {
    let import_val = pack_import(&[2]);
    let parent_code = vec![
        insn_abc(OP_LOADNIL, 0, 0, 0),
        insn_ad(OP_NEWCLOSURE, 1, 0),
        insn_abc(OP_CAPTURE, 1, 0, 0),
        insn_ad(OP_GETIMPORT, 2, 0),
        import_val, // AUX
        insn_abc(OP_NAMECALL, 2, 2, 0),
        1u32, // AUX: K1 "Connect"
        insn_abc(OP_MOVE, 4, 1, 0),
        insn_abc(OP_CALL, 2, 3, 2),
        insn_abc(OP_MOVE, 0, 2, 0),
        insn_abc(OP_CLOSEUPVALS, 0, 0, 0),
        insn_abc(OP_RETURN, 0, 2, 0),
    ];
    let parent_constants = vec![
        Constant::Import(import_val),
        Constant::String("Connect".to_string()),
        Constant::String("Signal".to_string()),
    ];
    let child_code = vec![
        insn_abc(OP_GETUPVAL, 0, 0, 0),
        insn_abc(OP_RETURN, 0, 2, 0),
    ];
    let mut parent = proto(parent_code, parent_constants, 0, "connection_parent");
    parent.child_protos = vec![1];
    let child = proto(child_code, Vec::new(), 1, "child");
    Chunk {
        version: 6,
        types_version: 0,
        strings: vec!["Connect".to_string(), "Signal".to_string()],
        protos: vec![parent, child],
        main_proto: 0,
    }
}

/// The declared connection local and the later store must land on the SAME
/// name: exactly one binding, whose declaration precedes an assignment to it.
#[test]
fn write_into_open_ref_capture_hits_the_declared_binding() {
    let chunk = connection_chunk();
    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    // Find the name declared for the ref-captured register: the first
    // `local <name>` whose initializer is missing or nil (the LOADNIL
    // materialized by the capture guard).
    let conn_name = out
        .lines()
        .map(str::trim_start)
        .find_map(|l| {
            let rest = l.strip_prefix("local ")?;
            let name = rest
                .split(|c: char| c == ' ' || c == '=')
                .next()?
                .trim();
            let is_nil_init = rest.trim_end() == name
                || rest
                    .split_once('=')
                    .is_some_and(|(_, v)| v.trim() == "nil");
            (is_nil_init && !name.is_empty()).then(|| name.to_string())
        })
        .unwrap_or_else(|| panic!("no nil-initialized local found for the captured register:\n{out}"));

    let assign_prefix = format!("{conn_name} = ");
    assert!(
        out.lines().any(|l| l.trim_start().starts_with(&assign_prefix)),
        "the Connect-result store must be an assignment to the captured \
         binding `{conn_name}` — parking or shadow-declaring it strands the \
         closure's upvalue at nil:\n{out}"
    );
}

/// After CLOSEUPVALS the slot is rebindable again: a write past the close
/// must NOT be redirected into the old binding.
#[test]
fn write_after_closeupvals_is_not_redirected() {
    // Same shape, but the interesting write comes AFTER the CLOSEUPVALS and
    // stores a fresh import; it must not be forced into `conn`.
    let import_val = pack_import(&[2]);
    let parent_code = vec![
        insn_abc(OP_LOADNIL, 0, 0, 0),
        insn_ad(OP_NEWCLOSURE, 1, 0),
        insn_abc(OP_CAPTURE, 1, 0, 0),
        insn_abc(OP_CLOSEUPVALS, 0, 0, 0),
        insn_ad(OP_GETIMPORT, 2, 0),
        import_val, // AUX
        insn_abc(OP_MOVE, 0, 2, 0),
        insn_abc(OP_RETURN, 0, 2, 0),
    ];
    let parent_constants = vec![
        Constant::Import(import_val),
        Constant::String("Connect".to_string()),
        Constant::String("Signal".to_string()),
    ];
    let child_code = vec![
        insn_abc(OP_GETUPVAL, 0, 0, 0),
        insn_abc(OP_RETURN, 0, 2, 0),
    ];
    let mut parent = proto(parent_code, parent_constants, 0, "closed_parent");
    parent.child_protos = vec![1];
    let child = proto(child_code, Vec::new(), 1, "child");
    let chunk = Chunk {
        version: 6,
        types_version: 0,
        strings: vec!["Connect".to_string(), "Signal".to_string()],
        protos: vec![parent, child],
        main_proto: 0,
    };

    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    let conn_name = out
        .lines()
        .map(str::trim_start)
        .find_map(|l| {
            let rest = l.strip_prefix("local ")?;
            let name = rest
                .split(|c: char| c == ' ' || c == '=')
                .next()?
                .trim();
            let is_nil_init = rest.trim_end() == name
                || rest
                    .split_once('=')
                    .is_some_and(|(_, v)| v.trim() == "nil");
            (is_nil_init && !name.is_empty()).then(|| name.to_string())
        });

    if let Some(conn_name) = conn_name {
        let assign_prefix = format!("{conn_name} = Signal");
        assert!(
            !out.lines().any(|l| l.trim_start().starts_with(&assign_prefix)),
            "a write AFTER the CLOSEUPVALS is a rebindable slot, not the \
             captured variable — it must not be forced into `{conn_name}`:\n{out}"
        );
    }
}
