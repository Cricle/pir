//! TypeScript → JavaScript transpilation for extension sources.
//!
//! Extensions are authored in TypeScript (optionally with type-only imports of
//! `@earendil-works/pi-coding-agent`) and executed inside QuickJS. QuickJS
//! understands modern ESM natively, so the pipeline is deliberately minimal:
//! parse with swc (TypeScript syntax), strip types with the TypeScript
//! transform, and print back to JS. No bundling, no downleveling below ES2022.

use swc_common::sync::Lrc;
use swc_common::{FileName, Globals, Mark, SourceMap, DUMMY_SP, GLOBALS};
use swc_ecma_ast::{EsVersion, Program};
use swc_ecma_codegen::text_writer::JsWriter;
use swc_ecma_codegen::Emitter;
use swc_ecma_parser::{lexer::Lexer, EsSyntax, Parser, StringInput, Syntax, TsSyntax};
use swc_ecma_transforms_base::fixer::fixer;
use swc_ecma_transforms_base::hygiene::hygiene;
use swc_ecma_transforms_base::resolver;
use swc_ecma_transforms_typescript::typescript;

/// Transpile a TypeScript (or plain JS) module source into QuickJS-executable
/// JavaScript. Type-only imports are removed, everything else (including
/// `import`/`export` statements) is preserved as ESM.
///
/// Returns the emitted JavaScript or a human-readable error string on parse
/// failure.
pub fn transpile(name: &str, source: &str) -> Result<String, String> {
    // A UTF-8 BOM is not part of the module text. SWC trims it while parsing,
    // but the verbatim paths below return the input unchanged, and a BOM before
    // `#!` would also hide the shebang from the extension/shebang checks.
    let source = strip_bom(source);
    // Plain JavaScript needs no transform: hand the source back untouched so
    // the engine evaluates the file's own text. The swc reprint drops comments
    // and renumbers lines, which desynchronizes every runtime stack position
    // from the on-disk file (and breaks source-text consumers like assert's
    // generated messages).
    //
    // A `.js/.mjs/.cjs` file is plain JavaScript by definition (Node would
    // also reject TypeScript syntax there), so the extension decides WITHOUT
    // parsing; TypeScript syntax in such a file reaches the engine verbatim
    // and fails there (Node parity, pinned by the extension/ecosystem tests),
    // never silently stripped. Every other spelling takes the content probe:
    // plain JS is returned verbatim, while TS-carrying files (or syntax the JS
    // parse rejects) take the parse/print path. The old probe ran a full SWC
    // parse of every module just to conclude "plain JS"; on the real
    // 7-package set that transient AST is the load-time memory high-water mark
    // and the allocator never returns it (measured ~28 MB peak for the largest
    // package). A shebang still takes the transpile path (SWC re-emits it, and
    // QuickJS tolerates a leading shebang in modules).
    if is_plain_js_module(name) && !source.starts_with("#!") {
        return Ok(source.to_string());
    }
    if is_plain_js(source) {
        return Ok(source.to_string());
    }
    transpile_inner(name, source, false)
}

/// Drop a leading UTF-8 BOM (kept out of every emitted/verbatim module).
fn strip_bom(source: &str) -> &str {
    source.strip_prefix('\u{feff}').unwrap_or(source)
}

/// Whether `name` addresses a JavaScript (not TypeScript) module by extension.
fn is_plain_js_module(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".js") || lower.ends_with(".mjs") || lower.ends_with(".cjs")
}

/// Does `source` parse as plain JavaScript (ES syntax, no TypeScript-only
/// constructs)? A shebang is excluded from this probe because callers embed the
/// result inside function wrappers, where a `#!` line is a syntax error. The
/// transpile path re-emits the shebang (SWC stores and reprints it); QuickJS
/// itself skips a leading shebang, which is why verbatim `.js` modules with one
/// still work.
pub fn is_plain_js(source: &str) -> bool {
    let source = strip_bom(source);
    if source.starts_with("#!") {
        return false;
    }
    let cm: Lrc<SourceMap> = Lrc::default();
    let fm = cm.new_source_file(
        Lrc::new(FileName::Custom("plain-js-check".to_string())),
        source.to_string(),
    );
    let syntax = Syntax::Es(EsSyntax {
        jsx: false,
        fn_bind: false,
        decorators: false,
        decorators_before_export: false,
        export_default_from: true,
        import_attributes: true,
        allow_super_outside_method: false,
        allow_return_outside_function: false,
        auto_accessors: true,
        explicit_resource_management: true,
    });
    let lexer = Lexer::new(syntax, EsVersion::latest(), StringInput::from(&*fm), None);
    let mut parser = Parser::new_from(lexer);
    // The parser recovers from syntax it cannot parse (TypeScript annotations
    // among them), so a successful parse alone does not mean the source is
    // plain JavaScript: any recovered error sends the file to the transpiler.
    match parser.parse_program() {
        Ok(_) => parser.take_errors().is_empty(),
        Err(_) => false,
    }
}

/// Transpile a `pir js` script-mode entry (see `main_cli/js_mode.rs`): the
/// same pipeline as [`transpile`], plus the top-level-await downgrade —
/// QuickJS rejects `await` at module top level, so from the first top-level
/// `await` onward every execution statement is wrapped into an
/// immediately-invoked `async` arrow (which runs synchronously up to its
/// first `await`, preserving the original statement order). Exported
/// variable declarations in the wrapped region keep their names linkable via
/// top-level `let` bindings + export clauses (module live bindings); the
/// initializers move inside the wrapper as assignments. Only the script
/// entry gets this treatment — imported modules keep native ESM semantics.
pub fn transpile_script_entry(name: &str, source: &str) -> Result<String, String> {
    let source = strip_bom(source);
    // Same plain-JS shortcut as `transpile`, guarded by the top-level-await
    // check: without TLA the wrapper is a no-op, so the raw source is exactly
    // what the entry transpile would have preserved (minus the reprint). A
    // `.js/.mjs/.cjs` entry takes the extension fast path too, so a script
    // entry and an imported module treat JavaScript identically (TS syntax in
    // a .js entry is a runtime syntax error, not a silent strip).
    if !super::loader::source_has_top_level_await(source)
        && (is_plain_js_module(name) || is_plain_js(source))
    {
        return Ok(source.to_string());
    }
    transpile_inner(name, source, true)
}

fn transpile_inner(name: &str, source: &str, downgrade_tla: bool) -> Result<String, String> {
    let cm: Lrc<SourceMap> = Lrc::default();
    let fm = cm.new_source_file(
        Lrc::new(FileName::Custom(name.to_string())),
        source.to_string(),
    );

    let syntax = Syntax::Typescript(TsSyntax {
        decorators: false,
        tsx: false,
        dts: false,
        no_early_errors: false,
        disallow_ambiguous_jsx_like: false,
    });

    let lexer = Lexer::new(syntax, EsVersion::Es2022, StringInput::from(&*fm), None);
    let mut parser = Parser::new_from(lexer);
    let program = parser
        .parse_program()
        .map_err(|e| format!("parse error in {name}: {e:?}"))?;

    let mut program: Program = program;
    let globals = Globals::new();
    GLOBALS.set(&globals, || {
        // Marks require the GLOBALS scope; unresolved (global) identifiers
        // are marked vs top-level bindings before stripping TypeScript-only
        // syntax.
        let unresolved_mark = Mark::new();
        let top_level_mark = Mark::new();
        program.mutate(&mut resolver(unresolved_mark, top_level_mark, true));
        program.mutate(&mut typescript(
            swc_ecma_transforms_typescript::Config::default(),
            unresolved_mark,
            top_level_mark,
        ));
        if downgrade_tla {
            let taken = std::mem::replace(&mut program, swc_common::util::take::Take::dummy());
            program = wrap_top_level_await(taken);
        }
        program.mutate(&mut hygiene());
        program.mutate(&mut fixer(None));
    });

    let mut buf: Vec<u8> = Vec::new();
    {
        let mut emitter = Emitter {
            cfg: swc_ecma_codegen::Config::default().with_target(EsVersion::Es2022),
            cm: cm.clone(),
            comments: None,
            wr: JsWriter::new(cm.clone(), "\n", &mut buf, None),
        };
        emitter
            .emit_program(&program)
            .map_err(|e| format!("codegen error in {name}: {e}"))?;
    }

    String::from_utf8(buf).map_err(|e| format!("output is not utf-8 in {name}: {e}"))
}

// ---------------------------------------------------------------------------
// Top-level-await downgrade for script-mode entries (QuickJS has no TLA).
// ---------------------------------------------------------------------------

/// Wrap a module's top-level await into an async IIFE. Statements before the
/// first top-level `await` stay top-level (they run synchronously before the
/// wrapper either way, and their bindings stay visible to kept
/// function/class declarations); `import` declarations, export clauses and
/// function/class declarations always stay top-level (imports are hoisted by
/// the engine; declarations carry no await). Statements from the first TLA
/// statement onward move into the wrapper in order. If a binding name cannot
/// be extracted from an exported pattern, the whole wrapping is skipped —
/// the source then behaves exactly as before (QuickJS reports the TLA parse
/// error) rather than being silently mis-transformed.
fn wrap_top_level_await(program: Program) -> Program {
    use swc_common::SyntaxContext;
    use swc_ecma_ast::*;

    let mut module = match program {
        Program::Module(module) => module,
        other => return other,
    };

    let cut = match module.body.iter().position(module_item_has_tla) {
        Some(cut) => cut,
        None => return Program::Module(module), // no top-level await: no-op
    };

    let mut head: Vec<ModuleItem> = module.body.drain(..cut).collect();
    let mut wrapped: Vec<Stmt> = Vec::new();

    for item in module.body.drain(..) {
        match item {
            ModuleItem::ModuleDecl(ModuleDecl::Import(_))
            | ModuleItem::ModuleDecl(ModuleDecl::ExportAll(_))
            | ModuleItem::ModuleDecl(ModuleDecl::ExportNamed(_))
            | ModuleItem::ModuleDecl(ModuleDecl::ExportDefaultDecl(_))
            // TS-only module constructs (stripped or no-ops after the
            // typescript pass): keep them top-level untouched.
            | ModuleItem::ModuleDecl(ModuleDecl::TsImportEquals(_))
            | ModuleItem::ModuleDecl(ModuleDecl::TsExportAssignment(_))
            | ModuleItem::ModuleDecl(ModuleDecl::TsNamespaceExport(_)) => {
                // Link-time constructs: hoisted imports, re-exports, local
                // export clauses and default function/class declarations
                // must stay at module top level.
                head.push(item);
            }
            ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(export_decl)) => {
                match export_decl.decl {
                    // Function/class declarations cannot carry top-level
                    // await; the export must stay top-level for link time.
                    decl @ (Decl::Fn(_) | Decl::Class(_)) => {
                        head.push(ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(
                            ExportDecl {
                                span: export_decl.span,
                                decl,
                            },
                        )));
                    }
                    Decl::Var(var) => {
                        // `export const x = <init>`: keep the name linkable
                        // with a top-level `let x; export { x };` and move
                        // the initializer into the wrapper as `x = <init>`.
                        match convert_exported_var(&var) {
                            Some((lets, exports, assigns)) => {
                                head.extend(lets);
                                head.extend(exports);
                                wrapped.extend(assigns);
                            }
                            None => {
                                head.push(ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(
                                    ExportDecl {
                                        span: export_decl.span,
                                        decl: Decl::Var(var),
                                    },
                                )));
                            }
                        }
                    }
                    other => {
                        head.push(ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(
                            ExportDecl {
                                span: export_decl.span,
                                decl: other,
                            },
                        )));
                    }
                }
            }
            ModuleItem::ModuleDecl(ModuleDecl::ExportDefaultExpr(default_expr)) => {
                if expr_has_tla(&default_expr.expr) {
                    let name = Ident::new_private("__pirTlaDefault".into(), DUMMY_SP);
                    head.push(let_decl(vec![name.clone()]));
                    head.push(export_clause(
                        vec![name.clone()],
                        vec![Some("default")],
                    ));
                    wrapped.push(assign_stmt(
                        Pat::Ident(BindingIdent {
                            id: name,
                            type_ann: None,
                        }),
                        *default_expr.expr,
                    ));
                } else {
                    head.push(ModuleItem::ModuleDecl(ModuleDecl::ExportDefaultExpr(
                        default_expr,
                    )));
                }
            }
            ModuleItem::Stmt(stmt) => match stmt {
                // Function/class declarations are hoisted and cannot carry
                // top-level await: keep them module-scoped so kept exported
                // declarations can still reference them.
                Stmt::Decl(decl @ (Decl::Fn(_) | Decl::Class(_))) => {
                    head.push(ModuleItem::Stmt(Stmt::Decl(decl)));
                }
                stmt => wrapped.push(stmt),
            },
        }
    }

    // `(async () => { ...moved statements... })();` — an arrow inherits the
    // module's `this`/`arguments`/`import.meta`, and the call runs the body
    // synchronously up to its first `await`.
    if !wrapped.is_empty() {
        let arrow = ArrowExpr {
            span: DUMMY_SP,
            ctxt: SyntaxContext::empty(),
            params: Vec::new(),
            body: Box::new(ArrowFunctionBody::FunctionBody(FunctionBody {
                span: DUMMY_SP,
                stmts: wrapped,
            })),
            is_async: true,
            is_generator: false,
            type_params: None,
            return_type: None,
        };
        let call = CallExpr {
            span: DUMMY_SP,
            ctxt: SyntaxContext::empty(),
            callee: Callee::Expr(Box::new(Expr::Paren(ParenExpr {
                span: DUMMY_SP,
                expr: Box::new(Expr::Arrow(arrow)),
            }))),
            args: Vec::new(),
            type_args: None,
        };
        head.push(ModuleItem::Stmt(Stmt::Expr(ExprStmt {
            span: DUMMY_SP,
            expr: Box::new(Expr::Call(call)),
        })));
    }

    module.body = head;
    Program::Module(module)
}

/// Does any initializer of this variable declaration carry a top-level
/// `await`?
fn var_inits_have_tla(var: &swc_ecma_ast::VarDecl) -> bool {
    var.decls
        .iter()
        .any(|decl| decl.init.as_deref().is_some_and(expr_has_tla))
}

/// Convert `export const/let/var <pat> = <init>` into the TLA-wrapped form:
/// returns the top-level `let` declarations, the top-level export clauses and
/// the in-wrapper assignment statements. `None` when a pattern name cannot be
/// extracted (leave the declaration untouched).
fn convert_exported_var(
    var: &swc_ecma_ast::VarDecl,
) -> Option<(
    Vec<swc_ecma_ast::ModuleItem>,
    Vec<swc_ecma_ast::ModuleItem>,
    Vec<swc_ecma_ast::Stmt>,
)> {
    use swc_ecma_ast::*;

    let mut names: Vec<Ident> = Vec::new();
    for decl in &var.decls {
        pat_binding_names(&decl.name, &mut names)?;
    }

    let lets = vec![let_decl(names.clone())];
    let aliases = vec![None; names.len()];
    let exports = vec![export_clause(names, aliases)];
    let mut assigns: Vec<Stmt> = Vec::new();
    for decl in &var.decls {
        if let Some(init) = &decl.init {
            assigns.push(assign_stmt(decl.name.clone(), *(init.clone())));
        }
    }
    Some((lets, exports, assigns))
}

/// `let <n1>, <n2>, ...;` at module top level.
fn let_decl(names: Vec<swc_ecma_ast::Ident>) -> swc_ecma_ast::ModuleItem {
    use swc_common::SyntaxContext;
    use swc_ecma_ast::*;
    ModuleItem::Stmt(Stmt::Decl(Decl::Var(Box::new(VarDecl {
        span: DUMMY_SP,
        ctxt: SyntaxContext::empty(),
        kind: VarDeclKind::Let,
        declare: false,
        decls: names
            .into_iter()
            .map(|id| VarDeclarator {
                span: DUMMY_SP,
                name: Pat::Ident(BindingIdent { id, type_ann: None }),
                init: None,
                definite: false,
            })
            .collect(),
    }))))
}

/// `export { <n1>[ as <alias>], ... };` at module top level.
fn export_clause(
    names: Vec<swc_ecma_ast::Ident>,
    aliases: Vec<Option<&str>>,
) -> swc_ecma_ast::ModuleItem {
    use swc_ecma_ast::*;
    ModuleItem::ModuleDecl(ModuleDecl::ExportNamed(NamedExport {
        span: DUMMY_SP,
        specifiers: names
            .into_iter()
            .zip(aliases)
            .map(|(id, alias)| {
                ExportSpecifier::Named(ExportNamedSpecifier {
                    span: DUMMY_SP,
                    orig: ModuleExportName::Ident(id),
                    exported: alias.map(|atom| {
                        ModuleExportName::Ident(Ident::new_no_ctxt(atom.into(), DUMMY_SP))
                    }),
                    is_type_only: false,
                })
            })
            .collect(),
        src: None,
        type_only: false,
        with: None,
    }))
}

/// `<pat> = <expr>;` inside the async wrapper (parenthesized so object
/// patterns emit valid assignment expressions).
fn assign_stmt(pat: swc_ecma_ast::Pat, expr: swc_ecma_ast::Expr) -> swc_ecma_ast::Stmt {
    use swc_ecma_ast::*;
    let target = match pat {
        Pat::Object(object) => AssignTarget::Pat(AssignTargetPat::Object(object)),
        Pat::Array(array) => AssignTarget::Pat(AssignTargetPat::Array(array)),
        Pat::Ident(binding) => AssignTarget::Simple(SimpleAssignTarget::Ident(binding)),
        // Not a valid assignment target (and not constructible from a
        // variable-declaration pattern); nothing to assign.
        _ => return Stmt::Empty(EmptyStmt { span: DUMMY_SP }),
    };
    let assign = AssignExpr {
        span: DUMMY_SP,
        op: AssignOp::Assign,
        left: target,
        right: Box::new(expr),
    };
    Stmt::Expr(ExprStmt {
        span: DUMMY_SP,
        expr: Box::new(Expr::Paren(ParenExpr {
            span: DUMMY_SP,
            expr: Box::new(Expr::Assign(assign)),
        })),
    })
}

/// Collect the identifiers bound by a declaration pattern.
/// `Err(())`-equivalent: `None` when a name cannot be extracted statically.
fn pat_binding_names(pat: &swc_ecma_ast::Pat, out: &mut Vec<swc_ecma_ast::Ident>) -> Option<()> {
    use swc_ecma_ast::*;
    match pat {
        Pat::Ident(binding) => out.push(binding.id.clone()),
        Pat::Array(array) => {
            for elem in array.elems.iter().flatten() {
                pat_binding_names(elem, out)?;
            }
        }
        Pat::Object(object) => {
            for prop in &object.props {
                match prop {
                    ObjectPatProp::KeyValue(kv) => pat_binding_names(&kv.value, out)?,
                    ObjectPatProp::Assign(assign) => out.push(assign.key.id.clone()),
                    ObjectPatProp::Rest(rest) => pat_binding_names(&rest.arg, out)?,
                }
            }
        }
        Pat::Rest(rest) => pat_binding_names(&rest.arg, out)?,
        Pat::Assign(assign) => pat_binding_names(&assign.left, out)?,
        Pat::Invalid(_) | Pat::Expr(_) => return None,
    }
    Some(())
}

/// Does this module item contain a top-level `await` (i.e. must it move into
/// the async wrapper)?
fn module_item_has_tla(item: &swc_ecma_ast::ModuleItem) -> bool {
    use swc_ecma_ast::*;
    match item {
        ModuleItem::ModuleDecl(decl) => match decl {
            ModuleDecl::ExportDecl(ExportDecl {
                decl: Decl::Var(var),
                ..
            }) => var_inits_have_tla(var),
            ModuleDecl::ExportDefaultExpr(default_expr) => expr_has_tla(&default_expr.expr),
            _ => false,
        },
        ModuleItem::Stmt(stmt) => stmt_has_tla(stmt),
    }
}

// `pub(crate)`: `esm_cjs` reuses the traversal to reject top-level await in
// require(esm) with Node's ERR_REQUIRE_ASYNC_MODULE.
pub(crate) fn stmt_has_tla(stmt: &swc_ecma_ast::Stmt) -> bool {
    use swc_ecma_ast::*;
    match stmt {
        Stmt::Block(block) => block.stmts.iter().any(stmt_has_tla),
        Stmt::Empty(_) | Stmt::Debugger(_) | Stmt::Break(_) | Stmt::Continue(_) => false,
        Stmt::With(with) => expr_has_tla(&with.obj) || stmt_has_tla(&with.body),
        Stmt::Return(ret) => ret.arg.as_deref().is_some_and(expr_has_tla),
        Stmt::Labeled(labeled) => stmt_has_tla(&labeled.body),
        Stmt::If(if_stmt) => {
            expr_has_tla(&if_stmt.test)
                || stmt_has_tla(&if_stmt.cons)
                || if_stmt.alt.as_deref().is_some_and(stmt_has_tla)
        }
        Stmt::Switch(switch) => {
            expr_has_tla(&switch.discriminant)
                || switch.cases.iter().any(|case| {
                    case.test.as_deref().is_some_and(expr_has_tla)
                        || case.cons.iter().any(stmt_has_tla)
                })
        }
        Stmt::Throw(throw) => expr_has_tla(&throw.arg),
        Stmt::Try(try_stmt) => {
            try_stmt.block.stmts.iter().any(stmt_has_tla)
                || try_stmt
                    .handler
                    .as_ref()
                    .is_some_and(|handler| handler.body.stmts.iter().any(stmt_has_tla))
                || try_stmt
                    .finalizer
                    .as_ref()
                    .is_some_and(|finalizer| finalizer.stmts.iter().any(stmt_has_tla))
        }
        Stmt::While(while_stmt) => expr_has_tla(&while_stmt.test) || stmt_has_tla(&while_stmt.body),
        Stmt::DoWhile(do_while) => expr_has_tla(&do_while.test) || stmt_has_tla(&do_while.body),
        Stmt::For(for_stmt) => {
            let init_has = match &for_stmt.init {
                Some(VarDeclOrExpr::VarDecl(var)) => var_inits_have_tla(var),
                Some(VarDeclOrExpr::Expr(expr)) => expr_has_tla(expr),
                None => false,
            };
            init_has
                || for_stmt.test.as_deref().is_some_and(expr_has_tla)
                || for_stmt.update.as_deref().is_some_and(expr_has_tla)
                || stmt_has_tla(&for_stmt.body)
        }
        Stmt::ForIn(for_in) => {
            for_head_has_tla(&for_in.left)
                || expr_has_tla(&for_in.right)
                || stmt_has_tla(&for_in.body)
        }
        // `for await (...)` IS a top-level await by itself.
        Stmt::ForOf(for_of) => {
            for_of.is_await
                || for_head_has_tla(&for_of.left)
                || expr_has_tla(&for_of.right)
                || stmt_has_tla(&for_of.body)
        }
        Stmt::Decl(Decl::Var(var)) => var_inits_have_tla(var),
        // Function bodies, class bodies (static blocks included) and
        // TS-only declarations own their await context.
        Stmt::Decl(_) => false,
        Stmt::Expr(expr_stmt) => expr_has_tla(&expr_stmt.expr),
    }
}

fn for_head_has_tla(head: &swc_ecma_ast::ForHead) -> bool {
    use swc_ecma_ast::*;
    match head {
        ForHead::VarDecl(var) => var_inits_have_tla(var),
        ForHead::Pat(_) => false,
        ForHead::UsingDecl(using) => using
            .decls
            .iter()
            .any(|decl| decl.init.as_deref().is_some_and(expr_has_tla)),
    }
}

/// Does this expression contain an `await` that is not nested inside a
/// function/arrow/class boundary? (Function and arrow bodies belong to their
/// own async context; class bodies are conservatively treated as one too.)
pub(crate) fn expr_has_tla(expr: &swc_ecma_ast::Expr) -> bool {
    use swc_ecma_ast::*;
    match expr {
        Expr::This(_)
        | Expr::Ident(_)
        | Expr::Lit(_)
        | Expr::Fn(_)
        | Expr::Arrow(_)
        | Expr::Class(_)
        | Expr::MetaProp(_)
        | Expr::PrivateName(_)
        | Expr::Invalid(_)
        | Expr::JSXMember(_)
        | Expr::JSXNamespacedName(_)
        | Expr::JSXEmpty(_)
        | Expr::JSXElement(_)
        | Expr::JSXFragment(_)
        | Expr::SuperProp(_) => false,
        Expr::Await(_) => true,
        Expr::Array(array) => array
            .elems
            .iter()
            .flatten()
            .any(|elem| elem.spread.is_none() && expr_has_tla(&elem.expr)),
        Expr::Object(object) => object.props.iter().any(|prop| match prop {
            PropOrSpread::Spread(spread) => expr_has_tla(&spread.expr),
            PropOrSpread::Prop(prop) => match &**prop {
                Prop::Shorthand(_) | Prop::Getter(_) | Prop::Setter(_) | Prop::Method(_) => false,
                Prop::KeyValue(kv) => prop_name_has_tla(&kv.key) || expr_has_tla(&kv.value),
                Prop::Assign(assign) => expr_has_tla(&assign.value),
            },
        }),
        Expr::Unary(unary) => expr_has_tla(&unary.arg),
        Expr::Update(_) => false,
        Expr::Bin(bin) => expr_has_tla(&bin.left) || expr_has_tla(&bin.right),
        Expr::Assign(assign) => {
            let left_has = match &assign.left {
                AssignTarget::Simple(simple) => match simple {
                    SimpleAssignTarget::Ident(_) | SimpleAssignTarget::Invalid(_) => false,
                    SimpleAssignTarget::Member(member) => member_expr_has_tla(member),
                    SimpleAssignTarget::SuperProp(super_prop) => matches!(
                        &super_prop.prop,
                        swc_ecma_ast::SuperProp::Computed(computed) if expr_has_tla(&computed.expr)
                    ),
                    SimpleAssignTarget::Paren(paren) => expr_has_tla(&paren.expr),
                    SimpleAssignTarget::OptChain(opt_chain) => opt_chain_has_tla(opt_chain),
                    SimpleAssignTarget::TsAs(as_expr) => expr_has_tla(&as_expr.expr),
                    SimpleAssignTarget::TsSatisfies(satisfies) => expr_has_tla(&satisfies.expr),
                    SimpleAssignTarget::TsNonNull(non_null) => expr_has_tla(&non_null.expr),
                    SimpleAssignTarget::TsTypeAssertion(assertion) => expr_has_tla(&assertion.expr),
                    SimpleAssignTarget::TsInstantiation(instantiation) => {
                        expr_has_tla(&instantiation.expr)
                    }
                },
                // Destructuring assignment targets: only the default-value
                // expressions can carry an await.
                AssignTarget::Pat(pat) => match pat {
                    AssignTargetPat::Array(array) => {
                        array.elems.iter().flatten().any(pat_default_has_tla)
                    }
                    AssignTargetPat::Object(object) => object.props.iter().any(|prop| match prop {
                        ObjectPatProp::Assign(assign_prop) => {
                            assign_prop.value.as_deref().is_some_and(expr_has_tla)
                        }
                        _ => false,
                    }),
                    AssignTargetPat::Invalid(_) => false,
                },
            };
            left_has || expr_has_tla(&assign.right)
        }
        Expr::Member(member) => {
            expr_has_tla(&member.obj)
                || matches!(&member.prop, MemberProp::Computed(computed) if expr_has_tla(&computed.expr))
        }
        Expr::Cond(cond) => {
            expr_has_tla(&cond.test) || expr_has_tla(&cond.cons) || expr_has_tla(&cond.alt)
        }
        Expr::Call(call) => {
            let callee_has = match &call.callee {
                Callee::Expr(callee) => expr_has_tla(callee),
                Callee::Super(_) | Callee::Import(_) => false,
            };
            callee_has || call.args.iter().any(arg_has_tla)
        }
        Expr::New(new) => {
            expr_has_tla(&new.callee)
                || new
                    .args
                    .as_deref()
                    .is_some_and(|args| args.iter().any(arg_has_tla))
        }
        Expr::Seq(seq) => seq.exprs.iter().any(|expr| expr_has_tla(expr)),
        Expr::Tpl(tpl) => tpl.exprs.iter().any(|expr| expr_has_tla(expr)),
        Expr::TaggedTpl(tagged) => expr_has_tla(&tagged.tag) || tpl_has_tla(&tagged.tpl),
        Expr::Yield(yield_expr) => yield_expr.arg.as_deref().is_some_and(expr_has_tla),
        Expr::Paren(paren) => expr_has_tla(&paren.expr),
        Expr::OptChain(opt_chain) => opt_chain_has_tla(opt_chain),
        Expr::TsTypeAssertion(assertion) => expr_has_tla(&assertion.expr),
        Expr::TsConstAssertion(assertion) => expr_has_tla(&assertion.expr),
        Expr::TsNonNull(non_null) => expr_has_tla(&non_null.expr),
        Expr::TsAs(as_expr) => expr_has_tla(&as_expr.expr),
        Expr::TsInstantiation(instantiation) => expr_has_tla(&instantiation.expr),
        Expr::TsSatisfies(satisfies) => expr_has_tla(&satisfies.expr),
    }
}

fn arg_has_tla(arg: &swc_ecma_ast::ExprOrSpread) -> bool {
    arg.spread.is_none() && expr_has_tla(&arg.expr)
}

fn member_expr_has_tla(member: &swc_ecma_ast::MemberExpr) -> bool {
    expr_has_tla(&member.obj)
        || matches!(&member.prop, swc_ecma_ast::MemberProp::Computed(computed) if expr_has_tla(&computed.expr))
}

fn opt_chain_has_tla(opt_chain: &swc_ecma_ast::OptChainExpr) -> bool {
    match &*opt_chain.base {
        swc_ecma_ast::OptChainBase::Member(member) => member_expr_has_tla(member),
        swc_ecma_ast::OptChainBase::Call(call) => {
            expr_has_tla(&call.callee) || call.args.iter().any(arg_has_tla)
        }
    }
}

fn pat_default_has_tla(pat: &swc_ecma_ast::Pat) -> bool {
    match pat {
        swc_ecma_ast::Pat::Assign(assign) => expr_has_tla(&assign.right),
        _ => false,
    }
}

fn prop_name_has_tla(prop_name: &swc_ecma_ast::PropName) -> bool {
    match prop_name {
        swc_ecma_ast::PropName::Computed(computed) => expr_has_tla(&computed.expr),
        _ => false,
    }
}

fn tpl_has_tla(tpl: &swc_ecma_ast::Tpl) -> bool {
    tpl.exprs.iter().any(|expr| expr_has_tla(expr))
}
