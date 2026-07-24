//! Luau source for the embedded VM dispatcher + decryption bootstrap.
//!
//! The dispatcher is **generated programmatically** from a Rust-side table of
//! per-opcode handler bodies. This unlocks two obfuscation moves:
//!   * Phase 3: emit dispatch as a balanced binary if-tree instead of a flat
//!     `if/elseif` chain, so the opcode→handler mapping is harder to grep.
//!   * Phase 5: permute opcode IDs per build and rewrite the dispatch tree to
//!     match — without ever touching the encoder.

use crate::vm::opcodes::Op;

/// Common helpers (bit32 aliases + xs32 PRNG step) used by both the decrypt
/// bootstrap and the Phase 7B operand-key derivation. Always emitted —
/// they're cheap and let downstream sections share local names.
pub const COMMON_HELPERS: &str = r#"
local _bxor = bit32.bxor
local _band = bit32.band
local _lshift = bit32.lshift
local _rshift = bit32.rshift
local _str_byte = string.byte
local _str_char = string.char
local _t_concat = table.concat

local function _xs32(s)
    s = _bxor(s, _lshift(s, 13))
    s = _bxor(s, _rshift(s, 17))
    s = _bxor(s, _lshift(s, 5))
    return s
end

-- Phase 7C: recovered master seed lives here so the dispatcher's PushConst
-- handler can derive per-string keys at lazy-decryption time.
local _master_seed = 0
"#;

/// Phase 2 decryption bootstrap. Assumes [`COMMON_HELPERS`] has already been
/// emitted (so `_xs32`, `_bxor`, `_str_byte`, etc. are local-scoped above).
pub const DECRYPT_BOOTSTRAP: &str = r#"

local function _xor_buf(buf, state)
    local out = {}
    for j = 1, #buf do
        state = _xs32(state)
        out[j] = _str_char(_bxor(_str_byte(buf, j), _band(state, 255)))
    end
    return _t_concat(out), state
end

local function _hash_module()
    local h = 0
    for i = 1, #_P do
        local c = _P[i].c
        for j = 1, #c do
            h = _bxor(h, _str_byte(c, j))
            h = _xs32(h + 1)
        end
    end
    for i = 1, #_C do
        local v = _C[i]
        local s
        if type(v) == "string" then
            s = v
        elseif type(v) == "table" then
            s = v[1]  -- Phase 7C lazy-encrypted ciphertext lives in v[1]
        end
        if s then
            for j = 1, #s do
                h = _bxor(h, _str_byte(s, j))
                h = _xs32(h + 1)
            end
        end
    end
    return h
end

local function _decrypt_module(obfuscated_seed)
    -- Recover the real seed by XOR with the ciphertext hash. Tamper any
    -- encrypted byte -> hash changes -> recovered seed wrong -> decryption
    -- produces garbage. The integrity check is implicit in execution.
    local state = _bxor(obfuscated_seed, _hash_module())
    if state == 0 then state = 1 end
    _master_seed = state  -- stash for Phase 7C lazy string decryption
    for i = 1, #_P do
        local plain
        plain, state = _xor_buf(_P[i].c, state)
        _P[i].c = plain
    end
    for i = 1, #_C do
        -- Only bulk-decrypt entries that were emitted as plain Lua strings.
        -- Lazy-encrypted strings live inside `{...}` tables and stay
        -- encrypted until the dispatcher's PushConst hits them.
        if type(_C[i]) == "string" then
            local plain
            plain, state = _xor_buf(_C[i], state)
            _C[i] = plain
        end
    end
end
"#;

/// Build the dispatcher Luau source.
///
/// `perm[canonical_opcode_byte] = encoded_opcode_byte` — when the encoder
/// emits canonical op byte `c`, the bytecode stream actually contains
/// `perm[c]`. For Phase 3 callers pass an identity permutation. Phase 5
/// passes a random permutation and the dispatcher rebuilds its tree to
/// dispatch on the encoded byte.
pub fn build_dispatcher(perm: &[u8; 256]) -> String {
    let mut out = String::new();
    out.push_str(PREAMBLE);

    // Build (encoded_byte, handler_body) pairs and sort by encoded_byte so
    // the binary tree splits cleanly.
    let handlers = canonical_handlers();
    let mut pairs: Vec<(u8, &'static str)> = handlers
        .iter()
        .map(|(canon_op, body)| (perm[*canon_op as usize], *body))
        .collect();
    pairs.sort_by_key(|(b, _)| *b);

    emit_binary_tree(&mut out, &pairs, 1);
    out.push_str(EPILOGUE);
    out
}

/// Identity permutation — `id[c] = c`. Phase 3 default.
pub fn identity_perm() -> [u8; 256] {
    let mut p = [0u8; 256];
    for (i, v) in p.iter_mut().enumerate() {
        *v = i as u8;
    }
    p
}

fn canonical_handlers() -> Vec<(u8, &'static str)> {
    vec![
        (Op::PushNil as u8,         H_PUSH_NIL),
        (Op::PushTrue as u8,        H_PUSH_TRUE),
        (Op::PushFalse as u8,       H_PUSH_FALSE),
        (Op::PushConst as u8,       H_PUSH_CONST),
        (Op::Pop as u8,             H_POP),
        (Op::Dup as u8,             H_DUP),
        (Op::LoadLocal as u8,       H_LOAD_LOCAL),
        (Op::StoreLocal as u8,      H_STORE_LOCAL),
        (Op::LoadUpval as u8,       H_LOAD_UPVAL),
        (Op::StoreUpval as u8,      H_STORE_UPVAL),
        (Op::LoadGlobal as u8,      H_LOAD_GLOBAL),
        (Op::StoreGlobal as u8,     H_STORE_GLOBAL),
        (Op::NewTable as u8,        H_NEW_TABLE),
        (Op::GetField as u8,        H_GET_FIELD),
        (Op::SetField as u8,        H_SET_FIELD),
        (Op::GetIndex as u8,        H_GET_INDEX),
        (Op::SetIndex as u8,        H_SET_INDEX),
        (Op::AppendArray as u8,     H_APPEND_ARRAY),
        (Op::SetListIndex as u8,    H_SET_LIST_INDEX),
        (Op::BinOp as u8,           H_BIN_OP),
        (Op::UnOp as u8,            H_UN_OP),
        (Op::Jump as u8,            H_JUMP),
        (Op::JumpIfFalse as u8,     H_JUMP_IF_FALSE),
        (Op::JumpIfTrue as u8,      H_JUMP_IF_TRUE),
        (Op::JumpIfFalseKeep as u8, H_JUMP_IF_FALSE_KEEP),
        (Op::JumpIfTrueKeep as u8,  H_JUMP_IF_TRUE_KEEP),
        (Op::Call as u8,            H_CALL),
        (Op::MethodPrep as u8,      H_METHOD_PREP),
        (Op::Return as u8,          H_RETURN),
        (Op::Closure as u8,         H_CLOSURE),
        (Op::ClosureUpval as u8,    H_CLOSURE_UPVAL_ORPHAN),
        (Op::Vararg as u8,          H_VARARG),
    ]
}

/// Recursively emit a balanced binary if-tree dispatching on `op`.
fn emit_binary_tree(out: &mut String, pairs: &[(u8, &str)], indent: usize) {
    if pairs.is_empty() {
        push_indent(out, indent);
        out.push_str("_error(\"empty dispatch\")\n");
        return;
    }
    if pairs.len() == 1 {
        let (byte, body) = pairs[0];
        push_indent(out, indent);
        out.push_str(&format!("if op == {byte} then\n"));
        for line in body.lines() {
            push_indent(out, indent + 1);
            out.push_str(line);
            out.push('\n');
        }
        push_indent(out, indent);
        out.push_str("else\n");
        push_indent(out, indent + 1);
        out.push_str("_error(\"bad opcode \" .. op .. \" @ pc \" .. (pc - 5))\n");
        push_indent(out, indent);
        out.push_str("end\n");
        return;
    }
    let mid = pairs.len() / 2;
    let pivot = pairs[mid].0;
    push_indent(out, indent);
    out.push_str(&format!("if op < {pivot} then\n"));
    emit_binary_tree(out, &pairs[..mid], indent + 1);
    push_indent(out, indent);
    out.push_str("else\n");
    emit_binary_tree(out, &pairs[mid..], indent + 1);
    push_indent(out, indent);
    out.push_str("end\n");
}

fn push_indent(out: &mut String, n: usize) {
    for _ in 0..n {
        out.push_str("    ");
    }
}

const PREAMBLE: &str = r#"
local _table_unpack = table.unpack or unpack
local _type = type
local _error = error
local _env = _ENV or getfenv and getfenv() or _G

-- Phase 7C: every string-using handler funnels constant lookups through
-- this helper. If the slot still holds a table (lazy ciphertext wrapper),
-- decrypt once and cache the plaintext back into `_C` so subsequent reads
-- are direct table lookups.
local function _get_const(k)
    local v = _C[k + 1]
    if _type(v) == "table" then
        local cipher = v[1]
        local s_state = _xs32(_bxor(_master_seed, k))
        if s_state == 0 then s_state = 1 end
        local out = {}
        for j = 1, #cipher do
            s_state = _xs32(s_state)
            out[j] = _str_char(_bxor(_str_byte(cipher, j), _band(s_state, 255)))
        end
        v = _t_concat(out)
        _C[k + 1] = v
    end
    return v
end

local _exec

-- Phase 7B operand-key derivation. When proto_key is 0 the key is also 0,
-- so the byte XOR below is a no-op and plain builds run identically.
local function _instr_key(proto_key, byte_offset)
    if proto_key == 0 then return 0 end
    local mixed = _band(_xs32(byte_offset) + 2654435769, 4294967295)
    return _bxor(mixed, proto_key)
end

local function _read_operands(code, instr_start, proto_key)
    local k = _instr_key(proto_key, instr_start - 1)
    local key_lo = _band(k, 255)
    local key_hi = _band(_rshift(k, 8), 255)

    local lo = _bxor(_str_byte(code, instr_start + 1), key_lo)
    local hi = _bxor(_str_byte(code, instr_start + 2), key_hi)
    local a = lo + hi * 256
    if a >= 32768 then a = a - 65536 end

    lo = _bxor(_str_byte(code, instr_start + 3), key_lo)
    hi = _bxor(_str_byte(code, instr_start + 4), key_hi)
    local b = lo + hi * 256
    if b >= 32768 then b = b - 65536 end

    return a, b
end

_exec = function(proto_idx, args, upvals)
    local proto = _P[proto_idx + 1]
    local code = proto.c
    local proto_key = proto.k or 0
    local np = proto.np
    local locals = {}
    for i = 1, np do
        locals[i] = args[i]
    end
    local varargs = {}
    if proto.va and #args > np then
        for i = np + 1, #args do
            varargs[#varargs + 1] = args[i]
        end
    end
    upvals = upvals or {}

    local stack = {}
    local sp = 0
    local pc = 1
    local code_len = #code

    while pc <= code_len do
        local op = _str_byte(code, pc)
        local a, b = _read_operands(code, pc, proto_key)
        pc = pc + 5

"#;

const EPILOGUE: &str = r#"
    end
    return {}
end
"#;

// -- per-opcode handler bodies. Each one is a self-contained Luau snippet
// -- that mutates `pc`, `sp`, `stack`, `locals`, `upvals`, `varargs`. Operand
// -- registers `a` and `b` are already decoded.

const H_PUSH_NIL: &str = "sp = sp + 1\nstack[sp] = nil";
const H_PUSH_TRUE: &str = "sp = sp + 1\nstack[sp] = true";
const H_PUSH_FALSE: &str = "sp = sp + 1\nstack[sp] = false";
const H_PUSH_CONST: &str = "sp = sp + 1\nstack[sp] = _get_const(a)";
const H_POP: &str = "sp = sp - a";
const H_DUP: &str = "sp = sp + 1\nstack[sp] = stack[sp - 1]";
const H_LOAD_LOCAL: &str = "sp = sp + 1\nstack[sp] = locals[a + 1]";
const H_STORE_LOCAL: &str = "locals[a + 1] = stack[sp]\nsp = sp - 1";
const H_LOAD_UPVAL: &str = "local uv = upvals[a + 1]\nsp = sp + 1\nstack[sp] = uv[1][uv[2]]";
const H_STORE_UPVAL: &str = "local uv = upvals[a + 1]\nuv[1][uv[2]] = stack[sp]\nsp = sp - 1";
const H_LOAD_GLOBAL: &str = "sp = sp + 1\nstack[sp] = _env[_get_const(a)]";
const H_STORE_GLOBAL: &str = "_env[_get_const(a)] = stack[sp]\nsp = sp - 1";
const H_NEW_TABLE: &str = "sp = sp + 1\nstack[sp] = {}";
const H_GET_FIELD: &str = "local t = stack[sp]\nstack[sp] = t[_get_const(a)]";
const H_SET_FIELD: &str = "local v = stack[sp]\nlocal t = stack[sp - 1]\nt[_get_const(a)] = v\nsp = sp - 2";
const H_GET_INDEX: &str = "local k = stack[sp]\nlocal t = stack[sp - 1]\nstack[sp - 1] = t[k]\nsp = sp - 1";
const H_SET_INDEX: &str = "local v = stack[sp]\nlocal k = stack[sp - 1]\nlocal t = stack[sp - 2]\nt[k] = v\nsp = sp - 3";
const H_APPEND_ARRAY: &str = "local v = stack[sp]\nlocal t = stack[sp - 1]\nt[#t + 1] = v\nsp = sp - 1";
const H_SET_LIST_INDEX: &str = "local v = stack[sp]\nlocal t = stack[sp - 1]\nt[a] = v\nsp = sp - 1";

const H_BIN_OP: &str = r#"local rhs = stack[sp]
local lhs = stack[sp - 1]
sp = sp - 1
if a == 0 then stack[sp] = lhs + rhs
elseif a == 1 then stack[sp] = lhs - rhs
elseif a == 2 then stack[sp] = lhs * rhs
elseif a == 3 then stack[sp] = lhs / rhs
elseif a == 4 then stack[sp] = lhs % rhs
elseif a == 5 then stack[sp] = lhs ^ rhs
elseif a == 6 then stack[sp] = lhs .. rhs
elseif a == 7 then stack[sp] = lhs == rhs
elseif a == 8 then stack[sp] = lhs ~= rhs
elseif a == 9 then stack[sp] = lhs < rhs
elseif a == 10 then stack[sp] = lhs <= rhs
elseif a == 11 then stack[sp] = lhs > rhs
elseif a == 12 then stack[sp] = lhs >= rhs
elseif a == 13 then stack[sp] = lhs // rhs
else _error("bad binop") end"#;

const H_UN_OP: &str = r#"local x = stack[sp]
if a == 0 then stack[sp] = -x
elseif a == 1 then stack[sp] = not x
elseif a == 2 then stack[sp] = #x
else _error("bad unop") end"#;

const H_JUMP: &str = "pc = pc + a * 5";
const H_JUMP_IF_FALSE: &str = "local v = stack[sp]\nsp = sp - 1\nif not v then pc = pc + a * 5 end";
const H_JUMP_IF_TRUE: &str = "local v = stack[sp]\nsp = sp - 1\nif v then pc = pc + a * 5 end";
const H_JUMP_IF_FALSE_KEEP: &str = "if not stack[sp] then pc = pc + a * 5 end";
const H_JUMP_IF_TRUE_KEEP: &str = "if stack[sp] then pc = pc + a * 5 end";

const H_CALL: &str = r#"local nargs = a
local nret = b
local fn_slot = sp - nargs
local fn = stack[fn_slot]
local cargs = {}
for i = 1, nargs do
    cargs[i] = stack[fn_slot + i]
end
sp = fn_slot - 1
local results
if _type(fn) == "function" then
    results = {fn(_table_unpack(cargs, 1, nargs))}
elseif _type(fn) == "table" and fn[1] then
    results = _exec(fn[1], cargs, fn[2])
else
    _error("attempt to call a " .. _type(fn) .. " value")
end
if nret == -1 then
    for i = 1, #results do
        sp = sp + 1
        stack[sp] = results[i]
    end
else
    for i = 1, nret do
        sp = sp + 1
        stack[sp] = results[i]
    end
end"#;

const H_METHOD_PREP: &str = "local obj = stack[sp]\nstack[sp] = obj[_get_const(a)]\nsp = sp + 1\nstack[sp] = obj";

const H_RETURN: &str = r#"local n = a
local rets = {}
for i = 1, n do
    rets[i] = stack[sp - n + i]
end
return rets"#;

const H_CLOSURE: &str = r#"local pidx = a
local nup = b
local upvs = {}
for i = 1, nup do
    local kind, idx = _read_operands(code, pc, proto_key)
    pc = pc + 5
    if kind == 0 then
        upvs[i] = {locals, idx + 1}
    else
        upvs[i] = upvals[idx + 1]
    end
end
sp = sp + 1
stack[sp] = {pidx, upvs}"#;

const H_CLOSURE_UPVAL_ORPHAN: &str =
    "_error(\"orphan ClosureUpval at pc \" .. (pc - 5))";

const H_VARARG: &str = r#"local count = a
if count == -1 then
    for i = 1, #varargs do
        sp = sp + 1
        stack[sp] = varargs[i]
    end
else
    for i = 1, count do
        sp = sp + 1
        stack[sp] = varargs[i]
    end
end"#;
