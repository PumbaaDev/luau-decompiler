//! Records, per chunk, every site that minted a decompiler-generated name
//! (`vN`, `upval_N`, `cap_N`), so the largest remaining defect class can be
//! attributed to a call site instead of described as an absence.
//!
//! Gated on `LUAU_MINT_TRACE`; a no-op when unset, and verified
//! behaviour-neutral against the compile gate (621/628), the semantic checks
//! (266/628) and CoreScript ground truth (9/9).
//!
//! See `docs/GENERATED_NAME_ORIGINS.md` for the recipe and the measurement it
//! produced. Kept in the tree rather than reapplied as a patch because the
//! defect it measures is open, and the next attempt on it starts here.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

thread_local! {
    static MINTS: RefCell<BTreeMap<String, BTreeSet<&'static str>>> =
        RefCell::new(BTreeMap::new());
}

pub fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("LUAU_MINT_TRACE").is_ok())
}

/// A name can be minted at several sites within one chunk, so sites accumulate
/// into a set rather than overwriting -- the combination is itself a signal.
pub fn note(site: &'static str, name: &str) {
    if !enabled() {
        return;
    }
    MINTS.with(|m| {
        m.borrow_mut()
            .entry(name.to_string())
            .or_default()
            .insert(site);
    });
}

pub fn sites_for(name: &str) -> String {
    MINTS.with(|m| match m.borrow().get(name) {
        Some(s) => s.iter().cloned().collect::<Vec<_>>().join("+"),
        None => "NONE".to_string(),
    })
}

pub fn clear() {
    MINTS.with(|m| m.borrow_mut().clear());
}
