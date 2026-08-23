//! Semantic checks on decompiled output.
//!
//! ── WHY THIS EXISTS ──────────────────────────────────────────────────────
//! Decompiler quality was being judged by counting marker strings — how many
//! files contained `upval_N`, `return nil`, and so on. That measure is close
//! to worthless, and on 2026-08-03 it produced a confidently wrong report:
//! three Roblox modules were scored "0 defects in 3 of 4 categories" by grep,
//! and then reading them showed
//!
//!   * CameraModule       — 32 protos in, `return {}` out. Whole body gone.
//!   * ClickToMoveController — `game[1] = v8`,
//!                          `Players.LocalPlayer = Enum.KeyCode.Down`,
//!                          and undefined `v9`..`v27` referenced throughout.
//!   * Events.lua         — every `tbl.X` function carried a DIFFERENT
//!                          function's body, provable from the error strings
//!                          baked into each one.
//!
//! None of those files contained a single marker string. Grep called them all
//! clean. The Events.lua case is the dangerous one: correct-looking names on
//! correct-looking bodies, wired to each other wrongly, so calling
//! `Events.Create(...)` actually runs `ServerCall`.
//!
//! So these checks assert PROPERTIES OF MEANING that must hold for any honest
//! decompilation, rather than looking for known-bad substrings. A marker count
//! can only find defects someone already thought to name; these find defects by
//! their consequences.
//!
//! ── WHAT A CHECK MUST BE ────────────────────────────────────────────────
//! Every check here must be *sound*: if it fires, the output really is wrong.
//! A check that produces false positives trains people to ignore the report,
//! which is worse than having no check. Where a property can only be tested
//! heuristically, it belongs in [`Severity::Suspicious`], not [`Severity::Wrong`].

use std::collections::{HashMap, HashSet};

/// How confident we are that a finding is a genuine defect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// The output is provably wrong — it would not run, or would run as a
    /// different program than the bytecode describes.
    Wrong,
    /// Strong indication of a defect, but a legitimate program could in
    /// principle look like this.
    Suspicious,
}

#[derive(Debug, Clone)]
pub struct Finding {
    pub severity: Severity,
    /// Short stable identifier, e.g. "name_body_mismatch".
    pub check: &'static str,
    /// 1-based line in the decompiled output, when known.
    pub line: Option<usize>,
    /// What is wrong, in terms of consequence.
    pub detail: String,
}

impl Finding {
    fn wrong(check: &'static str, line: Option<usize>, detail: String) -> Self {
        Finding { severity: Severity::Wrong, check, line, detail }
    }
    fn suspicious(check: &'static str, line: Option<usize>, detail: String) -> Self {
        Finding { severity: Severity::Suspicious, check, line, detail }
    }
}

/// Run every semantic check over decompiled source.
///
/// `proto_count` is the number of protos in the chunk, used by the
/// body-recovery check. Pass `None` if unknown.
/// Blank the CONTENTS of string literals and comments, preserving length and
/// line structure so offsets and line numbers still line up.
///
/// The identifier scans tokenise anything name-shaped, and decompiled Roblox
/// code is full of API version strings and URL paths -- `"v1"`,
/// `"modals-api/v1/prompts"`, `"%s/v2/universes/%d/configuration"`. Those match
/// the generated-name shape `vN` and were reported as undefined locals; the
/// literal `"v1"` alone occurs 173 times in the CorePackages output.
///
/// COMMENTS MUST BE SKIPPED, and getting that wrong is not hypothetical. A
/// first version handled only quotes, and this decompiler's own header ends
/// `...measured against the client'` -- an apostrophe, present in 1815 of 1880
/// CoreGui files. Treated as an opening quote it swallowed the rest of each
/// file, leaving no identifiers to check: CoreGui "improved" from 240 defects
/// to 5 while an independent scan still found 100 stranded locals in the very
/// same output. A checker that reports nothing is not a clean corpus.
///
/// Deliberately NOT applied to `check_name_body_agreement`, which reads string
/// contents on purpose -- a function naming itself in its own error message is
/// exactly the evidence that check depends on.
fn blank_noncode(source: &str) -> String {
    let b = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut i = 0;

    // Long-bracket opener at `i`: returns (level, first_byte_after_opener).
    let long_open = |i: usize| -> Option<(usize, usize)> {
        if b.get(i) != Some(&b'[') {
            return None;
        }
        let mut eq = 0;
        let mut j = i + 1;
        while b.get(j) == Some(&b'=') {
            eq += 1;
            j += 1;
        }
        if b.get(j) == Some(&b'[') { Some((eq, j + 1)) } else { None }
    };
    let blank_to = |out: &mut String, from: usize, to: usize| {
        for k in from..to {
            out.push(if b[k] == b'\n' { '\n' } else { ' ' });
        }
    };

    while i < b.len() {
        // Comment first: `--`, optionally a long bracket, else to end of line.
        if b[i] == b'-' && b.get(i + 1) == Some(&b'-') {
            out.push_str("--");
            let after = i + 2;
            if let Some((eq, body)) = long_open(after) {
                let close: String = format!("]{}]", "=".repeat(eq));
                out.push_str(&source[after..body]);
                let end = source[body..].find(&close).map(|p| body + p).unwrap_or(b.len());
                blank_to(&mut out, body, end);
                if end < b.len() {
                    out.push_str(&close);
                    i = end + close.len();
                } else {
                    i = end;
                }
            } else {
                let end = source[after..].find('\n').map(|p| after + p).unwrap_or(b.len());
                blank_to(&mut out, after, end);
                i = end;
            }
            continue;
        }
        // Long string.
        if let Some((eq, body)) = long_open(i) {
            let close: String = format!("]{}]", "=".repeat(eq));
            out.push_str(&source[i..body]);
            let end = source[body..].find(&close).map(|p| body + p).unwrap_or(b.len());
            blank_to(&mut out, body, end);
            if end < b.len() {
                out.push_str(&close);
                i = end + close.len();
            } else {
                i = end;
            }
            continue;
        }
        // Quoted string. Unterminated by end of LINE is treated as closed, so a
        // stray apostrophe cannot consume the rest of the file.
        if b[i] == b'"' || b[i] == b'\'' {
            let quote = b[i];
            out.push(quote as char);
            i += 1;
            while i < b.len() && b[i] != quote && b[i] != b'\n' {
                if b[i] == b'\\' && i + 1 < b.len() && b[i + 1] != b'\n' {
                    out.push_str("  ");
                    i += 2;
                    continue;
                }
                out.push(' ');
                i += 1;
            }
            if i < b.len() && b[i] == quote {
                out.push(quote as char);
                i += 1;
            }
            continue;
        }
        out.push(b[i] as char);
        i += 1;
    }
    out
}

pub fn check(source: &str, proto_count: Option<usize>) -> Vec<Finding> {
    let mut out = Vec::new();
    // Reads string contents on purpose — must see the ORIGINAL source.
    check_name_body_agreement(source, &mut out);
    check_call_recovery(source, &mut out);
    let code = blank_noncode(source);
    check_undefined_locals(&code, &mut out);
    check_declared_but_never_assigned(&code, &mut out);
    check_body_recovered(source, proto_count, &mut out);
    check_branch_local_discarded(&code, &mut out);
    check_discarded_table_writes(source, &mut out);
    check_property_called_as_method(source, &mut out);
    out.sort_by_key(|f| (f.severity, f.line.unwrap_or(0)));
    out
}


// ── Check: a value computed in a branch and thrown away ───────────────────
//
// The shape, from `213_Collectibles_Tornado`:
//
//     local Lvl3 = Lvl2 + GetStacksBySource2 / 3.2
//     if 0 < GetStacksBySource then
//         local Lvl4 = Lvl3 + 2        -- computed, DISCARDED
//     end
//     local Lvl5 = Lvl3 * Multiplier   -- uses Lvl3, never the +2
//
// The original reassigned `Lvl3`; the lifter gave the branch arm a FRESH local
// instead, so the `+ 2` is silently lost. Commit 26b3b9f names this family.
// `734_StatModifiers_BaseConversionRate` is the same defect on strings and
// returns the un-appended text.
//
// Every other check here was blind to it - a dropped value leaves no undefined
// name, no missing body and no bad call - which is why the semantic measure
// read 98.7% while the sweep, which does look for it, read far worse.
//
// SOUNDNESS. Three conditions together, and all three are needed:
//   1. the local is never read ANYWHERE else in the file - one occurrence in
//      total, its own declaration;
//   2. its initialiser EXTENDS a base identifier (`base ..`, `base +`,
//      `base *`, `base /`) rather than being a fresh value - an unused
//      `local Players = game:GetService("Players")` is ordinary dead code, not
//      a lost reassignment, and requiring the extension form is what separates
//      them (measured: 1,161 candidates without it, 162 with);
//   3. that base is still read AFTER this line, so the value the arm computed
//      demonstrably failed to reach the code that goes on to use the base.
//
// Strings and comments are already blanked by `blank_noncode`, so words inside
// a string literal cannot masquerade as identifiers here.
fn check_branch_local_discarded(code: &str, out: &mut Vec<Finding>) {
    let lines: Vec<&str> = code.lines().collect();
    let mut occ: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, raw) in lines.iter().enumerate() {
        let t = raw.trim();
        if t.starts_with("--") {
            continue;
        }
        for tok in bare_idents(t) {
            occ.entry(tok).or_default().push(i);
        }
    }

    let mut reported = 0usize;
    for (i, raw) in lines.iter().enumerate() {
        let t = raw.trim();
        let Some(rest) = t.strip_prefix("local ") else { continue };
        let Some(eq) = rest.find('=') else { continue };
        let bytes = rest.as_bytes();
        if rest[eq..].starts_with("==")
            || (eq > 0 && matches!(bytes[eq - 1], b'~' | b'<' | b'>' | b'='))
        {
            continue;
        }
        let name = rest[..eq].trim();
        if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
            continue;
        }
        // (1) declared here and mentioned nowhere else
        if occ.get(name).map_or(0, |v| v.len()) != 1 {
            continue;
        }
        let rhs = rest[eq + 1..].trim_start().trim_start_matches('(');
        let base_end = rhs
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(rhs.len());
        if base_end == 0 {
            continue;
        }
        let base = &rhs[..base_end];
        if !base.starts_with(|c: char| c.is_alphabetic() || c == '_') {
            continue;
        }
        // (2) the initialiser EXTENDS that base
        let after = rhs[base_end..].trim_start();
        let extends = after.starts_with("..")
            || after.starts_with('+')
            || after.starts_with('*')
            || after.starts_with('/');
        if !extends {
            continue;
        }
        // (3) the base outlives this line
        if !occ.get(base).is_some_and(|v| v.iter().any(|&j| j > i)) {
            continue;
        }
        // (4) the value is not simply RECOMPUTED somewhere else.
        //
        // The finding claims "a value the bytecode carried was dropped". If the
        // identical expression is evaluated again elsewhere in the file, that
        // premise is false - the value exists, the emitter merely materialised
        // a dead copy of it:
        //
        //     local v34 = arg1 + 0.1
        //     if arg2 < arg1 + 0.1 then        -- recomputed inline
        //
        // Six such findings were confirmed by hand before this rule was added
        // (885_Easing x3 with `arg4 / 2` already in the condition above it,
        // 21_ClickToMove, 381_Eggs, 919_pprint). It is deliberately EXACT-text:
        // a looser match would suppress real losses, and a dropped value that
        // happens to share its spelling with a live computation is a case worth
        // missing rather than a case worth inventing.
        let rhs_norm: String = rhs.split_whitespace().collect::<Vec<_>>().join(" ");
        let recomputed = !rhs_norm.is_empty()
            && lines.iter().enumerate().any(|(j, other)| {
                j != i && {
                    let o: String = other.split_whitespace().collect::<Vec<_>>().join(" ");
                    !o.trim_start().starts_with("--") && o.contains(&rhs_norm)
                }
            });
        if recomputed {
            continue;
        }
        reported += 1;
        if reported > 6 {
            continue;
        }
        out.push(Finding::wrong(
            "branch_local_discarded",
            Some(i + 1),
            format!(
                "`{}` extends `{}` and is never read, while `{}` is used again below                  - the arm's value was written to a fresh local instead of back to                  `{}`, so it is silently dropped",
                name, base, base, base
            ),
        ));
    }
    if reported > 6 {
        out.push(Finding::wrong(
            "branch_local_discarded",
            None,
            format!("... and {} further discarded branch values", reported - 6),
        ));
    }
}


// ── Check: a function that makes calls but emitted none ───────────────────
//
// A dropped function body leaves NO undefined name, NO bad call and NO
// discarded value, so every other check here scores it clean. Measured on the
// 1,143-file corpus before this check existed: 8 protos emit ZERO calls where
// the bytecode has three or more (`99_BeequipTypes::_CheckReqs` loses 20,
// `277_DragManager::OnInputBegin` loses 15) and 27 more emit under half.
// Twenty-nine files, reported by nothing.
//
// The lifter writes the comparison into the header because it is the only
// place that holds both the bytecode and the emitted AST. This check reads it
// back, the same way `check_body_recovered` consumes the proto count.
//
// It reads the ORIGINAL source, not the blanked copy, because the evidence is
// in a comment.
fn check_call_recovery(source: &str, out: &mut Vec<Finding>) {
    let Some(line) = source
        .lines()
        .take(20)
        .find(|l| l.trim_start().starts_with("-- call recovery:"))
    else {
        return;
    };
    let nums: Vec<usize> = line
        .split_whitespace()
        .filter_map(|w| w.parse::<usize>().ok())
        .collect();
    let (protos, calls) = match nums.as_slice() {
        [p, c, ..] => (*p, *c),
        _ => return,
    };
    if protos == 0 {
        return;
    }
    out.push(Finding::wrong(
        "calls_not_emitted",
        None,
        format!(
            "{} function(s) emitted {} fewer call(s) than their bytecode contains              - a body that makes calls and emits none does nothing at runtime,              and leaves no undefined name for any other check to catch",
            protos, calls
        ),
    ));
}

// ── Check 1: a function's name must match the name it uses for itself ──────
//
// This is the check that catches the Events.lua defect. Hand-written Luau
// overwhelmingly refers to itself in its own diagnostics:
//
//     function tbl.ServerCall(name, ...)
//         error("Events.ServerCall: No event named " .. name)
//     end
//
// If the declared name and the self-reference disagree, the lifter paired a
// closure with the wrong SETTABLEKS key. That is provably wrong output, and
// it is invisible to every marker-based check.
//
// Soundness: we only fire when the body names a DIFFERENT function that is
// ALSO declared in this same file. A body mentioning some unrelated string is
// ignored, so a function that legitimately logs another function's name only
// trips this if that other name is itself a sibling declaration.
fn check_name_body_agreement(source: &str, out: &mut Vec<Finding>) {
    let decls = collect_declarations(source);
    if decls.len() < 2 {
        return;
    }
    let declared: HashSet<&str> = decls.iter().map(|d| d.name.as_str()).collect();

    for d in &decls {
        // Names referenced inside string literals in this body.
        for (line, referenced) in string_referenced_names(source, d) {
            if referenced == d.name {
                continue; // agrees — nothing to report
            }
            // A NESTED HELPER MAY NAME ITS PARENT, and that is correct code.
            //
            // 855_Stickers.lua:342 declares `local function inspectSF(self)`
            // INSIDE `function tbl.RepairInvalidStickersInBook(arg1)` (:333),
            // and warns under the enclosing function's name:
            //
            //     warn("Stickers.RepairInvalidStickersInBook: there are ...")
            //
            // That is what a diagnostic in a nested helper SHOULD say - the
            // caller-visible operation is the parent's. Proven nested: the
            // parent is at column 0, the helper is indented, and there is no
            // top-level `end` between them.
            //
            // The rule below was "referenced name is any declared name", which
            // cannot tell a mis-paired closure from a helper naming its parent.
            // Enclosure is the discriminant, and `Decl` already carries the
            // line range needed to test it. A SIBLING sharing a name is still
            // the mis-pairing shape and is still reported.
            let referenced_encloses = decls.iter().any(|o| {
                o.name == referenced && o.start < d.start && o.end >= d.end
            });
            if referenced_encloses {
                continue;
            }
            // A VARIANT MAY NAME THE FUNCTION IT IMPLEMENTS.
            //
            // `_new` is the internal constructor `new` delegates to, and its
            // error text names the PUBLIC entry point because that is what the
            // caller invoked:
            //
            //     error("Argument #2 to Promise.new must be a promise or nil", 2)
            //
            // That is correct code, and identical in kind to the nested-helper
            // case above - the caller-visible operation is not this function's
            // own name. `rc_0270` (evaera promise) trips it three times:
            // `_new`/`new`, `_all`/`all`, `retryWithDelay`/`retry`.
            //
            // Related by convention means: the field is the referenced name
            // with leading underscores, or the referenced name is a prefix of
            // it. A genuine mis-pairing has UNRELATED names, so this narrows
            // the check without blunting it.
            let stripped = d.name.trim_start_matches('_');
            let is_variant = (stripped == referenced && d.name.starts_with('_'))
                || (d.name.len() > referenced.len() && stripped.starts_with(referenced.as_str()));
            if is_variant {
                continue;
            }
            // Only a defect if the referenced name is a sibling declaration:
            // that is the shape produced by mis-paired closures.
            if declared.contains(referenced.as_str()) {
                out.push(Finding::wrong(
                    "name_body_mismatch",
                    Some(line),
                    format!(
                        "`{}` contains a body that identifies itself as `{}` — \
                         the closure is paired with the wrong field name, so calling \
                         `{}` would run `{}`",
                        d.name, referenced, d.name, referenced
                    ),
                ));
                break; // one finding per declaration is enough
            }
        }
    }
}

struct Decl {
    name: String,
    start: usize, // line index (0-based) of the declaration
    end: usize,   // line index (0-based) of its `end`
}

/// Find `function X.Y(...)`, `function X:Y(...)` and `local function Z(...)`
/// declarations and the line range of each body.
fn collect_declarations(source: &str) -> Vec<Decl> {
    let lines: Vec<&str> = source.lines().collect();
    let mut decls = Vec::new();

    for (i, raw) in lines.iter().enumerate() {
        let line = raw.trim();
        let name = if let Some(rest) = line.strip_prefix("local function ") {
            rest.split('(').next().map(|s| s.trim().to_string())
        } else if let Some(rest) = line.strip_prefix("function ") {
            // `tbl.Create` / `tbl:Create` -> take the final component
            rest.split('(')
                .next()
                .map(|s| s.trim().rsplit(['.', ':']).next().unwrap_or("").to_string())
        } else {
            None
        };
        let Some(name) = name.filter(|n| !n.is_empty()) else { continue };

        // Walk forward to the matching `end` by tracking block depth.
        let mut depth = 1usize;
        let mut end = i;
        for (j, l2) in lines.iter().enumerate().skip(i + 1) {
            let t = l2.trim();
            if t.starts_with("function ")
                || t.starts_with("local function ")
                || t.ends_with(" do")
                || t.ends_with(" then")
                || t == "do"
            {
                depth += 1;
            }
            if t == "end" || t.starts_with("end)") || t.starts_with("end,") || t == "end;" {
                depth -= 1;
                if depth == 0 {
                    end = j;
                    break;
                }
            }
        }
        decls.push(Decl { name, start: i, end });
    }
    decls
}

/// Names that appear inside string literals within a declaration's body,
/// e.g. `error("Events.ServerCall: ...")` yields `ServerCall`.
fn string_referenced_names(source: &str, d: &Decl) -> Vec<(usize, String)> {
    let lines: Vec<&str> = source.lines().collect();
    let mut found = Vec::new();
    for i in (d.start + 1)..=d.end.min(lines.len().saturating_sub(1)) {
        let line = lines[i];
        for lit in string_literals(line) {
            // Take the token after a `.` — the `Module.Function` convention.
            for part in lit.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.')) {
                if let Some((_, func)) = part.rsplit_once('.') {
                    if func.len() > 2 && func.chars().next().is_some_and(|c| c.is_alphabetic()) {
                        found.push((i + 1, func.to_string()));
                    }
                }
            }
        }
    }
    found
}

fn string_literals(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == '"' {
            let mut s = String::new();
            i += 1;
            while i < bytes.len() && bytes[i] != '"' {
                if bytes[i] == '\\' {
                    i += 1;
                }
                if i < bytes.len() {
                    s.push(bytes[i]);
                }
                i += 1;
            }
            out.push(s);
        }
        i += 1;
    }
    out
}

// ── Check 2: every local that is read must have been written ───────────────
//
// Catches the ClickToMoveController defect, where `v9`..`v27` were referenced
// but never bound. Decompiler-generated names (`vN`, `upval_N`, `cap_N`,
// `argN`) are the only ones considered, because a real script's globals are
// legitimately unbound at file scope and would produce false positives.
fn check_undefined_locals(source: &str, out: &mut Vec<Finding>) {
    let mut bound: HashSet<String> = HashSet::new();
    let mut first_use: HashMap<String, usize> = HashMap::new();

    for (i, raw) in source.lines().enumerate() {
        let line = raw.trim();
        if line.starts_with("--") {
            continue;
        }
        // Bindings: `local a, b = ...`, `for k, v in`, `function f(a, b)`
        if let Some(rest) = line.strip_prefix("local ") {
            let names = rest.split('=').next().unwrap_or("");
            for n in names.split(',') {
                let n = n.trim().trim_start_matches("function ").split('(').next().unwrap_or("").trim();
                if !n.is_empty() {
                    bound.insert(n.to_string());
                }
            }
        }
        if let Some(rest) = line.strip_prefix("for ") {
            for n in rest.split(" in ").next().unwrap_or("").split(['=', ',']) {
                let n = n.trim();
                if !n.is_empty() {
                    bound.insert(n.to_string());
                }
            }
        }
        if let Some(open) = line.find('(') {
            if line.starts_with("function ") || line.starts_with("local function ") {
                if let Some(close) = line[open..].find(')') {
                    for p in line[open + 1..open + close].split(',') {
                        let p = p.trim();
                        if !p.is_empty() {
                            bound.insert(p.to_string());
                        }
                    }
                }
            }
        }
        // Uses of decompiler-generated identifiers.
        for tok in line.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
            if is_generated_name(tok) {
                first_use.entry(tok.to_string()).or_insert(i + 1);
            }
        }
    }

    let mut unbound: Vec<(&String, &usize)> =
        first_use.iter().filter(|(n, _)| !bound.contains(n.as_str())).collect();
    unbound.sort_by_key(|(_, line)| **line);

    for (name, line) in unbound.iter().take(8) {
        out.push(Finding::wrong(
            "undefined_local",
            Some(**line),
            format!("`{}` is read but never assigned — the output would error at runtime", name),
        ));
    }
    if unbound.len() > 8 {
        out.push(Finding::wrong(
            "undefined_local",
            None,
            format!("... and {} further undefined identifiers", unbound.len() - 8),
        ));
    }
}

fn is_generated_name(tok: &str) -> bool {
    for prefix in ["upval_", "cap_", "field_"] {
        if let Some(rest) = tok.strip_prefix(prefix) {
            return !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit());
        }
    }
    // `v12`, but not `v` or `vector`
    if let Some(rest) = tok.strip_prefix('v') {
        return rest.len() >= 1 && rest.chars().all(|c| c.is_ascii_digit());
    }
    false
}

// ── Check 2b: a bare `local x` that is read but never assigned ─────────────
//
// This check exists because a FIX in this project disarmed check 2.
//
// `free_var_decls` was added to repair captured upvalues that kept their USE
// and lost their DECLARATION. It works by declaring every unbound name at
// chunk top. That took `undefined_local` from 48 to 0 — but the reported win
// conflated two different situations:
//
//   * a real captured upvalue that was genuinely missing a declaration
//     -> declaring it is correct
//   * a value the lifter dropped on the floor
//     -> declaring it converts "undefined variable" into "variable that is
//        permanently nil", which parses cleanly and is still wrong
//
// The second case is worse than the first was, because it is SILENT.
//
// Found in `ReplicatedStorage.Badges`: a helper is inlined at 25 call sites
// with only its receiver substituted --
//
//     if Honey.Count then
//         Honey.Count = v12      -- v12 declared at chunk top, never assigned
//     end
//
// -- so 25 badge counts are set to nil. `undefined_local` sees the top-level
// `local ... v12 ...` and considers it bound, so it passes.
//
// SOUNDNESS: a name introduced by a bare `local` (no initialiser), never
// appearing on the left of any assignment, and read at least once, is nil at
// every one of those reads. There is no program for which that is intentional
// and also correct — if nil were wanted, the read would be of a literal nil.
// Declared-and-never-read is dead code, not a defect, so it is not flagged.
/// Identifiers appearing as BARE names, excluding field and method positions.
///
/// `math.cos`, `table.insert` and `s:sub()` name fields - they are not reads of
/// locals called `cos`, `insert` or `sub`. Counting them as reads made five
/// files report a value the bytecode dropped where in truth the local was
/// simply never used, which is the checker inventing a defect out of a name
/// collision.
///
/// Only a SINGLE dot binds: `a .. b` is concatenation, so `b` is a real read.
fn bare_idents(line: &str) -> Vec<&str> {
    let b = line.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        if !(b[i].is_ascii_alphanumeric() || b[i] == b'_') {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
            i += 1;
        }
        let is_field = start > 0
            && match b[start - 1] {
                b'.' => start < 2 || b[start - 2] != b'.',
                b':' => start < 2 || b[start - 2] != b':',
                _ => false,
            };
        if !is_field {
            out.push(&line[start..i]);
        }
    }
    out
}

fn check_declared_but_never_assigned(source: &str, out: &mut Vec<Finding>) {
    let lines: Vec<&str> = source.lines().collect();

    // 1. names introduced by a bare `local a, b, c` with no `=`
    let mut bare: Vec<(String, usize)> = Vec::new();
    for (i, raw) in lines.iter().enumerate() {
        let t = raw.trim();
        if t.starts_with("--") {
            continue;
        }
        let Some(rest) = t.strip_prefix("local ") else { continue };
        if rest.contains('=') || rest.starts_with("function ") {
            continue;
        }
        for n in rest.split(',') {
            let n = n.trim();
            if !n.is_empty() && n.chars().all(|c| c.is_alphanumeric() || c == '_') {
                bare.push((n.to_string(), i + 1));
            }
        }
    }
    if bare.is_empty() {
        return;
    }

    // 2. every name that is ever written to
    let mut assigned: HashSet<String> = HashSet::new();
    for raw in &lines {
        let t = raw.trim();
        if t.starts_with("--") {
            continue;
        }
        // `a = ...`, `a, b = ...`, `local a = ...`, and compound `a += ...`
        let stripped = t.strip_prefix("local ").unwrap_or(t);
        let Some(eq) = stripped.find('=') else { continue };
        // skip comparisons: ==, ~=, <=, >=
        let bytes = stripped.as_bytes();
        if stripped[eq..].starts_with("==")
            || (eq > 0 && matches!(bytes[eq - 1], b'~' | b'<' | b'>' | b'='))
        {
            continue;
        }
        let lhs = &stripped[..eq];
        for part in lhs.split(',') {
            let mut name = part.trim();
            // compound assignment: `x +=` leaves a trailing operator
            name = name.trim_end_matches(['+', '-', '*', '/', '.', '%']);
            let name = name.trim();
            // field or index writes bind the base, not the name itself
            if name.is_empty() || name.contains('.') || name.contains('[') || name.contains(':') {
                continue;
            }
            if name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                assigned.insert(name.to_string());
            }
        }
    }

    // 3. and every name that is read
    let mut read: HashSet<String> = HashSet::new();
    for (i, raw) in lines.iter().enumerate() {
        let t = raw.trim();
        if t.starts_with("--") || t.starts_with("local ") && !t.contains('=') {
            continue; // the bare declaration itself is not a read
        }
        let _ = i;
        for tok in bare_idents(t) {
            read.insert(tok.to_string());
        }
    }

    let mut reported = 0usize;
    for (name, line) in &bare {
        if assigned.contains(name) || !read.contains(name) {
            continue;
        }
        reported += 1;
        if reported > 6 {
            continue;
        }
        let uses = lines
            .iter()
            .filter(|l| {
                !l.trim_start().starts_with("--")
                    && bare_idents(l).into_iter().any(|tok| tok == name)
            })
            .count()
            .saturating_sub(1); // minus the declaration
        // WHERE IT IS READ, not just where it is declared.
        //
        // The declaration of a hoisted free variable sits in the chunk-top
        // block, inside no function, so a report carrying only that line
        // cannot be joined to any lifter trace: the proto column reads `-`
        // and the only remaining join key is the register NUMBER, which
        // repeats across protos and produced a table that read one file as
        // "admitted and still broken" when no such contradiction existed.
        // The read site is inside the function that actually lost the value.
        let first_read = lines
            .iter()
            .enumerate()
            .find(|(k, l)| {
                *k + 1 != *line
                    && !l.trim_start().starts_with("--")
                    && bare_idents(l).into_iter().any(|tok| tok == name)
            })
            .map(|(k, _)| k + 1);
        let where_read = match first_read {
            Some(r) => format!(" (first read line {})", r),
            None => String::new(),
        };
        out.push(Finding::wrong(
            "declared_never_assigned",
            Some(*line),
            format!(
                "`{}` is declared but never assigned, yet read {} time(s){} - \
                 every use evaluates to nil, so a value the bytecode carried was dropped",
                name, uses, where_read
            ),
        ));
    }
    if reported > 6 {
        out.push(Finding::wrong(
            "declared_never_assigned",
            None,
            format!("... and {} further names that are always nil", reported - 6),
        ));
    }
}

// ── Check 3: a chunk with many protos must produce many functions ──────────
//
// Catches the CameraModule defect: 32 protos in, `return {}` out. Every proto
// is a function body that existed in the source, so output containing far
// fewer function declarations than the chunk has protos means bodies were
// dropped on the floor.
//
// Deliberately generous — protos can be inlined legitimately — so this only
// fires when the shortfall is severe.
fn check_body_recovered(source: &str, proto_count: Option<usize>, out: &mut Vec<Finding>) {
    let Some(protos) = proto_count else { return };
    if protos < 4 {
        return;
    }
    // Count the `function` KEYWORD, not the three line shapes a declaration can
    // take. A body passed inline as a call argument -- `sig:Connect(function(a, b)`
    // -- or returned directly is a real emitted body, and the shape test saw
    // none of them.
    //
    // Loosening a check that exists to catch dropped bodies is exactly how a
    // genuine drop could start scoring clean, so this was verified against the
    // three CoreGui files that fire it rather than assumed:
    //
    //   24875  10 protos, shape test 2, keyword count  9, 81 lines  -> bodies present
    //   25948  12 protos, shape test 2, keyword count 11, 188 lines -> bodies present
    //   26253  12 protos, shape test 1, keyword count  1, 8 lines   -> genuinely dropped
    //
    // The keyword count separates them; the shape test called all three broken.
    // 26253 still fires, which is the point -- it collapses 12 protos into an
    // empty function body and `cap_0[cap_0]`.
    let emitted = source
        .match_indices("function")
        .filter(|(i, _)| {
            let before_ok = *i == 0
                || !source.as_bytes()[i - 1].is_ascii_alphanumeric()
                    && source.as_bytes()[i - 1] != b'_';
            let after = i + "function".len();
            let after_ok = after >= source.len()
                || !source.as_bytes()[after].is_ascii_alphanumeric()
                    && source.as_bytes()[after] != b'_';
            before_ok && after_ok
        })
        .count();
    // main proto is not itself emitted as a declaration
    let expected = protos.saturating_sub(1);
    if expected >= 4 && emitted * 4 < expected {
        out.push(Finding::wrong(
            "bodies_dropped",
            None,
            format!(
                "chunk has {} protos but the output declares only {} functions — \
                 most function bodies were discarded",
                protos, emitted
            ),
        ));
    }
}

// ── Check 4: a table built in a loop and never used is a discarded write ───
//
// Catches `local tbl2 = { [v.Name] = v }` inside a `for`, which is the
// signature of a SETTABLE whose target register was lost: the source said
// `tbl[v.Name] = v`, and every write is thrown away each iteration.
fn check_discarded_table_writes(source: &str, out: &mut Vec<Finding>) {
    let lines: Vec<&str> = source.lines().collect();
    let mut depth_for = 0usize;
    for (i, raw) in lines.iter().enumerate() {
        let t = raw.trim();
        if t.starts_with("for ") && t.ends_with(" do") {
            depth_for += 1;
        } else if t == "end" && depth_for > 0 {
            depth_for -= 1;
        }
        if depth_for == 0 {
            continue;
        }
        // `local <name> = {` inside a loop, where <name> is generated
        if let Some(rest) = t.strip_prefix("local ") {
            let Some((name, tail)) = rest.split_once('=') else { continue };
            let name = name.trim();
            if !tail.trim().starts_with('{') {
                continue;
            }
            let is_generated = name.starts_with("tbl") || is_generated_name(name);
            if !is_generated {
                continue;
            }
            // Used anywhere else in the file?
            let uses = lines
                .iter()
                .enumerate()
                .filter(|(j, l)| *j != i && l.contains(name))
                .count();
            if uses == 0 {
                out.push(Finding::wrong(
                    "discarded_table_write",
                    Some(i + 1),
                    format!(
                        "`{}` is built inside a loop and never read — this is a table \
                         assignment whose target was lost, so every write is discarded",
                        name
                    ),
                ));
            }
        }
    }
}

// ── Check 5: Roblox properties must not be called as methods ───────────────
//
// `script:Parent()` is not merely odd, it errors: Parent is a property.
// A GETTABLEKS was emitted as a NAMECALL.
const ROBLOX_PROPERTIES: &[&str] = &[
    "Parent", "Name", "ClassName", "Value", "Position", "CFrame", "Size",
    "Transparency", "Anchored", "CanCollide", "Character", "LocalPlayer",
    "Text", "Visible", "Enabled", "Health", "WalkSpeed", "PlaceId", "JobId",
];

/// Receivers that are unambiguously Roblox Instances. Anything else may be a
/// user object whose methods legitimately share a name with a property.
const INSTANCE_RECEIVERS: &[&str] = &["script", "game", "workspace", "Workspace"];

fn check_property_called_as_method(source: &str, out: &mut Vec<Finding>) {
    // SOUNDNESS: an earlier version flagged `:Text(`, `:Name(`, `:Value(` etc.
    // on ANY receiver. On a full-machine scan that produced 1,890 findings, and
    // the largest were all false positives of the same shape:
    //
    //     local builder = ChangelogBuilder.new(...)
    //     builder:Section("...", function(p) p:Text("...") end)
    //
    // `:Text()` there is a real method on a user builder object. Nothing is
    // wrong with that code. Flagging it violates the rule this module is built
    // on -- a check that cries wolf gets the whole report ignored -- so the
    // receiver must now be provably an Instance.
    //
    // This deliberately trades recall for soundness. `someInstance:Parent()` on
    // a local will no longer be caught; `script:Parent()`, the form actually
    // observed coming out of the lifter, still is.
    for (i, raw) in source.lines().enumerate() {
        let line = raw.trim();
        if line.starts_with("--") {
            continue;
        }
        for prop in ROBLOX_PROPERTIES {
            let pat = format!(":{}(", prop);
            let Some(at) = line.find(&pat) else { continue };
            // Identify the receiver: the identifier immediately before the ':'.
            let before = &line[..at];
            let recv: String = before
                .chars()
                .rev()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            if !INSTANCE_RECEIVERS.contains(&recv.as_str()) {
                continue;
            }
            out.push(Finding::wrong(
                "property_called_as_method",
                Some(i + 1),
                format!(
                    "`{}:{}()` calls a property as a method — this would error at runtime; \
                     a property read was emitted as a method call",
                    recv, prop
                ),
            ));
        }
    }
}

/// Human-readable report.
pub fn format_report(findings: &[Finding]) -> String {
    if findings.is_empty() {
        return "no semantic defects found".to_string();
    }
    let wrong = findings.iter().filter(|f| f.severity == Severity::Wrong).count();
    let susp = findings.len() - wrong;
    let mut s = format!("{} provably wrong, {} suspicious\n", wrong, susp);
    for f in findings {
        let loc = f.line.map(|l| format!("line {}", l)).unwrap_or_else(|| "file".into());
        let sev = match f.severity {
            Severity::Wrong => "WRONG",
            Severity::Suspicious => "SUSPECT",
        };
        s.push_str(&format!("  [{}] {} ({}): {}\n", sev, f.check, loc, f.detail));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── branch_local_discarded: all three conditions are load-bearing ──────
    //
    // The positive case is `213_Collectibles_Tornado` reduced to its shape.
    // Each negative below removes exactly ONE condition, so a fix that widens
    // any of them turns that test red rather than silently inflating findings.

    fn fires(src: &str) -> bool {
        check(src, None)
            .iter()
            .any(|f| f.check == "branch_local_discarded")
    }

    #[test]
    fn catches_a_branch_value_written_to_a_fresh_local() {
        let src = "local Lvl3 = Lvl2 + 1
if cond then
    local Lvl4 = Lvl3 + 2
end
local Lvl5 = Lvl3 * m
";
        assert!(fires(src), "the discarded `+ 2` must be reported");
    }

    #[test]
    fn ignores_an_ordinary_unused_local() {
        // Condition 2. A fresh value that extends nothing is dead code, not a
        // lost reassignment - this is the case that separated 162 real sites
        // from 1,161 candidates.
        let src = "local Players = game.Players
local x = game.Workspace
print(game)
";
        assert!(!fires(src), "an unused local that extends nothing is not this defect");
    }

    #[test]
    fn ignores_a_local_that_is_read_later() {
        // Condition 1. If the value reaches anything, it was not discarded.
        let src = "local a = b + 1
local c = a + 2
print(c, b)
";
        assert!(!fires(src), "a local that is read again has not been dropped");
    }

    #[test]
    fn ignores_an_extension_whose_base_dies_with_it() {
        // Condition 3. If the base is never used after, nothing observed the
        // old value, so no value demonstrably failed to arrive.
        let src = "local base = 1
local ext = base + 2
print(other)
";
        assert!(!fires(src), "a base that is never read afterwards proves nothing");
    }


    #[test]
    fn ignores_a_value_that_is_recomputed_inline() {
        // Condition 4, from 21_ClickToMove: the local is dead, but the same
        // expression is evaluated again at the use site, so nothing was lost.
        let src = "local v34 = arg1 + 0.1
if arg2 < arg1 + 0.1 then
    x = 1
end
print(arg1)
";
        assert!(!fires(src), "a recomputed value has not been dropped");
    }

    #[test]
    fn still_reports_a_value_computed_only_once() {
        // 859_StickerPlacer: the expression appears nowhere else, so the
        // computation really is discarded. This is the case condition 4 must
        // NOT swallow.
        let src = "local Size = arg3 + arg2.Size.X / 2
local id = f(arg2)
if id then g(arg3) end
";
        assert!(fires(src), "a value computed once and never read is still a defect");
    }
    #[test]
    fn a_word_inside_a_string_is_not_an_identifier() {
        // blank_noncode must hide string contents, or `Lvl3` appearing in a
        // message would count as a later read and silence a real finding.
        let src = "local Lvl3 = Lvl2 + 1
if c then
    local Lvl4 = Lvl3 + 2
end
print(\"Lvl3 done\")
local z = Lvl3 * 2
";
        assert!(fires(src), "a string mentioning the name must not suppress the finding");
    }

    /// The Events.lua defect: names paired with the wrong bodies.

    // ── calls_not_emitted ────────────────────────────────────────────────
    //
    // The header line is written by the lifter, which is the only place that
    // holds both the bytecode and the emitted AST. These pin the reading of
    // it, including the case that must stay silent.

    fn fires_calls(src: &str) -> bool {
        check(src, None).iter().any(|f| f.check == "calls_not_emitted")
    }

    #[test]
    fn reports_a_function_that_emitted_no_calls() {
        let src = "-- call recovery: 1 proto(s) emitted 15 fewer call(s) than the bytecode contains
local x = 1
";
        assert!(fires_calls(src), "a dropped body must be reported");
    }

    #[test]
    fn stays_silent_when_no_header_line_is_present() {
        // The overwhelming majority of files: the lifter only writes the line
        // when a proto is short, so absence must never fire.
        let src = "-- Protos: 6 total, main=5
local x = 1
";
        assert!(!fires_calls(src));
    }

    #[test]
    fn stays_silent_on_a_zero_count() {
        let src = "-- call recovery: 0 proto(s) emitted 0 fewer call(s) than the bytecode contains
local x = 1
";
        assert!(!fires_calls(src), "zero short protos is not a defect");
    }

    #[test]
    fn ignores_the_line_beyond_the_header() {
        // Only the first 20 lines are the header; a later line mentioning it
        // is file content, not evidence.
        let mut src = String::new();
        for _ in 0..30 { src.push_str("local a = 1
"); }
        src.push_str("-- call recovery: 3 proto(s) emitted 9 fewer call(s) than the bytecode contains
");
        assert!(!fires_calls(&src));
    }
    #[test]
    fn catches_name_body_mismatch() {
        let src = r#"
function tbl.Create(arg1, ...)
    error("Events.ServerCall: No event named " .. arg1)
end
function tbl.ServerCall(arg1, ...)
    error("Events.ServerCall: No event named " .. arg1)
end
"#;
        let f = check(src, None);
        assert!(
            f.iter().any(|x| x.check == "name_body_mismatch"),
            "should catch Create carrying ServerCall's body: {:?}",
            f
        );
    }

    /// An internal or extended VARIANT naming the function it implements is
    /// correct code, not a mis-pairing.
    ///
    /// `rc_0270` (evaera promise) tripped this three times. `_new` is the
    /// internal constructor `Promise.new` delegates to, and its error text
    /// names the public entry point because that is what the caller invoked.
    /// Same for `_all`/`all` and `retryWithDelay`/`retry`.
    #[test]
    fn variant_naming_its_public_counterpart_is_not_a_mismatch() {
        let src = r#"
function tbl.new(arg1)
    return tbl._new(arg1)
end
function tbl._new(arg1)
    error("Argument #2 to Promise.new must be a promise or nil", 2)
end
function tbl.retry(fn, times)
    return fn(times)
end
function tbl.retryWithDelay(fn, times, delay)
    error("Please pass a callback to Promise.retry", 2)
end
"#;
        let f = check(src, None);
        assert!(
            !f.iter().any(|x| x.check == "name_body_mismatch"),
            "`_new` naming `new`, and `retryWithDelay` naming `retry`, are              conventional variants and must not fire: {:?}",
            f
        );
    }

    /// The exemption must stay NARROW: unrelated sibling names still fire.
    /// Without this the test above could be satisfied by deleting the check.
    #[test]
    fn unrelated_sibling_name_still_fires_after_variant_exemption() {
        let src = r#"
function tbl.Create(arg1, ...)
    error("Events.ServerCall: No event named " .. arg1)
end
function tbl.ServerCall(arg1, ...)
    error("Events.ServerCall: No event named " .. arg1)
end
"#;
        let f = check(src, None);
        assert!(
            f.iter().any(|x| x.check == "name_body_mismatch"),
            "`Create` and `ServerCall` are unrelated - the variant exemption              must not swallow this: {:?}",
            f
        );
    }

    /// A function naming itself is correct and must NOT fire.
    #[test]
    fn accepts_matching_name_and_body() {
        let src = r#"
function tbl.ServerCall(arg1, ...)
    error("Events.ServerCall: No event named " .. arg1)
end
function tbl.ClientCall(arg1, ...)
    error("Events.ClientCall: No event named " .. arg1)
end
"#;
        let f = check(src, None);
        assert!(
            !f.iter().any(|x| x.check == "name_body_mismatch"),
            "correct output must not be flagged: {:?}",
            f
        );
    }

    #[test]
    fn catches_undefined_generated_locals() {
        let src = "local service3 = {v9}\nreturn service3\n";
        let f = check(src, None);
        assert!(f.iter().any(|x| x.check == "undefined_local"));
    }

    #[test]
    fn catches_dropped_bodies() {
        let src = "local tbl = {}\nreturn {}\n";
        let f = check(src, Some(32));
        assert!(f.iter().any(|x| x.check == "bodies_dropped"));
    }

    #[test]
    fn catches_discarded_loop_table() {
        let src = "for k, v in pairs(t) do\n    local tbl2 = {\n        [v.Name] = v\n    }\nend\n";
        let f = check(src, None);
        assert!(f.iter().any(|x| x.check == "discarded_table_write"));
    }

    #[test]
    fn catches_property_called_as_method() {
        let src = "local p = script:Parent()\n";
        let f = check(src, None);
        assert!(f.iter().any(|x| x.check == "property_called_as_method"));
    }

    /// Clean output must produce nothing at all.
    #[test]
    fn clean_output_is_silent() {
        let src = r#"
local function add(a, b)
    return a + b
end
local t = {}
t.value = add(1, 2)
return t
"#;
        let f = check(src, Some(2));
        assert!(f.is_empty(), "clean source flagged: {:?}", f);
    }
}
