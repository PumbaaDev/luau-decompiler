//! Per-phase test modules extracted from `lifter.rs` as part of
//! Phase B0.52P6 (part 3/3 of the split).  Each file contains the
//! regression tests for one development phase so that the main
//! `lifter/mod.rs` stays focused on production code.
//!
//! References to lifter-internal items use `super::super::` (→ `lifter`).

#[cfg(test)]
mod phase_b03_numeric_for_tests;
#[cfg(test)]
mod phase_b042_control_flow_polish_tests;
#[cfg(test)]
mod phase_b043a_bounds_check_tests;
#[cfg(test)]
mod phase_b043b_upval_inference_tests;
#[cfg(test)]
mod phase_b045a_aggressive_inline_tests;
#[cfg(test)]
mod phase_b051b_inline_pure_literals_tests;
#[cfg(test)]
mod phase_b046a_repeat_until_tests;
#[cfg(test)]
mod phase_b047_table_constructor_tests;
#[cfg(test)]
mod phase_b048_two_step_absorb_tests;
#[cfg(test)]
mod phase_b049_shadow_local_tests;
#[cfg(test)]
mod phase_b051_implicit_table_seed_tests;
#[cfg(test)]
mod phase_b052p5_opcode_coverage_tests;
#[cfg(test)]
mod phase_b086_b087_ternary_nil_init_tests;
#[cfg(test)]
mod phase_b092_simplify_fold_gaps_tests;
#[cfg(test)]
mod phase_b093c_if_return_bool_tests;
#[cfg(test)]
mod phase_b094_not_condition_swap_tests;
#[cfg(test)]
mod phase_b095_if_assign_bool_tests;
#[cfg(test)]
mod phase_b097_if_return_ternary_tests;
#[cfg(test)]
mod phase_c2_setupval_backscan_tests;
#[cfg(test)]
mod phase_c2_upval_recurse_tests;
#[cfg(test)]
mod phase_c2_multireturn_unpack_tests;
#[cfg(test)]
mod phase_c2_method_notation_tests;
#[cfg(test)]
mod phase_c2_setlist_coalesce_tests;
#[cfg(test)]
mod phase_c4_lifter_guards_tests;
#[cfg(test)]
mod phase_c5_pending_table_inline_tests;
#[cfg(test)]
mod phase_c6_require_naming_tests;
#[cfg(test)]
mod phase_c8_require_wrapper_unwrap_tests;
#[cfg(test)]
mod namecall_orphan_diagnosis_tests;
#[cfg(test)]
mod phase_c10q_kv_drop_any_name_tests;
#[cfg(test)]
mod phase_c10r_empty_fn_drop_tests;
#[cfg(test)]
mod phase_c10s_stdlib_lvalue_drop_tests;
#[cfg(test)]
mod phase_c10v_generic_prefix_drop_tests;
