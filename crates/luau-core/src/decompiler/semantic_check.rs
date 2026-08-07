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
pub fn check(source: &str, proto_count: Option<usize>) -> Vec<Finding> {
    let mut out = Vec::new();
    check_name_body_agreement(source, &mut out);
    check_undefined_locals(source, &mut out);
    check_declared_but_never_assigned(source, &mut out);
    check_body_recovered(source, proto_count, &mut out);
    check_discarded_table_writes(source, &mut out);
    check_property_called_as_method(source, &mut out);
    out.sort_by_key(|f| (f.severity, f.line.unwrap_or(0)));
    out
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
        for tok in t.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
            if !tok.is_empty() {
                read.insert(tok.to_string());
            }
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
                    && l.split(|c: char| !(c.is_alphanumeric() || c == '_'))
                        .any(|tok| tok == name)
            })
            .count()
            .saturating_sub(1); // minus the declaration
        out.push(Finding::wrong(
            "declared_never_assigned",
            Some(*line),
            format!(
                "`{}` is declared but never assigned, yet read {} time(s) — \
                 every use evaluates to nil, so a value the bytecode carried was dropped",
                name, uses
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
    let emitted = source
        .lines()
        .filter(|l| {
            let t = l.trim();
            t.starts_with("function ") || t.starts_with("local function ") || t.contains("= function(")
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

    /// The Events.lua defect: names paired with the wrong bodies.
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
