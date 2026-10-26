//! OR-chain return guards that `find_or_chain_return_guard` cannot take, on two
//! REAL scripts (v12 oracle `StickerStack` module and BloxFruits
//! `React.Factories.Hooks.ItemReplication`).
//!
//! Both lost the whole guard the same way: the plain if/else fallback reports
//! the guard body as the MERGE of the first link, so the body is emitted after
//! the continuation's own trailing `return` and dropped as dead, and the second
//! link's jump straight to the continuation is out of range and dropped too.
//!
//! * `IsOffCooldown` (pc16 JUMPIFNOT R2 -> 21; pc17-19 LENGTH, LOADN,
//!   JUMPIFNOTLE -> 23; pc21-22 `return true`): the second link needs `#mods`,
//!   which the pure-preamble whitelist did not admit. Now admitted, so the
//!   existing per-link emission (one `if ... return true end` per link) takes
//!   it.
//! * the `useEffect` callback in `use` (pc7 JUMPIFNOT R1 -> 10; pc8-9 GETUPVAL,
//!   JUMPIF -> 26; pc10-25 the inlined `apply(nil)` with its own early return):
//!   the body is three blocks, not one terminal return block.

const IS_OFF_COOLDOWN: &[u8] = include_bytes!("fixtures/or_guard_is_off_cooldown.luac");
const ITEM_REPLICATION: &[u8] = include_bytes!("fixtures/or_guard_item_replication.luac");

fn norm(s: &str) -> String {
    s.lines().map(|l| l.trim_end()).collect::<Vec<_>>().join("\n")
}

fn decompiled(bytes: &[u8]) -> String {
    norm(&crate::decompile(bytes).expect("fixture decompiles"))
}

fn assert_has(out: &str, want: &str) {
    let want = norm(want);
    assert!(out.contains(&want), "missing:\n{want}\n--- in output ---\n{out}");
}

#[test]
fn is_off_cooldown_keeps_its_length_bound_return_guard() {
    // Bytecode: `if not mods or #mods <= 0 then return true end` before the
    // duration arithmetic; emitted one guard per link (same behaviour).
    let out = decompiled(IS_OFF_COOLDOWN);
    assert_has(
        &out,
        "        local GetModsBySource = value.GetModsBySource(arg1, \"Sticker Stack\")
        if not GetModsBySource then
            return true
        end
        if #GetModsBySource <= 0 then
            return true
        end
        local ReplicatedStorage2 = game.ReplicatedStorage",
    );
}

#[test]
fn item_replication_effect_keeps_its_multi_block_return_guard() {
    // Bytecode: `if not IsInitialized or not id then <inlined apply(nil)> return
    // end` before `assert(IS_CLIENT, "bad service")`.
    let out = decompiled(ITEM_REPLICATION);
    assert_has(
        &out,
        "            if not IsInitialized3 or not getId then
                if useRef2.current == nil then
                    return
                end
                useRef2.current = nil
                result22(decode(nil))
                return
            end
            local IS_CLIENT3 = value5.IS_CLIENT",
    );
}

const DROP_BABY_HELPER: &[u8] = include_bytes!("fixtures/or_guard_drop_baby_helper.luac");

/// AdoptMe `DropBabyHelper.refresh_button_visibility` (pc0-7):
/// `GETUPVAL R0 U0; JUMPIFNOT R0; GETUPVAL R0 U1; JUMPXEQKNIL R0;
/// GETUPVAL R1 U1; LENGTH R0 #R1; JUMPXEQKN R0 0`. R0 is an upvalue alias of
/// U1 when LENGTH overwrites it and the only read of the new value is the
/// link's own branch, so the length must stay a local value: there is no
/// SETUPVAL, and `u = #u` would be a fabricated upvalue write.
#[test]
fn drop_baby_helper_length_link_does_not_write_the_upvalue() {
    let out = decompiled(DROP_BABY_HELPER);
    assert!(!out.contains("v11 = #v11"), "fabricated upvalue write:\n{out}");
    assert_has(
        &out,
        "    if v11 == nil then
        v12:hide()
        return
    end
    if #v11 == 0 then
        v12:hide()
        return
    end
    v12:show()
end",
    );
}

const CHANNEL_FOR: &[u8] = include_bytes!("fixtures/or_guard_typeof_link_channel_for.luac");

/// SAE `Notifications.channelFor` (proto 0, pc7/pc15): both links of
/// `if typeof(t) ~= "table" or typeof(t.Show) ~= "function" then warn(..)
/// return nil end return t` park a `typeof` call and test it with a
/// JUMPXEQKS compare. The call-link gate only took JUMPIF/JUMPIFNOT tests and
/// no FASTCALL1 in the preamble, so the guard was not recognised and the plain
/// fallback emitted two nested ifs, dropping the whole warn / `return nil`
/// body (3 calls).
#[test]
fn typeof_compare_links_keep_the_warn_and_nil_return_body() {
    let out = decompiled(CHANNEL_FOR);
    assert_has(
        &out,
        "    if typeof(tbl[arg1]) ~= \"table\" or typeof(tbl[arg1].Show) ~= \"function\" then
        local result = tostring(arg1)
        local formatted = (\"Alerts.Raise addressed an unknown notification channel: %*\"):format(result)
        warn(formatted)
        return nil
    end
    return tbl[arg1]",
    );
}

const DR_SCRAMBLE: &[u8] = include_bytes!("fixtures/or_guard_loop_elseif_dr_scramble.luac");

/// SAE `DrScrambleSequence.setRiftShown` (proto 10): inside the FORGPREP loop
/// `if v:IsA("BasePart") then .. elseif v:IsA("ParticleEmitter") or
/// v:IsA("Beam") or v:IsA("Light") then .. end`. Both arms contain a value
/// join (`arg2 and 0 or 1`, `arg3[v] ~= nil and arg3[v] or true`). Cutting the
/// loop body into regions at those joins left the first `if`'s jump to the
/// elseif (pc10 -> 18) and the or-chain's last exit (pc35 -> 49) past the end
/// of their range; the elseif arm and its three IsA calls were dropped.
#[test]
fn loop_elseif_or_chain_keeps_its_arm_and_all_three_isa_links() {
    let out = decompiled(DR_SCRAMBLE);
    assert!(
        out.contains("v:IsA(\"ParticleEmitter\")")
            && out.contains("v:IsA(\"Beam\")")
            && out.contains("v:IsA(\"Light\")")
            && out.contains("v.Enabled = "),
        "elseif arm lost:\n{out}"
    );
    assert!(
        !out.contains("emitted 3 fewer call"),
        "call recovery still reports a loss:\n{out}"
    );
}

/// Same proto: the `if arg2 then Enabled = arg3[v] ~= nil .. else
/// v.Enabled = false end` after the `or` chain (pc36 JUMPIFNOT R1 -> pc46, then
/// LOADB false / SETTABLEKS Enabled) must keep its else write instead of
/// collapsing to `if not arg2 then continue end`.
#[test]
fn loop_elseif_or_chain_keeps_the_enabled_false_else_write() {
    let out = decompiled(DR_SCRAMBLE);
    assert!(
        out.contains("v.Enabled = false"),
        "else write v.Enabled = false lost:
{out}"
    );
}
