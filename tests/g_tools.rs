// Merged test binary: one link unit for a whole subsystem (test-suite link
// time dominates `cargo test`; fewer binaries = faster compile). Each file
// keeps its own module namespace, so helpers cannot collide.
// `tests/common/mod.rs` is included once per test binary, so a static there is
// shared by every suite module of THIS binary — which is exactly what a
// process-wide env lock needs. The copy is duplicated per binary, and that is
// correct: separate test binaries are separate processes with separate
// environments, so their locks can never race each other.
mod common;

#[path = "suites/builtin_disable_w13.rs"]
mod builtin_disable_w13;
#[path = "suites/context_mode_exposure_w13.rs"]
mod context_mode_exposure_w13;
#[path = "suites/crash_log.rs"]
mod crash_log;
#[path = "suites/export_html.rs"]
mod export_html;
#[path = "suites/g11_context_shadow.rs"]
mod g11_context_shadow;
#[path = "suites/g11_skills_diagnostics.rs"]
mod g11_skills_diagnostics;
#[cfg(feature = "python")]
#[path = "suites/python_vm_lock.rs"]
mod python_vm_lock;
#[path = "suites/read_image_tool.rs"]
mod read_image_tool;
#[path = "suites/shell_tools.rs"]
mod shell_tools;
#[path = "suites/system_prompt.rs"]
mod system_prompt;
#[path = "suites/tool_exposure_parity_w14.rs"]
mod tool_exposure_parity_w14;
#[path = "suites/tool_exposure_parity_w16.rs"]
mod tool_exposure_parity_w16;
#[path = "suites/tool_exposure_virtual_models.rs"]
mod tool_exposure_virtual_models;
#[path = "suites/tool_parity_h.rs"]
mod tool_parity_h;
#[path = "suites/tool_repair.rs"]
mod tool_repair;
#[path = "suites/tool_repair_grammar.rs"]
mod tool_repair_grammar;
#[path = "suites/tool_result_size_w17.rs"]
mod tool_result_size_w17;
#[path = "suites/tool_schema_validation.rs"]
mod tool_schema_validation;
#[path = "suites/tool_schema_w11.rs"]
mod tool_schema_w11;
#[path = "suites/tool_selection_parity_w13.rs"]
mod tool_selection_parity_w13;
#[path = "suites/tool_selection_units.rs"]
mod tool_selection_units;
#[path = "suites/tools_hardening.rs"]
mod tools_hardening;
#[path = "suites/tools_security_hardening.rs"]
mod tools_security_hardening;
#[path = "suites/tools_security_w9.rs"]
mod tools_security_w9;
#[path = "suites/tui_ansi_theme.rs"]
mod tui_ansi_theme;
#[path = "suites/write_persistence_nn.rs"]
mod write_persistence_nn;
