pub mod emit;
pub mod lifter;

use crate::ast::*;
use crate::parser::opcodes::LuauOpcode;
use crate::parser::types::*;

/// Hints about how a register is used, derived from a pre-pass over bytecode.
/// Used to generate more descriptive variable names when debug info is absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterHint {
    /// Function parameter at given index (0=first, 1=second, ...)
    Param(usize),
    /// Self parameter (first param of a method-like function)
    SelfParam,
    /// Result of a call to a named function/method
    CallResult(String),
    /// Numeric for loop counter variable
    NumericForVar,
    /// Generic for loop variable (first variable, e.g., key/index)
    GenericForKey,
    /// Generic for loop variable (second variable, e.g., value)
    GenericForVal,
    /// Register holds a closure/function value
    Closure,
    /// Register holds a newly created table
    Table,
    /// Register holds a value loaded from a known import path
    Import(String),
    /// Explicit name suggestion (highest priority). Used for
    /// `game:GetService("X")` → name the result `X`, and
    /// `require(Path.To.Module)` → name the result `Module`.
    Named(String),
}

/// Per-proto naming state, scoped so each proto gets its own clean namespace.
struct ProtoNaming {
    /// Suffix counters per prefix to generate unique names within a proto
    prefix_counts: std::collections::HashMap<String, u32>,
    /// Names already assigned to (reg, pc) pairs in this proto
    assigned: std::collections::HashMap<(u8, usize), String>,
    /// Phase B0.51C — stable-identity names keyed by (reg, StableHintKey).
    /// For hints that represent a stable identity per register (Param, SelfParam,
    /// NumericForVar, GenericForKey, GenericForVal), we cache the synthesized
    /// name here so that ALL reads of the same register at ANY pc produce the
    /// same name.
    ///
    /// This fixes a bug where reading param-register 0 multiple times (once at
    /// pc=0 for initialization, then at later pcs via code paths like
    /// `store_complex` / CALL destination / B0.49 shadow) caused the per-prefix
    /// counter to bump on each call.  With a single param the register's
    /// `Param(0)` hint resolved to prefix `"arg1"`, and each subsequent call
    /// incremented the counter → `"arg1"`, `"arg12"`, `"arg13"`, ….  Corpus
    /// symptom in `ModuleScript.lua`: `return 100 - arg12 + 1000 / arg12` where
    /// the param was actually `arg1`.
    stable_names: std::collections::HashMap<(u8, StableHintKey), String>,
    /// Register hints from the pre-pass. Each hint is tagged with the PC at
    /// which it was observed so that `synthesize_name` can pick the most
    /// recent hint <= the current read PC (registers get rewritten over a
    /// proto's lifetime; the hint relevant to a read is the most recent write
    /// that precedes it, not the globally-last hint).
    hints: std::collections::HashMap<u8, Vec<(usize, RegisterHint)>>,
}

/// Phase B0.51C — compact key encoding a stable-identity hint.  Used by
/// `ProtoNaming.stable_names` to memoize the synthesized name for a
/// (register, stable-hint-identity) pair.  See the doc comment on
/// `stable_names` for the motivating bug.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum StableHintKey {
    Param(u32),
    SelfParam,
    NumericForVar,
    GenericForKey,
    GenericForVal,
}

/// Phase B0.51C — map a `RegisterHint` to a `StableHintKey` when the hint has
/// per-register-stable identity semantics.  Returns `None` for hints whose
/// value may be reassigned across different PCs for the same register
/// (Named / CallResult / Import / Closure / Table), so those continue to go
/// through the per-(reg,pc) cache + counter path.
fn stable_hint_key(h: &RegisterHint) -> Option<StableHintKey> {
    match h {
        RegisterHint::Param(idx) => Some(StableHintKey::Param(*idx as u32)),
        RegisterHint::SelfParam => Some(StableHintKey::SelfParam),
        RegisterHint::NumericForVar => Some(StableHintKey::NumericForVar),
        RegisterHint::GenericForKey => Some(StableHintKey::GenericForKey),
        RegisterHint::GenericForVal => Some(StableHintKey::GenericForVal),
        _ => None,
    }
}

impl ProtoNaming {
    fn new() -> Self {
        Self {
            prefix_counts: std::collections::HashMap::new(),
            assigned: std::collections::HashMap::new(),
            stable_names: std::collections::HashMap::new(),
            hints: std::collections::HashMap::new(),
        }
    }

    /// Generate a unique name with the given prefix within this proto.
    /// First use of a prefix gets the plain name (e.g., "result"),
    /// subsequent uses get suffixed (e.g., "result2", "result3").
    fn unique_name(&mut self, prefix: &str, used_names: &std::collections::HashSet<String>) -> String {
        let count = self.prefix_counts.entry(prefix.to_string()).or_insert(0);
        *count += 1;
        let name = if *count == 1 {
            prefix.to_string()
        } else {
            format!("{}{}", prefix, count)
        };
        // If it collides with a global reserved name, keep bumping until the
        // name is actually free. A single bump could still hand back a
        // colliding name (`result2` taken → returns `result3`, but `result3`
        // may be taken too).
        if used_names.contains(&name) {
            loop {
                *count += 1;
                let alt = format!("{}{}", prefix, count);
                if !used_names.contains(&alt) {
                    return alt;
                }
            }
        }
        name
    }

    /// Phase B0.51C — generate a stable-identity name within this proto that
    /// DOES NOT consult the globally-shared `used_names` set.  Used only for
    /// hints whose identity is stable per-register (`Param`, `SelfParam`,
    /// `NumericForVar`, `GenericForKey`, `GenericForVal`).  Param / loop-var
    /// names like `arg1`, `self`, `i`, `k`, `v` are SUPPOSED to shadow their
    /// outer-scope counterparts in Luau, not disambiguate from them.  Using
    /// the global set would cause nested protos whose param name coincides
    /// with an outer-scope param to bump from `arg1` → `arg12` on the FIRST
    /// call, defeating the stable-name memoization.
    ///
    /// Within a single proto we still disambiguate via the per-prefix
    /// counter, so two distinct stable-identity registers with the same
    /// prefix (e.g., two separate generic-for loops both asking for `k`)
    /// correctly produce `k`, `k2`, ….
    fn unique_stable_name(&mut self, prefix: &str) -> String {
        let count = self.prefix_counts.entry(prefix.to_string()).or_insert(0);
        *count += 1;
        if *count == 1 {
            prefix.to_string()
        } else {
            format!("{}{}", prefix, count)
        }
    }
}

/// Context for decompilation, holds chunk-level state
pub struct DecompileContext<'a> {
    pub chunk: &'a Chunk,
    pub var_counter: u32,
    /// Track names already used to avoid collisions
    used_names: std::collections::HashSet<String>,
    /// Track proto indices currently being decompiled (recursion guard)
    pub proto_stack: Vec<usize>,
    /// Upvalue names inferred from CAPTURE instructions in the parent proto,
    /// or from usage-based analysis for the main proto (which has no parent).
    /// Keyed by proto index in chunk.protos for O(1) lookup.
    pub inferred_upvalue_names: std::collections::HashMap<usize, Vec<String>>,
    /// Per-proto naming context, keyed by proto index.
    /// Each proto gets its own namespace to avoid cross-proto counter inflation.
    proto_naming: std::collections::HashMap<usize, ProtoNaming>,
    /// The proto index currently being lifted (set by lifter before calling reg_name)
    pub current_proto_index: Option<usize>,
    /// Two-phase upval resolution: tracks CAPTURE type 2 links.
    /// Maps child_proto_index -> vec of (child_upval_slot, parent_proto_index, parent_upval_slot).
    /// After rename_upvals resolves the parent's names, we re-propagate to children.
    pub upval_parent_links: std::collections::HashMap<usize, Vec<(usize, usize, u8)>>,
    /// True when the chunk is canonical open-source Luau bytecode rather than
    /// Roblox's shuffled dialect.
    ///
    /// Roblox repurposes several standard opcodes (LENGTH / NOT / MINUS) as
    /// type-annotation passthroughs, so the lifter deliberately drops those
    /// operators. That is correct for Roblox and wrong for canonical Luau,
    /// where `#t` must lift to a real `UnOp::Length`. DEFAULTS TO FALSE, so any
    /// caller that does not explicitly opt in keeps the Roblox behaviour.
    pub is_canonical_luau: bool,
}

impl<'a> DecompileContext<'a> {
    pub fn new(chunk: &'a Chunk) -> Self {
        Self {
            chunk,
            var_counter: 0,
            used_names: std::collections::HashSet::new(),
            proto_stack: Vec::new(),
            inferred_upvalue_names: std::collections::HashMap::new(),
            proto_naming: std::collections::HashMap::new(),
            current_proto_index: None,
            upval_parent_links: std::collections::HashMap::new(),
            is_canonical_luau: false,
        }
    }

    /// Mark this chunk as canonical (non-Roblox) Luau bytecode.
    pub fn set_canonical_luau(&mut self, canonical: bool) {
        self.is_canonical_luau = canonical;
    }

    /// Initialize naming context for a proto with register hints from pre-pass.
    pub fn init_proto_naming(&mut self, proto_index: usize, hints: std::collections::HashMap<u8, Vec<(usize, RegisterHint)>>) {
        let mut naming = ProtoNaming::new();
        naming.hints = hints;
        self.proto_naming.insert(proto_index, naming);
    }

    pub fn gen_var(&mut self, prefix: &str) -> String {
        // Use per-proto naming when available for cleaner scoped names
        if let Some(pi) = self.current_proto_index {
            if let Some(naming) = self.proto_naming.get_mut(&pi) {
                let name = naming.unique_name(prefix, &self.used_names);
                self.used_names.insert(name.clone());
                return name;
            }
        }
        // Fallback: global counter
        self.var_counter += 1;
        let name = format!("{}_{}", prefix, self.var_counter);
        self.used_names.insert(name.clone());
        name
    }

    /// Returns true if the plain name has already been reserved by `reserve_name`.
    pub fn is_name_used(&self, name: &str) -> bool {
        self.used_names.contains(name)
    }

    /// Reserve a plain name (no counter suffix) and return it.
    pub fn reserve_name(&mut self, name: &str) -> String {
        self.used_names.insert(name.to_string());
        name.to_string()
    }

    /// Pin `name` as the answer `reg_name` gives for `reg` at every PC in
    /// `[start_pc, end_pc)`.
    ///
    /// `reg_name` is PC-scoped and `unique_name` bumps its per-prefix counter on
    /// every miss, so the SAME register with the SAME hint yields `import` at
    /// one PC and `import2` at another. For a register that a loop carries
    /// across iterations that is fatal, not cosmetic: the lifter sees the two
    /// names disagree, classifies the body's write as a semantic Shadow, and
    /// emits `local import2 = import * j` inside the loop instead of
    /// `import = import * j` — so the accumulator is re-declared every
    /// iteration and never accumulates.
    ///
    /// Pinning is only used for registers that have just been force-
    /// materialized as loop-carried locals, where a single stable name for the
    /// whole loop is the correct answer by construction. Existing entries are
    /// never overwritten, so a name already committed at a PC still wins.
    pub fn pin_reg_name(&mut self, reg: u8, name: &str, start_pc: usize, end_pc: usize) {
        if let Some(pi) = self.current_proto_index {
            if let Some(naming) = self.proto_naming.get_mut(&pi) {
                for pc in start_pc..end_pc {
                    naming.assigned.entry((reg, pc)).or_insert_with(|| name.to_string());
                }
            }
        }
    }

    /// Get a register name from debug info, or synthesize a meaningful one
    pub fn reg_name(&mut self, proto: &Proto, reg: u8, pc: usize) -> String {
        // First, try debug info. Check both active-at-PC and starts-at-PC in a
        // single pass. The second condition (start_pc == end_pc == pc) catches
        // zero-length debug entries that the range check misses.
        if let Some(ref debug) = proto.debug_info {
            for local in &debug.locals {
                if local.reg == reg {
                    let in_range = pc >= local.start_pc as usize && pc < local.end_pc as usize;
                    let at_start = pc == local.start_pc as usize;
                    if in_range || at_start {
                        // Sanitize: debug info names can contain spaces or special chars
                        // that make invalid Luau identifiers
                        let name = &local.name;
                        if is_valid_luau_identifier(name) {
                            return name.clone();
                        }
                        // Invalid identifier — fall through to synthesis
                    }
                }
            }
        }

        // Check if we already assigned a name for this (reg, pc) in this proto
        if let Some(pi) = self.current_proto_index {
            if let Some(naming) = self.proto_naming.get(&pi) {
                if let Some(existing) = naming.assigned.get(&(reg, pc)) {
                    return existing.clone();
                }
            }
        }

        // Synthesize a context-aware name using register hints
        let name = self.synthesize_name(reg, pc);

        // Cache the assignment
        if let Some(pi) = self.current_proto_index {
            if let Some(naming) = self.proto_naming.get_mut(&pi) {
                naming.assigned.insert((reg, pc), name.clone());
            }
        }

        name
    }

    /// Synthesize a descriptive variable name based on register hints and context.
    fn synthesize_name(&mut self, reg: u8, pc: usize) -> String {
        let pi = self.current_proto_index.unwrap_or(0);

        // Pick the best hint for this register, PC-scoped.
        //
        // Hints are recorded with the PC at which they were observed. For a
        // read at `pc`, a hint is "in scope" when its own PC <= pc. Param /
        // SelfParam hints (recorded at pc=0) are always in scope. If no hints
        // are in scope yet (read before any recorded write — rare but can
        // happen for uninitialized regs), we fall back to all hints.
        //
        // Priority (highest first within the scoped set):
        //   1. Named — explicit suggestion (GetService, require, global name,
        //      field name, upvalue name, …)
        //   2. The MOST RECENT non-parameter hint by PC
        //   3. Parameter/SelfParam — stable, set at proto entry
        let hint: Option<RegisterHint> = self.proto_naming.get(&pi)
            .and_then(|n| n.hints.get(&reg))
            .and_then(|hints| select_hint(hints, pc));

        // ── Phase B0.51C: stable-identity hints memoize by (reg, hint-key) ──
        //
        // Hints whose identity is stable per-register (Param, SelfParam,
        // NumericForVar, GenericForKey, GenericForVal) must resolve to the
        // SAME name across every PC at which the register is read.  Without
        // this short-circuit, the per-(reg,pc) `assigned` cache misses for
        // each distinct pc, each call runs `unique_name(prefix)` which bumps
        // the per-prefix counter → `"arg1"`, `"arg12"`, `"arg13"`, … on
        // successive reads of the SAME param register.
        //
        // Hits here return the already-allocated name WITHOUT bumping any
        // counter and without allocating a duplicate entry in `used_names`.
        let stable_key = hint.as_ref().and_then(stable_hint_key);
        if let Some(key) = stable_key {
            if let Some(existing) = self
                .proto_naming
                .get(&pi)
                .and_then(|n| n.stable_names.get(&(reg, key)))
                .cloned()
            {
                return existing;
            }
        }

        // For stable-identity hints we must also bypass the globally-shared
        // `used_names` set on the FIRST synthesis — see `unique_stable_name`
        // for the rationale.  A nested proto whose Param(0) coincides with
        // an outer proto's Param(0) is supposed to render as `arg1` in BOTH
        // scopes (Luau `local` shadowing); using the global set would force
        // the inner `arg1` to bump to `arg12` on the very first call,
        // bypassing our stable-name memoization.
        if let Some(key) = stable_key {
            let prefix = match &hint {
                Some(RegisterHint::SelfParam) => "self".to_string(),
                Some(RegisterHint::Param(0)) => "arg1".to_string(),
                Some(RegisterHint::Param(idx)) => format!("arg{}", idx + 1),
                Some(RegisterHint::NumericForVar) => "i".to_string(),
                Some(RegisterHint::GenericForKey) => "k".to_string(),
                Some(RegisterHint::GenericForVal) => "v".to_string(),
                // Other stable kinds are handled above via `stable_hint_key`.
                // Defensive fallback: if the key maps but the hint shape
                // doesn't (shouldn't happen), fall through to `gen_scoped_name`.
                _ => String::new(),
            };
            if !prefix.is_empty() {
                let name = if let Some(naming) = self.proto_naming.get_mut(&pi) {
                    naming.unique_stable_name(&prefix)
                } else {
                    prefix.clone()
                };
                self.used_names.insert(name.clone());
                self.remember_stable_name(pi, reg, key, &name);
                return name;
            }
        }

        let prefix = match hint {
            Some(RegisterHint::Named(ref name)) => {
                // Phase B0.106: guard against Luau reserved words used as
                // variable names.  A Named hint carrying a reserved keyword
                // (e.g. "function", "end", "if", "type", "continue") would
                // render `local function = ...` — a parse error.  Check
                // is_valid_luau_identifier first (catches all reserved words),
                // then is_stdlib_shadow_name (catches globals like "game",
                // "workspace", "pcall" that aren't keywords but shouldn't
                // shadow their built-in).
                if !is_valid_luau_identifier(name) || is_stdlib_shadow_name(name) {
                    return self.gen_scoped_name("value");
                }
                // Explicit name — use directly, still scoped for uniqueness
                return self.gen_scoped_name(name);
            }
            Some(RegisterHint::SelfParam) => "self",
            Some(RegisterHint::Param(0)) => "arg1",
            Some(RegisterHint::Param(idx)) => {
                // Return early with formatted name to avoid borrow issues
                let p = format!("arg{}", idx + 1);
                let name = self.gen_scoped_name(&p);
                self.remember_stable_name(pi, reg, StableHintKey::Param(idx as u32), &name);
                return name;
            }
            Some(RegisterHint::CallResult(ref func_name)) => {
                return self.name_from_call_result(func_name);
            }
            Some(RegisterHint::NumericForVar) => "i",
            Some(RegisterHint::GenericForKey) => "k",
            Some(RegisterHint::GenericForVal) => "v",
            Some(RegisterHint::Closure) => "fn",
            Some(RegisterHint::Table) => "tbl",
            Some(RegisterHint::Import(ref path)) => {
                return self.name_from_import(path);
            }
            None => {
                // No hint -- fall back to register-based name.
                // Use "v" prefix with per-proto scoping for cleaner output.
                let p = format!("v{}", reg);
                return self.gen_scoped_name(&p);
            }
        };

        let name = self.gen_scoped_name(prefix);
        // Remember the stable-identity mapping for the branches that fall
        // through to the shared `gen_scoped_name(prefix)` call below (i.e.,
        // SelfParam, NumericForVar, GenericForKey, GenericForVal).
        if let Some(hint) = hint {
            if let Some(key) = stable_hint_key(&hint) {
                self.remember_stable_name(pi, reg, key, &name);
            }
        }
        name
    }

    /// Phase B0.51C — record a stable-identity name so subsequent reads of the
    /// SAME register with the SAME stable-hint kind return the SAME name.
    fn remember_stable_name(&mut self, pi: usize, reg: u8, key: StableHintKey, name: &str) {
        if let Some(naming) = self.proto_naming.get_mut(&pi) {
            naming.stable_names.insert((reg, key), name.to_string());
        }
    }

    /// Generate a scoped name using per-proto naming when available.
    fn gen_scoped_name(&mut self, prefix: &str) -> String {
        if let Some(pi) = self.current_proto_index {
            if let Some(naming) = self.proto_naming.get_mut(&pi) {
                let name = naming.unique_name(prefix, &self.used_names);
                self.used_names.insert(name.clone());
                return name;
            }
        }
        // Fallback
        self.var_counter += 1;
        let name = format!("{}_{}", prefix, self.var_counter);
        self.used_names.insert(name.clone());
        name
    }

    /// Derive a variable name from a call result.
    ///
    /// Accepts either a single segment (e.g., `"GetService"`, `"new"`) or a
    /// parent-qualified path (e.g., `"Vector3.new"`, `"math.floor"`). Phase
    /// B0.43C added the parent-qualified inputs plus many more last-segment
    /// patterns covering Roblox ctor types, math/string/table stdlib members,
    /// and common method-call idioms.
    fn name_from_call_result(&mut self, func_name: &str) -> String {
        // ── Phase B0.43C: parent-qualified dispatch first ────────────────
        // Roblox ctor types: the parent tells us the result flavor.
        let parent_qualified: Option<&str> = match func_name {
            // Vector / position
            "Vector3.new" | "Vector3.fromAxisAngle" | "Vector3.fromMatrix"
            | "Vector3.fromNormalId" | "Vector3.FromNormalId"
            | "Vector2.new" | "Vector2.fromScale" | "Vector2.fromOffset"
            | "Vector3int16.new" | "Vector2int16.new" => Some("vec"),
            // CFrame
            "CFrame.new" | "CFrame.fromAxisAngle" | "CFrame.fromMatrix"
            | "CFrame.fromEulerAngles" | "CFrame.fromEulerAnglesXYZ"
            | "CFrame.fromEulerAnglesYXZ" | "CFrame.fromOrientation"
            | "CFrame.lookAt" | "CFrame.Angles" | "CFrame.angles" => Some("cf"),
            // Color
            "Color3.new" | "Color3.fromRGB" | "Color3.fromHSV" | "Color3.fromHex"
            | "BrickColor.new" | "BrickColor.random" | "BrickColor.Random"
            | "ColorSequence.new" | "ColorSequenceKeypoint.new" => Some("color"),
            // UDim / sizes
            "UDim.new" => Some("udim"),
            "UDim2.new" | "UDim2.fromScale" | "UDim2.fromOffset" => Some("udim"),
            // Rect / region
            "Rect.new" => Some("rect"),
            "Region3.new" | "Region3int16.new" => Some("region"),
            // Number ranges / sequences
            "NumberRange.new" => Some("range"),
            "NumberSequence.new" | "NumberSequenceKeypoint.new" => Some("sequence"),
            // Misc Roblox
            "Ray.new" => Some("ray"),
            "Enum.new" => Some("enum"),
            "Instance.new" => Some("instance"),
            "TweenInfo.new" => Some("tweenInfo"),
            "RaycastParams.new" => Some("params"),
            "OverlapParams.new" => Some("params"),
            "PathfindingService.new" => Some("service"),
            "PhysicalProperties.new" => Some("properties"),
            "Random.new" => Some("random"),
            // Font
            "Font.new" | "Font.fromName" | "Font.fromEnum" | "Font.fromId" => Some("font"),
            // math stdlib — result type is baked into the method name
            "math.floor" => Some("floor"),
            "math.ceil" => Some("ceil"),
            "math.abs" => Some("abs"),
            "math.min" => Some("min"),
            "math.max" => Some("max"),
            "math.random" => Some("random"),
            "math.sqrt" => Some("sqrt"),
            "math.sign" => Some("sign"),
            "math.clamp" => Some("clamped"),
            "math.round" => Some("rounded"),
            "math.sin" | "math.cos" | "math.tan"
            | "math.asin" | "math.acos" | "math.atan" | "math.atan2"
            | "math.exp" | "math.log" | "math.log10"
            | "math.pow" | "math.deg" | "math.rad"
            | "math.sinh" | "math.cosh" | "math.tanh"
            | "math.fmod" | "math.modf" | "math.huge" | "math.pi"
            | "math.noise" => Some("result"),
            // string stdlib
            "string.upper" => Some("upper"),
            "string.lower" => Some("lower"),
            "string.format" => Some("formatted"),
            "string.sub" => Some("sub"),
            "string.find" => Some("found"),
            "string.gsub" => Some("replaced"),
            "string.match" => Some("matched"),
            "string.gmatch" => Some("matches"),
            "string.rep" => Some("rep"),
            "string.reverse" => Some("reversed"),
            "string.split" => Some("parts"),
            "string.byte" => Some("byte"),
            "string.char" => Some("char"),
            "string.len" => Some("length"),
            "string.pack" | "string.unpack" | "string.packsize" => Some("packed"),
            // table stdlib
            "table.find" => Some("found"),
            "table.concat" => Some("joined"),
            "table.sort" => Some("sorted"),
            "table.insert" | "table.remove" => Some("value"),
            "table.clone" => Some("clone"),
            "table.pack" => Some("packed"),
            "table.unpack" => Some("value"),
            "table.freeze" | "table.isfrozen" => Some("tbl"),
            "table.move" => Some("moved"),
            "table.create" => Some("tbl"),
            // bit32
            "bit32.band" | "bit32.bor" | "bit32.bxor" | "bit32.bnot"
            | "bit32.lshift" | "bit32.rshift" | "bit32.arshift"
            | "bit32.rol" | "bit32.ror" | "bit32.extract" | "bit32.replace"
            | "bit32.countlz" | "bit32.countrz" | "bit32.btest" => Some("bits"),
            // buffer
            "buffer.create" | "buffer.fromstring" => Some("buf"),
            "buffer.len" => Some("length"),
            "buffer.tostring" => Some("str"),
            // utf8
            "utf8.char" => Some("char"),
            "utf8.codepoint" => Some("codepoint"),
            "utf8.len" => Some("length"),
            // coroutine
            "coroutine.create" | "coroutine.wrap" => Some("co"),
            "coroutine.resume" => Some("ok"),
            "coroutine.status" => Some("status"),
            "coroutine.running" => Some("co"),
            // task
            "task.spawn" | "task.defer" | "task.delay" => Some("thread"),
            "task.wait" => Some("elapsed"),
            _ => None,
        };
        if let Some(p) = parent_qualified {
            return self.gen_scoped_name(p);
        }

        // ── Last-segment / method-name dispatch ──────────────────────────
        let prefix = match func_name {
            // Roblox service pattern
            n if n == "GetService" => "service",
            n if n == "WaitForChild" || n == "FindFirstChild"
              || n == "FindFirstChildOfClass" || n == "FindFirstChildWhichIsA"
              || n == "FindFirstAncestor" || n == "FindFirstAncestorOfClass"
              || n == "FindFirstAncestorWhichIsA" => "child",
            // Clone / copy
            n if n == "Clone" || n == "clone" => "clone",
            n if n == "Copy" || n == "copy" => "copy",
            // Children / descendants
            n if n == "GetChildren" || n == "children" => "children",
            n if n == "GetDescendants" => "descendants",
            n if n == "GetAttribute" => "attribute",
            n if n == "GetAttributes" => "attributes",
            n if n == "GetTags" => "tags",
            n if n == "GetPropertyChangedSignal" => "signal",
            // Connections / signals
            n if n == "Connect" || n == "connect" || n == "ConnectParallel" => "connection",
            n if n == "Once" || n == "once" => "once",
            n if n == "Wait" || n == "wait" => "result",
            // Constructors
            n if n == "new" => "instance",
            n if n == "Create" || n == "create" => "obj",
            // Math/string — bare last-segment (from NAMECALL-style `x:find(...)`)
            n if n == "format" || n == "Format" => "formatted",
            n if n == "find" || n == "Find" => "found",
            n if n == "match" || n == "Match" => "matched",
            n if n == "gmatch" => "matches",
            n if n == "gsub" => "replaced",
            n if n == "sub" || n == "Sub" => "sub",
            n if n == "len" || n == "Len" => "length",
            n if n == "upper" || n == "Upper" => "upper",
            n if n == "lower" || n == "Lower" => "lower",
            n if n == "reverse" => "reversed",
            n if n == "split" || n == "Split" => "parts",
            n if n == "rep" => "rep",
            n if n == "byte" => "byte",
            n if n == "char" => "char",
            // Math bare
            n if n == "floor" => "floor",
            n if n == "ceil" => "ceil",
            n if n == "abs" => "abs",
            n if n == "min" => "min",
            n if n == "max" => "max",
            n if n == "sqrt" => "sqrt",
            n if n == "sign" => "sign",
            n if n == "random" => "random",
            n if n == "clamp" => "clamped",
            n if n == "round" => "rounded",
            n if n == "deg" => "deg",
            n if n == "rad" => "rad",
            // Table bare
            n if n == "insert" => "value",
            n if n == "remove" => "value",
            n if n == "concat" => "joined",
            n if n == "sort" => "sorted",
            // Common method idioms — geometry/physics
            n if n == "GetPivot" || n == "GetBoundingBox" => "cf",
            n if n == "GetPosition" || n == "Position" => "pos",
            n if n == "GetSize" || n == "Size" => "size",
            n if n == "GetColor" || n == "Color" => "color",
            n if n == "Lerp" || n == "lerp" => "lerped",
            n if n == "Unit" => "unit",
            n if n == "Dot" || n == "dot" => "dot",
            n if n == "Cross" || n == "cross" => "cross",
            n if n == "Magnitude" => "magnitude",
            n if n == "Inverse" || n == "inverse" => "inv",
            // Phase B0.93: additional Roblox Instance / utility methods
            n if n == "GetPlayers" || n == "GetFriends" => "players",
            n if n == "GetMouse" => "mouse",
            n if n == "GetPlayer" || n == "GetPlayerFromCharacter"
              || n == "GetPlayerByUserId" => "player",
            n if n == "GetHumanoid" => "humanoid",
            n if n == "GetCharacter" => "character",
            n if n == "IsA" || n == "isA" => "isType",
            n if n == "IsDescendantOf" || n == "IsAncestorOf" => "isRelated",
            n if n == "Raycast" || n == "raycast" => "rayResult",
            n if n == "GetTouchingParts" || n == "GetPartsInPart" => "parts",
            n if n == "Kick" || n == "Destroy" || n == "Remove" => "result",
            n if n == "tostring" || n == "ToString" => "str",
            n if n == "tonumber" || n == "ToNumber" => "num",
            n if n == "typeof" || n == "TypeOf" || n == "type" => "typeStr",
            n if n == "tick" || n == "time" || n == "os.clock" => "now",
            n if n == "Encode" || n == "encode" => "encoded",
            n if n == "Decode" || n == "decode" => "decoded",
            n if n == "Serialize" || n == "serialize" => "serialized",
            n if n == "Deserialize" || n == "deserialize" => "deserialized",
            n if n == "Invoke" || n == "InvokeServer" || n == "InvokeClient" => "response",
            n if n == "FireServer" || n == "FireClient" || n == "FireAllClients" => "result",
            // Phase B0.38: never name the result after a stdlib / builtin function.
            // Using `local pcall = pcall(...)` or `local require = require(...)`
            // shadows the real global and produces broken output.
            _ if is_stdlib_shadow_name(func_name) => "result",
            // Also guard the parent-qualified path against shadowing: if the
            // qualified name starts with a stdlib module and the last segment
            // would shadow, fall through to "result".
            _ if func_name.split('.').last().map_or(false, is_stdlib_shadow_name) => "result",
            // Generic: use lowercased function name as prefix, but only if the
            // input is a single simple identifier (the parent-qualified path
            // would have been handled above). Dots / unusual chars fall back.
            // Phase B0.93: raised length limit from 12 to 20 to cover longer
            // Roblox method names (e.g., "GetPartBoundsInBox", "PromptGamePass").
            n if !n.contains('.')
              && n.len() <= 20
              && n.chars().all(|c| c.is_alphanumeric()) => {
                let lower = n.to_lowercase();
                // Phase B0.106: lowercased function names can produce
                // reserved words (e.g. "Function" -> "function", "Type" -> "type").
                if !is_valid_luau_identifier(&lower) || is_stdlib_shadow_name(&lower) {
                    return self.gen_scoped_name("result");
                }
                return self.gen_scoped_name(&lower);
            }
            _ => "result",
        };
        self.gen_scoped_name(prefix)
    }

    /// Derive a variable name from an import path.
    /// e.g., "game.Players" -> "Players", "math.random" -> "random"
    fn name_from_import(&mut self, path: &str) -> String {
        // Use the last segment of the import path
        let last = path.rsplit('.').next().unwrap_or(path);
        if last.is_empty() || last == "_G" {
            return self.gen_scoped_name("import");
        }
        // Phase B0.106: if the last segment is a Luau reserved word (e.g.
        // "function", "end") or a stdlib/builtin name (e.g. "require", "pcall"),
        // using it as a variable name would cause a parse error or shadow a
        // global.  Fall back to "import".
        if !is_valid_luau_identifier(last) || is_stdlib_shadow_name(last) {
            return self.gen_scoped_name("import");
        }
        // Use the import name directly (e.g., "Players", "workspace")
        self.gen_scoped_name(last)
    }

    pub fn upval_name(&self, proto: &Proto, proto_index: usize, idx: u8) -> String {
        // First: check bytecode debug info (present in non-stripped builds)
        if let Some(ref debug) = proto.debug_info {
            if let Some(name) = debug.upvalue_names.get(idx as usize) {
                // Phase B0.106: guard against reserved words in debug info
                if !name.is_empty() && is_valid_luau_identifier(name) && !is_stdlib_shadow_name(name) {
                    return name.clone();
                }
            }
        }
        // Second: check names inferred from parent CAPTURE instructions
        if let Some(names) = self.inferred_upvalue_names.get(&proto_index) {
            if let Some(name) = names.get(idx as usize) {
                // Phase B0.106: same guard for inferred names
                if !name.is_empty() && is_valid_luau_identifier(name) && !is_stdlib_shadow_name(name) {
                    return name.clone();
                }
            }
        }
        // Phase C10X: slots outside the proto's declared upvalue range are
        // decode corruption (partial opmap / misread SETUPVAL), not real
        // upvalues. Emit a distinct prefix so the quality classifier does
        // not lump these into the upval_ bucket with real unnamed captures.
        if (idx as usize) >= proto.num_upvalues as usize {
            return format!("cap_{}", idx);
        }
        format!("upval_{}", idx)
    }
}

/// Pick the best hint for a register at a given read PC.
///
/// Hints are tagged with the PC at which they were observed. For a read at
/// `pc`, we prefer hints whose PC <= pc (i.e., writes that actually precede
/// the read). Within that in-scope set, priorities are:
///   1. most-recent Named (explicit name — GetService, require, global name,
///      upvalue name, field name)
///   2. most-recent non-Param hint
///   3. first Param/SelfParam hint
///
/// When no hint is in scope (e.g., read before any recorded write), fall back
/// to the full hint set using the same priorities.
fn select_hint(hints: &[(usize, RegisterHint)], pc: usize) -> Option<RegisterHint> {
    fn pick(hs: impl Iterator<Item = (usize, RegisterHint)> + Clone) -> Option<RegisterHint> {
        // Collect once so we can iterate in reverse multiple times.
        let collected: Vec<(usize, RegisterHint)> = hs.collect();
        if collected.is_empty() {
            return None;
        }
        for (_, h) in collected.iter().rev() {
            if matches!(h, RegisterHint::Named(_)) {
                return Some(h.clone());
            }
        }
        for (_, h) in collected.iter().rev() {
            if !matches!(h, RegisterHint::Param(_) | RegisterHint::SelfParam) {
                return Some(h.clone());
            }
        }
        collected.first().map(|(_, h)| h.clone())
    }

    let in_scope = hints.iter().filter(|(hpc, _)| *hpc <= pc).cloned();
    let chosen = pick(in_scope);
    if chosen.is_some() {
        return chosen;
    }
    // Read-before-write fallback: consider all hints.
    pick(hints.iter().cloned())
}

/// Phase B0.43C: decide whether to preserve the parent segment of an Import
/// path when building a CallResult hint. The goal is to differentiate
/// `Vector3.new` from `Instance.new` and `math.floor` from `table.concat`
/// without inflating every single CallResult with the entire import path.
///
/// Returns true when:
///   * the `last` segment is a generic constructor-style name (`new`, `Create`,
///     `fromRGB`, `fromAxisAngle`, …) — the parent (e.g., `Vector3`) is what
///     tells us the result type
///   * the `parent` is a well-known Luau/Roblox module whose members produce
///     well-known result kinds (`math.floor` → "floor", `string.format` →
///     "formatted", `table.concat` → "joined", …)
fn is_parent_worth_keeping(parent: &str, last: &str) -> bool {
    // Generic constructor-ish names — parent is the meaningful type tag.
    let generic_last = matches!(last,
        "new" | "New"
        | "Create" | "create"
        | "fromRGB" | "fromHSV" | "fromHex"
        | "fromScale" | "fromOffset"
        | "fromAxisAngle" | "fromMatrix"
        | "fromEulerAngles" | "fromEulerAnglesXYZ"
        | "fromEulerAnglesYXZ" | "fromOrientation"
        | "lookAt" | "Angles"
        | "clone" | "Clone" | "copy" | "Copy"
    );
    if generic_last {
        return true;
    }
    // Known stdlib/Roblox modules whose member calls benefit from parent context.
    matches!(parent,
        "math" | "string" | "table" | "bit32" | "buffer" | "utf8"
        | "coroutine" | "debug" | "task" | "os"
    )
}

/// Phase B0.38: names that would shadow a Luau global if used as a local
/// variable. Used by `name_from_call_result`, `name_from_import`, and the
/// CALL destination-naming hint installer to avoid emitting
/// `local pcall = pcall(...)` / `local require = require(...)` etc.
pub fn is_stdlib_shadow_name(s: &str) -> bool {
    matches!(s,
        "pcall" | "xpcall" | "require" | "assert" | "error"
        | "select" | "type" | "typeof" | "tostring" | "tonumber"
        | "print" | "warn" | "next" | "pairs" | "ipairs"
        | "getmetatable" | "setmetatable" | "rawget" | "rawset"
        | "rawequal" | "rawlen" | "unpack" | "collectgarbage"
        | "loadstring" | "newproxy" | "wait" | "delay" | "spawn"
        | "tick" | "time" | "os" | "math" | "string" | "table"
        | "coroutine" | "debug" | "bit32" | "buffer" | "utf8"
        | "task" | "game" | "workspace" | "script")
}

/// Check if a string is a valid Luau identifier (starts with letter/underscore,
/// contains only alphanumeric/underscore, not a reserved keyword).
pub fn is_valid_luau_identifier(s: &str) -> bool {
    if s.is_empty() { return false; }
    let first = s.chars().next().unwrap();
    if !first.is_ascii_alphabetic() && first != '_' { return false; }
    if !s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') { return false; }
    // Check against Luau reserved words
    !matches!(s, "and" | "break" | "do" | "else" | "elseif" | "end" | "false"
        | "for" | "function" | "if" | "in" | "local" | "nil" | "not" | "or"
        | "repeat" | "return" | "then" | "true" | "until" | "while"
        | "continue" | "type" | "export")
}

/// Pre-pass: analyze a proto's bytecode to classify register usage.
/// Returns hints keyed by register number. Each hint is tagged with the PC at
/// which it was observed so that `synthesize_name` can pick the most recent
/// hint <= the current read PC.
///
/// `upval_names` — optional per-upvalue names (e.g., from parent CAPTURE
/// inference or SETUPVAL analysis). When provided, GETUPVAL writes hint the
/// target register with `Named(upval_name)` so that code using the upvalue
/// through a register gets a readable name instead of `v{reg}`.
///
/// `chunk_protos` — optional slice of all protos in the chunk. When provided,
/// NEWCLOSURE/DUPCLOSURE can resolve the child proto's `debug_name` and install
/// a `Named(debug_name)` hint so user-defined functions render with their real
/// names instead of `fn`, `fn2`, etc.
pub fn analyze_register_usage(
    proto: &Proto,
    strings: &[String],
    upval_names: Option<&[String]>,
    chunk_protos: Option<&[Proto]>,
) -> std::collections::HashMap<u8, Vec<(usize, RegisterHint)>> {
    let mut hints: std::collections::HashMap<u8, Vec<(usize, RegisterHint)>> = std::collections::HashMap::new();

    // Mark parameters at PC 0 (they're in scope for the entire proto).
    for i in 0..proto.num_params {
        // Heuristic: if a function has >=1 params and uses NAMECALL or methods
        // on the first param, it's likely a method with self.
        // For now, mark all params with their index; self-detection comes below.
        hints.entry(i).or_default().push((0, RegisterHint::Param(i as usize)));
    }

    let code = &proto.code;
    let mut pc = 0;
    while pc < code.len() {
        let insn = code[pc];
        let op = LuauOpcode::from_u8(insn_op(insn));
        let a = insn_a(insn);
        let b = insn_b(insn);
        let c = insn_c(insn);
        let d = insn_d(insn);
        let aux = if op.has_aux() && pc + 1 < code.len() { Some(code[pc + 1]) } else { None };

        match op {
            LuauOpcode::NewClosure | LuauOpcode::DupClosure => {
                // Wave 1C: if the child proto carries a debug_name, prefer it
                // as a Named hint so user-defined functions render with their
                // real names (e.g., `local animate = function(...)`) instead
                // of the generic `fn`, `fn2`, ... sequence.
                //
                // Resolution mirrors the lifter: for NEWCLOSURE, D indexes
                // proto.child_protos → chunk-global proto idx (with Roblox
                // fallback of D-as-direct-global-idx). For DUPCLOSURE, D
                // indexes proto.constants to a Constant::Closure(child_idx),
                // with the same Roblox fallback.
                let mut named_installed = false;
                if let Some(all_protos) = chunk_protos {
                    let d_unsigned = d as u16 as usize;
                    let resolved_idx: Option<usize> = if op == LuauOpcode::NewClosure {
                        proto.child_protos.get(d_unsigned).map(|&i| i as usize)
                            .or_else(|| {
                                if d_unsigned < all_protos.len() { Some(d_unsigned) } else { None }
                            })
                    } else {
                        let from_const = match proto.constants.get(d_unsigned) {
                            Some(Constant::Closure(child_idx)) => {
                                proto.child_protos.get(*child_idx as usize).map(|&i| i as usize)
                                    .or_else(|| {
                                        let g = *child_idx as usize;
                                        if g < all_protos.len() { Some(g) } else { None }
                                    })
                            }
                            _ => None,
                        };
                        from_const
                            .or_else(|| proto.child_protos.get(d_unsigned).map(|&i| i as usize))
                            .or_else(|| {
                                if d_unsigned < all_protos.len() { Some(d_unsigned) } else { None }
                            })
                    };
                    if let Some(idx) = resolved_idx {
                        if let Some(child) = all_protos.get(idx) {
                            if let Some(name) = child.debug_name.as_deref() {
                                if !name.is_empty()
                                    && is_valid_luau_identifier(name)
                                    && !is_stdlib_shadow_name(name)
                                {
                                    hints.entry(a).or_default()
                                        .push((pc, RegisterHint::Named(name.to_string())));
                                    named_installed = true;
                                }
                            }
                        }
                    }
                }
                if !named_installed {
                    hints.entry(a).or_default().push((pc, RegisterHint::Closure));
                }
            }
            LuauOpcode::NewTable | LuauOpcode::DupTable => {
                hints.entry(a).or_default().push((pc, RegisterHint::Table));
            }
            LuauOpcode::GetImport => {
                // Try to resolve the import path for naming.
                // Import IDs are 0-based indices into proto.constants (Luau VM k[id]).
                let import_val = aux.unwrap_or_else(|| {
                    let d_unsigned = d as u16 as usize;
                    match proto.constants.get(d_unsigned) {
                        Some(Constant::Import(v)) => *v,
                        _ => 0,
                    }
                });
                let ids = decode_import(import_val);
                let parts: Vec<String> = ids.iter()
                    .filter_map(|&id| {
                        // Primary: proto.constants (authoritative per Luau VM k[id])
                        if let Some(Constant::String(s)) = proto.constants.get(id as usize) {
                            Some(s.clone())
                        } else if let Some(s) = strings.get(id as usize) {
                            // Fallback: chunk.strings
                            Some(s.clone())
                        } else {
                            None
                        }
                    })
                    .collect();
                if !parts.is_empty() {
                    let path = parts.join(".");
                    hints.entry(a).or_default().push((pc, RegisterHint::Import(path)));
                }
            }
            LuauOpcode::GetGlobal => {
                // GETGLOBAL A _ [AUX]: AUX is an index into proto.constants
                // where the constant is the global's name string. The A
                // register receives the global's value — use the name as a
                // hint so `local UserSettings = UserSettings` renders nicely.
                if let Some(ax) = aux {
                    if let Some(name) = get_aux_string(proto, strings, ax) {
                        if is_valid_luau_identifier(&name) {
                            hints.entry(a).or_default().push((pc, RegisterHint::Named(name)));
                        }
                    }
                }
            }
            LuauOpcode::GetUpval => {
                // GETUPVAL A B: loads upvalue B into R(A). If the caller
                // provided upvalue names (from CAPTURE inference or SETUPVAL
                // tracing), use that name as the hint — otherwise the register
                // falls back to the generic v{reg} name.
                if let Some(names) = upval_names {
                    if let Some(uv_name) = names.get(b as usize) {
                        if !uv_name.is_empty() && is_valid_luau_identifier(uv_name) {
                            hints.entry(a).or_default().push((pc, RegisterHint::Named(uv_name.clone())));
                        }
                    }
                }
            }
            LuauOpcode::GetTableKS => {
                // GETTABLEKS A B [AUX]: R(A) = R(B)[K[AUX]] where AUX is a
                // string-constant index. The field name is a reasonable hint
                // for the destination register (e.g., `local Parent = x.Parent`).
                if let Some(ax) = aux {
                    if let Some(field) = get_aux_string(proto, strings, ax) {
                        if is_valid_luau_identifier(&field) {
                            hints.entry(a).or_default().push((pc, RegisterHint::Named(field)));
                        }
                    }
                }
            }
            LuauOpcode::NameCall => {
                // A = result register, B = object, AUX = method string index
                // The method name gives us a hint for the CALL result that follows.
                //
                // B0.54A: for methods whose first literal-string argument is the
                // canonical name of the returned instance/value, promote the literal
                // string to `Named(literal)` on the result register. This extends the
                // original B0.38 `GetService` special case to the broader Instance
                // lookup family (FindFirstChild, WaitForChild, FindFirstAncestor…).
                // All these methods share the signature `obj:Method("Name", ...)` and
                // return an object whose conventional variable name IS that Name.
                if let Some(ax) = aux {
                    let method = get_aux_string(proto, strings, ax);
                    if let Some(m) = method {
                        // Methods where arg0 is a string naming the returned object.
                        // The compiler may emit `LOADK argreg "Name"` either BEFORE
                        // or AFTER the NAMECALL (Luau pre-loads args). Scan both.
                        const LITERAL_NAMING_METHODS: &[&str] = &[
                            "GetService",
                            "FindFirstChild",
                            "FindFirstChildOfClass",
                            "FindFirstChildWhichIsA",
                            "FindFirstAncestor",
                            "FindFirstAncestorOfClass",
                            "FindFirstAncestorWhichIsA",
                            "WaitForChild",
                        ];
                        if LITERAL_NAMING_METHODS.contains(&m.as_str()) {
                            let arg_reg = a.saturating_add(2);
                            let found_name = find_loadk_string_for_reg(&proto, strings, code, pc, arg_reg);
                            // Reject literals that would shadow a stdlib/global name
                            // so we don't get `local pcall = x:FindFirstChild("pcall")`.
                            let usable = found_name.filter(|n|
                                is_valid_luau_identifier(n) && !is_stdlib_shadow_name(n)
                            );
                            if let Some(name) = usable {
                                hints.entry(a).or_default().push((pc, RegisterHint::Named(name)));
                            } else {
                                hints.entry(a).or_default().push((pc, RegisterHint::CallResult(m)));
                            }
                        } else {
                            hints.entry(a).or_default().push((pc, RegisterHint::CallResult(m)));
                        }
                    }
                }
            }
            LuauOpcode::Call => {
                // A = func register, B = nargs+1, C = nresults+1
                // The function expression in register A can hint the result name
                // This is secondary to NameCall (which already set a hint on A)
                if c >= 2 {
                    // Special case: `require(Path.To.Module)` — name result "Module"
                    // (last path segment). The function register A holds require (Import),
                    // the arg at A+1 holds the module path (another Import).
                    let fn_hints = hints.get(&a);
                    let is_require = fn_hints.map_or(false, |hs| {
                        hs.iter().any(|(_, h)| matches!(h, RegisterHint::Import(p) if p == "require"))
                    });
                    // Phase B0.101: evaluate Instance.new check before mutable borrows
                    let is_instance_new = fn_hints.map_or(false, |hs| {
                        hs.iter().any(|(_, h)| matches!(h, RegisterHint::Import(p) if p == "Instance.new"))
                    });
                    if is_require && b == 2 {
                        // Single-arg require: find the "last component" of the arg path.
                        // The arg register A+1 may be built via:
                        //   1. GETIMPORT — path stored in Import hint
                        //   2. GETTABLEKS chain — walk backward collecting the last key
                        let arg_reg = a.saturating_add(1);
                        let mut name_from_arg: Option<String> = None;
                        // Try hints first (GETIMPORT case)
                        if let Some(hs) = hints.get(&arg_reg) {
                            for (_, h) in hs.iter().rev() {
                                if let RegisterHint::Import(path) = h {
                                    let last = path.rsplit('.').next().unwrap_or(path);
                                    if !last.is_empty() && is_valid_luau_identifier(last) {
                                        name_from_arg = Some(last.to_string());
                                    }
                                    break;
                                }
                            }
                        }
                        // Fallback: backward scan for last GETTABLEKS on arg_reg
                        if name_from_arg.is_none() {
                            let mut back = pc;
                            let mut steps = 0;
                            while back > 0 && steps < 32 {
                                back -= 1;
                                let s_insn = code[back];
                                let s_op = LuauOpcode::from_u8(insn_op(s_insn));
                                let s_a = insn_a(s_insn);
                                if s_a == arg_reg && matches!(s_op, LuauOpcode::GetTableKS) {
                                    // AUX holds the string-constant index for the key
                                    if back + 1 < code.len() {
                                        let key_aux = code[back + 1];
                                        if let Some(key) = get_aux_string(proto, strings, key_aux) {
                                            if is_valid_luau_identifier(&key) {
                                                name_from_arg = Some(key);
                                            }
                                        }
                                    }
                                    break;
                                }
                                if s_a == arg_reg && matches!(s_op, LuauOpcode::GetImport) {
                                    // Handled by hints path above; skip
                                    break;
                                }
                                steps += 1;
                            }
                        }
                        if let Some(name) = name_from_arg {
                            hints.entry(a).or_default().push((pc, RegisterHint::Named(name)));
                        }
                    }
                    // Phase B0.101: Instance.new("ClassName") — name result
                    // from the class string argument, same as NAMECALL's
                    // literal-naming methods. Pattern: GETIMPORT Instance.new
                    // → LOADK "Part" → CALL. Single-arg call (B==2).
                    if is_instance_new && b == 2 {
                        let arg_reg = a.saturating_add(1);
                        let found = find_loadk_string_for_reg(&proto, strings, code, pc, arg_reg);
                        let usable = found.filter(|n|
                            is_valid_luau_identifier(n) && !is_stdlib_shadow_name(n)
                        );
                        if let Some(class_name) = usable {
                            // Use lowercase first letter as convention:
                            // Instance.new("Part") → part, Instance.new("Frame") → frame
                            let lowered = {
                                let mut chars = class_name.chars();
                                match chars.next() {
                                    Some(c) => {
                                        let mut s = c.to_lowercase().to_string();
                                        s.push_str(chars.as_str());
                                        s
                                    }
                                    None => class_name.clone(),
                                }
                            };
                            hints.entry(a).or_default().push((pc, RegisterHint::Named(lowered)));
                        }
                    }
                    // Has at least one captured result, stored back into A.
                    // If NameCall or the require special case already contributed a
                    // Named/CallResult hint for A at ANY prior PC, leave it alone —
                    // NAMECALL sets its hint at pc-N, so a strict `hpc == pc` check
                    // misses it.
                    // Phase B0.82: multi-return CALL result hints.
                    // When C >= 3 (2+ results), install hints for the extra
                    // result registers (A+1, A+2, ...) which otherwise get no
                    // hint and fall back to generic `vN`.
                    //
                    // Most common pattern: `local ok, result = pcall(fn, ...)`
                    // → R[A] = success boolean, R[A+1] = result or error.
                    if c >= 3 {
                        let is_pcall_xpcall = hints.get(&a).map_or(false, |hs| {
                            hs.iter().any(|(_, h)| matches!(h,
                                RegisterHint::Import(p)
                                    if p == "pcall" || p == "xpcall"))
                        });
                        let num_extra = (c - 2) as usize; // extra result count beyond R[A]
                        for offset in 1..=num_extra {
                            let extra_reg = a.wrapping_add(offset as u8);
                            if is_pcall_xpcall && offset == 1 {
                                // Second result of pcall/xpcall is the result/error
                                hints.entry(extra_reg).or_default()
                                    .push((pc, RegisterHint::Named("err".to_string())));
                            } else {
                                // Generic extra result
                                let name = format!("result{}", offset + 1);
                                hints.entry(extra_reg).or_default()
                                    .push((pc, RegisterHint::Named(name)));
                            }
                        }
                        // For pcall/xpcall, also improve the primary result name
                        // from generic "result" to the more accurate "success"
                        if is_pcall_xpcall {
                            hints.entry(a).or_default()
                                .push((pc, RegisterHint::Named("success".to_string())));
                        }
                    }
                    let has_result_hint = hints.get(&a).map_or(false, |h|
                        h.iter().any(|(_, x)| matches!(x,
                            RegisterHint::Named(_) | RegisterHint::CallResult(_)))
                    );
                    if !has_result_hint {
                        // Phase B0.38: derive CallResult name from the callee's
                        // pre-CALL hint on register A. For direct function calls
                        // (no preceding NAMECALL), register A was loaded by
                        // GETIMPORT / GETGLOBAL / GETUPVAL / GETTABLEKS, so its
                        // hint list already carries a Named or Import tag.
                        // (NAMECALL pairs are already handled by the has_result_hint
                        // short-circuit above.)
                        // Extract a candidate CallResult name from the callee
                        // register's pre-CALL Import hint. Returns:
                        //   Some(Ok(name))  — safe to use as CallResult
                        //   Some(Err(()))   — saw an Import but it was stdlib-
                        //                     shadowed (prefer "result")
                        //   None            — no Import hint at all (use "call")
                        // Phase B0.43C: enrich CallResult with parent segment
                        // when the last segment is a generic constructor-style
                        // method (`new`, `fromRGB`, etc.) OR when the parent is
                        // a well-known module (`math`, `string`, `table`, …).
                        // This lets `name_from_call_result` differentiate
                        // `Vector3.new` ("vec") from `Instance.new` ("instance")
                        // and `math.floor` ("floor") from `string.sub` ("sub").
                        let fn_probe: Option<Result<String, ()>> = hints.get(&a).and_then(|hs| {
                            let mut before: Vec<&(usize, RegisterHint)> = hs.iter()
                                .filter(|(hpc, _)| *hpc < pc)
                                .collect();
                            before.sort_by_key(|(hpc, _)| *hpc);
                            before.iter().rev().find_map(|(_, h)| match h {
                                RegisterHint::Import(p) => {
                                    let segs: Vec<&str> = p.split('.').collect();
                                    let last = *segs.last().unwrap_or(&p.as_str());
                                    if last.is_empty() || !is_valid_luau_identifier(last) {
                                        return Some(Err(()));
                                    }
                                    if is_stdlib_shadow_name(last) {
                                        // Single-segment stdlib callee
                                        // (require/pcall/etc). Signal downstream
                                        // to use "result" so we emit
                                        // `local result = pcall(...)`.
                                        return Some(Err(()));
                                    }
                                    // If a parent segment exists and is useful,
                                    // keep "Parent.last" as the key for richer
                                    // downstream dispatch.
                                    if segs.len() >= 2 {
                                        let parent = segs[segs.len() - 2];
                                        if is_parent_worth_keeping(parent, last) {
                                            return Some(Ok(format!("{}.{}", parent, last)));
                                        }
                                    }
                                    Some(Ok(last.to_string()))
                                }
                                _ => None,
                            })
                        });
                        let call_result_name = match fn_probe {
                            Some(Ok(name)) => name,
                            Some(Err(())) => "result".to_string(),
                            None => "call".to_string(),
                        };
                        hints.entry(a).or_default()
                            .push((pc, RegisterHint::CallResult(call_result_name)));
                    }
                }
            }
            LuauOpcode::ForNPrep => {
                // Phase B0.3 fix: Luau v6 numeric-for loop variable lives in R(A+2),
                // NOT R(A+3). The layout is:
                //   R(A+0) = limit
                //   R(A+1) = step
                //   R(A+2) = initial index + loop variable during body
                // Verified against ModuleScript.luac Proto 9 / Proto 11 (see
                // lifter.rs Region::NumericFor handler for detail).
                let counter_reg = (a as usize + 2) & 0xFF;
                hints.entry(counter_reg as u8).or_default().push((pc, RegisterHint::NumericForVar));
            }
            LuauOpcode::ForGPrep | LuauOpcode::ForGPrepINext | LuauOpcode::ForGPrepNext => {
                // Generic for: first var at A+3, second at A+4
                let key_reg = (a as usize + 3) & 0xFF;
                let val_reg = (a as usize + 4) & 0xFF;
                hints.entry(key_reg as u8).or_default().push((pc, RegisterHint::GenericForKey));
                hints.entry(val_reg as u8).or_default().push((pc, RegisterHint::GenericForVal));
            }
            // Phase B0.43C: propagate LHS name across arithmetic ops (and K-
            // variants). Most arithmetic in hand-written Luau is of the form
            // `count = count + 1` — the destination register IS the same
            // conceptual variable. We only propagate when the LHS already has
            // a *meaningful* (non-generic) Named or Import last-segment hint.
            // Phase B0.93: extended to Mod/Pow/ModK/PowK — in Roblox code,
            // `offset = offset % total` and `val = val ^ exp` are common
            // wrapping/cycling/exponentiation patterns where the result
            // retains the LHS identity.
            LuauOpcode::Add | LuauOpcode::Sub | LuauOpcode::Mul
            | LuauOpcode::Div | LuauOpcode::IDiv
            | LuauOpcode::Mod | LuauOpcode::Pow
            | LuauOpcode::AddK | LuauOpcode::SubK | LuauOpcode::MulK
            | LuauOpcode::DivK | LuauOpcode::IDivK
            | LuauOpcode::ModK | LuauOpcode::PowK => {
                let lhs_reg = b;
                if let Some(name) = lhs_name_for_propagation(&hints, lhs_reg, pc) {
                    hints.entry(a).or_default()
                        .push((pc, RegisterHint::Named(name)));
                }
            }
            // Phase B0.84: reverse-K arithmetic: R(A) = K(B) op R(C).
            // The meaningful variable is in R(C) (right operand), not B (constant).
            LuauOpcode::SubRK | LuauOpcode::DivRK => {
                if let Some(name) = lhs_name_for_propagation(&hints, c, pc) {
                    hints.entry(a).or_default()
                        .push((pc, RegisterHint::Named(name)));
                }
            }
            // Phase B0.81: propagate hints across MOVE R(A), R(B).
            // When the source register already has a Named or Import hint,
            // the destination should inherit it. Common pattern: a local
            // variable is MOVEd to a new register for argument passing or
            // scope transfer. Without this, the destination falls back to
            // the generic `vN` name even though we know what it holds.
            LuauOpcode::Move => {
                // Only propagate semantic hints (Named, Import), not generic
                // positional ones (Param, Closure, Table) which would confuse
                // naming downstream.
                // Clone the hint to avoid borrow conflict on `hints`.
                let propagated: Option<RegisterHint> = hints.get(&b).and_then(|src| {
                    src.iter().rev().find(|(hpc, h)| {
                        *hpc <= pc && matches!(h, RegisterHint::Named(_) | RegisterHint::Import(_))
                    }).map(|(_, h)| h.clone())
                });
                if let Some(hint) = propagated {
                    hints.entry(a).or_default().push((pc, hint));
                }
            }
            // Phase B0.93: propagate LHS name across CONCAT.
            // Pattern: `str = str .. suffix` — the result is the same
            // conceptual string variable. B = first register in the
            // concatenation range.
            LuauOpcode::Concat => {
                if let Some(name) = lhs_name_for_propagation(&hints, b, pc) {
                    hints.entry(a).or_default()
                        .push((pc, RegisterHint::Named(name)));
                }
            }
            // Phase B0.93: propagate LHS name across bitwise ops.
            // Pattern: `flags = flags & mask`, `bits = bits | flag`.
            // The result retains the LHS identity in typical Roblox code.
            LuauOpcode::Band | LuauOpcode::Bor | LuauOpcode::Bxor
            | LuauOpcode::Bandk | LuauOpcode::Bork => {
                let lhs_reg = b;
                if let Some(name) = lhs_name_for_propagation(&hints, lhs_reg, pc) {
                    hints.entry(a).or_default()
                        .push((pc, RegisterHint::Named(name)));
                }
            }
            // Phase B0.96: Roblox repurposed many standard opcodes as
            // passthrough (type-annotation propagation). In the lifter these
            // are `regs[a] = regs[b]` — semantically identical to MOVE.
            // Propagate Named/Import hints from the source register so the
            // destination doesn't fall back to the generic `v{reg}` name.
            // Covered: Not, Minus, Length, BNot (standard unary ops),
            //          Shl, Shr (standard bitwise shifts),
            //          RbxExt92/93/94/98 (Roblox type-annotation passthroughs).
            LuauOpcode::Not | LuauOpcode::Minus | LuauOpcode::Length
            | LuauOpcode::Bnot | LuauOpcode::Shl | LuauOpcode::Shr
            | LuauOpcode::RbxExt92 | LuauOpcode::RbxExt93
            | LuauOpcode::RbxExt94 | LuauOpcode::RbxExt98 => {
                let propagated: Option<RegisterHint> = hints.get(&b).and_then(|src| {
                    src.iter().rev().find(|(hpc, h)| {
                        *hpc <= pc && matches!(h, RegisterHint::Named(_) | RegisterHint::Import(_))
                    }).map(|(_, h)| h.clone())
                });
                if let Some(hint) = propagated {
                    hints.entry(a).or_default().push((pc, hint));
                }
            }
            // Phase B0.100: And/Or/AndK/OrK — result goes to A, propagate
            // from B (first operand).  Common pattern: `x = x or default`
            // should keep the name of x rather than falling back to v{N}.
            LuauOpcode::And | LuauOpcode::Or
            | LuauOpcode::AndK | LuauOpcode::OrK => {
                if let Some(name) = lhs_name_for_propagation(&hints, b, pc) {
                    hints.entry(a).or_default()
                        .push((pc, RegisterHint::Named(name)));
                }
            }
            _ => {}
        }

        // Advance PC, skipping AUX word
        pc += if op.has_aux() { 2 } else { 1 };
    }

    // ── Phase B0.45B: SETTABLEKS field-name → source register back-propagation
    //
    // Pattern: `_M.foo = function() end` compiles to
    //   NEWCLOSURE R(value), <child>
    //   SETTABLEKS R(value), R(_M), AUX=K["foo"]
    //      (encoding: R(B)[K(AUX)] = R(A)  → A=value, B=table)
    //
    // We want `value` to render as `foo` rather than `v3`. Run the SETTABLEKS
    // assignment BACKWARDS: if R(A) was written at some prior PC p', install
    // a `Named(K[aux])` hint at p' so downstream reads of R(A) (including the
    // write-back at p') pick up the field name.
    //
    // Guards:
    //   * field name must be a valid Luau identifier
    //   * field name must NOT be a stdlib shadow (avoid `local pcall = ...`)
    //   * A == B (self-assign like `obj.X = obj`) is skipped
    //   * if the same source register is assigned to MULTIPLE different field
    //     names from different prior writes, keep only the first to avoid
    //     ambiguity ("obj.A = x; obj.B = x" loses the second name)
    install_settableks_source_hints(proto, strings, &mut hints);

    // Self-parameter detection: if the first param is used as the object
    // of NameCall/method calls or field accesses, it's likely "self"
    if proto.num_params >= 1 {
        let mut first_param_is_self = false;
        pc = 0;
        while pc < code.len() {
            let insn = code[pc];
            let op = LuauOpcode::from_u8(insn_op(insn));
            let b = insn_b(insn);

            match op {
                LuauOpcode::NameCall | LuauOpcode::GetTableKS | LuauOpcode::SetTableKS => {
                    // B = object register. If it's register 0 (first param), likely self.
                    if b == 0 {
                        first_param_is_self = true;
                        break;
                    }
                }
                _ => {}
            }
            pc += if op.has_aux() { 2 } else { 1 };
        }
        if first_param_is_self {
            // Recorded at PC 0 — self-param is in scope for the entire proto.
            hints.insert(0, vec![(0, RegisterHint::SelfParam)]);
        }
    }

    hints
}

/// Search nearby instructions for a `LOADK target_reg K<str>` and return the
/// constant string, if found. Searches backward first (Luau pre-loads args),
/// then forward. Used by GetService name inference.
fn find_loadk_string_for_reg(
    proto: &Proto,
    _strings: &[String],
    code: &[u32],
    anchor_pc: usize,
    target_reg: u8,
) -> Option<String> {
    // Backward scan: walk back up to 8 instructions. Luau compiles args just
    // before the call site.
    let mut back = anchor_pc;
    let mut steps = 0;
    while back > 0 && steps < 16 {
        back -= 1;
        let insn = code[back];
        let op = LuauOpcode::from_u8(insn_op(insn));
        let a = insn_a(insn);
        if matches!(op, LuauOpcode::LoadK) && a == target_reg {
            let d = insn_d(insn) as u16 as usize;
            if let Some(Constant::String(s)) = proto.constants.get(d) {
                if is_valid_luau_identifier(s) {
                    return Some(s.clone());
                }
            }
            return None;
        }
        // Stop if we hit anything that writes to target_reg (it's been overwritten).
        // Most op-A writes target_reg.
        if a == target_reg
            && !matches!(op, LuauOpcode::Call | LuauOpcode::NameCall
                | LuauOpcode::GetImport | LuauOpcode::GetTableKS | LuauOpcode::GetUpval
                | LuauOpcode::Move | LuauOpcode::LoadN | LuauOpcode::LoadB | LuauOpcode::LoadNil)
        {
            // Tolerate these — they're common arg-setup ops
        }
        steps += 1;
    }
    // Forward scan up to 8 instructions from anchor_pc.
    let mut scan = anchor_pc + 2; // skip past the NAMECALL + AUX word
    let end = (anchor_pc + 16).min(code.len());
    while scan < end {
        let insn = code[scan];
        let op = LuauOpcode::from_u8(insn_op(insn));
        let a = insn_a(insn);
        if matches!(op, LuauOpcode::Call) { break; }
        if matches!(op, LuauOpcode::LoadK) && a == target_reg {
            let d = insn_d(insn) as u16 as usize;
            if let Some(Constant::String(s)) = proto.constants.get(d) {
                if is_valid_luau_identifier(s) {
                    return Some(s.clone());
                }
            }
            return None;
        }
        scan += if op.has_aux() { 2 } else { 1 };
    }
    None
}

/// Phase B0.43C: pick a Named-hint-worthy name for the LHS of a binary op,
/// for use in propagating that name onto the destination register.
///
/// Rules:
///   * Must have an in-scope `Named(n)` or `Import(p)` hint (pc <= current).
///   * For `Named`, the name must be a valid identifier, not a stdlib shadow,
///     and not a generic fallback (no leading-digit, no `arg\d+`, no single-
///     letter loop / key names like `i`/`k`/`v`, no `v\d+` register fallback).
///   * For `Import`, use the last path segment under the same filtering.
///
/// Returns `None` when no meaningful name is found — the destination will fall
/// back to whatever hint it already had (or the generic `v\d+` path).
fn lhs_name_for_propagation(
    hints: &std::collections::HashMap<u8, Vec<(usize, RegisterHint)>>,
    lhs_reg: u8,
    pc: usize,
) -> Option<String> {
    let hs = hints.get(&lhs_reg)?;
    // Prefer most-recent in-scope hint.
    let mut before: Vec<&(usize, RegisterHint)> =
        hs.iter().filter(|(hpc, _)| *hpc <= pc).collect();
    before.sort_by_key(|(hpc, _)| *hpc);
    for (_, h) in before.iter().rev() {
        let candidate: Option<String> = match h {
            RegisterHint::Named(n) => Some(n.clone()),
            RegisterHint::Import(p) => p.rsplit('.').next().map(|s| s.to_string()),
            // Do NOT propagate: CallResult, Closure, Table, Param, SelfParam,
            // NumericForVar, GenericForKey, GenericForVal. These are either
            // already good enough on their own, or propagating them would be
            // noisy (e.g., `arg1 + 1` shouldn't name the result `arg1`).
            _ => None,
        };
        if let Some(name) = candidate {
            if is_meaningful_propagation_name(&name) {
                return Some(name);
            }
        }
    }
    None
}

/// Phase B0.43C: is a name worth propagating through an arithmetic op?
///
/// Rejects:
///   * empty / invalid identifiers
///   * stdlib-shadow names
///   * generic register fallbacks (`v\d+`)
///   * generic argument names (`arg\d+`)
///   * single-letter loop iterators (`i`, `j`, `k`, `v`) — these are already
///     contextual and propagating them buries the signal in noise
fn is_meaningful_propagation_name(name: &str) -> bool {
    if name.is_empty() { return false; }
    if !is_valid_luau_identifier(name) { return false; }
    if is_stdlib_shadow_name(name) { return false; }
    // v\d+ — register fallback
    if name.starts_with('v') && name.len() >= 2
        && name[1..].chars().all(|c| c.is_ascii_digit())
    {
        return false;
    }
    // arg\d+ — parameter fallback
    if name.starts_with("arg") && name.len() >= 4
        && name[3..].chars().all(|c| c.is_ascii_digit())
    {
        return false;
    }
    // Single-letter loop vars (i, j, k, v, x, y, z, n, m).
    if name.len() == 1 {
        return false;
    }
    true
}

/// Phase B0.45B: SETTABLEKS field-name → source register back-propagation.
///
/// Scan the proto's code for `SETTABLEKS A B AUX` (R(B)[K(AUX)] = R(A)) and,
/// for each instance where the AUX string is a usable identifier, install a
/// `Named(field_name)` hint for register A at the PC where A was most recently
/// written. This captures the idiomatic Roblox module pattern
/// `_M.someFunction = function() end` — the value register gets the function's
/// field name as its local-variable name.
///
/// Ambiguity handling: if the same source (register, prior-write PC) pair is
/// touched by multiple SETTABLEKS field names, the FIRST one wins. Subsequent
/// differing names are dropped (to avoid renaming the register twice with
/// conflicting suggestions in the same scope).
fn install_settableks_source_hints(
    proto: &Proto,
    strings: &[String],
    hints: &mut std::collections::HashMap<u8, Vec<(usize, RegisterHint)>>,
) {
    let code = &proto.code;
    // Track (source_reg, prior_write_pc) → field_name already installed, so
    // the SECOND differing SETTABLEKS naming the same source writer is skipped.
    let mut installed: std::collections::HashMap<(u8, usize), String> =
        std::collections::HashMap::new();

    let mut pc = 0;
    while pc < code.len() {
        let insn = code[pc];
        let op = LuauOpcode::from_u8(insn_op(insn));
        if !matches!(op, LuauOpcode::SetTableKS) {
            pc += if op.has_aux() { 2 } else { 1 };
            continue;
        }
        let a = insn_a(insn);        // value register
        let b = insn_b(insn);        // table register
        let aux = if pc + 1 < code.len() { code[pc + 1] } else { 0 };

        // Skip self-assignment (`obj.X = obj` confusion guard).
        if a == b {
            pc += 2;
            continue;
        }

        let field = match get_aux_string(proto, strings, aux) {
            Some(s) => s,
            None => { pc += 2; continue; }
        };
        if !is_valid_luau_identifier(&field) || is_stdlib_shadow_name(&field) {
            pc += 2;
            continue;
        }

        // Find the most recent PC < pc whose A field == a (write to source reg).
        // Simple linear back-scan over instructions; skip AUX words by tracking
        // the forward op-layout (needs a walk-forward mapping, but easier:
        // scan backwards one word at a time and skip words that are AUX for
        // the previous instruction).
        let prior_write_pc = find_most_recent_write_pc(code, pc, a);
        if let Some(p_prev) = prior_write_pc {
            match installed.get(&(a, p_prev)) {
                Some(existing) if existing == &field => {
                    // Same name re-installed (idempotent); nothing to do.
                }
                Some(_) => {
                    // Ambiguous: same source reg/PC with a different field
                    // already recorded. Keep the first, skip this one.
                }
                None => {
                    hints.entry(a).or_default()
                        .push((p_prev, RegisterHint::Named(field.clone())));
                    installed.insert((a, p_prev), field);
                }
            }
        }
        pc += 2;
    }
}

/// Walk forward through `code` from PC 0 to `anchor_pc`, returning the PC of
/// the most recent instruction that WRITES R(`target_reg`). Instructions that
/// merely READ from A (SET* family, RETURN) are skipped.
///
/// Forward iteration (rather than a naive backward walk) correctly handles
/// Luau's variable-length AUX words — we always know whether the current word
/// is an instruction or an AUX.
fn find_most_recent_write_pc(code: &[u32], anchor_pc: usize, target_reg: u8) -> Option<usize> {
    let mut last_hit: Option<usize> = None;
    let mut pc = 0;
    while pc < anchor_pc && pc < code.len() {
        let insn = code[pc];
        let op = LuauOpcode::from_u8(insn_op(insn));
        let a = insn_a(insn);
        if a == target_reg && opcode_writes_a(op) {
            last_hit = Some(pc);
        }
        pc += if op.has_aux() { 2 } else { 1 };
    }
    last_hit
}

/// True if the given opcode writes its result into register A (i.e., A is the
/// destination, not a source). Used by B0.45B back-propagation to avoid
/// treating `SETTABLEKS A B AUX` / `SETGLOBAL A …` (which READ from A) as a
/// prior-write for the purpose of naming hints.
fn opcode_writes_a(op: LuauOpcode) -> bool {
    match op {
        // SET family — A is a SOURCE (value being written elsewhere).
        LuauOpcode::SetGlobal
        | LuauOpcode::SetUpval
        | LuauOpcode::SetTable
        | LuauOpcode::SetTableKS
        | LuauOpcode::SetTableN
        | LuauOpcode::SetList
        | LuauOpcode::Return
        // Jumps, prepares, and unconditional control flow don't produce a
        // value in A at all.
        | LuauOpcode::Jump
        | LuauOpcode::JumpBack
        | LuauOpcode::JumpX
        | LuauOpcode::JumpIf
        | LuauOpcode::JumpIfNot
        | LuauOpcode::JumpIfEq
        | LuauOpcode::JumpIfLE
        | LuauOpcode::JumpIfLT
        | LuauOpcode::JumpIfNotEq
        | LuauOpcode::JumpIfNotLE
        | LuauOpcode::JumpIfNotLT
        | LuauOpcode::JumpXEqKNil
        | LuauOpcode::JumpXEqKB
        | LuauOpcode::JumpXEqKN
        | LuauOpcode::JumpXEqKS
        | LuauOpcode::Break
        | LuauOpcode::Nop => false,
        // Everything else — LOAD*, MOVE, arithmetic, GETs, NEWs, CALLs, etc.
        // — writes to A.
        _ => true,
    }
}

/// Try to resolve an AUX word to a string name (used by pre-pass).
fn get_aux_string(proto: &Proto, strings: &[String], aux: u32) -> Option<String> {
    // AUX for NAMECALL/GETTABLEKS/SETTABLEKS is an index into proto.constants
    // where the constant should be a String
    if let Some(Constant::String(s)) = proto.constants.get(aux as usize) {
        return Some(s.clone());
    }
    // Fallback: try chunk strings
    if let Some(s) = strings.get(aux as usize) {
        if !s.is_empty() {
            return Some(s.clone());
        }
    }
    None
}

/// Decompile a single proto into Luau source
pub fn decompile_proto(ctx: &mut DecompileContext, proto: &Proto, proto_index: usize, depth: usize) -> String {
    let stmts = lifter::lift_proto(ctx, proto, proto_index);
    let mut output = String::new();
    emit::emit_block(&mut output, &stmts, depth);
    output
}

/// Convert a constant to an AST expression.
/// `proto_constants` is needed for Import ID resolution — Import IDs are
/// 0-based indices into the proto's constant table (proto.constants), where
/// the constants at those indices should be Strings.
pub fn constant_to_expr(k: &Constant, strings: &[String], proto_constants: &[Constant]) -> Expr {
    match k {
        Constant::Nil => Expr::Nil,
        Constant::Boolean(b) => Expr::Bool(*b),
        Constant::Number(n) => Expr::Number(*n),
        Constant::String(s) => Expr::String(s.clone()),
        Constant::Import(val) => {
            let ids = decode_import(*val);
            // Import IDs are 0-based indices into proto.constants, where
            // each referenced constant should be a String. The VM resolves
            // them via k[id] (constant table lookup).
            let parts: Vec<String> = ids
                .iter()
                .filter_map(|&id| {
                    // Primary: proto.constants (the authoritative source per Luau VM)
                    if let Some(Constant::String(s)) = proto_constants.get(id as usize) {
                        return Some(s.clone());
                    }
                    // Fallback: chunk.strings with 0-based indexing
                    // (works because parsed String constants resolve from chunk.strings)
                    if let Some(s) = strings.get(id as usize) {
                        return Some(s.clone());
                    }
                    None
                })
                .collect();
            // Guard: all resolved parts must be valid identifiers, otherwise
            // the import IDs pointed at data strings (not global/field names).
            let all_valid = !parts.is_empty()
                && parts.iter().all(|p| is_valid_luau_identifier(p));
            if all_valid && parts.len() == 1 {
                Expr::Name(parts[0].clone())
            } else if all_valid && parts.len() >= 2 {
                let mut expr = Expr::Name(parts[0].clone());
                for part in &parts[1..] {
                    expr = Expr::Field {
                        object: Box::new(expr),
                        field: part.clone(),
                    };
                }
                expr
            } else if parts.len() == 1 {
                // Single non-identifier string → emit as quoted string literal
                Expr::String(parts[0].clone())
            } else {
                // Import IDs could not be resolved to strings or contained
                // non-identifier parts. Use _G as a safe placeholder name.
                Expr::Name("_G".to_string())
            }
        }
        Constant::Vector(x, y, z, _) => Expr::Vector(*x, *y, *z),
        Constant::Table(entries) => {
            // Table constant: create a table with named fields from key indices.
            // Key indices reference the PROTO's constant table (not chunk strings).
            // Each referenced constant should be a String.
            //
            // For LBC_CONSTANT_TABLE_WITH_CONSTANTS (bytecode v7+), the template
            // also stores a *value* constant index per entry. When present, use
            // it — the compiler already baked the field value into the template
            // and no runtime SETTABLEKS will follow. Otherwise the field stays
            // Nil and will be overwritten by a later SETTABLEKS instruction.
            let mut fields = Vec::new();
            for &(key_idx, value_idx) in entries {
                let key_name = if let Some(Constant::String(s)) = proto_constants.get(key_idx as usize) {
                    s.clone()
                } else if let Some(s) = strings.get(key_idx as usize) {
                    // Fallback: chunk strings with 0-based indexing
                    // (consistent with get_table_string_from_aux)
                    s.clone()
                } else {
                    continue;
                };
                let value_expr = match value_idx {
                    Some(idx) => match proto_constants.get(idx as usize) {
                        Some(k) => constant_to_expr(k, strings, proto_constants),
                        None => Expr::Nil,
                    },
                    None => Expr::Nil,
                };
                fields.push(TableField::Named(key_name, value_expr));
            }
            Expr::Table { fields }
        }
        Constant::Closure(_) => Expr::Name("_closure".to_string()),
    }
}

#[cfg(test)]
mod hint_path_tests {
    //! Phase B0.39B regression tests — lock in the B0.37 / B0.38 hint-installer
    //! paths in `analyze_register_usage` plus the `is_stdlib_shadow_name` /
    //! `is_valid_luau_identifier` classifiers.
    //!
    //! These tests were written after B0.39 (register-lifetime-aware hint expiry)
    //! was rejected on a same-cache A/B (total lines +47%, `v\d+` +51%, `upval_`
    //! +442%). The rejection showed the current stale-hint behavior is load-bearing
    //! for the GETUPVAL → SETUPVAL round-trip DCE path. Any future attempt to
    //! change the hint installers must not silently regress the B0.37 global /
    //! upval / table-field naming wins or the B0.38 CALL-destination naming +
    //! stdlib-shadow blacklist.
    //!
    //! See vault notes [[Phase B0.37 Naming Hint Pack]],
    //! [[Phase B0.38 CALL Destination Naming]], and the Phase B0.39 REJECTED
    //! entry in [[Phase History]].
    use super::{
        analyze_register_usage, is_stdlib_shadow_name, is_valid_luau_identifier,
        RegisterHint,
    };
    use crate::parser::types::{Constant, Proto};

    // Canonical (non-shuffled) Luau v6 opcode bytes used in the tests.
    const OP_LOADN: u8      = 4;
    const OP_GETGLOBAL: u8  = 7;
    const OP_GETUPVAL: u8   = 9;
    const OP_GETIMPORT: u8  = 12;
    const OP_GETTABLEKS: u8 = 15;
    const OP_CALL: u8       = 21;
    const OP_RETURN: u8     = 22;
    const OP_NEWCLOSURE: u8 = 19;
    const OP_DUPCLOSURE: u8 = 82;
    // Phase B0.43C — arithmetic opcodes for binary-op LHS propagation tests.
    const OP_ADD: u8        = 33;
    const OP_SUB: u8        = 34;
    const OP_MUL: u8        = 35;
    const OP_DIV: u8        = 36;
    const OP_MOD: u8        = 37;
    const OP_POW: u8        = 38;
    const OP_ADDK: u8       = 39;

    fn insn_ad(op: u8, a: u8, d: i16) -> u32 {
        let du = d as u16 as u32;
        (op as u32) | ((a as u32) << 8) | (du << 16)
    }

    fn insn_abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
        (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
    }

    fn make_proto(code: Vec<u32>, constants: Vec<Constant>) -> Proto {
        Proto {
            max_stack_size: 16,
            num_params: 0,
            num_upvalues: 0,
            is_vararg: false,
            flags: 0,
            typeinfo: None,
            code,
            constants,
            child_protos: Vec::new(),
            line_defined: 1,
            debug_name: None,
            line_info: None,
            debug_info: None,
        }
    }

    /// Pack a Luau import value: `count << 30 | id0 << 20 | id1 << 10 | id2`.
    /// `ids` is a slice of constant-pool indices to pack (length 1-3).
    fn pack_import(ids: &[u32]) -> u32 {
        let count = (ids.len() as u32) & 0x3;
        let mut v = count << 30;
        if !ids.is_empty()   { v |= (ids[0] & 0x3FF) << 20; }
        if ids.len() >= 2    { v |= (ids[1] & 0x3FF) << 10; }
        if ids.len() >= 3    { v |=  ids[2] & 0x3FF;        }
        v
    }

    // ── is_stdlib_shadow_name ─────────────────────────────────────────────

    #[test]
    fn stdlib_shadow_flags_common_globals() {
        // Subset of the blacklist — if any of these regress to `false`, the
        // B0.38 stdlib-shadow gate will leak `local pcall = pcall(...)` etc.
        assert!(is_stdlib_shadow_name("pcall"));
        assert!(is_stdlib_shadow_name("xpcall"));
        assert!(is_stdlib_shadow_name("require"));
        assert!(is_stdlib_shadow_name("pairs"));
        assert!(is_stdlib_shadow_name("ipairs"));
        assert!(is_stdlib_shadow_name("next"));
        assert!(is_stdlib_shadow_name("assert"));
        assert!(is_stdlib_shadow_name("game"));
        assert!(is_stdlib_shadow_name("workspace"));
        assert!(is_stdlib_shadow_name("script"));
        assert!(is_stdlib_shadow_name("task"));
    }

    #[test]
    fn stdlib_shadow_ignores_user_names() {
        assert!(!is_stdlib_shadow_name("UserSettings"));
        assert!(!is_stdlib_shadow_name("TestUtils"));
        assert!(!is_stdlib_shadow_name("my_var"));
        // Case-sensitive: only the canonical lowercase forms shadow.
        assert!(!is_stdlib_shadow_name("Pcall"));
        assert!(!is_stdlib_shadow_name("PAIRS"));
        assert!(!is_stdlib_shadow_name(""));
    }

    // ── is_valid_luau_identifier ──────────────────────────────────────────

    #[test]
    fn identifier_accepts_normal_names() {
        assert!(is_valid_luau_identifier("UserSettings"));
        assert!(is_valid_luau_identifier("_private"));
        assert!(is_valid_luau_identifier("x1"));
        assert!(is_valid_luau_identifier("snake_case"));
        assert!(is_valid_luau_identifier("CamelCase"));
    }

    #[test]
    fn identifier_rejects_invalid_input() {
        assert!(!is_valid_luau_identifier(""));
        assert!(!is_valid_luau_identifier("1foo"));    // starts with digit
        assert!(!is_valid_luau_identifier("foo-bar")); // non-identifier char
        assert!(!is_valid_luau_identifier("foo.bar")); // dotted path
        assert!(!is_valid_luau_identifier("space name"));
    }

    #[test]
    fn identifier_rejects_reserved_keywords() {
        // Using a Luau keyword as a hint would emit invalid source.
        assert!(!is_valid_luau_identifier("if"));
        assert!(!is_valid_luau_identifier("then"));
        assert!(!is_valid_luau_identifier("end"));
        assert!(!is_valid_luau_identifier("local"));
        assert!(!is_valid_luau_identifier("return"));
        assert!(!is_valid_luau_identifier("function"));
        assert!(!is_valid_luau_identifier("continue"));
        assert!(!is_valid_luau_identifier("type"));
    }

    // ── analyze_register_usage: B0.37 installer paths ─────────────────────

    #[test]
    fn b37_getimport_installs_single_segment_import_hint() {
        // GETIMPORT R0, K0  (AUX = packed single-segment path referring to K0)
        // → expect hints[0] to contain (pc=0, Import("UserSettings")).
        let code = vec![
            insn_ad(OP_GETIMPORT, 0, 0),
            pack_import(&[0]),
            insn_abc(OP_RETURN, 0, 1, 0),
        ];
        let constants = vec![Constant::String("UserSettings".to_string())];
        let proto = make_proto(code, constants);
        let hints = analyze_register_usage(&proto, &[], None, None);

        let r0 = hints.get(&0).expect("R0 must have at least one hint");
        let import = r0.iter().find_map(|(pc, h)| match h {
            RegisterHint::Import(p) => Some((*pc, p.clone())),
            _ => None,
        });
        assert_eq!(import, Some((0, "UserSettings".to_string())));
    }

    #[test]
    fn b37_getimport_joins_multi_segment_paths() {
        // Two-segment path "game.Workspace" → Import("game.Workspace").
        let code = vec![
            insn_ad(OP_GETIMPORT, 1, 0),
            pack_import(&[0, 1]),
            insn_abc(OP_RETURN, 1, 1, 0),
        ];
        let constants = vec![
            Constant::String("game".to_string()),
            Constant::String("Workspace".to_string()),
        ];
        let proto = make_proto(code, constants);
        let hints = analyze_register_usage(&proto, &[], None, None);

        let r1 = hints.get(&1).expect("R1 must have hints");
        let path = r1.iter().find_map(|(_, h)| match h {
            RegisterHint::Import(p) => Some(p.clone()),
            _ => None,
        });
        assert_eq!(path, Some("game.Workspace".to_string()));
    }

    #[test]
    fn b37_getglobal_installs_named_hint() {
        // GETGLOBAL R0, _ [AUX=0 → K0="UserSettings"] → Named("UserSettings").
        let code = vec![
            insn_abc(OP_GETGLOBAL, 0, 0, 0),
            0u32, // AUX: constant index
            insn_abc(OP_RETURN, 0, 1, 0),
        ];
        let constants = vec![Constant::String("UserSettings".to_string())];
        let proto = make_proto(code, constants);
        let hints = analyze_register_usage(&proto, &[], None, None);

        let r0 = hints.get(&0).expect("R0 must have hints");
        let named = r0.iter().find_map(|(_, h)| match h {
            RegisterHint::Named(n) => Some(n.clone()),
            _ => None,
        });
        assert_eq!(named, Some("UserSettings".to_string()));
    }

    #[test]
    fn b37_getglobal_ignores_non_identifier_aux() {
        // An AUX that resolves to a non-identifier (e.g., a string with a space)
        // must NOT produce a Named hint — that would emit invalid Luau.
        let code = vec![
            insn_abc(OP_GETGLOBAL, 0, 0, 0),
            0u32,
            insn_abc(OP_RETURN, 0, 1, 0),
        ];
        let constants = vec![Constant::String("has space".to_string())];
        let proto = make_proto(code, constants);
        let hints = analyze_register_usage(&proto, &[], None, None);

        let any_named = hints.get(&0).map_or(false, |h| {
            h.iter().any(|(_, x)| matches!(x, RegisterHint::Named(_)))
        });
        assert!(!any_named, "non-identifier AUX must not install a Named hint");
    }

    #[test]
    fn b37_getupval_installs_named_from_upval_names() {
        // GETUPVAL R0, 1 with caller-provided names [_, "counter"].
        let code = vec![
            insn_abc(OP_GETUPVAL, 0, 1, 0),
            insn_abc(OP_RETURN, 0, 1, 0),
        ];
        let proto = make_proto(code, Vec::new());
        let upval_names = vec!["outer_self".to_string(), "counter".to_string()];
        let hints = analyze_register_usage(&proto, &[], Some(&upval_names), None);

        let r0 = hints.get(&0).expect("R0 must have hints");
        let named = r0.iter().find_map(|(_, h)| match h {
            RegisterHint::Named(n) => Some(n.clone()),
            _ => None,
        });
        assert_eq!(named, Some("counter".to_string()));
    }

    #[test]
    fn b37_getupval_without_names_installs_nothing() {
        // No upval_names supplied → installer must be a no-op (no Named hint).
        let code = vec![
            insn_abc(OP_GETUPVAL, 2, 0, 0),
            insn_abc(OP_RETURN, 2, 1, 0),
        ];
        let proto = make_proto(code, Vec::new());
        let hints = analyze_register_usage(&proto, &[], None, None);

        let any_named = hints.get(&2).map_or(false, |h| {
            h.iter().any(|(_, x)| matches!(x, RegisterHint::Named(_)))
        });
        assert!(!any_named, "GETUPVAL without names must not invent a name");
    }

    #[test]
    fn b37_gettableks_installs_named_field_hint() {
        // GETTABLEKS R1, R0, _ [AUX=K0="Parent"] → Named("Parent") on R1.
        // (num_params=0 so self-param detection is skipped; B=0 is harmless.)
        let code = vec![
            insn_abc(OP_GETTABLEKS, 1, 0, 0),
            0u32,
            insn_abc(OP_RETURN, 1, 1, 0),
        ];
        let constants = vec![Constant::String("Parent".to_string())];
        let proto = make_proto(code, constants);
        let hints = analyze_register_usage(&proto, &[], None, None);

        let r1 = hints.get(&1).expect("R1 must have hints");
        let field = r1.iter().find_map(|(_, h)| match h {
            RegisterHint::Named(n) => Some(n.clone()),
            _ => None,
        });
        assert_eq!(field, Some("Parent".to_string()));
    }

    // ── analyze_register_usage: B0.38 CALL destination-naming ─────────────

    #[test]
    fn b38_call_inherits_import_last_segment() {
        // R0 = GETIMPORT "game.Workspace.UserSettings"  →  CALL R0 (nargs=0, nres=1)
        // Expect CallResult("UserSettings") pushed at the CALL's PC (= 2).
        let code = vec![
            insn_ad(OP_GETIMPORT, 0, 0),
            pack_import(&[0, 1, 2]),
            insn_abc(OP_CALL, 0, 1, 2),
            insn_abc(OP_RETURN, 0, 1, 0),
        ];
        let constants = vec![
            Constant::String("game".to_string()),
            Constant::String("Workspace".to_string()),
            Constant::String("UserSettings".to_string()),
        ];
        let proto = make_proto(code, constants);
        let hints = analyze_register_usage(&proto, &[], None, None);

        let r0 = hints.get(&0).expect("R0 must have hints");
        let call_result = r0.iter().find_map(|(pc, h)| match h {
            RegisterHint::CallResult(n) if *pc == 2 => Some(n.clone()),
            _ => None,
        });
        assert_eq!(
            call_result,
            Some("UserSettings".to_string()),
            "CALL result must inherit the callee Import's last path segment"
        );
    }

    #[test]
    fn b38_call_uses_result_when_callee_is_stdlib() {
        // R0 = GETIMPORT "pcall" → CALL R0.
        // B0.38 blacklist: pcall is stdlib-shadowed → CallResult must be "result",
        // not "pcall" (avoids `local pcall = pcall(...)`).
        let code = vec![
            insn_ad(OP_GETIMPORT, 0, 0),
            pack_import(&[0]),
            insn_abc(OP_CALL, 0, 1, 2),
            insn_abc(OP_RETURN, 0, 1, 0),
        ];
        let constants = vec![Constant::String("pcall".to_string())];
        let proto = make_proto(code, constants);
        let hints = analyze_register_usage(&proto, &[], None, None);

        let r0 = hints.get(&0).expect("R0 must have hints");
        let call_result = r0.iter().find_map(|(pc, h)| match h {
            RegisterHint::CallResult(n) if *pc == 2 => Some(n.clone()),
            _ => None,
        });
        assert_eq!(
            call_result,
            Some("result".to_string()),
            "stdlib-shadowed callees must yield `result`, not the shadowed name"
        );
    }

    #[test]
    fn b38_call_uses_call_when_no_import_hint() {
        // R0 set by LOADN (no Import hint) → fn_probe returns None → "call".
        let code = vec![
            insn_ad(OP_LOADN, 0, 42),
            insn_abc(OP_CALL, 0, 1, 2),
            insn_abc(OP_RETURN, 0, 1, 0),
        ];
        let proto = make_proto(code, Vec::new());
        let hints = analyze_register_usage(&proto, &[], None, None);

        let r0 = hints.get(&0).expect("R0 must have CallResult hint");
        let call_result = r0.iter().find_map(|(pc, h)| match h {
            RegisterHint::CallResult(n) if *pc == 1 => Some(n.clone()),
            _ => None,
        });
        assert_eq!(
            call_result,
            Some("call".to_string()),
            "absent Import hint must fall back to the neutral `call` name"
        );
    }

    #[test]
    fn b38_require_extracts_module_name_from_import_arg() {
        // R0 = GETIMPORT "require"
        // R1 = GETIMPORT "script.Parent.TestUtils"
        // CALL R0 nargs=1 nres=1 → expect Named("TestUtils") on R0 at CALL's PC.
        let code = vec![
            insn_ad(OP_GETIMPORT, 0, 0),
            pack_import(&[0]),
            insn_ad(OP_GETIMPORT, 1, 0),
            pack_import(&[1, 2, 3]),
            insn_abc(OP_CALL, 0, 2, 2),
            insn_abc(OP_RETURN, 0, 1, 0),
        ];
        let constants = vec![
            Constant::String("require".to_string()),
            Constant::String("script".to_string()),
            Constant::String("Parent".to_string()),
            Constant::String("TestUtils".to_string()),
        ];
        let proto = make_proto(code, constants);
        let hints = analyze_register_usage(&proto, &[], None, None);

        let r0 = hints.get(&0).expect("R0 must have hints");
        let named = r0.iter().find_map(|(pc, h)| match h {
            RegisterHint::Named(n) if *pc == 4 => Some(n.clone()),
            _ => None,
        });
        assert_eq!(
            named,
            Some("TestUtils".to_string()),
            "require(path) must name the result after the last path segment"
        );
    }

    // ── Wave 1C: debug_name → NEWCLOSURE/DUPCLOSURE Named hint ────────────
    //
    // When the parent proto emits NEWCLOSURE/DUPCLOSURE for a child proto
    // that carries a `debug_name`, the destination register should receive
    // a `Named(debug_name)` hint instead of the generic `Closure` sentinel —
    // unless the name is empty, a stdlib shadow, or not a valid identifier.

    /// Build a parent+child chunk-proto slice for NEWCLOSURE D=0 tests.
    /// Child proto has the given `debug_name` (None → empty).
    fn make_closure_pair(child_debug_name: Option<&str>) -> (Proto, Vec<Proto>) {
        let parent_code = vec![
            // NEWCLOSURE R0, D=0 → resolves (Roblox fallback) to child proto idx 0.
            insn_ad(OP_NEWCLOSURE, 0, 0),
            insn_abc(OP_RETURN, 0, 1, 0),
        ];
        let parent = make_proto(parent_code, Vec::new());
        let child = Proto {
            max_stack_size: 2,
            num_params: 0,
            num_upvalues: 0,
            is_vararg: false,
            flags: 0,
            typeinfo: None,
            code: vec![insn_abc(OP_RETURN, 0, 1, 0)],
            constants: Vec::new(),
            child_protos: Vec::new(),
            line_defined: 1,
            debug_name: child_debug_name.map(|s| s.to_string()),
            line_info: None,
            debug_info: None,
        };
        (parent, vec![child])
    }

    #[test]
    fn w1c_newclosure_installs_named_hint_from_debug_name() {
        let (parent, protos) = make_closure_pair(Some("doSomething"));
        let hints = analyze_register_usage(&parent, &[], None, Some(&protos));

        let r0 = hints.get(&0).expect("R0 must have hints");
        let named = r0.iter().find_map(|(_, h)| match h {
            RegisterHint::Named(n) => Some(n.clone()),
            _ => None,
        });
        assert_eq!(named, Some("doSomething".to_string()));
        // When a Named hint wins, the generic Closure fallback must NOT be
        // installed too (it would confuse select_hint preference ordering).
        let any_closure = r0.iter().any(|(_, h)| matches!(h, RegisterHint::Closure));
        assert!(!any_closure, "Named hint must replace, not duplicate, Closure");
    }

    #[test]
    fn w1c_newclosure_empty_debug_name_falls_back_to_closure() {
        // None → absent debug_name
        let (parent, protos) = make_closure_pair(None);
        let hints = analyze_register_usage(&parent, &[], None, Some(&protos));

        let r0 = hints.get(&0).expect("R0 must have the Closure fallback hint");
        let any_named = r0.iter().any(|(_, h)| matches!(h, RegisterHint::Named(_)));
        let any_closure = r0.iter().any(|(_, h)| matches!(h, RegisterHint::Closure));
        assert!(!any_named, "missing debug_name must not install a Named hint");
        assert!(any_closure, "must still install Closure fallback");

        // Explicit empty string also falls back.
        let (parent2, protos2) = make_closure_pair(Some(""));
        let hints2 = analyze_register_usage(&parent2, &[], None, Some(&protos2));
        let any_named2 = hints2.get(&0).map_or(false, |h| {
            h.iter().any(|(_, x)| matches!(x, RegisterHint::Named(_)))
        });
        assert!(!any_named2, "empty debug_name must not install a Named hint");
    }

    #[test]
    fn w1c_newclosure_stdlib_shadow_name_is_rejected() {
        // A child proto named "pcall" would shadow the stdlib function — the
        // B0.38 blacklist already forbids this shape elsewhere, and we must
        // honor it here too.
        let (parent, protos) = make_closure_pair(Some("pcall"));
        let hints = analyze_register_usage(&parent, &[], None, Some(&protos));

        let r0 = hints.get(&0).expect("R0 must have the Closure fallback hint");
        let any_named = r0.iter().any(|(_, h)| matches!(h, RegisterHint::Named(_)));
        assert!(!any_named, "stdlib shadow debug_name must not install a Named hint");
        assert!(r0.iter().any(|(_, h)| matches!(h, RegisterHint::Closure)));
    }

    #[test]
    fn w1c_newclosure_invalid_identifier_debug_name_is_rejected() {
        // "1foo" starts with a digit — emitting `local 1foo = function()` is
        // invalid Luau, so is_valid_luau_identifier must block this path.
        let (parent, protos) = make_closure_pair(Some("1foo"));
        let hints = analyze_register_usage(&parent, &[], None, Some(&protos));

        let r0 = hints.get(&0).expect("R0 must have the Closure fallback hint");
        let any_named = r0.iter().any(|(_, h)| matches!(h, RegisterHint::Named(_)));
        assert!(!any_named, "invalid identifier must not install a Named hint");
        assert!(r0.iter().any(|(_, h)| matches!(h, RegisterHint::Closure)));
    }

    #[test]
    fn w1c_newclosure_reserved_keyword_debug_name_is_rejected() {
        // A child proto literally named "if" would emit invalid Luau — the
        // reserved-keyword branch of is_valid_luau_identifier must block it.
        let (parent, protos) = make_closure_pair(Some("if"));
        let hints = analyze_register_usage(&parent, &[], None, Some(&protos));

        let r0 = hints.get(&0).expect("R0 must have the Closure fallback hint");
        let any_named = r0.iter().any(|(_, h)| matches!(h, RegisterHint::Named(_)));
        assert!(!any_named, "reserved keyword must not install a Named hint");
        assert!(r0.iter().any(|(_, h)| matches!(h, RegisterHint::Closure)));
    }

    #[test]
    fn w1c_newclosure_without_chunk_protos_falls_back_to_closure() {
        // When chunk_protos is None (legacy callers / unit fixtures), the
        // Named path is unreachable — behavior must exactly match the pre-
        // Wave-1C `Closure`-only hint so B0.37/B0.38 tests still pass.
        let (parent, _protos) = make_closure_pair(Some("doSomething"));
        let hints = analyze_register_usage(&parent, &[], None, None);

        let r0 = hints.get(&0).expect("R0 must have the Closure fallback hint");
        let any_named = r0.iter().any(|(_, h)| matches!(h, RegisterHint::Named(_)));
        assert!(!any_named, "without chunk_protos we cannot resolve debug_name");
        assert!(r0.iter().any(|(_, h)| matches!(h, RegisterHint::Closure)));
    }

    #[test]
    fn w1c_dupclosure_uses_constant_closure_child_idx() {
        // DUPCLOSURE R0, K0 where K0 = Constant::Closure(0) → resolves to
        // chunk_protos[0] which has debug_name="animate".
        let parent_code = vec![
            insn_ad(OP_DUPCLOSURE, 0, 0),  // D=0 indexes constants
            insn_abc(OP_RETURN, 0, 1, 0),
        ];
        let mut parent = make_proto(parent_code, vec![Constant::Closure(0)]);
        parent.child_protos = Vec::new(); // Roblox shape: empty
        let child = Proto {
            max_stack_size: 2,
            num_params: 0,
            num_upvalues: 0,
            is_vararg: false,
            flags: 0,
            typeinfo: None,
            code: vec![insn_abc(OP_RETURN, 0, 1, 0)],
            constants: Vec::new(),
            child_protos: Vec::new(),
            line_defined: 1,
            debug_name: Some("animate".to_string()),
            line_info: None,
            debug_info: None,
        };
        let protos = vec![child];
        let hints = analyze_register_usage(&parent, &[], None, Some(&protos));

        let r0 = hints.get(&0).expect("R0 must have hints");
        let named = r0.iter().find_map(|(_, h)| match h {
            RegisterHint::Named(n) => Some(n.clone()),
            _ => None,
        });
        assert_eq!(named, Some("animate".to_string()));
    }

    // ── Phase B0.43C: CallResult universalization + LHS propagation ───────
    //
    // The following tests lock in two expansions of the B0.37/B0.38 hint
    // pipeline:
    //   1. CallResult hints now preserve a parent segment for generic
    //      constructor-style callees (`Vector3.new`) and stdlib-module
    //      callees (`math.floor`, `string.format`, …). The downstream
    //      `name_from_call_result` dispatches on the enriched key to pick
    //      richer names ("vec", "floor", "formatted", …) in place of the
    //      B0.38 catch-all "instance"/"call".
    //   2. Arithmetic opcodes (Add/Sub/Mul/Div/IDiv + K variants) propagate
    //      a meaningful LHS Named/Import name onto the destination register
    //      so that `count = count + 1` renders as `local count = count + 1`
    //      instead of `local v42 = count + 1`.

    // ── 1. CallResult enrichment ──────────────────────────────────────────

    #[test]
    fn b43c_call_vector3_new_yields_parent_qualified_key() {
        // R0 = GETIMPORT "Vector3.new" → CALL R0.
        // Expect CallResult("Vector3.new") on R0 at the CALL's PC.
        let code = vec![
            insn_ad(OP_GETIMPORT, 0, 0),
            pack_import(&[0, 1]),
            insn_abc(OP_CALL, 0, 1, 2),
            insn_abc(OP_RETURN, 0, 1, 0),
        ];
        let constants = vec![
            Constant::String("Vector3".to_string()),
            Constant::String("new".to_string()),
        ];
        let proto = make_proto(code, constants);
        let hints = analyze_register_usage(&proto, &[], None, None);

        let r0 = hints.get(&0).expect("R0 must have hints");
        let call_result = r0.iter().find_map(|(pc, h)| match h {
            RegisterHint::CallResult(n) if *pc == 2 => Some(n.clone()),
            _ => None,
        });
        assert_eq!(
            call_result,
            Some("Vector3.new".to_string()),
            "generic constructor `new` must keep the parent segment for type discrimination"
        );
    }

    #[test]
    fn b43c_call_math_floor_yields_parent_qualified_key() {
        // R0 = GETIMPORT "math.floor" → CALL R0.
        let code = vec![
            insn_ad(OP_GETIMPORT, 0, 0),
            pack_import(&[0, 1]),
            insn_abc(OP_CALL, 0, 1, 2),
            insn_abc(OP_RETURN, 0, 1, 0),
        ];
        let constants = vec![
            Constant::String("math".to_string()),
            Constant::String("floor".to_string()),
        ];
        let proto = make_proto(code, constants);
        let hints = analyze_register_usage(&proto, &[], None, None);

        let r0 = hints.get(&0).expect("R0 must have hints");
        let call_result = r0.iter().find_map(|(pc, h)| match h {
            RegisterHint::CallResult(n) if *pc == 2 => Some(n.clone()),
            _ => None,
        });
        assert_eq!(
            call_result,
            Some("math.floor".to_string()),
            "stdlib module callees must keep the parent segment so names dispatch correctly"
        );
    }

    #[test]
    fn b43c_call_non_generic_last_still_keeps_single_segment() {
        // The B0.38 test locks in `CallResult("UserSettings")` for
        // `game.Workspace.UserSettings`. Make sure the enrichment path does
        // NOT retroactively change names that were already meaningful on
        // their own.
        let code = vec![
            insn_ad(OP_GETIMPORT, 0, 0),
            pack_import(&[0, 1, 2]),
            insn_abc(OP_CALL, 0, 1, 2),
            insn_abc(OP_RETURN, 0, 1, 0),
        ];
        let constants = vec![
            Constant::String("game".to_string()),
            Constant::String("Workspace".to_string()),
            Constant::String("UserSettings".to_string()),
        ];
        let proto = make_proto(code, constants);
        let hints = analyze_register_usage(&proto, &[], None, None);

        let r0 = hints.get(&0).expect("R0 must have hints");
        let call_result = r0.iter().find_map(|(pc, h)| match h {
            RegisterHint::CallResult(n) if *pc == 2 => Some(n.clone()),
            _ => None,
        });
        assert_eq!(
            call_result,
            Some("UserSettings".to_string()),
            "non-generic last segment must NOT be parent-qualified"
        );
    }

    #[test]
    fn b43c_call_stdlib_shadow_still_uses_result_even_when_qualified() {
        // A hypothetical import like `script.pcall` with last segment
        // `pcall` must NOT be naively kept as-is. The stdlib-shadow guard
        // still applies. We check via CallResult naming end-to-end using a
        // DecompileContext.
        use crate::decompiler::DecompileContext;
        use crate::parser::types::Chunk;

        let chunk = Chunk {
            version: 6,
            types_version: 0,
            strings: Vec::new(),
            protos: Vec::new(),
            main_proto: 0,
        };
        let mut ctx = DecompileContext::new(&chunk);
        ctx.init_proto_naming(0, std::collections::HashMap::new());
        ctx.current_proto_index = Some(0);
        // Force the synthesizer path: empty naming state, call the helper.
        let name = ctx.name_from_call_result("pcall");
        assert_eq!(
            name, "result",
            "stdlib shadow must not be surfaced as a local name via CallResult"
        );
    }

    // ── 2. name_from_call_result dispatch (direct unit tests) ────────────

    fn helper_name_for(func: &str) -> String {
        use crate::decompiler::DecompileContext;
        use crate::parser::types::Chunk;
        let chunk = Chunk {
            version: 6,
            types_version: 0,
            strings: Vec::new(),
            protos: Vec::new(),
            main_proto: 0,
        };
        let mut ctx = DecompileContext::new(&chunk);
        // Initialize per-proto naming state so gen_scoped_name uses the
        // counter-suffix shape (first use = bare prefix, second = "prefix2")
        // instead of the fallback "prefix_1", "prefix_2" shape used when no
        // proto is active.
        ctx.init_proto_naming(0, std::collections::HashMap::new());
        ctx.current_proto_index = Some(0);
        ctx.name_from_call_result(func)
    }

    #[test]
    fn b43c_name_from_call_result_vector3_new_is_vec() {
        assert_eq!(helper_name_for("Vector3.new"), "vec");
    }

    #[test]
    fn b43c_name_from_call_result_cframe_new_is_cf() {
        assert_eq!(helper_name_for("CFrame.new"), "cf");
        assert_eq!(helper_name_for("CFrame.lookAt"), "cf");
        assert_eq!(helper_name_for("CFrame.Angles"), "cf");
    }

    #[test]
    fn b43c_name_from_call_result_color3_variants_are_color() {
        assert_eq!(helper_name_for("Color3.new"), "color");
        assert_eq!(helper_name_for("Color3.fromRGB"), "color");
        assert_eq!(helper_name_for("Color3.fromHSV"), "color");
    }

    #[test]
    fn b43c_name_from_call_result_udim2_is_udim() {
        assert_eq!(helper_name_for("UDim2.new"), "udim");
        assert_eq!(helper_name_for("UDim2.fromScale"), "udim");
    }

    #[test]
    fn b43c_name_from_call_result_math_members_map_to_result_kind() {
        assert_eq!(helper_name_for("math.floor"), "floor");
        assert_eq!(helper_name_for("math.ceil"), "ceil");
        assert_eq!(helper_name_for("math.abs"), "abs");
        assert_eq!(helper_name_for("math.min"), "min");
        assert_eq!(helper_name_for("math.max"), "max");
        assert_eq!(helper_name_for("math.clamp"), "clamped");
    }

    #[test]
    fn b43c_name_from_call_result_string_members_map_to_operation_kind() {
        assert_eq!(helper_name_for("string.format"), "formatted");
        assert_eq!(helper_name_for("string.upper"), "upper");
        assert_eq!(helper_name_for("string.lower"), "lower");
        assert_eq!(helper_name_for("string.find"), "found");
        assert_eq!(helper_name_for("string.gsub"), "replaced");
        assert_eq!(helper_name_for("string.match"), "matched");
    }

    #[test]
    fn b43c_name_from_call_result_table_members_map_to_collection_kind() {
        assert_eq!(helper_name_for("table.find"), "found");
        assert_eq!(helper_name_for("table.concat"), "joined");
        assert_eq!(helper_name_for("table.sort"), "sorted");
    }

    #[test]
    fn b43c_name_from_call_result_bare_method_names_cover_common_idioms() {
        // These are the NAMECALL-style hints (no parent segment available).
        assert_eq!(helper_name_for("Clone"), "clone");
        assert_eq!(helper_name_for("Connect"), "connection");
        assert_eq!(helper_name_for("Once"), "once");
        assert_eq!(helper_name_for("GetDescendants"), "descendants");
        assert_eq!(helper_name_for("floor"), "floor");
        assert_eq!(helper_name_for("format"), "formatted");
    }

    #[test]
    fn b43c_name_from_call_result_b38_baseline_names_preserved() {
        // The original B0.37/B0.38 dispatch must keep working: any regression
        // here would change the emitted names for the B0.39B corpus lock-in
        // tests upstream.
        assert_eq!(helper_name_for("GetService"), "service");
        assert_eq!(helper_name_for("WaitForChild"), "child");
        assert_eq!(helper_name_for("FindFirstChild"), "child");
        assert_eq!(helper_name_for("new"), "instance");
        assert_eq!(helper_name_for("Create"), "obj");
    }

    // ── 3. LHS name propagation ──────────────────────────────────────────

    #[test]
    fn b43c_add_propagates_named_lhs_to_dest() {
        // R0 = GETGLOBAL "counter" (AUX=K0) ;  R1 = R0 + R2  (ADD)
        // Expect R1 to pick up Named("counter") at the ADD's PC (=2).
        let code = vec![
            insn_abc(OP_GETGLOBAL, 0, 0, 0),
            0u32,
            insn_abc(OP_ADD, 1, 0, 2),
            insn_abc(OP_RETURN, 1, 1, 0),
        ];
        let constants = vec![Constant::String("counter".to_string())];
        let proto = make_proto(code, constants);
        let hints = analyze_register_usage(&proto, &[], None, None);

        let r1 = hints.get(&1).expect("R1 must have a propagated hint");
        let named = r1.iter().find_map(|(pc, h)| match h {
            RegisterHint::Named(n) if *pc == 2 => Some(n.clone()),
            _ => None,
        });
        assert_eq!(
            named,
            Some("counter".to_string()),
            "ADD must propagate the LHS Named hint onto the destination register"
        );
    }

    #[test]
    fn b43c_addk_propagates_named_lhs_to_dest() {
        // R0 = GETGLOBAL "total"; R1 = R0 +K K0  (ADDK)
        let code = vec![
            insn_abc(OP_GETGLOBAL, 0, 0, 0),
            0u32,
            insn_abc(OP_ADDK, 1, 0, 1),
            insn_abc(OP_RETURN, 1, 1, 0),
        ];
        let constants = vec![
            Constant::String("total".to_string()),
            Constant::Number(1.0),
        ];
        let proto = make_proto(code, constants);
        let hints = analyze_register_usage(&proto, &[], None, None);

        let r1 = hints.get(&1).expect("R1 must have a propagated hint");
        let named = r1.iter().find_map(|(pc, h)| match h {
            RegisterHint::Named(n) if *pc == 2 => Some(n.clone()),
            _ => None,
        });
        assert_eq!(named, Some("total".to_string()));
    }

    #[test]
    fn b43c_sub_mul_div_idiv_all_propagate() {
        // A single fixture with four back-to-back ops. Destination registers
        // R1..R4 should all carry `Named("counter")`.
        const OP_MUL_L: u8 = OP_MUL;
        const OP_DIV_L: u8 = OP_DIV;
        const OP_IDIV: u8  = 76;
        let code = vec![
            insn_abc(OP_GETGLOBAL, 0, 0, 0),
            0u32,
            insn_abc(OP_SUB, 1, 0, 5),
            insn_abc(OP_MUL_L, 2, 0, 5),
            insn_abc(OP_DIV_L, 3, 0, 5),
            insn_abc(OP_IDIV, 4, 0, 5),
            insn_abc(OP_RETURN, 1, 1, 0),
        ];
        let constants = vec![Constant::String("counter".to_string())];
        let proto = make_proto(code, constants);
        let hints = analyze_register_usage(&proto, &[], None, None);

        for reg in [1u8, 2, 3, 4] {
            let found = hints.get(&reg).and_then(|hs| {
                hs.iter().find_map(|(_, h)| match h {
                    RegisterHint::Named(n) => Some(n.clone()),
                    _ => None,
                })
            });
            assert_eq!(
                found,
                Some("counter".to_string()),
                "R{} must carry propagated Named(counter)",
                reg
            );
        }
    }

    #[test]
    fn b093_mod_and_pow_propagate_lhs_name() {
        // Phase B0.93: MOD and POW now propagate the LHS name.
        // In Roblox code, `offset = offset % total` and `val = val ^ exp`
        // are common patterns where the result retains the LHS identity.
        let code = vec![
            insn_abc(OP_GETGLOBAL, 0, 0, 0),
            0u32,
            insn_abc(OP_MOD, 1, 0, 5),
            insn_abc(OP_POW, 2, 0, 5),
            insn_abc(OP_RETURN, 1, 1, 0),
        ];
        let constants = vec![Constant::String("counter".to_string())];
        let proto = make_proto(code, constants);
        let hints = analyze_register_usage(&proto, &[], None, None);

        for reg in [1u8, 2] {
            let any_named = hints.get(&reg).map_or(false, |hs| {
                hs.iter().any(|(_, h)| matches!(h, RegisterHint::Named(_)))
            });
            assert!(
                any_named,
                "R{} should inherit LHS name via MOD/POW (B0.93)",
                reg
            );
        }
    }

    #[test]
    fn b43c_generic_lhs_does_not_propagate() {
        // If the LHS register has NO meaningful hint (e.g., only LOADN), the
        // destination must not inherit a generic `v\d+` name.
        let code = vec![
            insn_ad(OP_LOADN, 0, 5),
            insn_abc(OP_ADD, 1, 0, 2),
            insn_abc(OP_RETURN, 1, 1, 0),
        ];
        let proto = make_proto(code, Vec::new());
        let hints = analyze_register_usage(&proto, &[], None, None);

        let r1 = hints.get(&1);
        let any_named = r1.map_or(false, |hs| {
            hs.iter().any(|(_, h)| matches!(h, RegisterHint::Named(_)))
        });
        assert!(
            !any_named,
            "ADD with a bare-register LHS must NOT invent a Named hint"
        );
    }

    #[test]
    fn b43c_stdlib_shadow_lhs_name_is_rejected_for_propagation() {
        // If, for some reason, the LHS already had a Named hint that is a
        // stdlib shadow (pcall/require/...), we must NOT propagate it.
        //
        // We synthesize this directly by manipulating the GETUPVAL path: a
        // supplied upval_names slice with a stdlib name installs the Named
        // hint that the B0.43C propagation filter must reject.
        let code = vec![
            insn_abc(OP_GETUPVAL, 0, 0, 0),  // R0 ← upval[0] (would be Named("pcall") if not filtered)
            insn_abc(OP_ADD, 1, 0, 2),
            insn_abc(OP_RETURN, 1, 1, 0),
        ];
        let proto = make_proto(code, Vec::new());
        // "pcall" is blocked by the pre-existing B0.37 upval-install guard
        // (is_stdlib_shadow_name is NOT currently in that path, but
        // is_valid_luau_identifier is; use a legitimate name then verify the
        // propagation filter still rejects it when it comes out as a shadow).
        //
        // To exercise the propagation-side guard specifically, we use a name
        // that WOULD install (is_valid_luau_identifier("pcall") == true)
        // but is_stdlib_shadow_name("pcall") == true. The B0.37 installer
        // has no shadow check, so R0 ends up with Named("pcall"). Then we
        // check that R1 does NOT also pick it up via propagation.
        let upval_names = vec!["pcall".to_string()];
        let hints = analyze_register_usage(&proto, &[], Some(&upval_names), None);

        // Sanity: R0 should have the Named("pcall") hint installed.
        let r0_named = hints.get(&0).and_then(|hs| hs.iter().find_map(|(_, h)| match h {
            RegisterHint::Named(n) => Some(n.clone()),
            _ => None,
        }));
        assert_eq!(r0_named, Some("pcall".to_string()),
            "fixture precondition: R0 must carry Named(pcall)");
        // The real assertion: R1 must NOT have a Named hint at all — the
        // propagation filter rejects stdlib shadows.
        let r1_any_named = hints.get(&1).map_or(false, |hs|
            hs.iter().any(|(_, h)| matches!(h, RegisterHint::Named(_)))
        );
        assert!(!r1_any_named,
            "propagation must reject stdlib-shadow LHS names (no `local pcall = pcall + 1`)");
    }

    #[test]
    fn b43c_import_lhs_propagates_last_segment() {
        // R0 = GETIMPORT "game.Workspace.score" ;  R1 = R0 + R2 (ADD).
        // Expect R1 to inherit Named("score") from the Import's last segment.
        let code = vec![
            insn_ad(OP_GETIMPORT, 0, 0),
            pack_import(&[0, 1, 2]),
            insn_abc(OP_ADD, 1, 0, 3),
            insn_abc(OP_RETURN, 1, 1, 0),
        ];
        let constants = vec![
            Constant::String("game".to_string()),
            Constant::String("Workspace".to_string()),
            Constant::String("score".to_string()),
        ];
        let proto = make_proto(code, constants);
        let hints = analyze_register_usage(&proto, &[], None, None);

        let r1 = hints.get(&1).expect("R1 must have a propagated hint");
        let named = r1.iter().find_map(|(_, h)| match h {
            RegisterHint::Named(n) => Some(n.clone()),
            _ => None,
        });
        assert_eq!(
            named,
            Some("score".to_string()),
            "Import LHS should propagate its last path segment as the destination Named hint"
        );
    }

    #[test]
    fn b43c_single_letter_loop_var_is_not_propagated() {
        // A one-character name (e.g., loop counter `i`) must NOT propagate —
        // propagating `i` buries the signal by making every arithmetic
        // destination named `i2`, `i3`, etc.
        //
        // We simulate by wiring up GETGLOBAL with a one-char name. Since
        // is_valid_luau_identifier accepts "i", the B0.37 installer puts
        // Named("i") on R0. The propagation filter must reject it.
        let code = vec![
            insn_abc(OP_GETGLOBAL, 0, 0, 0),
            0u32,
            insn_abc(OP_ADD, 1, 0, 2),
            insn_abc(OP_RETURN, 1, 1, 0),
        ];
        let constants = vec![Constant::String("i".to_string())];
        let proto = make_proto(code, constants);
        let hints = analyze_register_usage(&proto, &[], None, None);

        let r0_named = hints.get(&0).and_then(|hs| hs.iter().find_map(|(_, h)| match h {
            RegisterHint::Named(n) => Some(n.clone()),
            _ => None,
        }));
        assert_eq!(r0_named, Some("i".to_string()),
            "fixture precondition: R0 must carry Named(i)");
        let r1_any_named = hints.get(&1).map_or(false, |hs|
            hs.iter().any(|(_, h)| matches!(h, RegisterHint::Named(_)))
        );
        assert!(!r1_any_named,
            "single-letter LHS must not propagate — too noisy to be useful");
    }

    // ── Phase B0.45B: SETTABLEKS field-name → source register back-propagation
    //
    // Pattern under test: `_M.someFunction = function() end` compiles to:
    //   NEWCLOSURE R(value), <child>
    //   SETTABLEKS R(value), R(_M), AUX=K["someFunction"]
    //      (encoding: R(B)[K(AUX)] = R(A) → A=value, B=table, AUX=field name)
    //
    // The SETTABLEKS tells us the function literal's "real name" is the field
    // key. B0.45B runs the assignment backwards and installs Named(field)
    // for R(A) at the PC where A was most recently written, so the emitted
    // local picks up the field name instead of a generic `v3` / `fn2`.

    const OP_SETTABLEKS: u8 = 16;
    const OP_SETLIST: u8    = 55;
    const OP_NAMECALL: u8   = 20;
    const OP_NEWTABLE: u8   = 53;

    #[test]
    fn b45b_settableks_installs_named_at_prior_write_pc() {
        // R0 = NEWCLOSURE (child proto 0, no debug_name)
        // R1 = NEWTABLE (the module table `_M`)
        // SETTABLEKS R0, R1, AUX=K0 ("someFunction")   // _M.someFunction = R0
        // expect hints[R0] to contain Named("someFunction") at the
        // NEWCLOSURE's PC (=0), not just the inherent Closure fallback.
        let code = vec![
            insn_ad(OP_NEWCLOSURE, 0, 0),
            insn_abc(OP_NEWTABLE, 1, 0, 0),
            0u32, // AUX for NEWTABLE
            insn_abc(OP_SETTABLEKS, 0, 1, 0),
            0u32, // AUX for SETTABLEKS → K0 = "someFunction"
            insn_abc(OP_RETURN, 0, 1, 0),
        ];
        let constants = vec![Constant::String("someFunction".to_string())];
        let proto = make_proto(code, constants);
        let hints = analyze_register_usage(&proto, &[], None, None);

        let r0 = hints.get(&0).expect("R0 must have hints");
        let named = r0.iter().find_map(|(pc, h)| match h {
            RegisterHint::Named(n) if *pc == 0 => Some(n.clone()),
            _ => None,
        });
        assert_eq!(
            named,
            Some("someFunction".to_string()),
            "SETTABLEKS must back-install Named(field) at the NEWCLOSURE's PC"
        );
    }

    #[test]
    fn b45b_settableks_multi_field_same_source_keeps_first_only() {
        // R0 = LOADN 42  (some value register we're about to alias twice)
        // R1 = NEWTABLE
        // SETTABLEKS R0, R1, AUX=K0="alpha"   // _M.alpha = R0
        // SETTABLEKS R0, R1, AUX=K1="beta"    // _M.beta  = R0  (ambiguous!)
        // Only the FIRST field (alpha) may be installed. The second must be
        // dropped so we don't flip-flop the register's name.
        let code = vec![
            insn_ad(OP_LOADN, 0, 42),
            insn_abc(OP_NEWTABLE, 1, 0, 0),
            0u32,
            insn_abc(OP_SETTABLEKS, 0, 1, 0),
            0u32, // K0
            insn_abc(OP_SETTABLEKS, 0, 1, 0),
            1u32, // K1
            insn_abc(OP_RETURN, 0, 1, 0),
        ];
        let constants = vec![
            Constant::String("alpha".to_string()),
            Constant::String("beta".to_string()),
        ];
        let proto = make_proto(code, constants);
        let hints = analyze_register_usage(&proto, &[], None, None);

        let r0 = hints.get(&0).expect("R0 must have hints");
        let named_installs: Vec<String> = r0.iter()
            .filter_map(|(_, h)| match h {
                RegisterHint::Named(n) => Some(n.clone()),
                _ => None,
            })
            .collect();
        // The FIRST field wins — beta must be rejected.
        assert!(
            named_installs.iter().any(|n| n == "alpha"),
            "first SETTABLEKS should install Named(alpha); got {:?}",
            named_installs,
        );
        assert!(
            !named_installs.iter().any(|n| n == "beta"),
            "second SETTABLEKS with different field name must NOT install; got {:?}",
            named_installs,
        );
    }

    #[test]
    fn b45b_settableks_stdlib_shadow_field_is_rejected() {
        // Field name "pcall" would create `local pcall = ...` — must be
        // blocked by the B0.38 stdlib-shadow guard.
        let code = vec![
            insn_ad(OP_NEWCLOSURE, 0, 0),
            insn_abc(OP_NEWTABLE, 1, 0, 0),
            0u32,
            insn_abc(OP_SETTABLEKS, 0, 1, 0),
            0u32,
            insn_abc(OP_RETURN, 0, 1, 0),
        ];
        let constants = vec![Constant::String("pcall".to_string())];
        let proto = make_proto(code, constants);
        let hints = analyze_register_usage(&proto, &[], None, None);

        let r0 = hints.get(&0).expect("R0 must have Closure hint at minimum");
        let any_named_pcall = r0.iter().any(|(_, h)|
            matches!(h, RegisterHint::Named(n) if n == "pcall")
        );
        assert!(
            !any_named_pcall,
            "stdlib-shadow field name must not install a Named hint"
        );
    }

    #[test]
    fn b45b_settableks_invalid_identifier_field_is_rejected() {
        // A field with a space (or leading digit) is a perfectly legal Luau
        // table key, but an invalid identifier — it cannot be used as a
        // local name.
        let code = vec![
            insn_ad(OP_NEWCLOSURE, 0, 0),
            insn_abc(OP_NEWTABLE, 1, 0, 0),
            0u32,
            insn_abc(OP_SETTABLEKS, 0, 1, 0),
            0u32,
            insn_abc(OP_RETURN, 0, 1, 0),
        ];
        let constants = vec![Constant::String("has space".to_string())];
        let proto = make_proto(code, constants);
        let hints = analyze_register_usage(&proto, &[], None, None);

        let r0 = hints.get(&0).expect("R0 must have Closure hint");
        let any_named = r0.iter().any(|(_, h)| matches!(h, RegisterHint::Named(_)));
        assert!(
            !any_named,
            "invalid-identifier field name must not install a Named hint"
        );
    }

    #[test]
    fn b45b_settableks_self_assign_is_skipped() {
        // SETTABLEKS with A == B (e.g., `obj.X = obj`) is confusing — we do
        // not want to rename the object after one of its own fields.
        let code = vec![
            insn_abc(OP_NEWTABLE, 0, 0, 0),
            0u32,
            insn_abc(OP_SETTABLEKS, 0, 0, 0),
            0u32, // K0 = "weird"
            insn_abc(OP_RETURN, 0, 1, 0),
        ];
        let constants = vec![Constant::String("weird".to_string())];
        let proto = make_proto(code, constants);
        let hints = analyze_register_usage(&proto, &[], None, None);

        let any_named_weird = hints.get(&0).map_or(false, |hs|
            hs.iter().any(|(_, h)|
                matches!(h, RegisterHint::Named(n) if n == "weird"))
        );
        assert!(
            !any_named_weird,
            "self-assign SETTABLEKS (A == B) must not back-propagate a field name"
        );
    }

    #[test]
    fn b45b_setlist_and_namecall_do_not_install_field_hints() {
        // SETLIST and NAMECALL both take an AUX word in the stream but their
        // semantics are unrelated to the field-naming pattern B0.45B targets.
        // Feeding a non-SETTABLEKS AUX word into the back-propagator would
        // corrupt the register hints.
        let code = vec![
            insn_ad(OP_NEWCLOSURE, 0, 0),
            // SETLIST R1, R0, C  [AUX=0]
            insn_abc(OP_SETLIST, 1, 0, 1),
            0u32,
            // NAMECALL R2, R0, _ [AUX=K0="Connect"]
            insn_abc(OP_NAMECALL, 2, 0, 0),
            0u32,
            insn_abc(OP_RETURN, 0, 1, 0),
        ];
        // Important: "Connect" is an identifier. If SETLIST or NAMECALL were
        // mistakenly routed through the SETTABLEKS installer, R0 would pick
        // up Named("Connect") as its local name. It must not.
        let constants = vec![Constant::String("Connect".to_string())];
        let proto = make_proto(code, constants);
        let hints = analyze_register_usage(&proto, &[], None, None);

        let r0 = hints.get(&0).expect("R0 must have Closure hint from NEWCLOSURE");
        let any_named_connect = r0.iter().any(|(_, h)|
            matches!(h, RegisterHint::Named(n) if n == "Connect")
        );
        assert!(
            !any_named_connect,
            "SETLIST/NAMECALL must not trigger the SETTABLEKS field-naming path"
        );
    }

    #[test]
    fn b45b_b43c_lhs_propagation_regression_guard() {
        // Ensure B0.43C's arithmetic LHS propagation is NOT changed by the
        // new SETTABLEKS pass: `counter = counter + 1`-style patterns still
        // emit Named("counter") on the destination. This is the key
        // non-regression test for the previous phase.
        let code = vec![
            insn_abc(OP_GETGLOBAL, 0, 0, 0),
            0u32,
            insn_abc(OP_ADD, 1, 0, 2),
            insn_abc(OP_RETURN, 1, 1, 0),
        ];
        let constants = vec![Constant::String("counter".to_string())];
        let proto = make_proto(code, constants);
        let hints = analyze_register_usage(&proto, &[], None, None);

        let r1 = hints.get(&1).expect("R1 must have propagated hint");
        let named = r1.iter().find_map(|(pc, h)| match h {
            RegisterHint::Named(n) if *pc == 2 => Some(n.clone()),
            _ => None,
        });
        assert_eq!(
            named,
            Some("counter".to_string()),
            "B0.45B must not regress B0.43C LHS propagation"
        );
    }

    #[test]
    fn b45b_overwrite_between_write_and_settableks_uses_latest_write() {
        // R0 = NEWCLOSURE (closure A) — PC 0
        // R0 = NEWCLOSURE (closure B) — PC 1  (overwrites R0)
        // SETTABLEKS R0, R1, AUX=K0 ("fn")
        // The install must target PC 1 (the most recent write to R0), NOT
        // PC 0. Otherwise closure A gets named "fn" even though it's been
        // replaced.
        let code = vec![
            insn_ad(OP_NEWCLOSURE, 0, 0),  // PC 0
            insn_ad(OP_NEWCLOSURE, 0, 0),  // PC 1
            insn_abc(OP_NEWTABLE, 1, 0, 0),// PC 2
            0u32,                          // PC 3 (AUX for NEWTABLE)
            insn_abc(OP_SETTABLEKS, 0, 1, 0),  // PC 4
            0u32,                          // PC 5 (AUX, K0 = "fn")
            insn_abc(OP_RETURN, 0, 1, 0),
        ];
        let constants = vec![Constant::String("fn".to_string())];
        let proto = make_proto(code, constants);
        let hints = analyze_register_usage(&proto, &[], None, None);

        let r0 = hints.get(&0).expect("R0 must have hints");
        // Exactly one SETTABLEKS-installed Named("fn") should exist, and its
        // PC must be the most recent NEWCLOSURE (PC 1), not PC 0.
        let named_pcs: Vec<usize> = r0.iter()
            .filter_map(|(pc, h)| match h {
                RegisterHint::Named(n) if n == "fn" => Some(*pc),
                _ => None,
            })
            .collect();
        assert_eq!(
            named_pcs,
            vec![1],
            "SETTABLEKS back-install must target the LATEST write, not the earliest"
        );
    }

    // ── Phase B0.93: extended hint propagation tests ──────────────────

    const OP_CONCAT: u8 = 49;
    const OP_BAND: u8   = 84;
    const OP_BOR: u8    = 85;
    const OP_BXOR: u8   = 86;
    const OP_MODK: u8   = 43;
    const OP_POWK: u8   = 44;

    #[test]
    fn b093_concat_propagates_lhs_name() {
        // R0 = GETGLOBAL "message"; R1 = R0 .. R2 (CONCAT B=0, C=2)
        // Expect R1 to inherit Named("message") from R0.
        let code = vec![
            insn_abc(OP_GETGLOBAL, 0, 0, 0),
            0u32, // AUX for GETGLOBAL
            insn_abc(OP_CONCAT, 1, 0, 2),
            insn_abc(OP_RETURN, 1, 1, 0),
        ];
        let constants = vec![Constant::String("message".to_string())];
        let proto = make_proto(code, constants);
        let hints = analyze_register_usage(&proto, &[], None, None);

        let any_named = hints.get(&1).map_or(false, |hs| {
            hs.iter().any(|(_, h)| matches!(h, RegisterHint::Named(n) if n == "message"))
        });
        assert!(
            any_named,
            "CONCAT should propagate LHS name 'message' to destination"
        );
    }

    #[test]
    fn b093_bitwise_propagates_lhs_name() {
        // R0 = GETGLOBAL "flags"; R1 = R0 & R2 (BAND)
        let code = vec![
            insn_abc(OP_GETGLOBAL, 0, 0, 0),
            0u32,
            insn_abc(OP_BAND, 1, 0, 3),
            insn_abc(OP_RETURN, 1, 1, 0),
        ];
        let constants = vec![Constant::String("flags".to_string())];
        let proto = make_proto(code, constants);
        let hints = analyze_register_usage(&proto, &[], None, None);

        let any_named = hints.get(&1).map_or(false, |hs| {
            hs.iter().any(|(_, h)| matches!(h, RegisterHint::Named(n) if n == "flags"))
        });
        assert!(any_named, "BAND should propagate LHS name 'flags' to destination");
    }

    #[test]
    fn b093_bor_propagates_lhs_name() {
        let code = vec![
            insn_abc(OP_GETGLOBAL, 0, 0, 0),
            0u32,
            insn_abc(OP_BOR, 1, 0, 3),
            insn_abc(OP_RETURN, 1, 1, 0),
        ];
        let constants = vec![Constant::String("bits".to_string())];
        let proto = make_proto(code, constants);
        let hints = analyze_register_usage(&proto, &[], None, None);

        let any_named = hints.get(&1).map_or(false, |hs| {
            hs.iter().any(|(_, h)| matches!(h, RegisterHint::Named(n) if n == "bits"))
        });
        assert!(any_named, "BOR should propagate LHS name 'bits' to destination");
    }

    #[test]
    fn b093_modk_propagates_lhs_name() {
        // R0 = GETGLOBAL "offset"; R1 = R0 % K(C) (MODK)
        let code = vec![
            insn_abc(OP_GETGLOBAL, 0, 0, 0),
            0u32,
            insn_abc(OP_MODK, 1, 0, 1),
            insn_abc(OP_RETURN, 1, 1, 0),
        ];
        let constants = vec![
            Constant::String("offset".to_string()),
            Constant::Number(360.0),
        ];
        let proto = make_proto(code, constants);
        let hints = analyze_register_usage(&proto, &[], None, None);

        let any_named = hints.get(&1).map_or(false, |hs| {
            hs.iter().any(|(_, h)| matches!(h, RegisterHint::Named(n) if n == "offset"))
        });
        assert!(any_named, "MODK should propagate LHS name 'offset' to destination");
    }
}

/// Phase B0.106 — reserved-word variable name guard tests.
///
/// Ensures that Luau reserved words (function, end, if, type, etc.) are never
/// emitted as local variable names, even when Named/Import/CallResult hints
/// carry them. Guards is_valid_luau_identifier and is_stdlib_shadow_name
/// integration points in synthesize_name, name_from_call_result, and
/// name_from_import.
#[cfg(test)]
mod b0106_reserved_word_guard_tests {
    use super::{is_valid_luau_identifier, DecompileContext, RegisterHint};
    use crate::parser::types::Chunk;
    use std::collections::HashMap;

    fn test_chunk() -> Chunk {
        Chunk {
            version: 6,
            types_version: 0,
            strings: vec![],
            protos: vec![],
            main_proto: 0,
        }
    }

    fn make_ctx_with_hint(chunk: &Chunk, reg: u8, pc: usize, hint: RegisterHint) -> DecompileContext<'_> {
        let mut ctx = DecompileContext::new(chunk);
        let mut hints: HashMap<u8, Vec<(usize, RegisterHint)>> = HashMap::new();
        hints.entry(reg).or_default().push((pc, hint));
        ctx.init_proto_naming(0, hints);
        ctx.current_proto_index = Some(0);
        ctx
    }

    fn make_ctx(chunk: &Chunk) -> DecompileContext<'_> {
        let mut ctx = DecompileContext::new(chunk);
        ctx.init_proto_naming(0, HashMap::new());
        ctx.current_proto_index = Some(0);
        ctx
    }

    #[test]
    fn is_valid_luau_identifier_rejects_all_hard_keywords() {
        let keywords = [
            "and", "break", "do", "else", "elseif", "end", "false",
            "for", "function", "if", "in", "local", "nil", "not", "or",
            "repeat", "return", "then", "true", "until", "while",
            "continue", "type", "export",
        ];
        for kw in &keywords {
            assert!(!is_valid_luau_identifier(kw),
                "is_valid_luau_identifier should reject reserved word '{}'", kw);
        }
    }

    #[test]
    fn is_valid_luau_identifier_accepts_valid_names() {
        let valid = ["Players", "game2", "_private", "myFunc", "value", "i", "k"];
        for name in &valid {
            assert!(is_valid_luau_identifier(name),
                "is_valid_luau_identifier should accept valid name '{}'", name);
        }
    }

    #[test]
    fn synthesize_name_rejects_reserved_word_function() {
        let chunk = test_chunk();
        let mut ctx = make_ctx_with_hint(&chunk, 0, 0, RegisterHint::Named("function".to_string()));
        let name = ctx.synthesize_name(0, 1);
        assert_ne!(name, "function",
            "synthesize_name must not produce reserved word 'function'");
        assert!(is_valid_luau_identifier(&name),
            "synthesize_name output '{}' must be a valid Luau identifier", name);
    }

    #[test]
    fn synthesize_name_rejects_reserved_word_type() {
        let chunk = test_chunk();
        let mut ctx = make_ctx_with_hint(&chunk, 0, 0, RegisterHint::Named("type".to_string()));
        let name = ctx.synthesize_name(0, 1);
        assert_ne!(name, "type",
            "synthesize_name must not produce reserved word 'type'");
    }

    #[test]
    fn synthesize_name_rejects_reserved_word_end() {
        let chunk = test_chunk();
        let mut ctx = make_ctx_with_hint(&chunk, 0, 0, RegisterHint::Named("end".to_string()));
        let name = ctx.synthesize_name(0, 1);
        assert_ne!(name, "end",
            "synthesize_name must not produce reserved word 'end'");
    }

    #[test]
    fn synthesize_name_rejects_reserved_word_if() {
        let chunk = test_chunk();
        let mut ctx = make_ctx_with_hint(&chunk, 0, 0, RegisterHint::Named("if".to_string()));
        let name = ctx.synthesize_name(0, 1);
        assert_ne!(name, "if",
            "synthesize_name must not produce reserved word 'if'");
    }

    #[test]
    fn synthesize_name_rejects_reserved_word_local() {
        let chunk = test_chunk();
        let mut ctx = make_ctx_with_hint(&chunk, 0, 0, RegisterHint::Named("local".to_string()));
        let name = ctx.synthesize_name(0, 1);
        assert_ne!(name, "local",
            "synthesize_name must not produce reserved word 'local'");
    }

    #[test]
    fn synthesize_name_accepts_normal_named_hint() {
        let chunk = test_chunk();
        let mut ctx = make_ctx_with_hint(&chunk, 0, 0, RegisterHint::Named("Players".to_string()));
        let name = ctx.synthesize_name(0, 1);
        assert_eq!(name, "Players",
            "synthesize_name should use valid Named hint directly");
    }

    #[test]
    fn name_from_import_rejects_reserved_word() {
        let chunk = test_chunk();
        let mut ctx = make_ctx(&chunk);
        let name = ctx.name_from_import("game.function");
        assert_ne!(name, "function",
            "name_from_import must not produce reserved word 'function'");
        assert!(is_valid_luau_identifier(&name),
            "name_from_import output '{}' must be a valid Luau identifier", name);
    }

    #[test]
    fn name_from_import_rejects_reserved_word_type() {
        let chunk = test_chunk();
        let mut ctx = make_ctx(&chunk);
        let name = ctx.name_from_import("game.ReplicatedStorage.type");
        assert_ne!(name, "type",
            "name_from_import must not produce reserved word 'type'");
    }

    #[test]
    fn name_from_import_accepts_valid_segment() {
        let chunk = test_chunk();
        let mut ctx = make_ctx(&chunk);
        let name = ctx.name_from_import("game.Players");
        assert_eq!(name, "Players",
            "name_from_import should use valid last segment");
    }

    #[test]
    fn name_from_call_result_rejects_lowercased_reserved_word() {
        let chunk = test_chunk();
        let mut ctx = make_ctx(&chunk);
        let name = ctx.name_from_call_result("Function");
        assert_ne!(name, "function",
            "name_from_call_result must not produce 'function' from 'Function'");
    }

    #[test]
    fn name_from_call_result_rejects_type_via_stdlib() {
        // "type" is in the stdlib shadow list AND is a reserved word
        let chunk = test_chunk();
        let mut ctx = make_ctx(&chunk);
        let name = ctx.name_from_call_result("type");
        assert_ne!(name, "type",
            "name_from_call_result must not produce 'type'");
    }

    #[test]
    fn name_from_call_result_rejects_end_lowercase() {
        let chunk = test_chunk();
        let mut ctx = make_ctx(&chunk);
        let name = ctx.name_from_call_result("End");
        assert_ne!(name, "end",
            "name_from_call_result must not produce 'end' from 'End'");
    }
}

/// Phase B0.96 — passthrough hint propagation tests.
///
/// Roblox repurposed standard Not/Minus/Length/BNot/Shl/Shr as passthroughs
/// (type-annotation propagation). In the lifter these are `regs[a] = regs[b]`
/// — semantically identical to MOVE. The hint system should propagate
/// Named/Import hints from the source register.
#[cfg(test)]
mod b096_passthrough_hint_tests {
    use super::{analyze_register_usage, RegisterHint};
    use crate::parser::types::{Constant, Proto};

    const OP_GETGLOBAL: u8 = 7;
    const OP_NOT: u8 = 50;
    const OP_MINUS: u8 = 51;
    const OP_LENGTH: u8 = 52;
    const OP_BNOT: u8 = 87;
    const OP_SHL: u8 = 88;
    const OP_SHR: u8 = 89;
    const OP_RETURN: u8 = 22;

    fn insn_abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
        (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
    }

    fn make_proto(code: Vec<u32>, constants: Vec<Constant>) -> Proto {
        Proto {
            max_stack_size: 16,
            num_params: 0,
            num_upvalues: 0,
            is_vararg: false,
            flags: 0,
            typeinfo: None,
            code,
            constants,
            child_protos: Vec::new(),
            line_defined: 1,
            debug_name: Some("test".to_string()),
            line_info: None,
            debug_info: None,
        }
    }

    fn has_named_hint(hints: &std::collections::HashMap<u8, Vec<(usize, RegisterHint)>>,
                      reg: u8, expected: &str) -> bool {
        hints.get(&reg).map_or(false, |hs| {
            hs.iter().any(|(_, h)| matches!(h, RegisterHint::Named(n) if n == expected))
        })
    }

    #[test]
    fn b096_not_passthrough_propagates_hint() {
        // R0 = GETGLOBAL "player"; R1 = NOT R0 (passthrough)
        let code = vec![
            insn_abc(OP_GETGLOBAL, 0, 0, 0),
            0u32, // AUX
            insn_abc(OP_NOT, 1, 0, 0),
            insn_abc(OP_RETURN, 1, 1, 0),
        ];
        let constants = vec![Constant::String("player".to_string())];
        let proto = make_proto(code, constants);
        let hints = analyze_register_usage(&proto, &[], None, None);
        assert!(has_named_hint(&hints, 1, "player"),
            "NOT passthrough should propagate Named('player') from R0 to R1");
    }

    #[test]
    fn b096_minus_passthrough_propagates_hint() {
        let code = vec![
            insn_abc(OP_GETGLOBAL, 0, 0, 0),
            0u32,
            insn_abc(OP_MINUS, 1, 0, 0),
            insn_abc(OP_RETURN, 1, 1, 0),
        ];
        let constants = vec![Constant::String("offset".to_string())];
        let proto = make_proto(code, constants);
        let hints = analyze_register_usage(&proto, &[], None, None);
        assert!(has_named_hint(&hints, 1, "offset"),
            "MINUS passthrough should propagate Named('offset') from R0 to R1");
    }

    #[test]
    fn b096_length_passthrough_propagates_hint() {
        let code = vec![
            insn_abc(OP_GETGLOBAL, 0, 0, 0),
            0u32,
            insn_abc(OP_LENGTH, 1, 0, 0),
            insn_abc(OP_RETURN, 1, 1, 0),
        ];
        let constants = vec![Constant::String("items".to_string())];
        let proto = make_proto(code, constants);
        let hints = analyze_register_usage(&proto, &[], None, None);
        assert!(has_named_hint(&hints, 1, "items"),
            "LENGTH passthrough should propagate Named('items') from R0 to R1");
    }

    #[test]
    fn b096_bnot_passthrough_propagates_hint() {
        let code = vec![
            insn_abc(OP_GETGLOBAL, 0, 0, 0),
            0u32,
            insn_abc(OP_BNOT, 1, 0, 0),
            insn_abc(OP_RETURN, 1, 1, 0),
        ];
        let constants = vec![Constant::String("flags".to_string())];
        let proto = make_proto(code, constants);
        let hints = analyze_register_usage(&proto, &[], None, None);
        assert!(has_named_hint(&hints, 1, "flags"),
            "BNOT passthrough should propagate Named('flags') from R0 to R1");
    }

    #[test]
    fn b096_shl_passthrough_propagates_hint() {
        let code = vec![
            insn_abc(OP_GETGLOBAL, 0, 0, 0),
            0u32,
            insn_abc(OP_SHL, 1, 0, 2),
            insn_abc(OP_RETURN, 1, 1, 0),
        ];
        let constants = vec![Constant::String("mask".to_string())];
        let proto = make_proto(code, constants);
        let hints = analyze_register_usage(&proto, &[], None, None);
        assert!(has_named_hint(&hints, 1, "mask"),
            "SHL passthrough should propagate Named('mask') from R0 to R1");
    }

    #[test]
    fn b096_shr_passthrough_propagates_hint() {
        let code = vec![
            insn_abc(OP_GETGLOBAL, 0, 0, 0),
            0u32,
            insn_abc(OP_SHR, 1, 0, 2),
            insn_abc(OP_RETURN, 1, 1, 0),
        ];
        let constants = vec![Constant::String("value".to_string())];
        let proto = make_proto(code, constants);
        let hints = analyze_register_usage(&proto, &[], None, None);
        assert!(has_named_hint(&hints, 1, "value"),
            "SHR passthrough should propagate Named('value') from R0 to R1");
    }

    #[test]
    fn b096_no_hint_no_propagation() {
        // R0 has no hint (no GETGLOBAL/etc.), NOT R0 should NOT install a hint
        let code = vec![
            insn_abc(OP_NOT, 1, 0, 0),
            insn_abc(OP_RETURN, 1, 1, 0),
        ];
        let proto = make_proto(code, vec![]);
        let hints = analyze_register_usage(&proto, &[], None, None);
        // R1 should have no Named hints
        let has_named = hints.get(&1).map_or(false, |hs| {
            hs.iter().any(|(_, h)| matches!(h, RegisterHint::Named(_)))
        });
        assert!(!has_named, "NOT on unhinted source should not create a Named hint");
    }

    // Phase B0.100: And/Or hint propagation tests
    const OP_AND: u8 = 45;
    const OP_OR: u8 = 46;

    #[test]
    fn b100_or_propagates_hint_from_b() {
        // R0 = GETGLOBAL "config"; R1 = OR R0, R2 (R1 = R0 if truthy else R2)
        // Should propagate Named("config") from R0 to R1.
        let code = vec![
            insn_abc(OP_GETGLOBAL, 0, 0, 0),
            0u32, // AUX
            insn_abc(OP_OR, 1, 0, 2),
            insn_abc(OP_RETURN, 1, 1, 0),
        ];
        let constants = vec![Constant::String("config".to_string())];
        let proto = make_proto(code, constants);
        let hints = analyze_register_usage(&proto, &[], None, None);
        assert!(has_named_hint(&hints, 1, "config"),
            "OR should propagate Named('config') from R0 to R1");
    }

    #[test]
    fn b100_and_propagates_hint_from_b() {
        // R0 = GETGLOBAL "player"; R1 = AND R0, R2
        let code = vec![
            insn_abc(OP_GETGLOBAL, 0, 0, 0),
            0u32,
            insn_abc(OP_AND, 1, 0, 2),
            insn_abc(OP_RETURN, 1, 1, 0),
        ];
        let constants = vec![Constant::String("player".to_string())];
        let proto = make_proto(code, constants);
        let hints = analyze_register_usage(&proto, &[], None, None);
        assert!(has_named_hint(&hints, 1, "player"),
            "AND should propagate Named('player') from R0 to R1");
    }
}

/// Phase B0.51C — stable-identity hint memoization tests.
///
/// Regression coverage for the `arg{N1}{N2}` parameter-shadow naming bug in
/// `ModuleScript.lua`.  The synthesized name for a register with a stable
/// hint (Param, SelfParam, NumericForVar, GenericForKey, GenericForVal)
/// MUST be the same on every read of that register, regardless of pc.
///
/// Pre-fix symptom:
///   `reverse_k_arith = function(v0)` body emitted
///   `return 100 - arg12 + 1000 / arg12` where the param was actually `arg1`.
///   Root cause: `reg_name(proto, 0, pc)` missed the per-(reg,pc) cache at
///   every distinct pc, calling `synthesize_name` → `gen_scoped_name("arg1")`
///   which bumped `prefix_counts["arg1"]` each time (1 → 2 → 3 → …), so a
///   single-param register produced `arg1`, `arg12`, `arg13`, ….
#[cfg(test)]
mod b51c_stable_hint_naming_tests {
    use super::{DecompileContext, RegisterHint};
    use crate::parser::types::{Chunk, Proto};
    use std::collections::HashMap;

    fn make_ctx(chunk: &Chunk) -> DecompileContext<'_> {
        DecompileContext::new(chunk)
    }

    fn empty_chunk() -> Chunk {
        Chunk {
            version: 6,
            types_version: 0,
            strings: Vec::new(),
            protos: Vec::new(),
            main_proto: 0,
        }
    }

    fn fresh_proto(num_params: u8) -> Proto {
        Proto {
            max_stack_size: 16,
            num_params,
            num_upvalues: 0,
            is_vararg: false,
            flags: 0,
            typeinfo: None,
            code: Vec::new(),
            constants: Vec::new(),
            child_protos: Vec::new(),
            line_defined: 1,
            debug_name: None,
            line_info: None,
            debug_info: None,
        }
    }

    fn install_hints(
        ctx: &mut DecompileContext,
        proto_idx: usize,
        reg_to_hints: &[(u8, Vec<(usize, RegisterHint)>)],
    ) {
        let mut hints: HashMap<u8, Vec<(usize, RegisterHint)>> = HashMap::new();
        for (reg, hs) in reg_to_hints {
            hints.insert(*reg, hs.clone());
        }
        ctx.init_proto_naming(proto_idx, hints);
        ctx.current_proto_index = Some(proto_idx);
    }

    #[test]
    fn b51c_single_param_returns_arg1_at_every_pc() {
        // Phase B0.51C: a one-param function must render the param as "arg1"
        // at every read, not arg1/arg12/arg13/... as the per-prefix counter
        // would produce.  Matches `reverse_k_arith = function(v0)` in the
        // ModuleScript.lua corpus.
        let chunk = empty_chunk();
        let mut ctx = make_ctx(&chunk);
        let proto = fresh_proto(1);
        install_hints(
            &mut ctx,
            0,
            &[(0u8, vec![(0, RegisterHint::Param(0))])],
        );
        let first = ctx.reg_name(&proto, 0, 0);
        let second = ctx.reg_name(&proto, 0, 5);
        let third = ctx.reg_name(&proto, 0, 10);
        let fourth = ctx.reg_name(&proto, 0, 25);
        assert_eq!(first, "arg1");
        assert_eq!(
            second, "arg1",
            "pc=5 must reuse the arg1 name, not bump to arg12 (got {:?})",
            second
        );
        assert_eq!(
            third, "arg1",
            "pc=10 must reuse the arg1 name (got {:?})",
            third
        );
        assert_eq!(
            fourth, "arg1",
            "pc=25 must reuse the arg1 name (got {:?})",
            fourth
        );
    }

    #[test]
    fn b51c_two_param_returns_arg1_and_arg2_not_arg12_or_arg22() {
        // Phase B0.51C: both params must render stably.  Matches
        // `comparisons = function(v0, service8)` where the body referenced
        // `arg12 == arg22` instead of `arg1 == arg2`.
        let chunk = empty_chunk();
        let mut ctx = make_ctx(&chunk);
        let proto = fresh_proto(2);
        install_hints(
            &mut ctx,
            0,
            &[
                (0u8, vec![(0, RegisterHint::Param(0))]),
                (1u8, vec![(0, RegisterHint::Param(1))]),
            ],
        );
        let a1_first = ctx.reg_name(&proto, 0, 0);
        let a2_first = ctx.reg_name(&proto, 1, 0);
        let a1_body = ctx.reg_name(&proto, 0, 7);
        let a2_body = ctx.reg_name(&proto, 1, 11);
        let a1_later = ctx.reg_name(&proto, 0, 42);
        assert_eq!(a1_first, "arg1");
        assert_eq!(a2_first, "arg2");
        assert_eq!(
            a1_body, "arg1",
            "R0 at pc=7 must be arg1, got {:?}",
            a1_body
        );
        assert_eq!(
            a2_body, "arg2",
            "R1 at pc=11 must be arg2, got {:?}",
            a2_body
        );
        assert_eq!(a1_later, "arg1", "R0 at pc=42 must remain arg1, got {:?}", a1_later);
    }

    #[test]
    fn b51c_nested_protos_each_keep_their_own_arg1() {
        // Phase B0.51C: the outer proto and the inner proto each have a
        // Param(0) at R0.  Each proto's R0 reads should produce "arg1"
        // independently — the inner's arg1 must NOT bump to arg12 just
        // because the outer already used arg1 (Luau `local` allows scope
        // shadowing, and param names `arg1`/`arg2`/`self`/`i`/`k`/`v` are
        // the canonical representation regardless of outer scope).
        let chunk = empty_chunk();
        let mut ctx = make_ctx(&chunk);
        let outer = fresh_proto(1);
        let inner = fresh_proto(1);

        install_hints(
            &mut ctx,
            0,
            &[(0u8, vec![(0, RegisterHint::Param(0))])],
        );
        let outer_a1 = ctx.reg_name(&outer, 0, 0);
        let outer_a1_again = ctx.reg_name(&outer, 0, 8);
        assert_eq!(outer_a1, "arg1");
        assert_eq!(outer_a1_again, "arg1");

        // Switch to the nested proto — it gets its OWN naming scope.
        install_hints(
            &mut ctx,
            1,
            &[(0u8, vec![(0, RegisterHint::Param(0))])],
        );
        let inner_a1 = ctx.reg_name(&inner, 0, 0);
        let inner_a1_again = ctx.reg_name(&inner, 0, 4);
        assert_eq!(
            inner_a1, "arg1",
            "inner proto R0 must also be arg1 (not arg12 via used_names collision), got {:?}",
            inner_a1
        );
        assert_eq!(
            inner_a1_again, "arg1",
            "inner proto R0 at a later pc must still be arg1, got {:?}",
            inner_a1_again
        );

        // Returning to the outer proto must still return arg1 for its R0.
        ctx.current_proto_index = Some(0);
        let outer_later = ctx.reg_name(&outer, 0, 20);
        assert_eq!(outer_later, "arg1", "outer R0 after inner visit must still be arg1");
    }

    #[test]
    fn b51c_high_proto_index_with_multi_param_no_concatenation() {
        // Phase B0.51C: a proto at index 12 with 2 params must NOT produce
        // `arg122`, `arg212`, etc.  The proto index MUST NOT leak into the
        // synthesized parameter name, only the param index (0-based) + 1.
        let chunk = empty_chunk();
        let mut ctx = make_ctx(&chunk);
        let proto = fresh_proto(2);
        install_hints(
            &mut ctx,
            12,
            &[
                (0u8, vec![(0, RegisterHint::Param(0))]),
                (1u8, vec![(0, RegisterHint::Param(1))]),
            ],
        );
        let a1 = ctx.reg_name(&proto, 0, 3);
        let a2 = ctx.reg_name(&proto, 1, 7);
        let a1_again = ctx.reg_name(&proto, 0, 19);
        let a2_again = ctx.reg_name(&proto, 1, 23);
        assert_eq!(a1, "arg1", "proto_index 12 must not concatenate into arg122");
        assert_eq!(a2, "arg2", "proto_index 12 must not concatenate into arg212");
        assert_eq!(
            a1_again, "arg1",
            "repeat read at proto_index 12 must still be arg1, got {:?}",
            a1_again
        );
        assert_eq!(
            a2_again, "arg2",
            "repeat read at proto_index 12 must still be arg2, got {:?}",
            a2_again
        );
    }

    #[test]
    fn b51c_self_param_stable_across_pcs() {
        // Phase B0.51C: SelfParam hint must resolve to "self" at every pc,
        // not self/self2/self3.
        let chunk = empty_chunk();
        let mut ctx = make_ctx(&chunk);
        let proto = fresh_proto(1);
        install_hints(
            &mut ctx,
            0,
            &[(0u8, vec![(0, RegisterHint::SelfParam)])],
        );
        let s0 = ctx.reg_name(&proto, 0, 0);
        let s1 = ctx.reg_name(&proto, 0, 9);
        let s2 = ctx.reg_name(&proto, 0, 17);
        assert_eq!(s0, "self");
        assert_eq!(s1, "self", "SelfParam at pc=9 must stay `self`, got {:?}", s1);
        assert_eq!(s2, "self", "SelfParam at pc=17 must stay `self`, got {:?}", s2);
    }

    #[test]
    fn b51c_numeric_for_var_stable_across_body() {
        // Phase B0.51C: NumericForVar hint (loop variable) must stay `i`
        // across every body read — a 20-iteration loop should not produce
        // i/i2/i3/.../i20 at its reads.
        let chunk = empty_chunk();
        let mut ctx = make_ctx(&chunk);
        let proto = fresh_proto(0);
        install_hints(
            &mut ctx,
            0,
            &[(0u8, vec![(1, RegisterHint::NumericForVar)])],
        );
        let mut seen = Vec::new();
        for pc in 1..20usize {
            seen.push(ctx.reg_name(&proto, 0, pc));
        }
        // Every read should be "i" (SelfParam pattern — one stable name).
        for (idx, name) in seen.iter().enumerate() {
            assert_eq!(
                name, "i",
                "NumericForVar read #{} at pc={} must be `i`, got {:?}",
                idx,
                idx + 1,
                name
            );
        }
    }

    #[test]
    fn b51c_generic_for_key_and_val_stable() {
        // Phase B0.51C: GenericForKey + GenericForVal should stabilize to
        // `k` and `v` respectively across all body reads.
        let chunk = empty_chunk();
        let mut ctx = make_ctx(&chunk);
        let proto = fresh_proto(0);
        install_hints(
            &mut ctx,
            0,
            &[
                (0u8, vec![(2, RegisterHint::GenericForKey)]),
                (1u8, vec![(2, RegisterHint::GenericForVal)]),
            ],
        );
        assert_eq!(ctx.reg_name(&proto, 0, 3), "k");
        assert_eq!(ctx.reg_name(&proto, 1, 3), "v");
        assert_eq!(ctx.reg_name(&proto, 0, 9), "k", "k must persist at pc=9");
        assert_eq!(ctx.reg_name(&proto, 1, 9), "v", "v must persist at pc=9");
        assert_eq!(ctx.reg_name(&proto, 0, 18), "k", "k must persist at pc=18");
        assert_eq!(ctx.reg_name(&proto, 1, 18), "v", "v must persist at pc=18");
    }

    #[test]
    fn b51c_reader_sees_arg2_never_arg22_for_second_param() {
        // Phase B0.51C: direct regression on the `arg22` corpus symptom.  A
        // two-param function read many times at its second register (R1)
        // must yield "arg2" each time, never "arg22".
        let chunk = empty_chunk();
        let mut ctx = make_ctx(&chunk);
        let proto = fresh_proto(2);
        install_hints(
            &mut ctx,
            0,
            &[
                (0u8, vec![(0, RegisterHint::Param(0))]),
                (1u8, vec![(0, RegisterHint::Param(1))]),
            ],
        );
        for pc in (0..40usize).step_by(3) {
            let n = ctx.reg_name(&proto, 1, pc);
            assert_eq!(
                n, "arg2",
                "R1 at pc={} must be `arg2`, got {:?} (regression: arg22/arg23/…)",
                pc, n
            );
        }
    }

    #[test]
    fn b51c_non_stable_hints_still_use_counter_for_disambiguation() {
        // Phase B0.51C non-regression: Named/CallResult/Import hints are
        // intentionally NOT stable-keyed (different writes to the same
        // register could legitimately produce different names).  Two calls
        // with the same explicit Named hint must still go through
        // `gen_scoped_name` / `unique_name` and produce `foo`, `foo2` style
        // suffixes when the prefix collides.  This guards against
        // over-stabilization.
        let chunk = empty_chunk();
        let mut ctx = make_ctx(&chunk);
        let proto = fresh_proto(0);
        install_hints(
            &mut ctx,
            0,
            &[
                (
                    0u8,
                    vec![(0, RegisterHint::Named("foo".to_string()))],
                ),
                (
                    1u8,
                    vec![(0, RegisterHint::Named("foo".to_string()))],
                ),
            ],
        );
        let n0 = ctx.reg_name(&proto, 0, 0);
        let n1 = ctx.reg_name(&proto, 1, 0);
        assert_eq!(n0, "foo");
        // Named-hint collision between two DIFFERENT registers must still
        // disambiguate (otherwise we'd emit two `local foo = ...` for
        // different variables, which would compile but be confusing).
        assert_eq!(
            n1, "foo2",
            "Named hint collision must still disambiguate with a counter, got {:?}",
            n1
        );
    }

    #[test]
    fn b51c_param_and_selfparam_for_different_registers_do_not_collide() {
        // Phase B0.51C: SelfParam at R0 and Param(1) at R1 live in different
        // keys — both should memoize independently.  The names `self` and
        // `arg2` should both be stable across pcs.
        let chunk = empty_chunk();
        let mut ctx = make_ctx(&chunk);
        let proto = fresh_proto(2);
        install_hints(
            &mut ctx,
            0,
            &[
                (0u8, vec![(0, RegisterHint::SelfParam)]),
                (1u8, vec![(0, RegisterHint::Param(1))]),
            ],
        );
        assert_eq!(ctx.reg_name(&proto, 0, 1), "self");
        assert_eq!(ctx.reg_name(&proto, 1, 1), "arg2");
        assert_eq!(ctx.reg_name(&proto, 0, 15), "self");
        assert_eq!(ctx.reg_name(&proto, 1, 22), "arg2");
        assert_eq!(ctx.reg_name(&proto, 0, 99), "self");
        assert_eq!(ctx.reg_name(&proto, 1, 99), "arg2");
    }
}

/// Phase B0.68 — CONCAT operand-type guard regression tests.
///
/// These lock in the `mk_concat` helper (in `lifter/mod.rs`) that rejects
/// operand types the Luau `..` operator can't consume at runtime. The
/// 746-script production corpus was emitting garbage concatenations like
/// `((v1 .. false) .. v3) .. false` and `((v3 .. false) .. v3) .. false`,
/// all caused by either a misidentified CONCAT opcode or CONCAT chains
/// pulling stale register state that contained `Bool`/`Nil`/stdlib-shadow
/// Names. The guard mirrors B0.58's arithmetic guard on `mk_binop` and
/// B0.43's string-rejection guard that preceded it.
///
/// When a guard fires, the salvage prefers the valid side — if the right
/// operand is `Bool(false)` but the left is `String("x")`, `mk_concat`
/// returns `Expr::String("x")` (no BinOp emitted). When both sides are
/// invalid, we fall through to the left operand; that path is not
/// asserted here because production reg-state makes double-invalid
/// vanishingly rare, but it's covered by the `mk_concat` fallthrough.
#[cfg(test)]
mod b068_tests {
    use super::lifter::mk_concat;
    use crate::ast::{BinOp, Expr};

    // 1. `Expr::String("x") .. Expr::Bool(false)` → salvages to the
    //    String side; no BinOp with a Bool operand should exist.
    #[test]
    fn b068_string_concat_bool_salvages() {
        let result = mk_concat(
            Expr::String("x".into()),
            Expr::Bool(false),
        );
        match result {
            Expr::String(s) => assert_eq!(s, "x"),
            other => panic!("expected Expr::String salvage, got {:?}", other),
        }
    }

    // 2. `Expr::String("x") .. Expr::Nil` → salvages to the String side.
    #[test]
    fn b068_string_concat_nil_salvages() {
        let result = mk_concat(
            Expr::String("x".into()),
            Expr::Nil,
        );
        match result {
            Expr::String(s) => assert_eq!(s, "x"),
            other => panic!("expected Expr::String salvage, got {:?}", other),
        }
    }

    // 3. `Expr::String("x") .. Expr::Name("workspace")` → salvages via
    //    the stdlib-shadow branch. Concatenating with a stdlib global
    //    (`workspace`, `script`, `game`, `Players`, etc.) is always a bug
    //    from a misidentified opcode — the guard must catch it even
    //    though `Name(_)` is generally a valid concat operand.
    #[test]
    fn b068_string_concat_stdlib_shadow_name_salvages() {
        let result = mk_concat(
            Expr::String("x".into()),
            Expr::Name("workspace".into()),
        );
        match result {
            Expr::String(s) => assert_eq!(s, "x"),
            other => panic!("expected Expr::String salvage, got {:?}", other),
        }
    }

    // 4. `Expr::String("x") .. Expr::String("y")` — NEGATIVE CASE.
    //    Both sides are valid concat operands; we must produce a real
    //    BinOp::Concat and NOT collapse. This is the most important
    //    non-regression: the guard must not break legitimate concat.
    #[test]
    fn b068_valid_string_string_concat_emits_binop() {
        let result = mk_concat(
            Expr::String("x".into()),
            Expr::String("y".into()),
        );
        match result {
            Expr::BinOp { left, op: BinOp::Concat, right } => {
                assert!(matches!(*left, Expr::String(ref s) if s == "x"));
                assert!(matches!(*right, Expr::String(ref s) if s == "y"));
            }
            other => panic!("expected BinOp::Concat, got {:?}", other),
        }
    }

    // 5. `Expr::Name("msg") .. Expr::Name("user")` — both non-stdlib
    //    names are allowed through. Without this negative test the guard
    //    could overreach and kill every `v1 .. v2` in the corpus.
    #[test]
    fn b068_non_stdlib_name_concat_emits_binop() {
        let result = mk_concat(
            Expr::Name("msg".into()),
            Expr::Name("user".into()),
        );
        match result {
            Expr::BinOp { left, op: BinOp::Concat, right } => {
                assert!(matches!(*left, Expr::Name(ref n) if n == "msg"));
                assert!(matches!(*right, Expr::Name(ref n) if n == "user"));
            }
            other => panic!("expected BinOp::Concat, got {:?}", other),
        }
    }

    // 6. `Expr::Number(5.0) .. Expr::String("x")` — number `..` string
    //    is valid Luau (numbers auto-coerce to strings in concat).
    #[test]
    fn b068_number_string_concat_emits_binop() {
        let result = mk_concat(
            Expr::Number(5.0),
            Expr::String("x".into()),
        );
        match result {
            Expr::BinOp { left, op: BinOp::Concat, right } => {
                assert!(matches!(*left, Expr::Number(n) if n == 5.0));
                assert!(matches!(*right, Expr::String(ref s) if s == "x"));
            }
            other => panic!("expected BinOp::Concat, got {:?}", other),
        }
    }
}

/// Phase B0.66 — guard the `Named` hint path against stdlib-shadow names.
///
/// B0.38 installed a stdlib-shadow blacklist (`is_stdlib_shadow_name`) used
/// from three places:
///   * `name_from_call_result` — rejects using stdlib function names as
///     result var names
///   * `name_from_import`      — rejects using the last Import segment if
///                               it is a stdlib identifier
///   * the CALL destination-naming installer in `analyze_register_usage` —
///     rejects stdlib-shadow Import last segments
///
/// Gap: when a callee register carries a `RegisterHint::Named(name)` from
/// GETTABLEKS / GETGLOBAL / GETUPVAL and that name happens to also be a
/// stdlib identifier (workspace / game / script / string / math / table /
/// task / …), the Named value reached `synthesize_name` directly and
/// propagated into the CALL result naming path — producing output like
///   `local workspace = require(v0.Client.Atom)`
/// which shadows the real `workspace` global for the rest of the scope.
///
/// Fix: add the `is_stdlib_shadow_name(name)` guard at the top of the
/// `Some(RegisterHint::Named(ref name))` arm in `synthesize_name`.  When
/// the Named hint would shadow a stdlib identifier, fall back to a neutral
/// `"value"` prefix.
///
/// Corpus symptom: 55 shadow cases across 746 scripts in the production
/// corpus (workspace / game / script / string / math / table / task / …).
#[cfg(test)]
mod b066_tests {
    use super::{DecompileContext, RegisterHint};
    use crate::parser::types::{Chunk, Proto};
    use std::collections::HashMap;

    fn make_ctx(chunk: &Chunk) -> DecompileContext<'_> {
        DecompileContext::new(chunk)
    }

    fn empty_chunk() -> Chunk {
        Chunk {
            version: 6,
            types_version: 0,
            strings: Vec::new(),
            protos: Vec::new(),
            main_proto: 0,
        }
    }

    fn fresh_proto(num_params: u8) -> Proto {
        Proto {
            max_stack_size: 16,
            num_params,
            num_upvalues: 0,
            is_vararg: false,
            flags: 0,
            typeinfo: None,
            code: Vec::new(),
            constants: Vec::new(),
            child_protos: Vec::new(),
            line_defined: 1,
            debug_name: None,
            line_info: None,
            debug_info: None,
        }
    }

    fn install_hints(
        ctx: &mut DecompileContext,
        proto_idx: usize,
        reg_to_hints: &[(u8, Vec<(usize, RegisterHint)>)],
    ) {
        let mut hints: HashMap<u8, Vec<(usize, RegisterHint)>> = HashMap::new();
        for (reg, hs) in reg_to_hints {
            hints.insert(*reg, hs.clone());
        }
        ctx.init_proto_naming(proto_idx, hints);
        ctx.current_proto_index = Some(proto_idx);
    }

    #[test]
    fn b066_named_workspace_shadow_falls_back_to_value() {
        // Direct regression on the primary corpus symptom: a register with
        // `Named("workspace")` installed (e.g. via GETGLOBAL "workspace" or
        // a GETTABLEKS chain resolving to a field named "workspace") must
        // NOT be rendered as `local workspace = ...` — that shadows the
        // real `workspace` global.  The guard should fall back to "value".
        let chunk = empty_chunk();
        let mut ctx = make_ctx(&chunk);
        let proto = fresh_proto(0);
        install_hints(
            &mut ctx,
            0,
            &[(0u8, vec![(0, RegisterHint::Named("workspace".to_string()))])],
        );
        let n = ctx.reg_name(&proto, 0, 5);
        assert_ne!(
            n, "workspace",
            "Named(workspace) must not render as `workspace` (stdlib shadow), got {:?}",
            n
        );
        assert!(
            n.starts_with("value"),
            "Named(workspace) must fall back to `value` prefix, got {:?}",
            n
        );
    }

    #[test]
    fn b066_named_user_settings_preserved_negative_case() {
        // Negative case (B0.37 preservation): a Named hint that is NOT a
        // stdlib shadow MUST still render with the given name.  This was the
        // original B0.37 win — `local UserSettings = UserSettings` is
        // valid and readable.  The B0.66 guard must not regress it.
        let chunk = empty_chunk();
        let mut ctx = make_ctx(&chunk);
        let proto = fresh_proto(0);
        install_hints(
            &mut ctx,
            0,
            &[(0u8, vec![(0, RegisterHint::Named("UserSettings".to_string()))])],
        );
        let n = ctx.reg_name(&proto, 0, 3);
        assert_eq!(
            n, "UserSettings",
            "Named(UserSettings) is not a stdlib shadow — must render as `UserSettings`, got {:?}",
            n
        );
    }

    #[test]
    fn b066_named_game_shadow_rejected() {
        // `local game = ...` shadows the root `game` global used by every
        // Roblox script.  Must fall back to "value".
        let chunk = empty_chunk();
        let mut ctx = make_ctx(&chunk);
        let proto = fresh_proto(0);
        install_hints(
            &mut ctx,
            0,
            &[(0u8, vec![(0, RegisterHint::Named("game".to_string()))])],
        );
        let n = ctx.reg_name(&proto, 0, 4);
        assert_ne!(n, "game", "Named(game) must not render as `game` (stdlib shadow), got {:?}", n);
        assert!(n.starts_with("value"), "expected `value` fallback, got {:?}", n);
    }

    #[test]
    fn b066_named_script_shadow_rejected() {
        // `local script = ...` shadows the per-script `script` global.
        let chunk = empty_chunk();
        let mut ctx = make_ctx(&chunk);
        let proto = fresh_proto(0);
        install_hints(
            &mut ctx,
            0,
            &[(0u8, vec![(0, RegisterHint::Named("script".to_string()))])],
        );
        let n = ctx.reg_name(&proto, 0, 6);
        assert_ne!(n, "script", "Named(script) must not render as `script` (stdlib shadow), got {:?}", n);
        assert!(n.starts_with("value"), "expected `value` fallback, got {:?}", n);
    }

    #[test]
    fn b066_named_require_shadow_rejected() {
        // Named("require") from a GETTABLEKS field chain pointing at a
        // field called "require" must not emit `local require = ...`.
        let chunk = empty_chunk();
        let mut ctx = make_ctx(&chunk);
        let proto = fresh_proto(0);
        install_hints(
            &mut ctx,
            0,
            &[(0u8, vec![(0, RegisterHint::Named("require".to_string()))])],
        );
        let n = ctx.reg_name(&proto, 0, 7);
        assert_ne!(n, "require", "Named(require) must not render as `require`, got {:?}", n);
        assert!(n.starts_with("value"), "expected `value` fallback, got {:?}", n);
    }

    #[test]
    fn b066_named_table_shadow_rejected() {
        // `local table = ...` shadows the stdlib `table` module.
        let chunk = empty_chunk();
        let mut ctx = make_ctx(&chunk);
        let proto = fresh_proto(0);
        install_hints(
            &mut ctx,
            0,
            &[(0u8, vec![(0, RegisterHint::Named("table".to_string()))])],
        );
        let n = ctx.reg_name(&proto, 0, 2);
        assert_ne!(n, "table", "Named(table) must not render as `table`, got {:?}", n);
        assert!(n.starts_with("value"), "expected `value` fallback, got {:?}", n);
    }

    #[test]
    fn b066_named_task_shadow_rejected() {
        // `local task = ...` shadows the Luau `task` library.
        let chunk = empty_chunk();
        let mut ctx = make_ctx(&chunk);
        let proto = fresh_proto(0);
        install_hints(
            &mut ctx,
            0,
            &[(0u8, vec![(0, RegisterHint::Named("task".to_string()))])],
        );
        let n = ctx.reg_name(&proto, 0, 1);
        assert_ne!(n, "task", "Named(task) must not render as `task`, got {:?}", n);
        assert!(n.starts_with("value"), "expected `value` fallback, got {:?}", n);
    }

    #[test]
    fn b066_named_test_utils_preserved_negative_case() {
        // Another non-shadow name — module identifiers like "TestUtils"
        // must still propagate.  Regression lock on the B0.37 win.
        let chunk = empty_chunk();
        let mut ctx = make_ctx(&chunk);
        let proto = fresh_proto(0);
        install_hints(
            &mut ctx,
            0,
            &[(0u8, vec![(0, RegisterHint::Named("TestUtils".to_string()))])],
        );
        let n = ctx.reg_name(&proto, 0, 8);
        assert_eq!(
            n, "TestUtils",
            "Named(TestUtils) is not a stdlib shadow — must render as `TestUtils`, got {:?}",
            n
        );
    }

    #[test]
    fn b066_named_my_var_preserved_negative_case() {
        // Lowercase non-stdlib identifiers must also pass through unchanged.
        let chunk = empty_chunk();
        let mut ctx = make_ctx(&chunk);
        let proto = fresh_proto(0);
        install_hints(
            &mut ctx,
            0,
            &[(0u8, vec![(0, RegisterHint::Named("my_var".to_string()))])],
        );
        let n = ctx.reg_name(&proto, 0, 3);
        assert_eq!(
            n, "my_var",
            "Named(my_var) is not a stdlib shadow — must render as `my_var`, got {:?}",
            n
        );
    }
}

/// Phase B0.65 — `store_complex` fresh-hint peek regression tests.
///
/// Fixes the "register-reuse naming bug" where GETGLOBAL + GETTABLEKS on the
/// same register emitted `local script = script.Parent` instead of
/// `local Parent = script.Parent`.  The blind-test corpus had 559 such bugs
/// across 746 scripts.
///
/// Root cause: `store_complex` detected self-mutation (the new value
/// references the register's current Name) and blindly reused the carried
/// name, never consulting the fresh `Named` hint that GETTABLEKS installed
/// at its own PC.
///
/// Fix: peek at `ctx.reg_name(proto, reg, pc)` — when it yields a different
/// semantic name than the carried one, treat the write as a semantic rebind
/// (Shadow / FirstDecl via `classify_write`).  When the peek matches the
/// carried name (the benign `count = count + 1` case — B0.43C propagates
/// Named("count") through arithmetic), reuse as before.
#[cfg(test)]
mod b065_tests {
    use super::{decompile_proto, DecompileContext};
    use crate::parser::types::{Chunk, Constant, Proto};

    // Canonical (non-shuffled) Luau v6 opcode bytes.
    const OP_GETGLOBAL: u8  = 7;
    const OP_GETTABLEKS: u8 = 15;
    const OP_ADDK: u8       = 39;
    const OP_RETURN: u8     = 22;

    fn insn_ad(op: u8, a: u8, d: i16) -> u32 {
        let du = d as u16 as u32;
        (op as u32) | ((a as u32) << 8) | (du << 16)
    }

    fn insn_abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
        (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
    }

    fn make_proto(code: Vec<u32>, constants: Vec<Constant>, max_stack: u8) -> Proto {
        Proto {
            max_stack_size: max_stack,
            num_params: 0,
            num_upvalues: 0,
            is_vararg: false,
            flags: 0,
            typeinfo: None,
            code,
            constants,
            child_protos: Vec::new(),
            line_defined: 1,
            debug_name: Some("test".to_string()),
            line_info: None,
            debug_info: None,
        }
    }

    fn make_chunk(proto: Proto) -> Chunk {
        Chunk {
            version: 6,
            types_version: 0,
            strings: Vec::new(),
            protos: vec![proto],
            main_proto: 0,
        }
    }

    /// Primary regression test: GETGLOBAL + GETTABLEKS must emit
    /// `local Parent = script.Parent`, NOT `local script = script.Parent`.
    ///
    /// Bytecode pattern (matches the corpus smoking gun):
    ///   GETGLOBAL R0, "script"     — K0 = "script"
    ///   GETTABLEKS R0, R0, "Parent" — K1 = "Parent"
    ///   RETURN R0..R0
    ///
    /// Register layout: only R0 is used.  GETGLOBAL seeds R0 with the Name
    /// "script" (direct, not through `reg_name`), and the GETTABLEKS self-
    /// mutation triggers `store_complex`.  With the fresh-hint peek, the
    /// Named("Parent") hint installed at the GETTABLEKS PC wins over the
    /// carried "script" name.
    #[test]
    fn b065_getglobal_then_gettableks_renames_to_field() {
        let code = vec![
            insn_ad(OP_GETGLOBAL, 0, 0),        // 0: GETGLOBAL R0 = K0 ("script")
            0,                                   // 1: AUX for GETGLOBAL (unused)
            insn_abc(OP_GETTABLEKS, 0, 0, 0),   // 2: GETTABLEKS R0 = R0[K[AUX]]
            1,                                   // 3: AUX = 1 (points at K1 = "Parent")
            insn_abc(OP_RETURN, 0, 2, 0),       // 4: RETURN R0..R0
        ];
        let constants = vec![
            Constant::String("script".to_string()),
            Constant::String("Parent".to_string()),
        ];
        let chunk = make_chunk(make_proto(code, constants, 2));
        let mut ctx = DecompileContext::new(&chunk);
        let source = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

        // Must contain a local declaration referencing the field name.
        assert!(
            source.contains("Parent"),
            "B0.65: output must mention field name 'Parent', got:\n{}",
            source
        );

        // Must NOT re-emit `local script = script.Parent` — that is the
        // corpus-dominant bug this patch fixes.
        assert!(
            !source.contains("local script = script.Parent"),
            "B0.65: must NOT emit `local script = script.Parent`, got:\n{}",
            source
        );

        // The emitted declaration should be `local Parent = ...` (fresh
        // hint wins over the carried "script" name).
        assert!(
            source.contains("local Parent"),
            "B0.65: expected `local Parent = ...`, got:\n{}",
            source
        );
    }

    /// Benign self-mutation must not trigger the rebind.  Pattern:
    /// GETGLOBAL seeds R0 with "count"; ADDK R0, R0, 1 is arithmetic
    /// self-mutation whose B0.43C propagation keeps Named("count") current.
    ///
    /// Key assertion: the register should remain bound to "count" after
    /// the arithmetic, and no unrelated name like `value` or `result` should
    /// replace it.  (B0.43C's arithmetic hint propagator keeps Named("count")
    /// current at the ADDK PC, so the peek matches the carried name and
    /// the reuse path fires as before.)
    #[test]
    fn b065_arithmetic_self_mutation_keeps_carried_name() {
        // GETGLOBAL R0, "count" ; ADDK R0, R0, 1  (C is constant index)
        let code = vec![
            insn_ad(OP_GETGLOBAL, 0, 0),        // 0: GETGLOBAL R0 = K0 ("count")
            0,                                   // 1: AUX
            insn_abc(OP_ADDK, 0, 0, 1),         // 2: ADDK R0 = R0 + K[1]
            insn_abc(OP_RETURN, 0, 2, 0),       // 3: RETURN R0..R0
        ];
        let constants = vec![
            Constant::String("count".to_string()),
            Constant::Number(1.0),
        ];
        let chunk = make_chunk(make_proto(code, constants, 2));
        let mut ctx = DecompileContext::new(&chunk);
        let source = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

        // The output should still reference the carried name `count`
        // SOMEWHERE — we must not rename it to an unrelated identifier.
        assert!(
            source.contains("count"),
            "B0.65: benign self-mutation must keep carried name `count`, got:\n{}",
            source
        );

        // Must NOT emit an arithmetic-result fallback like `local value =`
        // or `local result =` — those would indicate the peek misfired when
        // it shouldn't have (arithmetic propagator B0.43C keeps Named(count)).
        assert!(
            !source.contains("local value ="),
            "B0.65: arithmetic self-mutation must not fall back to `value`, got:\n{}",
            source
        );
        assert!(
            !source.contains("local result ="),
            "B0.65: arithmetic self-mutation must not fall back to `result`, got:\n{}",
            source
        );
    }

    /// Multi-level GETTABLEKS chain: the innermost field wins as the final
    /// local's name.  Pattern:
    ///   GETGLOBAL  R0, "game"
    ///   GETTABLEKS R0, R0, "Players"     — first chain step, rebinds to Players
    ///   GETTABLEKS R0, R0, "LocalPlayer" — second step, rebinds again
    ///
    /// Each GETTABLEKS step materializes a local with the freshest field
    /// name.  Before B0.65 both steps would have reused the carried name
    /// "game" and emitted `local game = game.Players` / `local game =
    /// game.LocalPlayer`.  With the fresh-hint peek, each step picks up
    /// its own field name.  The terminal local (returned) must be named
    /// after the DEEPEST field, `LocalPlayer`.
    #[test]
    fn b065_multi_level_gettableks_chain_innermost_wins() {
        let code = vec![
            insn_ad(OP_GETGLOBAL, 0, 0),        // 0: GETGLOBAL R0 = K0 ("game")
            0,                                   // 1: AUX
            insn_abc(OP_GETTABLEKS, 0, 0, 0),   // 2: GETTABLEKS R0 = R0.Players
            1,                                   // 3: AUX = 1 → K1 = "Players"
            insn_abc(OP_GETTABLEKS, 0, 0, 0),   // 4: GETTABLEKS R0 = R0.LocalPlayer
            2,                                   // 5: AUX = 2 → K2 = "LocalPlayer"
            insn_abc(OP_RETURN, 0, 2, 0),       // 6: RETURN R0..R0
        ];
        let constants = vec![
            Constant::String("game".to_string()),
            Constant::String("Players".to_string()),
            Constant::String("LocalPlayer".to_string()),
        ];
        let chunk = make_chunk(make_proto(code, constants, 2));
        let mut ctx = DecompileContext::new(&chunk);
        let source = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

        // Final local must expose the deepest field name `LocalPlayer`.
        assert!(
            source.contains("local LocalPlayer"),
            "B0.65: multi-level chain must emit `local LocalPlayer = ...`, got:\n{}",
            source
        );

        // The returned value must be the `LocalPlayer` local — i.e., the
        // register's final binding, not the intermediate `Players`.
        assert!(
            source.contains("return LocalPlayer"),
            "B0.65: terminal local must be returned, got:\n{}",
            source
        );

        // Must NOT emit `local game = game.X` — that is the corpus-dominant
        // "register reuse" bug (GETGLOBAL carried name reused by GETTABLEKS
        // instead of the fresh field hint).
        assert!(
            !source.contains("local game = game."),
            "B0.65: must not emit `local game = game.X`, got:\n{}",
            source
        );
    }
}
#[cfg(test)]
mod b067_tests {
    //! Phase B0.67 — `mk_unop` operand-type guard regression tests.
    //!
    //! Mirrors the B0.58 `mk_binop` guard pattern: when a MINUS / NOT / LENGTH
    //! / BNOT opcode is misidentified in the shuffle and reads a register
    //! holding an Instance / Name / Field expression, the lifter was wrapping
    //! it as `UnOp::Negate` etc. The corpus output then contained nonsense
    //! like `ScreenGui.X.Parent = -ReplicatedStorage` — syntactically valid
    //! Luau, but a runtime error and a visible garbage marker.
    //!
    //! `mk_unop` now rejects operand types that the op cannot produce and
    //! salvages by returning `Expr::Name("v{reg}")`.
    //!
    //! Coverage (per task spec):
    //!   1. Negate on stdlib-shadow Name → salvages (no UnOp::Negate wrapping)
    //!   2. Negate on Number → produces UnOp::Negate (negative case)
    //!   3. Length on stdlib-shadow Name → salvages
    //!   4. BNot on String → salvages
    //!   5. Not on Nil → produces UnOp::Not (not-breakage test)
    use crate::ast::{Expr, UnOp};
    use crate::decompiler::lifter::{mk_unop, RegVal};
    use crate::decompiler::is_stdlib_shadow_name;

    /// Build a `regs` vec where register 0 holds `expr`.
    fn regs_with(expr: Expr) -> Vec<RegVal> {
        vec![RegVal::Expr(expr)]
    }

    #[test]
    fn b067_negate_stdlib_shadow_name_is_salvaged() {
        // `game` is in the stdlib-shadow set — mirrors the real-world bug
        // where `-ReplicatedStorage` leaked into output on misfires.
        assert!(is_stdlib_shadow_name("game"));
        let regs = regs_with(Expr::Name("game".into()));
        let out = mk_unop(&regs, 0, UnOp::Negate);
        assert!(
            !matches!(out, Expr::UnOp { op: UnOp::Negate, .. }),
            "Negate on stdlib-shadow Name must not wrap as UnOp::Negate, got {:?}",
            out
        );
        // Phase B0.99: salvage returns the operand itself (passthrough
        // preserves the source register's value, not a generic v{N}).
        assert!(matches!(&out, Expr::Name(n) if n == "game"),
            "expected salvage to Expr::Name(\"game\"), got {:?}", out);
    }

    #[test]
    fn b067_negate_on_number_produces_unop_negate() {
        // Negative case: real unary minus on a numeric literal must still work.
        // Without this, the guard would break all legitimate `-x` emissions.
        let regs = regs_with(Expr::Number(5.0));
        let out = mk_unop(&regs, 0, UnOp::Negate);
        match out {
            Expr::UnOp { op: UnOp::Negate, operand } => {
                assert!(matches!(*operand, Expr::Number(n) if n == 5.0),
                    "expected operand Number(5.0), got {:?}", operand);
            }
            other => panic!("expected UnOp::Negate wrapping Number(5.0), got {:?}", other),
        }
    }

    #[test]
    fn b067_length_on_stdlib_shadow_name_is_salvaged() {
        // `#game`, `#workspace`, `#script`, etc. are runtime errors — must
        // salvage rather than wrap.
        assert!(is_stdlib_shadow_name("game"));
        let regs = regs_with(Expr::Name("game".into()));
        let out = mk_unop(&regs, 0, UnOp::Length);
        assert!(
            !matches!(out, Expr::UnOp { op: UnOp::Length, .. }),
            "Length on stdlib-shadow Name must not wrap, got {:?}",
            out
        );
        // Phase B0.99: salvage returns operand itself
        assert!(matches!(&out, Expr::Name(n) if n == "game"),
            "expected salvage to Expr::Name(\"game\"), got {:?}", out);
    }

    #[test]
    fn b067_bnot_on_string_is_salvaged() {
        // Bitwise `~"hello"` is a runtime error — misidentified BNOT on a
        // string constant is a common corpus misfire.
        let regs = regs_with(Expr::String("hello".into()));
        let out = mk_unop(&regs, 0, UnOp::BNot);
        assert!(
            !matches!(out, Expr::UnOp { op: UnOp::BNot, .. }),
            "BNot on String must not wrap as UnOp::BNot, got {:?}",
            out
        );
        // Phase B0.99: salvage returns operand itself
        assert!(matches!(&out, Expr::String(s) if s == "hello"),
            "expected salvage to Expr::String(\"hello\"), got {:?}", out);
    }

    #[test]
    fn b067_not_on_nil_produces_unop_not() {
        // Negative / breakage-check: `not nil` is defined Luau (yields `true`).
        // Guard for `Not` must reject nothing — otherwise we'd break common
        // boolean-coercion patterns like `return not tbl[k]`.
        let regs = regs_with(Expr::Nil);
        let out = mk_unop(&regs, 0, UnOp::Not);
        match out {
            Expr::UnOp { op: UnOp::Not, operand } => {
                assert!(matches!(*operand, Expr::Nil),
                    "expected operand Nil, got {:?}", operand);
            }
            other => panic!("expected UnOp::Not wrapping Nil, got {:?}", other),
        }
    }
}
