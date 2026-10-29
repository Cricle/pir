//! CJS named-export discovery for the loader's ESM interop wrapper (phase 4b).
//!
//! `import { x } from "<cjs file>"` in Node links against the names a
//! cjs-module-lexer scan finds in the file — the named exports are a link-time
//! snapshot, not the full runtime surface. This module reproduces that scan
//! with a real parser: swc parses the file and the walk below collects
//!
//! - `exports.x = ...` / `exports["x"] = ...` / `module.exports.x = ...`
//!   (the tail target of any member chain rooted at the exports object),
//! - `Object.defineProperty(exports | module.exports, "x", ...)`,
//! - object-literal keys of `module.exports = { x, ... }` (and the chained
//!   `exports = module.exports = { ... }`),
//! - chained assignments (`exports.a = exports.b = ... = void 0`, the shape
//!   the TypeScript compiler and UMD bundlers use to declare a whole surface),
//! - re-exports `module.exports = require("<spec>")`, followed recursively
//!   (depth-bounded, each file visited once); relative specifiers resolve
//!   through the npm resolver's file probing, bare ones through the
//!   `node_modules` walk-up with `require` conditions (`vscode-languageserver-
//!   protocol` and friends re-export whole packages this way),
//! - the TypeScript compiler's deferred re-export helpers:
//!   `__exportStar(require("<spec>"), exports)`, `__exportStar(<ident>, exports)`
//!   where `<ident>` is bound to a `require("<spec>")` in the same file, and
//!   `__createBinding(exports, <ident>, "<name>"[, "<name2>"])` — the source
//!   module is scanned recursively, again with the same cycle guard,
//! - the export surface a UMD wrapper hands to its factory
//!   (`(function (factory) { … })(function (require, exports) { exports.x = …; })`),
//!   whose body shares the module's `exports` object.
//!
//! Anything dynamic (`exports[name] = ...`, loops, `Object.assign`, the
//! `__createBinding(exports, m, p)` inside the helper definitions themselves)
//! is not seen — the same limitation as Node's cjs-module-lexer. A parse
//! failure yields an empty set and the caller degrades to the default-only
//! wrapper.
//!
//! Declared from `loader.rs` via `#[path]` so the lexer sits next to its only
//! integration point without touching `extensions/mod.rs`.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use swc_common::sync::Lrc;
use swc_common::{FileName, SourceMap};
use swc_ecma_ast::{
    ArrowFunctionBody, AssignOp, AssignTarget, CallExpr, Callee, Decl, EsVersion, Expr,
    FunctionBody, Lit, MemberExpr, MemberProp, ModuleItem, ObjectLit, Pat, Program, Prop, PropName,
    PropOrSpread, SimpleAssignTarget, Stmt,
};
use swc_ecma_parser::{lexer::Lexer, EsSyntax, Parser, StringInput, Syntax};

/// `var x = require("<spec>")` bindings of one file, so a deferred re-export
/// helper (`__exportStar(m_main, exports)`, `__createBinding(exports, m_main,
/// "n")`) can be traced back to the module it names.
type RequireBindings = HashMap<String, String>;

/// Which TypeScript re-export helper a call is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Helper {
    /// `__exportStar(source, exports)`: the whole surface of `source`.
    ExportStar,
    /// `__createBinding(exports, source, "name"[, "name2"])`: named bindings.
    CreateBinding,
}

/// Re-export recursion bound (re-export chains, relative or bare).
const MAX_REEXPORT_DEPTH: usize = 5;

/// Named export names of the CJS file at `path`: its own patterns plus the
/// names of its re-export targets (depth <= 5, each file visited once).
/// Unreadable or unparseable input yields an empty set so the caller keeps
/// the default-only bridge.
pub fn named_exports(path: &Path) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut visited = BTreeSet::new();
    collect_file(path, None, 0, &mut visited, &mut names);
    names
}

/// [`named_exports`] for a caller that already holds `path`'s text: the loader
/// reads an ambiguous `.js` once for classification (`module_kind` scans the
/// source for module syntax) and would otherwise read the same file a second
/// time here. The root file is scanned from `source`; re-export targets are
/// still read by path, as they must be. `source` must be the file's content —
/// the scan is identical, only the read moves to the caller.
pub(crate) fn named_exports_with_source(path: &Path, source: String) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut visited = BTreeSet::new();
    collect_file(path, Some(source), 0, &mut visited, &mut names);
    names
}

/// Test-friendly exit: names found in one source text, without following
/// `require` re-exports (no base directory exists for path resolution).
pub fn named_exports_in_source(source: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let _ = collect_source(source.to_string(), &mut names);
    names
}

/// Walk one file into `names`: `source` is its text when the caller already
/// read it, `None` means read it here (re-export targets and every other
/// entry do that).
fn collect_file(
    path: &Path,
    source: Option<String>,
    depth: usize,
    visited: &mut BTreeSet<PathBuf>,
    names: &mut BTreeSet<String>,
) {
    // Depth bound + visit-once keep `module.exports = require(...)` cycles
    // from recursing forever (a -> b -> a terminates on the second entry).
    if depth > MAX_REEXPORT_DEPTH || !visited.insert(path.to_path_buf()) {
        return;
    }
    let source = match source {
        Some(source) => source,
        // An unreadable file yields no names (the caller keeps the
        // default-only bridge), exactly as before.
        None => match std::fs::read_to_string(path) {
            Ok(source) => source,
            Err(_) => return,
        },
    };
    let base = path.parent().unwrap_or_else(|| Path::new(""));
    for specifier in collect_source(source, names) {
        let resolved = if specifier.starts_with("./") || specifier.starts_with("../") {
            // Reuse the npm resolver's candidate order (`./x` -> x.js/ts/index…).
            super::npm_resolve::resolve_path_specifier(base, &specifier).ok()
        } else if specifier.starts_with('/') {
            // Absolute specifiers are not module paths the loader serves.
            None
        } else {
            // Bare specifier (`__exportStar(require("vscode-languageserver-types"),
            // exports)`): walk `node_modules` up from this file with `require`
            // conditions — the same build the CJS runtime will load at run time.
            super::npm_resolve::resolve_bare(
                base,
                &specifier,
                super::npm_resolve::ResolveMode::Require,
            )
            .ok()
            .map(|resolved| resolved.path)
        };
        if let Some(resolved) = resolved {
            collect_file(&resolved, None, depth + 1, visited, names);
        }
    }
}

/// Parse one source text: add its direct export names to `names` and return
/// the specifiers of its re-exports (`module.exports = require("<spec>")`,
/// `__exportStar(require("<spec>"), exports)` and the ident-bound spellings of
/// both). Takes the text by value so the parser can hand it to swc's source
/// map as-is (a `&str` would be copied into a new `String` first).
fn collect_source(source: String, names: &mut BTreeSet<String>) -> Vec<String> {
    let Some(program) = parse(source) else {
        return Vec::new();
    };
    let mut reexports = Vec::new();
    let mut statements: Vec<&Stmt> = Vec::new();
    match &program {
        Program::Module(module) => {
            for item in &module.body {
                if let ModuleItem::Stmt(stmt) = item {
                    statements.push(stmt);
                }
            }
        }
        Program::Script(script) => statements.extend(script.body.iter()),
    }
    let bindings = collect_require_bindings(&statements);
    visit_statements(&statements, names, &mut reexports, &bindings);
    reexports
}

/// Walk a statement list that shares the module's `exports` object: the file's
/// top level, or a UMD factory body (see [`visit_invoked_factories`]).
fn visit_statements(
    statements: &[&Stmt],
    names: &mut BTreeSet<String>,
    reexports: &mut Vec<String>,
    bindings: &RequireBindings,
) {
    for stmt in statements {
        if let Stmt::Expr(expr_stmt) = stmt {
            visit_expr(&expr_stmt.expr, names, reexports, bindings);
        }
    }
}

/// Top-level `var x = require("<literal>")` (also `let`/`const`) bindings: the
/// deferred TS re-export helpers take the module object by name instead of
/// re-requiring it.
fn collect_require_bindings(statements: &[&Stmt]) -> RequireBindings {
    let mut bindings = RequireBindings::new();
    for stmt in statements {
        let Stmt::Decl(Decl::Var(var)) = stmt else {
            continue;
        };
        for declarator in &var.decls {
            let Pat::Ident(binding) = &declarator.name else {
                continue;
            };
            let Some(init) = &declarator.init else {
                continue;
            };
            if let Some(specifier) = require_literal_expr(init) {
                bindings.insert(binding.id.sym.to_string(), specifier);
            }
        }
    }
    bindings
}

fn parse(source: String) -> Option<Program> {
    let cm: Lrc<SourceMap> = Lrc::default();
    let fm = cm.new_source_file(
        Lrc::new(FileName::Custom("cjs-lexer.js".to_string())),
        source,
    );
    let lexer = Lexer::new(
        Syntax::Es(EsSyntax::default()),
        EsVersion::Es2022,
        StringInput::from(&*fm),
        None,
    );
    let mut parser = Parser::new_from(lexer);
    parser.parse_program().ok()
}

/// Statement-level expression forms only; nested (function-body) assignments
/// are deliberately not walked, mirroring the lexer's static-file view — the
/// one exception is the UMD factory body ([`visit_invoked_factories`]), which
/// receives the module's own `exports` object as a parameter.
fn visit_expr(
    expr: &Expr,
    names: &mut BTreeSet<String>,
    reexports: &mut Vec<String>,
    bindings: &RequireBindings,
) {
    match expr {
        Expr::Assign(assign) => {
            if assign.op == AssignOp::Assign {
                handle_target(&assign.left, &assign.right, names, reexports, bindings);
            }
        }
        Expr::Call(call) => {
            handle_define_property(call, names);
            handle_helper_call(call, names, reexports, bindings);
            visit_invoked_factories(call, names, reexports, bindings);
        }
        Expr::Seq(seq) => {
            for item in &seq.exprs {
                visit_expr(item, names, reexports, bindings);
            }
        }
        Expr::Paren(paren) => visit_expr(&paren.expr, names, reexports, bindings),
        _ => {}
    }
}

fn handle_target(
    target: &AssignTarget,
    value: &Expr,
    names: &mut BTreeSet<String>,
    reexports: &mut Vec<String>,
    bindings: &RequireBindings,
) {
    let AssignTarget::Simple(simple) = target else {
        return;
    };
    match simple {
        SimpleAssignTarget::Member(member) => {
            let Some((root, path)) = member_chain_at(member) else {
                return;
            };
            classify_chain_assignment(&root, &path, value, names, reexports, bindings);
        }
        // `exports = ...` aliases the exports object (Node's `module.exports`
        // is the same slot; the common `exports = module.exports = {...}`
        // chain re-enters through the assignment RHS below).
        SimpleAssignTarget::Ident(binding) => {
            if binding.id.sym.as_str() == "exports" {
                handle_exports_object(value, names, reexports, bindings);
            }
        }
        SimpleAssignTarget::Paren(paren) => {
            handle_target_like_expr(&paren.expr, value, names, reexports, bindings)
        }
        _ => {}
    }
}

/// `(expr) = value` shapes: only the exports alias / member chains matter.
fn handle_target_like_expr(
    target: &Expr,
    value: &Expr,
    names: &mut BTreeSet<String>,
    reexports: &mut Vec<String>,
    bindings: &RequireBindings,
) {
    match target {
        Expr::Member(member) => {
            if let Some((root, path)) = member_chain_at(member) {
                classify_chain_assignment(&root, &path, value, names, reexports, bindings);
            }
        }
        Expr::Ident(ident) => {
            if ident.sym.as_str() == "exports" {
                handle_exports_object(value, names, reexports, bindings);
            }
        }
        Expr::Paren(paren) => {
            handle_target_like_expr(&paren.expr, value, names, reexports, bindings)
        }
        _ => {}
    }
}

/// Decide what an assignment to a chain rooted at `exports` / `module`
/// contributes: a named export (tail target) or the exports-object
/// replacement (`module.exports` exactly).
fn classify_chain_assignment(
    root: &str,
    path: &[String],
    value: &Expr,
    names: &mut BTreeSet<String>,
    reexports: &mut Vec<String>,
    bindings: &RequireBindings,
) {
    match root {
        "exports" if !path.is_empty() => {
            if let Some(tail) = path.last() {
                add_name(tail, names);
            }
            // `exports.a = exports.b = … = void 0`: the TS compiler and UMD
            // bundlers declare the whole surface in one chained assignment, so
            // every further `exports.*` link on the right-hand side is an
            // export too.
            if matches!(value, Expr::Assign(_)) {
                visit_expr(value, names, reexports, bindings);
            }
        }
        "module" if path.first().map(String::as_str) == Some("exports") => {
            if path.len() == 1 {
                handle_exports_object(value, names, reexports, bindings);
            } else if let Some(tail) = path.last() {
                add_name(tail, names);
                if matches!(value, Expr::Assign(_)) {
                    visit_expr(value, names, reexports, bindings);
                }
            }
        }
        _ => {}
    }
}

/// `module.exports = value` / `exports = value`: an object literal contributes
/// its keys, a `require("<lit>")` is a re-export edge, and a chained
/// assignment recurses (`exports = module.exports = ...`).
fn handle_exports_object(
    value: &Expr,
    names: &mut BTreeSet<String>,
    reexports: &mut Vec<String>,
    bindings: &RequireBindings,
) {
    match value {
        Expr::Object(object) => collect_object_keys(object, names),
        Expr::Assign(nested) if nested.op == AssignOp::Assign => {
            handle_target(&nested.left, &nested.right, names, reexports, bindings);
        }
        Expr::Paren(paren) => handle_exports_object(&paren.expr, names, reexports, bindings),
        Expr::Call(call) => {
            if let Some(specifier) = require_literal(call) {
                reexports.push(specifier);
            }
        }
        // `var m = require("./m"); module.exports = m;` — the same edge told
        // through a binding.
        Expr::Ident(ident) => {
            if let Some(specifier) = bindings.get(ident.sym.as_str()) {
                reexports.push(specifier.clone());
            }
        }
        _ => {}
    }
}

/// The TypeScript compiler's deferred re-export helpers, which put names on
/// `exports` without ever writing `exports.<name> = …`:
///
/// - `__exportStar(require("<spec>"), exports)` and `__exportStar(m, exports)`
///   with `var m = require("<spec>")` — the whole surface of `<spec>`;
/// - `__createBinding(exports, m, "<name>"[, "<name2>"])` — one named binding,
///   where a fourth argument is the exported alias.
///
/// The helper *definitions* (`var __exportStar = function (m, exports) { for
/// (var p in m) … __createBinding(exports, m, p); }`, inline or from tslib)
/// never match: they sit in `var` statements the walk does not visit, and even
/// reached they pass a dynamic `p`, which the string-literal requirement below
/// rejects — so no helper-internal name can leak in.
///
/// A source expression that does not trace back to a static
/// `require("<literal>")` contributes nothing: names must be provably the
/// module's own, and a wrong name would be a link error on the ESM side.
fn handle_helper_call(
    call: &CallExpr,
    names: &mut BTreeSet<String>,
    reexports: &mut Vec<String>,
    bindings: &RequireBindings,
) {
    let Some(helper) = helper_name(&call.callee) else {
        return;
    };
    if call.args.iter().any(|arg| arg.spread.is_some()) {
        return;
    }
    match helper {
        Helper::ExportStar => {
            if call.args.len() != 2 || !is_exports_object(&call.args[1].expr) {
                return;
            }
            if let Some(specifier) = require_specifier(&call.args[0].expr, bindings) {
                reexports.push(specifier);
            }
        }
        Helper::CreateBinding => {
            if call.args.len() < 3 || !is_exports_object(&call.args[0].expr) {
                return;
            }
            // tslib's signature is `(o, m, k, k2)`: `k2` (when a string) is the
            // name the binding lands under, otherwise it defaults to `k`.
            let alias = call
                .args
                .get(3)
                .and_then(string_arg)
                .or_else(|| call.args.get(2).and_then(string_arg));
            if let Some(name) = alias {
                add_name(name, names);
            }
        }
    }
}

/// The helper a call target names — bare (`__exportStar`), tslib-namespaced
/// (`tslib_1.__exportStar`) or reached through a comma expression
/// (`(0, tslib_1.__exportStar)(…)`).
fn helper_name(callee: &Callee) -> Option<Helper> {
    let Callee::Expr(expr) = callee else {
        return None;
    };
    helper_name_in_expr(expr)
}

fn helper_name_in_expr(expr: &Expr) -> Option<Helper> {
    match expr {
        Expr::Ident(ident) => helper_for(ident.sym.as_str()),
        Expr::Member(member) => member_prop_name(&member.prop)
            .as_deref()
            .and_then(helper_for),
        Expr::Seq(seq) => seq.exprs.last().and_then(|expr| helper_name_in_expr(expr)),
        Expr::Paren(paren) => helper_name_in_expr(&paren.expr),
        _ => None,
    }
}

fn helper_for(name: &str) -> Option<Helper> {
    match name {
        "__exportStar" => Some(Helper::ExportStar),
        "__createBinding" => Some(Helper::CreateBinding),
        _ => None,
    }
}

/// The module specifier behind a re-export source expression:
/// `require("<lit>")`, or an identifier bound to one in this file.
fn require_specifier(expr: &Expr, bindings: &RequireBindings) -> Option<String> {
    match expr {
        Expr::Call(_) => require_literal_expr(expr),
        Expr::Ident(ident) => bindings.get(ident.sym.as_str()).cloned(),
        Expr::Paren(paren) => require_specifier(&paren.expr, bindings),
        _ => None,
    }
}

/// Whether an expression is the module's `exports` object (`exports` or
/// `module.exports`) — the target a re-export helper must write to.
fn is_exports_object(expr: &Expr) -> bool {
    match expr {
        Expr::Ident(ident) => ident.sym.as_str() == "exports",
        Expr::Member(member) => member_chain_at(member)
            .is_some_and(|(root, path)| root == "module" && path == ["exports"]),
        Expr::Paren(paren) => is_exports_object(&paren.expr),
        _ => false,
    }
}

fn string_arg(arg: &swc_ecma_ast::ExprOrSpread) -> Option<&str> {
    match &*arg.expr {
        Expr::Lit(Lit::Str(str_lit)) => str_lit.value.as_str(),
        _ => None,
    }
}

/// UMD wrappers hand the module's `exports` object to a factory function as a
/// parameter:
///
/// ```js
/// (function (factory) { … var v = factory(require, exports); … })(
///   function (require, exports) { exports.DiagnosticSeverity = …; });
/// ```
///
/// The factory's body shares the module's `exports` object, so it is walked
/// with the same statement visitor (nested `exports.*` assignments there are
/// real exports, unlike the ones in an ordinary function body). Both the
/// immediately-invoked-callee shape and the `exports` parameter are required,
/// so the usual callback spellings (`arr.forEach(function (exports) { … })`,
/// `define(["require", "exports"], factory)`) are not mistaken for a factory;
/// only a UMD wrapper's own call shape reaches the walk.
fn visit_invoked_factories(
    call: &CallExpr,
    names: &mut BTreeSet<String>,
    reexports: &mut Vec<String>,
    bindings: &RequireBindings,
) {
    let Callee::Expr(callee) = &call.callee else {
        return;
    };
    if !matches!(unwrap_paren(callee), Expr::Fn(_) | Expr::Arrow(_)) {
        return;
    }
    for arg in &call.args {
        if arg.spread.is_some() {
            continue;
        }
        let Some((params, body)) = function_scope(&arg.expr) else {
            continue;
        };
        if !is_umd_factory(&params) {
            continue;
        }
        let statements: Vec<&Stmt> = body.stmts.iter().collect();
        // The factory body may require modules of its own; outer bindings stay
        // visible (the factory is compiled into the same module scope).
        let mut factory_bindings = bindings.clone();
        factory_bindings.extend(collect_require_bindings(&statements));
        visit_statements(&statements, names, reexports, &factory_bindings);
    }
}

/// The parameter list + body of a function expression (nothing for an arrow
/// with an expression body, which has no statements).
fn function_scope(expr: &Expr) -> Option<(Vec<&Pat>, &FunctionBody)> {
    match unwrap_paren(expr) {
        Expr::Fn(fn_expr) => {
            let function = &*fn_expr.function;
            let body = function.body.as_ref()?;
            Some((
                function.params.iter().map(|param| &param.pat).collect(),
                body,
            ))
        }
        Expr::Arrow(arrow) => match &*arrow.body {
            ArrowFunctionBody::FunctionBody(body) => Some((arrow.params.iter().collect(), body)),
            ArrowFunctionBody::Expr(_) => None,
        },
        _ => None,
    }
}

/// `(require, exports)` (TypeScript/webpack UMD) or `(exports)` (Rollup UMD).
fn is_umd_factory(params: &[&Pat]) -> bool {
    param_named(params, 0, "exports")
        || (param_named(params, 0, "require") && param_named(params, 1, "exports"))
}

fn param_named(params: &[&Pat], index: usize, name: &str) -> bool {
    matches!(params.get(index), Some(Pat::Ident(binding)) if binding.id.sym.as_str() == name)
}

fn unwrap_paren(mut expr: &Expr) -> &Expr {
    while let Expr::Paren(paren) = expr {
        expr = &paren.expr;
    }
    expr
}

/// `Object.defineProperty(exports | module.exports, "name", ...)`.
fn handle_define_property(call: &CallExpr, names: &mut BTreeSet<String>) {
    let Callee::Expr(callee) = &call.callee else {
        return;
    };
    let Expr::Member(member) = &**callee else {
        return;
    };
    if member_prop_name(&member.prop).as_deref() != Some("defineProperty") {
        return;
    }
    let Expr::Ident(object_ident) = &*member.obj else {
        return;
    };
    if object_ident.sym.as_str() != "Object" {
        return;
    }
    if call.args.len() < 2 || call.args[0].spread.is_some() || call.args[1].spread.is_some() {
        return;
    }
    let target_is_exports = match &*call.args[0].expr {
        Expr::Ident(ident) => ident.sym.as_str() == "exports",
        Expr::Member(exports_member) => member_chain_at(exports_member)
            .is_some_and(|(root, path)| root == "module" && path == ["exports"]),
        _ => false,
    };
    if !target_is_exports {
        return;
    }
    if let Expr::Lit(Lit::Str(str_lit)) = &*call.args[1].expr {
        if let Some(name) = str_lit.value.as_str() {
            add_name(name, names);
        }
    }
}

fn collect_object_keys(object: &ObjectLit, names: &mut BTreeSet<String>) {
    for prop_or_spread in &object.props {
        // Spread elements are skipped: their keys are not statically known.
        let PropOrSpread::Prop(prop) = prop_or_spread else {
            continue;
        };
        let key = match &**prop {
            Prop::Shorthand(ident) => Some(ident.sym.to_string()),
            Prop::KeyValue(kv) => prop_name_string(&kv.key),
            Prop::Method(method) => prop_name_string(&method.key),
            Prop::Getter(getter) => prop_name_string(&getter.key),
            Prop::Setter(setter) => prop_name_string(&setter.key),
            Prop::Assign(_) => None,
        };
        if let Some(key) = key {
            add_name(&key, names);
        }
    }
}

fn prop_name_string(name: &PropName) -> Option<String> {
    match name {
        PropName::Ident(ident) => Some(ident.sym.to_string()),
        PropName::Str(str_lit) => str_lit.value.as_str().map(str::to_owned),
        // Computed / numeric / bigint keys are not statically resolvable here.
        _ => None,
    }
}

/// `require("<literal>")` with exactly one string argument.
fn require_literal(call: &CallExpr) -> Option<String> {
    let Callee::Expr(callee) = &call.callee else {
        return None;
    };
    let Expr::Ident(ident) = &**callee else {
        return None;
    };
    if ident.sym.as_str() != "require" || call.args.len() != 1 || call.args[0].spread.is_some() {
        return None;
    }
    match &*call.args[0].expr {
        Expr::Lit(Lit::Str(str_lit)) => str_lit.value.as_str().map(str::to_owned),
        _ => None,
    }
}

/// [`require_literal`] over an expression (through parentheses).
fn require_literal_expr(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Call(call) => require_literal(call),
        Expr::Paren(paren) => require_literal_expr(&paren.expr),
        _ => None,
    }
}

fn member_prop_name(prop: &MemberProp) -> Option<String> {
    match prop {
        MemberProp::Ident(ident) => Some(ident.sym.to_string()),
        // `exports["name"]` — string-literal computed keys are statically known.
        MemberProp::Computed(computed) => match &*computed.expr {
            Expr::Lit(Lit::Str(str_lit)) => str_lit.value.as_str().map(str::to_owned),
            _ => None,
        },
        MemberProp::PrivateName(_) => None,
    }
}

/// Root identifier + property path of a member chain (`module.exports.foo` ->
/// `("module", ["exports", "foo"])`). `None` when any object link is not an
/// identifier or a property is not statically known.
fn member_chain(expr: &Expr) -> Option<(String, Vec<String>)> {
    match expr {
        Expr::Member(member) => member_chain_at(member),
        Expr::Ident(ident) => Some((ident.sym.to_string(), Vec::new())),
        Expr::Paren(paren) => member_chain(&paren.expr),
        _ => None,
    }
}

fn member_chain_at(member: &MemberExpr) -> Option<(String, Vec<String>)> {
    let tail = member_prop_name(&member.prop)?;
    let (root, mut path) = member_chain(&member.obj)?;
    path.push(tail);
    Some((root, path))
}

/// Names must be importable bindings: `export const <name> = ...` only links
/// for a valid non-reserved identifier, and `default` is taken by
/// `module.exports`. `__esModule` is the interop marker TS/Babel put on the
/// exports object (`Object.defineProperty(exports, "__esModule", …)`) — a flag,
/// not a value, so it is not re-exported as a binding.
fn add_name(name: &str, names: &mut BTreeSet<String>) {
    if name != "default"
        && name != "__esModule"
        && is_valid_identifier(name)
        && !is_reserved_word(name)
    {
        names.insert(name.to_string());
    }
}

/// ECMAScript reserved words plus the strict-mode-only ones (`let`, `yield`,
/// `await`, `static`, …) and `eval`/`arguments`: a `export const <name>` line
/// for any of these is a syntax error, so such names are skipped like the
/// cjs-module-lexer does.
const RESERVED_WORDS: &[&str] = &[
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "default",
    "delete",
    "do",
    "else",
    "enum",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "function",
    "if",
    "import",
    "in",
    "instanceof",
    "new",
    "null",
    "return",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "typeof",
    "var",
    "void",
    "while",
    "with",
    "let",
    "static",
    "yield",
    "await",
    "implements",
    "interface",
    "package",
    "private",
    "protected",
    "public",
    "eval",
    "arguments",
];

fn is_reserved_word(name: &str) -> bool {
    RESERVED_WORDS.contains(&name)
}

/// Validity per the ECMAScript identifier grammar, approximated without the
/// full Unicode ID tables: letter/underscore/dollar start, alphanumeric
/// continuation (ASCII exactly; non-ASCII via `char::is_alphanumeric`).
fn is_valid_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    let start_ok = first.is_alphabetic() || matches!(first, '_' | '$');
    start_ok && chars.all(|c| c.is_alphanumeric() || matches!(c, '_' | '$'))
}
