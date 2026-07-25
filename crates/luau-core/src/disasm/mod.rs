use crate::parser::opcodes::{builtin_name, LuauOpcode};
use crate::parser::types::*;

/// Disassemble an entire chunk into human-readable text
pub fn disassemble(chunk: &Chunk, show_debug: bool) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "; Luau bytecode v{} | {} protos | {} strings\n\n",
        chunk.version,
        chunk.protos.len(),
        chunk.strings.len()
    ));

    for (i, proto) in chunk.protos.iter().enumerate() {
        let is_main = i == chunk.main_proto as usize;
        disassemble_proto(&mut out, chunk, proto, i, is_main, show_debug);
        out.push('\n');
    }
    out
}

fn disassemble_proto(
    out: &mut String,
    chunk: &Chunk,
    proto: &Proto,
    index: usize,
    is_main: bool,
    show_debug: bool,
) {
    let name = proto
        .debug_name
        .as_deref()
        .unwrap_or(if is_main { "<main>" } else { "<anonymous>" });

    out.push_str(&format!(
        "; === Proto {} \"{}\" {} ===\n",
        index,
        name,
        if is_main { "(main)" } else { "" }
    ));
    out.push_str(&format!(
        "; params={} stack={} upvalues={} vararg={} line={}\n",
        proto.num_params,
        proto.max_stack_size,
        proto.num_upvalues,
        proto.is_vararg,
        proto.line_defined,
    ));

    // Constants
    if !proto.constants.is_empty() {
        out.push_str(&format!("; constants ({}):\n", proto.constants.len()));
        for (i, k) in proto.constants.iter().enumerate() {
            out.push_str(&format!(";   K{}: {}\n", i, k.display(&chunk.strings)));
        }
    }

    // Child protos
    if !proto.child_protos.is_empty() {
        out.push_str(&format!("; child protos: {:?}\n", proto.child_protos));
    }

    out.push('\n');

    // Instructions
    let code = &proto.code;
    let mut pc = 0usize;
    while pc < code.len() {
        let insn = code[pc];
        let op = LuauOpcode::from_u8(insn_op(insn));
        let a = insn_a(insn);
        let b = insn_b(insn);
        let c = insn_c(insn);
        let d = insn_d(insn);
        let e = insn_e(insn);

        // Line info
        let line_str = if show_debug {
            proto
                .line_info
                .as_ref()
                .and_then(|li| li.lines.get(pc))
                .map(|l| format!("[L{}] ", l))
                .unwrap_or_default()
        } else {
            String::new()
        };

        let aux = if op.has_aux() && pc + 1 < code.len() {
            Some(code[pc + 1])
        } else {
            None
        };

        let desc = format_instruction(proto, &chunk.strings, op, a, b, c, d, e, aux);
        let raw_byte = insn_op(insn);
        out.push_str(&format!(
            "  {:>4}: [0x{:02X}] {}{:<16} {}\n",
            pc,
            raw_byte,
            line_str,
            op.name(),
            desc
        ));

        pc += 1;
        if op.has_aux() {
            pc += 1; // skip AUX word
        }
        // NEWCLOSURE is followed by CAPTURE instructions, but those are separate opcodes
    }
}

fn format_instruction(
    proto: &Proto,
    strings: &[String],
    op: LuauOpcode,
    a: u8,
    b: u8,
    c: u8,
    d: i16,
    e: i32,
    aux: Option<u32>,
) -> String {
    match op {
        LuauOpcode::Nop | LuauOpcode::Break | LuauOpcode::Deprecated61 => String::new(),

        LuauOpcode::LoadNil => format!("R{}", a),
        LuauOpcode::LoadB => {
            if c > 0 {
                format!("R{} {} +{}", a, b != 0, c)
            } else {
                format!("R{} {}", a, b != 0)
            }
        }
        LuauOpcode::LoadN => format!("R{} {}", a, d),
        LuauOpcode::LoadK => {
            let k = const_str(proto, d as u32);
            format!("R{} K{} ; {}", a, d, k)
        }
        LuauOpcode::Move => format!("R{} R{}", a, b),

        LuauOpcode::GetGlobal => {
            let name = aux.map(|ax| const_str_or_string(proto, strings, ax)).unwrap_or_default();
            format!("R{} ; {}", a, name)
        }
        LuauOpcode::SetGlobal => {
            let name = aux.map(|ax| const_str_or_string(proto, strings, ax)).unwrap_or_default();
            format!("R{} ; {}", a, name)
        }

        LuauOpcode::GetUpval => format!("R{} U{}", a, b),
        LuauOpcode::SetUpval => format!("R{} U{}", a, b),
        LuauOpcode::CloseUpvals => format!("R{}", a),

        LuauOpcode::GetImport => {
            let k = const_str(proto, d as u32);
            format!("R{} ; {}", a, k)
        }

        LuauOpcode::GetTable => format!("R{} R{}[R{}]", a, b, c),
        LuauOpcode::SetTable => format!("R{}[R{}] R{}", b, c, a),
        LuauOpcode::GetTableKS => {
            let key = aux.map(|ax| const_str_or_string(proto, strings, ax)).unwrap_or_default();
            format!("R{} R{}.{}", a, b, key)
        }
        LuauOpcode::SetTableKS => {
            let key = aux.map(|ax| const_str_or_string(proto, strings, ax)).unwrap_or_default();
            format!("R{}.{} R{}", b, key, a)
        }
        LuauOpcode::GetTableN => format!("R{} R{}[{}]", a, b, c as u32 + 1),
        LuauOpcode::SetTableN => format!("R{}[{}] R{}", b, c as u32 + 1, a),

        LuauOpcode::NewClosure => format!("R{} P{}", a, d),
        LuauOpcode::DupClosure => {
            let k = const_str(proto, d as u32);
            format!("R{} K{} ; {}", a, d, k)
        }

        LuauOpcode::NameCall => {
            let name = aux.map(|ax| const_str_or_string(proto, strings, ax)).unwrap_or_default();
            format!("R{} R{}:{}", a, a + 1, name)
        }

        LuauOpcode::Call => {
            let nargs = if b == 0 {
                "vararg".to_string()
            } else {
                format!("{}", b - 1)
            };
            let nresults = if c == 0 {
                "multret".to_string()
            } else {
                format!("{}", c - 1)
            };
            format!("R{} args={} results={}", a, nargs, nresults)
        }
        LuauOpcode::Return => {
            if b == 0 {
                format!("R{} multret", a)
            } else if b == 1 {
                "".to_string()
            } else {
                format!("R{}..R{}", a, a as u16 + b as u16 - 2)
            }
        }

        LuauOpcode::Jump => format!("-> {}", (d as i32) + pc_placeholder()),
        LuauOpcode::JumpBack => format!("-> {}", (d as i32) + pc_placeholder()),
        LuauOpcode::JumpX => format!("-> {}", e + pc_placeholder()),
        LuauOpcode::JumpIf => format!("R{} -> +{}", a, d),
        LuauOpcode::JumpIfNot => format!("R{} -> +{}", a, d),

        LuauOpcode::JumpIfEq | LuauOpcode::JumpIfNotEq => {
            let rhs = aux.unwrap_or(0);
            let op_str = if matches!(op, LuauOpcode::JumpIfEq) { "==" } else { "~=" };
            format!("R{} {} R{} -> +{}", a, op_str, rhs, d)
        }
        LuauOpcode::JumpIfLE | LuauOpcode::JumpIfNotLE => {
            let rhs = aux.unwrap_or(0);
            let op_str = if matches!(op, LuauOpcode::JumpIfLE) { "<=" } else { ">" };
            format!("R{} {} R{} -> +{}", a, op_str, rhs, d)
        }
        LuauOpcode::JumpIfLT | LuauOpcode::JumpIfNotLT => {
            let rhs = aux.unwrap_or(0);
            let op_str = if matches!(op, LuauOpcode::JumpIfLT) { "<" } else { ">=" };
            format!("R{} {} R{} -> +{}", a, op_str, rhs, d)
        }

        LuauOpcode::JumpXEqKNil => {
            let not = if aux.unwrap_or(0) & 0x80000000 != 0 { "not " } else { "" };
            format!("R{} {}== nil -> +{}", a, not, d)
        }
        LuauOpcode::JumpXEqKB => {
            let aux_val = aux.unwrap_or(0);
            let val = (aux_val & 1) != 0;
            let not = if aux_val & 0x80000000 != 0 { "not " } else { "" };
            format!("R{} {}== {} -> +{}", a, not, val, d)
        }
        LuauOpcode::JumpXEqKN | LuauOpcode::JumpXEqKS => {
            let aux_val = aux.unwrap_or(0);
            let kidx = aux_val & 0x00FFFFFF;
            let not = if aux_val & 0x80000000 != 0 { "not " } else { "" };
            let k = const_str(proto, kidx);
            format!("R{} {}== K{} -> +{} ; {}", a, not, kidx, d, k)
        }

        // Arithmetic
        LuauOpcode::Add => format!("R{} R{} + R{}", a, b, c),
        LuauOpcode::Sub => format!("R{} R{} - R{}", a, b, c),
        LuauOpcode::Mul => format!("R{} R{} * R{}", a, b, c),
        LuauOpcode::Div => format!("R{} R{} / R{}", a, b, c),
        LuauOpcode::Mod => format!("R{} R{} % R{}", a, b, c),
        LuauOpcode::Pow => format!("R{} R{} ^ R{}", a, b, c),
        LuauOpcode::IDiv => format!("R{} R{} // R{}", a, b, c),

        LuauOpcode::AddK => format!("R{} R{} + K{} ; {}", a, b, c, const_str(proto, c as u32)),
        LuauOpcode::SubK => format!("R{} R{} - K{} ; {}", a, b, c, const_str(proto, c as u32)),
        LuauOpcode::MulK => format!("R{} R{} * K{} ; {}", a, b, c, const_str(proto, c as u32)),
        LuauOpcode::DivK => format!("R{} R{} / K{} ; {}", a, b, c, const_str(proto, c as u32)),
        LuauOpcode::ModK => format!("R{} R{} % K{} ; {}", a, b, c, const_str(proto, c as u32)),
        LuauOpcode::PowK => format!("R{} R{} ^ K{} ; {}", a, b, c, const_str(proto, c as u32)),
        LuauOpcode::IDivK => format!("R{} R{} // K{} ; {}", a, b, c, const_str(proto, c as u32)),

        LuauOpcode::SubRK => format!("R{} K{} - R{} ; {}", a, b, c, const_str(proto, b as u32)),
        LuauOpcode::DivRK => format!("R{} K{} / R{} ; {}", a, b, c, const_str(proto, b as u32)),

        LuauOpcode::And => format!("R{} R{} and R{}", a, b, c),
        LuauOpcode::Or => format!("R{} R{} or R{}", a, b, c),
        LuauOpcode::AndK => format!("R{} R{} and K{}", a, b, c),
        LuauOpcode::OrK => format!("R{} R{} or K{}", a, b, c),

        LuauOpcode::Concat => format!("R{} R{}..R{}", a, b, c),
        LuauOpcode::Not => format!("R{} not R{}", a, b),
        LuauOpcode::Minus => format!("R{} -R{}", a, b),
        LuauOpcode::Length => format!("R{} #R{}", a, b),

        LuauOpcode::NewTable => {
            let hash_size = aux.unwrap_or(0);
            format!("R{} array={} hash={}", a, b, hash_size)
        }
        LuauOpcode::DupTable => format!("R{} K{}", a, d),
        LuauOpcode::SetList => {
            let offset = aux.unwrap_or(0);
            // C==0 is the legitimate "up to top of stack" encoding; the closed
            // form b+c-1 underflows for it.
            if c == 0 {
                format!("R{} R{}..top offset={}", a, b, offset)
            } else {
                format!("R{} R{}..R{} offset={}", a, b, b as u16 + c as u16 - 1, offset)
            }
        }

        LuauOpcode::ForNPrep => format!("R{} -> +{}", a, d),
        LuauOpcode::ForNLoop => format!("R{} -> +{}", a, d),
        LuauOpcode::ForGPrep => format!("R{} -> +{}", a, d),
        LuauOpcode::ForGLoop => {
            let raw_aux = aux.unwrap_or(0);
            let nresults = raw_aux & 0x7FFFFFFF;
            let inext = raw_aux & 0x80000000 != 0;
            if inext {
                format!("R{} -> +{} nresults={} [inext]", a, d, nresults)
            } else {
                format!("R{} -> +{} nresults={}", a, d, nresults)
            }
        }
        LuauOpcode::ForGPrepINext => format!("R{} -> +{}", a, d),
        LuauOpcode::ForGPrepNext => format!("R{} -> +{}", a, d),

        LuauOpcode::GetVarargs => {
            if b == 0 {
                format!("R{} all", a)
            } else {
                format!("R{} count={}", a, b - 1)
            }
        }
        LuauOpcode::PrepVarargs => format!("nfixed={}", a),

        LuauOpcode::LoadKX => {
            let kidx = aux.unwrap_or(0);
            format!("R{} K{} ; {}", a, kidx, const_str(proto, kidx))
        }

        LuauOpcode::FastCall => format!("{} skip={}", builtin_name(a), c),
        LuauOpcode::FastCall1 => format!("{} R{} skip={}", builtin_name(a), b, c),
        LuauOpcode::FastCall2 => {
            let arg2 = aux.unwrap_or(0);
            format!("{} R{} R{} skip={}", builtin_name(a), b, arg2, c)
        }
        LuauOpcode::FastCall2K => {
            let kidx = aux.unwrap_or(0);
            format!("{} R{} K{} skip={}", builtin_name(a), b, kidx, c)
        }
        LuauOpcode::FastCall3 => {
            let aux_val = aux.unwrap_or(0);
            let arg2 = (aux_val & 0xFF) as u8;
            let arg3 = ((aux_val >> 8) & 0xFF) as u8;
            format!("{} R{} R{} R{} skip={}", builtin_name(a), b, arg2, arg3, c)
        }

        LuauOpcode::Capture => {
            let kind = match a {
                0 => "val",
                1 => "ref",
                2 => "upval",
                _ => "?",
            };
            format!("{} {}", kind, b)
        }

        LuauOpcode::Coverage => format!("hits={}", e),
        LuauOpcode::NativeCall => String::new(),
        // Deprecated61 is already handled in the Nop | Break | Deprecated61 arm above.

        LuauOpcode::Band  => format!("R{} = R{} & R{}", a, b, c),
        LuauOpcode::Bor   => format!("R{} = R{} | R{}", a, b, c),
        LuauOpcode::Bxor  => format!("R{} = R{} ~ R{}", a, b, c),
        LuauOpcode::Bnot  => format!("R{} = ~R{}", a, b),
        LuauOpcode::Shl   => format!("R{} = R{} << R{}", a, b, c),
        LuauOpcode::Shr   => format!("R{} = R{} >> R{}", a, b, c),
        LuauOpcode::Bandk => format!("R{} = R{} & K{}", a, b, c),
        LuauOpcode::Bork  => format!("R{} = R{} | K{}", a, b, c),

        LuauOpcode::RbxExt92 => format!("R{} = __rbx92(R{})", a, b),
        LuauOpcode::RbxExt93 => format!("R{} = __rbx93(R{})", a, b),
        LuauOpcode::RbxExt94 => format!("R{} = __rbx94(R{})", a, b),
        LuauOpcode::RbxExt95 => format!("R{} = __rbx95(R{}, R{})", a, b, c),
        LuauOpcode::RbxExt96 => format!("R{} = __rbx96(R{})", a, b),
        LuauOpcode::RbxExt97 => format!("R{} = __rbx97(R{})", a, b),
        LuauOpcode::RbxExt98 => format!("R{} = __rbx98(R{})", a, b),
        LuauOpcode::RbxExt99 => format!("R{} = __rbx99(R{}, R{})", a, b, c),
        LuauOpcode::RbxExt100 => format!("R{} = __rbx100(R{}, R{})", a, b, c),
        LuauOpcode::RbxExt101 => format!("R{} = __rbx101(R{}, R{})", a, b, c),
        LuauOpcode::RbxExt102 => format!("R{} = __rbx102(R{}, R{})", a, b, c),
        LuauOpcode::RbxExt103 => format!("R{} = __rbx103(R{}, R{})", a, b, c),
        LuauOpcode::RbxExt104 => format!("R{} = __rbx104(R{}, R{})", a, b, c),
        LuauOpcode::RbxExt105 => format!("R{} = __rbx105(R{}, R{})", a, b, c),

        LuauOpcode::Unknown => format!("0x{:08X}", ((a as u32) << 8) | (b as u32) << 16 | (c as u32) << 24),
    }
}

fn const_str(proto: &Proto, idx: u32) -> String {
    proto
        .constants
        .get(idx as usize)
        .map(|k| match k {
            Constant::String(s) => format!("\"{}\"", s),
            Constant::Number(n) => {
                if *n == (*n as i64) as f64 && n.is_finite() {
                    format!("{}", *n as i64)
                } else {
                    format!("{}", n)
                }
            }
            Constant::Boolean(b) => b.to_string(),
            Constant::Nil => "nil".to_string(),
            Constant::Import(v) => format!("import(0x{:08X})", v),
            Constant::Closure(p) => format!("proto({})", p),
            Constant::Vector(x, y, z, _) => format!("vec({},{},{})", x, y, z),
            Constant::Table(entries) => format!("table({})", entries.len()),
        })
        .unwrap_or_else(|| format!("K({})", idx))
}

/// Look up a string from either proto.constants or chunk.strings.
/// Per the Luau VM, AUX values for GetGlobal, SetGlobal, GetTableKS,
/// SetTableKS, NameCall are 0-based indices into proto.constants.
/// Also tries 1-based indexing as a fallback for edge cases.
fn const_str_or_string(proto: &Proto, strings: &[String], idx: u32) -> String {
    // Primary: proto.constants with 0-based indexing (matches Luau VM: VM_KV(aux))
    if let Some(k) = proto.constants.get(idx as usize) {
        return match k {
            Constant::String(s) => format!("\"{}\"", s),
            _ => const_str(proto, idx),
        };
    }

    // Fallback 1: chunk.strings with 0-based indexing
    if let Some(s) = strings.get(idx as usize) {
        return format!("\"{}\"", s);
    }

    // Fallback 2: proto.constants with 1-based indexing (idx-1)
    if idx > 0 {
        if let Some(k) = proto.constants.get((idx as usize) - 1) {
            if let Constant::String(s) = k {
                return format!("\"{}\"", s);
            }
        }
    }

    // Fallback 3: chunk.strings with 1-based indexing (idx-1)
    if idx > 0 {
        if let Some(s) = strings.get((idx as usize) - 1) {
            return format!("\"{}\"", s);
        }
    }

    format!("<unknown string {}>", idx)
}

fn pc_placeholder() -> i32 {
    // Used as a placeholder since we don't have actual PC in the format function
    // The actual target address should be computed by the caller
    0
}
