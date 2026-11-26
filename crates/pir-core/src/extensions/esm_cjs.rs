//! ESM → CommonJS source transform backing `require(esm)` (Node 22+
//! semantics, unflagged since 22.12): a `require()` of an ES module must run
//! it synchronously, which the loader's native async ESM pipeline cannot do
//! from inside a CJS body. The transform rewrites the module into a CJS body
//! that the existing `__pirCjsLoad` wrapper evaluates, so `require()` of ESM
//! and CJS shares one cache and one error surface.
//!
//! Scope (documented minimal tier, mirrors the tree's "minimal real" policy):
//! - static `import` declarations (default / namespace / named / bare) are
//!   hoisted into `require` bindings with Node's interop rules (default of a
//!   CJS target is `module.exports`; named exports of a CJS target come from
//!   own keys);
//! - `export default`, `export <decl>`, `export { .. }`, `export { .. } from`,
//!   `export * as ns from` and `export * from` are rewritten onto `exports`;
//! - top-level `await` fails with Node's `ERR_REQUIRE_ASYNC_MODULE` (a CJS
//!   body cannot host it);
//! - live bindings degrade to copy-assignments at the export site, and
//!   `import.meta` is not available (runtime error on use). These follow
//!   from synchronous CJS evaluation and are accepted, fail-loud gaps.
//!
//! The emitted module is marked `__pirEsmNamespace` (non-enumerable, set by
//! the JS loader) so downstream `require()` calls apply Node's
//! default-unwrap rule (a namespace holding only `default` is returned as
//! the default value itself).

use swc_common::sync::Lrc;
use swc_common::{FileName, SourceMap, DUMMY_SP};
use swc_ecma_ast::{EsVersion, Module, ModuleItem, Program};
use swc_ecma_codegen::text_writer::JsWriter;
use swc_ecma_codegen::Emitter;
use swc_ecma_parser::{lexer::Lexer, EsSyntax, Parser, StringInput, Syntax};

/// A transform failure carrying the Node error code the require path must
/// surface (`ERR_REQUIRE_ASYNC_MODULE` for TLA, `ERR_INVALID_SYNTAX` for
/// parse/codegen failures).
pub struct EsmCjsError {
    pub code: &'static str,
    pub message: String,
}

fn err(code: &'static str, message: impl Into<String>) -> EsmCjsError {
    EsmCjsError {
        code,
        message: message.into(),
    }
}

/// Transform an ES module source (already TS-stripped by the caller when it
/// came from a `.ts` path) into a CommonJS body. A source without any module
/// declaration parses as Script and is returned unchanged — it is already
/// CJS-compatible.
pub fn esm_to_cjs(name: &str, source: &str) -> Result<String, EsmCjsError> {
    let cm: Lrc<SourceMap> = Lrc::default();
    let fm = cm.new_source_file(
        Lrc::new(FileName::Custom(name.to_string())),
        source.to_string(),
    );

    let lexer = Lexer::new(
        Syntax::Es(EsSyntax::default()),
        EsVersion::Es2022,
        StringInput::from(&*fm),
        None,
    );
    let mut parser = Parser::new_from(lexer);
    let program = parser.parse_program().map_err(|e| {
        err(
            "ERR_INVALID_SYNTAX",
            format!("parse error in {name}: {e:?}"),
        )
    })?;

    let module = match program {
        Program::Module(module) => module,
        // No import/export anywhere: a plain script runs as-is under the CJS
        // wrapper.
        Program::Script(_) => return Ok(source.to_string()),
    };

    // Top-level await can never execute under a synchronous require: fail
    // with Node's code instead of emitting a body that dies on `await`.
    for item in &module.body {
        let has_tla = match item {
            ModuleItem::Stmt(stmt) => crate::extensions::transpile::stmt_has_tla(stmt),
            ModuleItem::ModuleDecl(swc_ecma_ast::ModuleDecl::ExportDefaultExpr(default_expr)) => {
                crate::extensions::transpile::expr_has_tla(&default_expr.expr)
            }
            ModuleItem::ModuleDecl(swc_ecma_ast::ModuleDecl::ExportDecl(export_decl)) => {
                matches!(&export_decl.decl, swc_ecma_ast::Decl::Var(var) if var.decls.iter().any(|d| d.init.as_deref().is_some_and(crate::extensions::transpile::expr_has_tla)))
            }
            _ => false,
        };
        if has_tla {
            return Err(err(
                "ERR_REQUIRE_ASYNC_MODULE",
                format!("require() of an ES module with top-level await: {name}"),
            ));
        }
    }

    let mut out = String::new();
    out.push_str(HELPERS);
    // Deferred export assignments (`export { .. }` clauses) go to the end of
    // the body so they cannot hit the TDZ of a `const` declared later.
    let mut deferred = String::new();
    let mut import_seq = 0usize;

    for item in &module.body {
        match item {
            ModuleItem::ModuleDecl(swc_ecma_ast::ModuleDecl::Import(decl)) => {
                import_seq += 1;
                emit_import(&mut out, decl, import_seq)?;
            }
            ModuleItem::ModuleDecl(swc_ecma_ast::ModuleDecl::ExportAll(decl)) => {
                import_seq += 1;
                let ns = format!("__pirEsmRe{import_seq}");
                let key = format!("__pirEsmK{import_seq}");
                let spec = specifier_string(&decl.src);
                out.push_str(&format!(
                    "const {ns} = __pirInteropNamespace(require({spec}));\n\
                     for (const {key} of Object.keys({ns})) {{\n\
                     \x20 if ({key} !== \"default\") exports[{key}] = {ns}[{key}];\n}}\n"
                ));
            }
            ModuleItem::ModuleDecl(swc_ecma_ast::ModuleDecl::ExportNamed(decl)) => {
                if let Some(src) = &decl.src {
                    import_seq += 1;
                    let ns = format!("__pirEsmRe{import_seq}");
                    let spec = specifier_string(src);
                    out.push_str(&format!(
                        "const {ns} = __pirInteropNamespace(require({spec}));\n"
                    ));
                    for specifier in &decl.specifiers {
                        emit_named_reexport(&mut deferred, specifier, &ns);
                    }
                } else {
                    for specifier in &decl.specifiers {
                        emit_named_export(&mut deferred, specifier);
                    }
                }
            }
            ModuleItem::ModuleDecl(swc_ecma_ast::ModuleDecl::ExportDecl(decl)) => {
                emit_export_decl(&mut out, &decl.decl)?;
            }
            ModuleItem::ModuleDecl(swc_ecma_ast::ModuleDecl::ExportDefaultDecl(decl)) => {
                emit_default_decl(&mut out, decl)?;
            }
            ModuleItem::ModuleDecl(swc_ecma_ast::ModuleDecl::ExportDefaultExpr(decl)) => {
                let expr = emit_expr(&decl.expr)?;
                out.push_str(&format!("exports.default = {expr};\n"));
            }
            // TS-only module constructs cannot survive the caller's TS strip;
            // drop them defensively instead of mis-emitting.
            ModuleItem::ModuleDecl(
                swc_ecma_ast::ModuleDecl::TsImportEquals(_)
                | swc_ecma_ast::ModuleDecl::TsExportAssignment(_)
                | swc_ecma_ast::ModuleDecl::TsNamespaceExport(_),
            ) => {}
            ModuleItem::Stmt(stmt) => {
                out.push_str(&emit_stmt(stmt)?);
            }
        }
    }

    out.push_str(&deferred);
    Ok(out)
}

/// Interop helpers prepended to every transformed module: Node's
/// CJS↔ESM interop rules at the require boundary. `__pirEsmNamespace` is the
/// marker the JS loader sets on transformed-module exports.
const HELPERS: &str = "const __pirEsmIsNs = (m) => m !== null && (typeof m === \"object\" || typeof m === \"function\") && m.__pirEsmNamespace === true;\n\
const __pirInteropDefault = (m) => (__pirEsmIsNs(m) ? m.default : m);\n\
const __pirInteropNamespace = (m) => {\n\
\x20 if (__pirEsmIsNs(m)) return m;\n\
\x20 const ns = {};\n\
\x20 for (const k of Object.keys(m)) ns[k] = m[k];\n\
\x20 ns.default = m;\n\
\x20 return ns;\n\
};\n";

fn emit_import(
    out: &mut String,
    decl: &swc_ecma_ast::ImportDecl,
    seq: usize,
) -> Result<(), EsmCjsError> {
    use swc_ecma_ast::ImportSpecifier;

    let ns = format!("__pirEsmI{seq}");
    let spec = specifier_string(&decl.src);
    // `import {} from "x"` and `import "x"` are side-effect requires.
    if decl.specifiers.is_empty() {
        out.push_str(&format!("require({spec});\n"));
        return Ok(());
    }
    out.push_str(&format!(
        "const {ns} = __pirInteropNamespace(require({spec}));\n"
    ));
    let mut named: Vec<String> = Vec::new();
    for specifier in &decl.specifiers {
        match specifier {
            ImportSpecifier::Named(named_spec) => {
                if named_spec.is_type_only {
                    continue;
                }
                let local = named_spec.local.sym.as_str();
                match &named_spec.imported {
                    Some(swc_ecma_ast::ModuleExportName::Ident(imported)) => {
                        named.push(format!("{}: {local}", quote_key(imported.sym.as_str())));
                    }
                    Some(swc_ecma_ast::ModuleExportName::Str(imported)) => {
                        named.push(format!(
                            "{}: {local}",
                            quote_key(&imported.value.to_string_lossy())
                        ));
                    }
                    // Shorthand: a string literal alone is not a valid
                    // destructuring target (`{ "a" } = x` is a syntax error);
                    // the local is always a plain identifier binding name.
                    None => named.push(local.to_string()),
                }
            }
            ImportSpecifier::Default(default_spec) => {
                let local = default_spec.local.sym.as_str();
                out.push_str(&format!("const {local} = {ns}.default;\n"));
            }
            ImportSpecifier::Namespace(namespace_spec) => {
                let local = namespace_spec.local.sym.as_str();
                out.push_str(&format!("const {local} = {ns};\n"));
            }
        }
    }
    if !named.is_empty() {
        out.push_str(&format!("const {{ {} }} = {ns};\n", named.join(", ")));
    }
    Ok(())
}

fn emit_named_reexport(deferred: &mut String, specifier: &swc_ecma_ast::ExportSpecifier, ns: &str) {
    use swc_ecma_ast::{ExportSpecifier, ModuleExportName};
    if let ExportSpecifier::Named(named_spec) = specifier {
        let source_key = match &named_spec.orig {
            ModuleExportName::Ident(ident) => quote_key(ident.sym.as_str()),
            ModuleExportName::Str(text) => quote_key(&text.value.to_string_lossy()),
        };
        let exported = match &named_spec.exported {
            Some(ModuleExportName::Ident(ident)) => quote_key(ident.sym.as_str()),
            Some(ModuleExportName::Str(text)) => quote_key(&text.value.to_string_lossy()),
            None => source_key.clone(),
        };
        deferred.push_str(&format!("exports[{exported}] = {ns}[{source_key}];\n"));
    }
    // `export default from "x"` (legacy) and namespace specifiers cannot
    // appear without `src`; with `src` the namespace form is handled by the
    // caller below.
}

fn emit_named_export(deferred: &mut String, specifier: &swc_ecma_ast::ExportSpecifier) {
    use swc_ecma_ast::{ExportSpecifier, ModuleExportName};
    if let ExportSpecifier::Named(named_spec) = specifier {
        let ModuleExportName::Ident(orig) = &named_spec.orig else {
            // `export { "str" }` without a source is not valid ES.
            return;
        };
        let local = orig.sym.as_str();
        let exported = match &named_spec.exported {
            Some(ModuleExportName::Ident(ident)) => quote_key(ident.sym.as_str()),
            Some(ModuleExportName::Str(text)) => quote_key(&text.value.to_string_lossy()),
            None => quote_key(local),
        };
        deferred.push_str(&format!("exports[{exported}] = {local};\n"));
    }
}

fn emit_export_decl(out: &mut String, decl: &swc_ecma_ast::Decl) -> Result<(), EsmCjsError> {
    use swc_ecma_ast::{Decl, Pat};
    match decl {
        Decl::Var(var) => {
            out.push_str(&emit_item(ModuleItem::Stmt(swc_ecma_ast::Stmt::Decl(
                Decl::Var(var.clone()),
            )))?);
            for declarator in &var.decls {
                let mut names = Vec::new();
                pat_binding_names(&declarator.name, &mut names);
                for name in names {
                    out.push_str(&format!("exports.{name} = {name};\n"));
                }
            }
        }
        Decl::Fn(fn_decl) => {
            let name = fn_decl.ident.sym.as_str().to_string();
            out.push_str(&emit_item(ModuleItem::Stmt(swc_ecma_ast::Stmt::Decl(
                Decl::Fn(fn_decl.clone()),
            )))?);
            out.push_str(&format!("exports.{name} = {name};\n"));
        }
        Decl::Class(class_decl) => {
            let name = class_decl.ident.sym.as_str().to_string();
            out.push_str(&emit_item(ModuleItem::Stmt(swc_ecma_ast::Stmt::Decl(
                Decl::Class(class_decl.clone()),
            )))?);
            out.push_str(&format!("exports.{name} = {name};\n"));
        }
        // `export using ...` and TS-only declarations: TS constructs cannot
        // survive the TS strip; `using` has no export form in stable ES.
        Decl::Using(using_decl) => {
            out.push_str(&emit_item(ModuleItem::Stmt(swc_ecma_ast::Stmt::Decl(
                Decl::Using(using_decl.clone()),
            )))?);
            for declarator in &using_decl.decls {
                if let Pat::Ident(binding) = &declarator.name {
                    let name = binding.id.sym.as_str();
                    out.push_str(&format!("exports.{name} = {name};\n"));
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn emit_default_decl(
    out: &mut String,
    decl: &swc_ecma_ast::ExportDefaultDecl,
) -> Result<(), EsmCjsError> {
    use swc_ecma_ast::{Decl, DefaultDecl};
    match &decl.decl {
        DefaultDecl::Fn(fn_expr) => match &fn_expr.ident {
            Some(ident) => {
                let name = ident.sym.as_str().to_string();
                out.push_str(&emit_item(ModuleItem::Stmt(swc_ecma_ast::Stmt::Decl(
                    Decl::Fn(swc_ecma_ast::FnDecl {
                        declare: false,
                        ident: ident.clone(),
                        function: fn_expr.function.clone(),
                    }),
                )))?);
                out.push_str(&format!("exports.default = {name};\n"));
            }
            None => {
                let emitted = emit_item(ModuleItem::Stmt(swc_ecma_ast::Stmt::Expr(
                    swc_ecma_ast::ExprStmt {
                        span: DUMMY_SP,
                        expr: Box::new(swc_ecma_ast::Expr::Fn(fn_expr.clone())),
                    },
                )))?;
                out.push_str(&format!(
                    "exports.default = {};\n",
                    strip_semicolon(&emitted)
                ));
            }
        },
        DefaultDecl::Class(class_expr) => match &class_expr.ident {
            Some(ident) => {
                let name = ident.sym.as_str().to_string();
                out.push_str(&emit_item(ModuleItem::Stmt(swc_ecma_ast::Stmt::Decl(
                    Decl::Class(swc_ecma_ast::ClassDecl {
                        declare: false,
                        ident: ident.clone(),
                        class: class_expr.class.clone(),
                    }),
                )))?);
                out.push_str(&format!("exports.default = {name};\n"));
            }
            None => {
                let emitted = emit_item(ModuleItem::Stmt(swc_ecma_ast::Stmt::Expr(
                    swc_ecma_ast::ExprStmt {
                        span: DUMMY_SP,
                        expr: Box::new(swc_ecma_ast::Expr::Class(class_expr.clone())),
                    },
                )))?;
                out.push_str(&format!(
                    "exports.default = {};\n",
                    strip_semicolon(&emitted)
                ));
            }
        },
        DefaultDecl::TsInterfaceDecl(_) => {}
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// emit plumbing: each kept piece is emitted through a one-item synthetic
// module so the codegen scaffolding stays identical to `transpile.rs`.
// ---------------------------------------------------------------------------

fn emit_item(item: ModuleItem) -> Result<String, EsmCjsError> {
    let cm: Lrc<SourceMap> = Lrc::default();
    let mut buf: Vec<u8> = Vec::new();
    let program = Program::Module(Module {
        span: DUMMY_SP,
        body: vec![item],
        shebang: None,
    });
    {
        let mut emitter = Emitter {
            cfg: swc_ecma_codegen::Config::default().with_target(EsVersion::Es2022),
            cm: cm.clone(),
            comments: None,
            wr: JsWriter::new(cm.clone(), "\n", &mut buf, None),
        };
        emitter
            .emit_program(&program)
            .map_err(|e| err("ERR_INVALID_SYNTAX", format!("codegen error: {e}")))?;
    }
    String::from_utf8(buf)
        .map_err(|e| err("ERR_INVALID_SYNTAX", format!("output is not utf-8: {e}")))
}

fn emit_stmt(stmt: &swc_ecma_ast::Stmt) -> Result<String, EsmCjsError> {
    emit_item(ModuleItem::Stmt(stmt.clone()))
}

fn emit_expr(expr: &swc_ecma_ast::Expr) -> Result<String, EsmCjsError> {
    let emitted = emit_item(ModuleItem::Stmt(swc_ecma_ast::Stmt::Expr(
        swc_ecma_ast::ExprStmt {
            span: DUMMY_SP,
            expr: Box::new(expr.clone()),
        },
    )))?;
    Ok(strip_semicolon(&emitted).to_string())
}

fn strip_semicolon(emitted: &str) -> &str {
    emitted.trim_end().trim_end_matches(';').trim_end()
}

/// JSON-quote a module specifier / export name for embedding in emitted
/// source, matching the codegen target's string syntax.
fn specifier_string(spec: &swc_ecma_ast::Str) -> String {
    let raw = spec.value.to_string_lossy();
    match serde_json::to_string(&*raw) {
        Ok(quoted) => quoted,
        Err(_) => format!("\"{}\"", raw.replace('"', "\\\"")),
    }
}

/// Quote a property name for the computed member accesses this emitter uses
/// (`exports["x-y"]`, `ns["x-y"]`, destructuring keys). ALWAYS quoted: a bare
/// identifier inside brackets would be evaluated as an expression — the
/// original "leave plain names unquoted" nicety produced
/// `exports[parse] = parse` (key = `String(parse)`), which shipped a
/// babel-parser-shaped namespace whose keys were function sources.
fn quote_key(name: &str) -> String {
    match serde_json::to_string(name) {
        Ok(quoted) => quoted,
        Err(_) => format!("\"{}\"", name.replace('"', "\\\"")),
    }
}

/// Collect the identifiers bound by a declaration pattern (same traversal as
/// `transpile.rs`'s `pat_binding_names`; unextractable patterns contribute no
/// export name rather than failing the module).
fn pat_binding_names(pat: &swc_ecma_ast::Pat, out: &mut Vec<String>) {
    use swc_ecma_ast::Pat;
    match pat {
        Pat::Ident(binding) => out.push(binding.id.sym.as_str().to_string()),
        Pat::Array(array) => {
            for elem in array.elems.iter().flatten() {
                pat_binding_names(elem, out);
            }
        }
        Pat::Object(object) => {
            for prop in &object.props {
                match prop {
                    swc_ecma_ast::ObjectPatProp::KeyValue(kv) => pat_binding_names(&kv.value, out),
                    swc_ecma_ast::ObjectPatProp::Assign(assign) => {
                        out.push(assign.key.id.sym.as_str().to_string())
                    }
                    swc_ecma_ast::ObjectPatProp::Rest(rest) => pat_binding_names(&rest.arg, out),
                }
            }
        }
        Pat::Rest(rest) => pat_binding_names(&rest.arg, out),
        Pat::Assign(assign) => pat_binding_names(&assign.left, out),
        Pat::Invalid(_) | Pat::Expr(_) => {}
    }
}
