//! Per-call v10-v14 decode policy, without mutating the process environment.
//!
//! The parser gates v10+ containers behind two switches (see `read_chunk`):
//!
//! * `LUAU_ALLOW_V10PLUS` - accept version 10-14 at all (default: reject).
//! * `LUAU_V10PLUS_RAW`   - skip the upstream CALLFB/class-opcode fixup, because
//!   the opcodes are Roblox-shuffled and an opmap will interpret them.
//!
//! The CLI turns both on for a v10-14 input by calling `std::env::set_var`.
//! A library cannot do that safely: the environment is process-global, so a
//! host (a server decoding requests on several threads, a C program linking
//! `luau-ffi`) would have its own configuration changed underneath it, and
//! `set_var` racing a concurrent `getenv` is undefined behaviour on some
//! platforms. So the library states the same policy for ONE call on ONE
//! thread with [`with_policy`], and the parser consults it only when the
//! variable is absent from the environment.
//!
//! Precedence per switch, which is exactly what the CLI produces:
//!
//! 1. the environment variable, when present (`""` means off, anything else on);
//! 2. otherwise the scoped policy, when one is in force on this thread;
//! 3. otherwise off.
//!
//! The CLI only sets a variable that is absent, so an explicit environment
//! setting wins there too - the two front ends agree for every environment.
//!
//! WHEN the library installs the policy is decided in `lib.rs`: only for a
//! chunk a measured opcode-database entry verifies (unless the caller opts
//! into the CLI's detector fallback). Applied to every v10-14 chunk, RAW mode
//! turns stock `luau-compile` v12 (call feedback on by default) into
//! plausible-but-wrong code, which is what the environment gate exists to
//! refuse.

use std::cell::Cell;

/// What the parser may do with a v10-v14 container during one call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct V10PlusPolicy {
    /// Accept bytecode versions 10-14 (`LUAU_ALLOW_V10PLUS`).
    pub allow: bool,
    /// Leave opcodes >= 84 untouched for an opmap to interpret
    /// (`LUAU_V10PLUS_RAW`).
    pub raw: bool,
}

impl V10PlusPolicy {
    /// The policy the CLI applies to live Roblox client bytecode: accept the
    /// container and pass the shuffled opcodes through raw.
    pub const ROBLOX_CLIENT: V10PlusPolicy = V10PlusPolicy { allow: true, raw: true };
}

thread_local! {
    static SCOPED: Cell<Option<V10PlusPolicy>> = const { Cell::new(None) };
}

/// The policy the CLI's `enable_v10plus_for` applies to these bytes: Roblox
/// client policy for a v10-14 chunk, nothing otherwise. v3-9 is never touched
/// (both switches are version-gated to >= 10 inside the parser anyway).
pub fn roblox_policy_for(bytecode: &[u8]) -> Option<V10PlusPolicy> {
    matches!(bytecode.first(), Some(10..=14)).then_some(V10PlusPolicy::ROBLOX_CLIENT)
}

/// Has the host configured v10+ decoding itself? True when
/// `LUAU_ALLOW_V10PLUS` is present in the environment at all (empty means an
/// explicit "off"). The library's v10+ gate then stands aside entirely, so an
/// explicit configuration behaves exactly as it did before the gate existed.
/// The CLI exports the variable for every v10-14 input, which is why its output
/// does not depend on the library's gate.
pub fn environment_configures_v10plus() -> bool {
    std::env::var_os("LUAU_ALLOW_V10PLUS").is_some()
}

/// Run `f` with `policy` in force for parses on the current thread.
///
/// Nests, and restores the previous policy on exit, including on unwind.
pub fn with_policy<R>(policy: V10PlusPolicy, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<V10PlusPolicy>);
    impl Drop for Restore {
        fn drop(&mut self) {
            SCOPED.with(|s| s.set(self.0));
        }
    }
    let _restore = Restore(SCOPED.with(|s| s.replace(Some(policy))));
    f()
}

/// Resolve one switch: environment first, then the scoped policy, then off.
fn resolve(var: &str, pick: fn(V10PlusPolicy) -> bool) -> bool {
    match std::env::var(var) {
        Ok(v) => !v.is_empty(),
        // Present but not UTF-8 read as off before this module existed; keep it.
        Err(std::env::VarError::NotUnicode(_)) => false,
        Err(std::env::VarError::NotPresent) => SCOPED.with(|s| s.get()).is_some_and(pick),
    }
}

pub(crate) fn allow_v10plus() -> bool {
    resolve("LUAU_ALLOW_V10PLUS", |p| p.allow)
}

pub(crate) fn raw_v10plus() -> bool {
    resolve("LUAU_V10PLUS_RAW", |p| p.raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    // These run on their own test threads and only touch the thread-local, so
    // they cannot disturb (or be disturbed by) any other test. An explicit
    // environment setting outranks the scope by design, so they stand down if
    // the person running the suite has exported either variable.
    fn env_is_clean() -> bool {
        std::env::var_os("LUAU_ALLOW_V10PLUS").is_none()
            && std::env::var_os("LUAU_V10PLUS_RAW").is_none()
    }

    #[test]
    fn policy_is_off_outside_a_scope_and_restored_after_one() {
        if !env_is_clean() {
            return;
        }
        assert!(!allow_v10plus() && !raw_v10plus());
        with_policy(V10PlusPolicy::ROBLOX_CLIENT, || {
            assert!(allow_v10plus() && raw_v10plus());
            with_policy(V10PlusPolicy { allow: true, raw: false }, || {
                assert!(allow_v10plus() && !raw_v10plus());
            });
            assert!(raw_v10plus(), "inner scope must restore the outer policy");
        });
        assert!(!allow_v10plus() && !raw_v10plus());
    }

    #[test]
    fn policy_is_restored_on_unwind() {
        if !env_is_clean() {
            return;
        }
        let r = std::panic::catch_unwind(|| {
            with_policy(V10PlusPolicy::ROBLOX_CLIENT, || panic!("boom"))
        });
        assert!(r.is_err());
        assert!(!allow_v10plus(), "a panic inside the scope must not leak the policy");
    }

    #[test]
    fn roblox_policy_only_for_v10_to_v14() {
        for v in [0u8, 3, 6, 9, 15, 255] {
            assert_eq!(roblox_policy_for(&[v, 0]), None, "version {v}");
        }
        for v in 10u8..=14 {
            assert_eq!(roblox_policy_for(&[v]), Some(V10PlusPolicy::ROBLOX_CLIENT));
        }
        assert_eq!(roblox_policy_for(&[]), None);
    }
}
