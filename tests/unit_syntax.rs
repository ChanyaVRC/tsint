//! 構文ごとのユニットテスト。各テストは短いスニペットをパースし、
//! 得られた AST の形状を assert する。

use tsint::ast::*;
use tsint::parse_module;

fn parse(src: &str) -> Module {
    parse_module(src, "test.ts").unwrap_or_else(|e| panic!("パース失敗: {e}\nsource:\n{src}"))
}

fn parse_err(src: &str) {
    assert!(
        parse_module(src, "test.ts").is_err(),
        "パースが成功してしまいました（エラーを期待）: {src}"
    );
}

fn first_expr_stmt(m: &Module) -> &Expr {
    match &m.body[0].kind {
        StmtKind::Expr(e) => e,
        other => panic!("式文を期待しましたが {other:?}"),
    }
}

// ---------- ASI ----------

#[test]
fn asi_inserts_semicolons_between_lines() {
    let m = parse("let a = 1\nlet b = 2\n");
    assert_eq!(m.body.len(), 2);
    assert!(matches!(m.body[0].kind, StmtKind::VarDecl { kind: VarKind::Let, .. }));
    assert!(matches!(m.body[1].kind, StmtKind::VarDecl { kind: VarKind::Let, .. }));
}

#[test]
fn asi_return_with_newline_yields_no_argument() {
    let m = parse("function f() {\n  return\n  1\n}\n");
    let StmtKind::FunctionDecl(func) = &m.body[0].kind else {
        panic!("関数宣言を期待")
    };
    let body = func.body.as_ref().unwrap();
    assert_eq!(body.len(), 2, "return と 1 の 2 文に ASI で分割されるはず");
    assert!(matches!(&body[0].kind, StmtKind::Return(None)));
    assert!(matches!(&body[1].kind, StmtKind::Expr(_)));
}

#[test]
fn asi_allows_omitting_semicolon_before_closing_brace() {
    let m = parse("function f() { return 1 }");
    let StmtKind::FunctionDecl(func) = &m.body[0].kind else {
        panic!("関数宣言を期待")
    };
    assert_eq!(func.body.as_ref().unwrap().len(), 1);
}

// ---------- 正規表現 vs 除算 ----------

#[test]
fn division_after_identifier_is_binary_op() {
    let m = parse("a / b;");
    let e = first_expr_stmt(&m);
    assert!(matches!(
        &e.kind,
        ExprKind::Binary { op: BinaryOp::Div, .. }
    ));
}

#[test]
fn division_after_paren_is_binary_op() {
    let m = parse("(a + b) / c;");
    let e = first_expr_stmt(&m);
    assert!(matches!(
        &e.kind,
        ExprKind::Binary { op: BinaryOp::Div, .. }
    ));
}

#[test]
fn regex_literal_after_assignment() {
    let m = parse("let r = /abc/gi;");
    let StmtKind::VarDecl { decls, .. } = &m.body[0].kind else {
        panic!("var decl を期待")
    };
    let init = decls[0].init.as_ref().unwrap();
    match &init.kind {
        ExprKind::RegexLit { pattern, flags } => {
            assert_eq!(pattern, "abc");
            assert_eq!(flags, "gi");
        }
        other => panic!("正規表現リテラルを期待: {other:?}"),
    }
}

#[test]
fn regex_literal_with_escaped_slash_in_call_arg() {
    let m = parse(r#"f(/a\/b/);"#);
    let e = first_expr_stmt(&m);
    let ExprKind::Call { args, .. } = &e.kind else {
        panic!("call を期待")
    };
    match &args[0].kind {
        ExprKind::RegexLit { pattern, .. } => assert_eq!(pattern, r"a\/b"),
        other => panic!("正規表現を期待: {other:?}"),
    }
}

#[test]
fn regex_literal_with_char_class_containing_slash_free_bracket() {
    let m = parse(r#"const r = /[a-z\]]/;"#);
    let StmtKind::VarDecl { decls, .. } = &m.body[0].kind else {
        panic!()
    };
    assert!(matches!(
        decls[0].init.as_ref().unwrap().kind,
        ExprKind::RegexLit { .. }
    ));
}

// ---------- アロー関数 vs 括弧式 ----------

#[test]
fn parenthesized_expr_is_not_arrow() {
    let m = parse("(a);");
    let e = first_expr_stmt(&m);
    assert!(matches!(&e.kind, ExprKind::Ident(n) if n == "a"));
}

#[test]
fn arrow_with_parens_and_expr_body() {
    let m = parse("const f = (a) => a + 1;");
    let StmtKind::VarDecl { decls, .. } = &m.body[0].kind else {
        panic!()
    };
    let ExprKind::Arrow(func) = &decls[0].init.as_ref().unwrap().kind else {
        panic!("アロー関数を期待")
    };
    assert_eq!(func.params.len(), 1);
    assert!(func.expr_body.is_some());
    assert!(func.body.is_none());
}

#[test]
fn arrow_with_bare_single_param() {
    let m = parse("const f = x => x * 2;");
    let StmtKind::VarDecl { decls, .. } = &m.body[0].kind else {
        panic!()
    };
    let ExprKind::Arrow(func) = &decls[0].init.as_ref().unwrap().kind else {
        panic!("アロー関数を期待")
    };
    assert_eq!(func.params.len(), 1);
    assert!(matches!(&func.params[0].pattern, Pattern::Ident(n) if n == "x"));
}

#[test]
fn arrow_with_typed_params_and_default_and_return_type() {
    let m = parse(r#"const f = (a: number, b: string = "x"): string => b;"#);
    let StmtKind::VarDecl { decls, .. } = &m.body[0].kind else {
        panic!()
    };
    let ExprKind::Arrow(func) = &decls[0].init.as_ref().unwrap().kind else {
        panic!("アロー関数を期待")
    };
    assert_eq!(func.params.len(), 2);
    assert!(func.params[1].default.is_some());
}

#[test]
fn async_arrow_with_block_body() {
    let m = parse("const f = async (x) => { return x; };");
    let StmtKind::VarDecl { decls, .. } = &m.body[0].kind else {
        panic!()
    };
    let ExprKind::Arrow(func) = &decls[0].init.as_ref().unwrap().kind else {
        panic!("アロー関数を期待")
    };
    assert!(func.is_async);
    assert!(func.body.is_some());
}

#[test]
fn arrow_returning_object_literal_needs_parens() {
    // `x => ({ a: x })` は括弧式（オブジェクトリテラル）を式本体として持つ。
    let m = parse("const f = x => ({ a: x });");
    let StmtKind::VarDecl { decls, .. } = &m.body[0].kind else {
        panic!()
    };
    let ExprKind::Arrow(func) = &decls[0].init.as_ref().unwrap().kind else {
        panic!()
    };
    assert!(matches!(
        func.expr_body.as_ref().unwrap().kind,
        ExprKind::ObjectLit(_)
    ));
}

// ---------- 分割代入 ----------

#[test]
fn nested_object_destructuring_with_default() {
    let m = parse("const { a: { b, c = 1 } } = obj;");
    let StmtKind::VarDecl { decls, .. } = &m.body[0].kind else {
        panic!()
    };
    let Pattern::Object { props, .. } = &decls[0].name else {
        panic!("オブジェクトパターンを期待")
    };
    assert_eq!(props.len(), 1);
    let Pattern::Object { props: inner, .. } = &props[0].value else {
        panic!("ネストしたオブジェクトパターンを期待")
    };
    assert_eq!(inner.len(), 2);
    assert!(matches!(&inner[1].value, Pattern::Assign { .. }));
}

#[test]
fn array_destructuring_with_elision_and_rest() {
    let m = parse("const [a, , ...rest] = arr;");
    let StmtKind::VarDecl { decls, .. } = &m.body[0].kind else {
        panic!()
    };
    let Pattern::Array { elements, rest } = &decls[0].name else {
        panic!("配列パターンを期待")
    };
    assert_eq!(elements.len(), 2);
    assert!(elements[1].is_none(), "穴（elision）は None のはず");
    assert!(rest.is_some());
}

#[test]
fn function_param_destructuring_with_rest() {
    let m = parse("function f({ a, b = 2 }, ...rest) {}");
    let StmtKind::FunctionDecl(func) = &m.body[0].kind else {
        panic!()
    };
    assert_eq!(func.params.len(), 2);
    assert!(matches!(func.params[0].pattern, Pattern::Object { .. }));
    assert!(func.params[1].rest);
}

// ---------- オプショナルチェーン / null 合体 ----------

#[test]
fn optional_chaining_member_then_call() {
    let m = parse("a?.b?.();");
    let e = first_expr_stmt(&m);
    let ExprKind::Call { callee, optional: call_opt, .. } = &e.kind else {
        panic!("call を期待")
    };
    assert!(*call_opt);
    assert!(matches!(&callee.kind, ExprKind::Member { optional: true, .. }));
}

#[test]
fn optional_chaining_computed_member() {
    let m = parse("a?.[0];");
    let e = first_expr_stmt(&m);
    assert!(matches!(
        &e.kind,
        ExprKind::Member { optional: true, prop: PropertyKey::Computed(_), .. }
    ));
}

#[test]
fn nullish_coalescing_operator() {
    let m = parse("a ?? b;");
    let e = first_expr_stmt(&m);
    assert!(matches!(
        &e.kind,
        ExprKind::Logical { op: LogicalOp::Nullish, .. }
    ));
}

#[test]
fn nullish_assign_operator() {
    let m = parse("a ??= b;");
    let e = first_expr_stmt(&m);
    assert!(matches!(
        &e.kind,
        ExprKind::Assign { op: AssignOp::NullishAssign, .. }
    ));
}

// ---------- テンプレートリテラル ----------

#[test]
fn nested_template_literal() {
    let m = parse("const s = `a${ `b${c}` }d`;");
    let StmtKind::VarDecl { decls, .. } = &m.body[0].kind else {
        panic!()
    };
    let ExprKind::TemplateLit { quasis, exprs } = &decls[0].init.as_ref().unwrap().kind else {
        panic!("テンプレートリテラルを期待")
    };
    assert_eq!(quasis, &vec!["a".to_string(), "d".to_string()]);
    assert_eq!(exprs.len(), 1);
    assert!(matches!(&exprs[0].kind, ExprKind::TemplateLit { .. }));
}

#[test]
fn template_literal_no_substitution() {
    let m = parse("const s = `hello`;");
    let StmtKind::VarDecl { decls, .. } = &m.body[0].kind else {
        panic!()
    };
    let ExprKind::TemplateLit { quasis, exprs } = &decls[0].init.as_ref().unwrap().kind else {
        panic!()
    };
    assert_eq!(quasis.len(), 1);
    assert!(exprs.is_empty());
}

// ---------- ラベル付き break/continue ----------

#[test]
fn labeled_break() {
    let m = parse("outer: while (true) { break outer; }");
    let StmtKind::Labeled { label, body } = &m.body[0].kind else {
        panic!("ラベル文を期待")
    };
    assert_eq!(label, "outer");
    let StmtKind::While { body: while_body, .. } = &body.kind else {
        panic!()
    };
    let StmtKind::Block(stmts) = &while_body.kind else {
        panic!()
    };
    assert!(matches!(&stmts[0].kind, StmtKind::Break(Some(l)) if l == "outer"));
}

#[test]
fn labeled_continue() {
    let m = parse("loop: for (;;) { continue loop; }");
    assert!(matches!(&m.body[0].kind, StmtKind::Labeled { label, .. } if label == "loop"));
}

// ---------- class ----------

#[test]
fn class_members_of_every_kind() {
    let src = r#"
        class C extends Base {
            static readonly LIMIT = 6;
            private count = 0;
            constructor(x: number) { super(); this.count = x; }
            get value(): number { return this.count; }
            set value(v: number) { this.count = v; }
            increment(): void { this.count++; }
            static create(): C { return new C(0); }
        }
    "#;
    let m = parse(src);
    let StmtKind::ClassDecl(class) = &m.body[0].kind else {
        panic!("クラス宣言を期待")
    };
    assert_eq!(class.name.as_deref(), Some("C"));
    assert!(class.super_class.is_some());

    let kind_of = |name: &str| -> ClassMemberKind {
        class
            .members
            .iter()
            .find(|m| matches!(&m.name, PropertyKey::Ident(n) if n == name))
            .unwrap_or_else(|| panic!("メンバ {name} が見つかりません"))
            .kind
    };
    assert_eq!(kind_of("LIMIT"), ClassMemberKind::Field);
    assert_eq!(kind_of("count"), ClassMemberKind::Field);
    assert_eq!(kind_of("constructor"), ClassMemberKind::Constructor);
    assert_eq!(kind_of("value"), ClassMemberKind::Getter); // 最初にマッチした get
    assert_eq!(kind_of("increment"), ClassMemberKind::Method);
    assert_eq!(kind_of("create"), ClassMemberKind::Method);

    let limit = class
        .members
        .iter()
        .find(|m| matches!(&m.name, PropertyKey::Ident(n) if n == "LIMIT"))
        .unwrap();
    assert!(limit.is_static);

    let setters = class
        .members
        .iter()
        .filter(|m| m.kind == ClassMemberKind::Setter)
        .count();
    assert_eq!(setters, 1, "setter が 1 件見つかるはず");
}

#[test]
fn class_private_field_access() {
    let m = parse("class C { #x = 1; getX() { return this.#x; } }");
    let StmtKind::ClassDecl(class) = &m.body[0].kind else {
        panic!()
    };
    assert!(class
        .members
        .iter()
        .any(|m| matches!(&m.name, PropertyKey::Ident(n) if n == "#x")));
}

#[test]
fn class_expression_anonymous() {
    let m = parse("const C = class { greet() { return 1; } };");
    let StmtKind::VarDecl { decls, .. } = &m.body[0].kind else {
        panic!()
    };
    assert!(matches!(
        decls[0].init.as_ref().unwrap().kind,
        ExprKind::Class(_)
    ));
}

// ---------- import ----------

#[test]
fn import_default() {
    let m = parse(r#"import Foo from "mod";"#);
    let StmtKind::Import(decl) = &m.body[0].kind else {
        panic!("import を期待")
    };
    assert_eq!(decl.source, "mod");
    assert!(matches!(&decl.specifiers[0], ImportSpecifier::Default(n) if n == "Foo"));
}

#[test]
fn import_named_with_rename() {
    let m = parse(r#"import { a, b as c } from "mod";"#);
    let StmtKind::Import(decl) = &m.body[0].kind else {
        panic!()
    };
    assert_eq!(decl.specifiers.len(), 2);
    assert!(matches!(
        &decl.specifiers[1],
        ImportSpecifier::Named { imported, local } if imported == "b" && local == "c"
    ));
}

#[test]
fn import_namespace() {
    let m = parse(r#"import * as api from "noiria:api";"#);
    let StmtKind::Import(decl) = &m.body[0].kind else {
        panic!()
    };
    assert!(matches!(&decl.specifiers[0], ImportSpecifier::Namespace(n) if n == "api"));
}

#[test]
fn import_default_and_named_combined() {
    let m = parse(r#"import Foo, { a, b } from "mod";"#);
    let StmtKind::Import(decl) = &m.body[0].kind else {
        panic!()
    };
    assert_eq!(decl.specifiers.len(), 3);
    assert!(matches!(&decl.specifiers[0], ImportSpecifier::Default(_)));
}

#[test]
fn import_type_only() {
    let m = parse(r#"import type { KagOptions } from "noiria:kag/player";"#);
    let StmtKind::Import(decl) = &m.body[0].kind else {
        panic!()
    };
    assert!(decl.type_only);
}

#[test]
fn import_side_effect_only() {
    let m = parse(r#"import "polyfill";"#);
    let StmtKind::Import(decl) = &m.body[0].kind else {
        panic!()
    };
    assert!(decl.specifiers.is_empty());
    assert_eq!(decl.source, "polyfill");
}

// ---------- export ----------

#[test]
fn export_named_list() {
    let m = parse("export { a, b as c };");
    let StmtKind::ExportNamed { specifiers, source, .. } = &m.body[0].kind else {
        panic!("named export を期待")
    };
    assert!(source.is_none());
    assert_eq!(specifiers.len(), 2);
}

#[test]
fn export_named_reexport_from() {
    let m = parse(r#"export { KagPlayer } from "noiria:kag/player";"#);
    let StmtKind::ExportNamed { source, .. } = &m.body[0].kind else {
        panic!()
    };
    assert_eq!(source.as_deref(), Some("noiria:kag/player"));
}

#[test]
fn export_all_from() {
    let m = parse(r#"export * from "mod";"#);
    let StmtKind::ExportAll { as_name, source } = &m.body[0].kind else {
        panic!("export * を期待")
    };
    assert!(as_name.is_none());
    assert_eq!(source, "mod");
}

#[test]
fn export_all_as_namespace() {
    let m = parse(r#"export * as vars from "noiria:kag/vars";"#);
    let StmtKind::ExportAll { as_name, .. } = &m.body[0].kind else {
        panic!()
    };
    assert_eq!(as_name.as_deref(), Some("vars"));
}

#[test]
fn export_default_expression() {
    let m = parse("export default 42;");
    let StmtKind::ExportDefault(d) = &m.body[0].kind else {
        panic!()
    };
    assert!(matches!(d.as_ref(), DefaultExportable::Expr(_)));
}

#[test]
fn export_default_function() {
    let m = parse("export default function f() {}");
    let StmtKind::ExportDefault(d) = &m.body[0].kind else {
        panic!()
    };
    assert!(matches!(d.as_ref(), DefaultExportable::Function(_)));
}

#[test]
fn export_default_class() {
    let m = parse("export default class {}");
    let StmtKind::ExportDefault(d) = &m.body[0].kind else {
        panic!()
    };
    assert!(matches!(d.as_ref(), DefaultExportable::Class(_)));
}

#[test]
fn export_const_decl() {
    let m = parse("export const layer = { create() {} };");
    let StmtKind::ExportDecl(inner) = &m.body[0].kind else {
        panic!("export decl を期待")
    };
    assert!(matches!(inner.kind, StmtKind::VarDecl { .. }));
}

// ---------- TS 型注釈の除去 ----------

#[test]
fn interface_declaration_is_erased_to_marker() {
    let m = parse("interface Foo { x: number; y?: string; } const a = 1;");
    assert_eq!(m.body.len(), 2);
    assert!(matches!(&m.body[0].kind, StmtKind::TsInterface { name } if name == "Foo"));
    assert!(matches!(m.body[1].kind, StmtKind::VarDecl { .. }));
}

#[test]
fn type_alias_is_erased_to_marker() {
    let m = parse(r#"type Page = "fore" | "back"; const a = 1;"#);
    assert!(matches!(&m.body[0].kind, StmtKind::TsTypeAlias { name } if name == "Page"));
}

#[test]
fn variable_type_annotation_is_discarded() {
    let m = parse("let x: Record<string, unknown> = {};");
    let StmtKind::VarDecl { decls, .. } = &m.body[0].kind else {
        panic!()
    };
    assert!(matches!(decls[0].name, Pattern::Ident(ref n) if n == "x"));
    assert!(matches!(
        decls[0].init.as_ref().unwrap().kind,
        ExprKind::ObjectLit(_)
    ));
}

#[test]
fn function_generic_and_return_type_are_discarded() {
    let m = parse("function identity<T>(x: T): T { return x; }");
    let StmtKind::FunctionDecl(func) = &m.body[0].kind else {
        panic!()
    };
    assert_eq!(func.params.len(), 1);
    assert!(matches!(func.params[0].pattern, Pattern::Ident(ref n) if n == "x"));
}

#[test]
fn as_cast_is_erased_but_expr_kept() {
    let m = parse("const y = x as string;");
    let StmtKind::VarDecl { decls, .. } = &m.body[0].kind else {
        panic!()
    };
    let ExprKind::TsAs(inner) = &decls[0].init.as_ref().unwrap().kind else {
        panic!("TsAs を期待")
    };
    assert!(matches!(&inner.kind, ExprKind::Ident(n) if n == "x"));
}

#[test]
fn as_const_cast() {
    let m = parse("const z = [1, 2] as const;");
    let StmtKind::VarDecl { decls, .. } = &m.body[0].kind else {
        panic!()
    };
    assert!(matches!(
        decls[0].init.as_ref().unwrap().kind,
        ExprKind::TsAs(_)
    ));
}

#[test]
fn non_null_assertion() {
    let m = parse("const n = maybe!.value;");
    let StmtKind::VarDecl { decls, .. } = &m.body[0].kind else {
        panic!()
    };
    let ExprKind::Member { obj, .. } = &decls[0].init.as_ref().unwrap().kind else {
        panic!("member を期待")
    };
    assert!(matches!(obj.kind, ExprKind::TsNonNull(_)));
}

#[test]
fn generic_new_expression() {
    let m = parse("const s = new Set<string>();");
    let StmtKind::VarDecl { decls, .. } = &m.body[0].kind else {
        panic!()
    };
    let ExprKind::New { callee, args } = &decls[0].init.as_ref().unwrap().kind else {
        panic!("new 式を期待")
    };
    assert!(matches!(&callee.kind, ExprKind::Ident(n) if n == "Set"));
    assert!(args.is_empty());
}

#[test]
fn declare_module_ambient_block() {
    let m = parse(
        r#"declare module "noiria:x" {
            export function f(): void;
            export const c: number;
        }"#,
    );
    let StmtKind::TsModule { name, body } = &m.body[0].kind else {
        panic!("declare module を期待")
    };
    assert_eq!(name, "noiria:x");
    assert_eq!(body.len(), 2);
    let StmtKind::ExportDecl(inner) = &body[0].kind else {
        panic!()
    };
    let StmtKind::FunctionDecl(func) = &inner.kind else {
        panic!("アンビエント関数宣言を期待")
    };
    assert!(func.body.is_none(), "本体なしのアンビエント宣言のはず");
}

// ---------- 制御構造 ----------

#[test]
fn switch_with_multiple_cases_and_default() {
    let m = parse(
        r#"switch (x) {
            case 1: f(); break;
            case 2: {
                g();
                break;
            }
            default:
                h();
        }"#,
    );
    let StmtKind::Switch { cases, .. } = &m.body[0].kind else {
        panic!("switch を期待")
    };
    assert_eq!(cases.len(), 3);
    assert!(cases[2].test.is_none());
}

#[test]
fn try_catch_without_binding() {
    let m = parse("try { f(); } catch { g(); }");
    let StmtKind::Try { handler, finalizer, .. } = &m.body[0].kind else {
        panic!("try を期待")
    };
    assert!(finalizer.is_none());
    let h = handler.as_ref().unwrap();
    assert!(h.param.is_none());
}

#[test]
fn try_finally_without_catch() {
    let m = parse("try { f(); } finally { g(); }");
    let StmtKind::Try { handler, finalizer, .. } = &m.body[0].kind else {
        panic!()
    };
    assert!(handler.is_none());
    assert!(finalizer.is_some());
}

#[test]
fn for_of_with_destructuring() {
    let m = parse("for (const [k, v] of entries) { use(k, v); }");
    let StmtKind::ForIn { left, is_of, .. } = &m.body[0].kind else {
        panic!("for-of を期待")
    };
    assert!(is_of);
    let ForInit::VarDecl(VarKind::Const, decls) = left.as_ref() else {
        panic!()
    };
    assert!(matches!(decls[0].name, Pattern::Array { .. }));
}

#[test]
fn for_in_statement() {
    let m = parse("for (const k in obj) { use(k); }");
    let StmtKind::ForIn { is_of, .. } = &m.body[0].kind else {
        panic!()
    };
    assert!(!is_of);
}

#[test]
fn do_while_statement() {
    let m = parse("do { x++; } while (x < 10);");
    assert!(matches!(m.body[0].kind, StmtKind::DoWhile { .. }));
}

#[test]
fn classic_for_loop_shape() {
    let m = parse("for (let i = 0; i < 10; i++) { sum += i; }");
    let StmtKind::For { init, test, update, .. } = &m.body[0].kind else {
        panic!("for を期待")
    };
    assert!(init.is_some());
    assert!(test.is_some());
    assert!(update.is_some());
}

// ---------- 演算子 ----------

#[test]
fn spread_in_call_and_array() {
    let m = parse("f(...args); const a = [...b, 1];");
    let e0 = first_expr_stmt(&m);
    let ExprKind::Call { args, .. } = &e0.kind else {
        panic!()
    };
    assert!(matches!(args[0].kind, ExprKind::Spread(_)));

    let StmtKind::VarDecl { decls, .. } = &m.body[1].kind else {
        panic!()
    };
    let ExprKind::ArrayLit(elements) = &decls[0].init.as_ref().unwrap().kind else {
        panic!()
    };
    assert!(matches!(
        elements[0].as_ref().unwrap().kind,
        ExprKind::Spread(_)
    ));
}

#[test]
fn computed_member_assignment() {
    let m = parse("obj[key] = 1;");
    let e = first_expr_stmt(&m);
    let ExprKind::Assign { target, .. } = &e.kind else {
        panic!("代入を期待")
    };
    assert!(matches!(
        target.kind,
        ExprKind::Member { prop: PropertyKey::Computed(_), .. }
    ));
}

#[test]
fn sequence_comma_operator() {
    let m = parse("a, b, c;");
    let e = first_expr_stmt(&m);
    let ExprKind::Sequence(items) = &e.kind else {
        panic!("シーケンス式を期待")
    };
    assert_eq!(items.len(), 3);
}

#[test]
fn exponent_operator_is_right_associative() {
    let m = parse("let p = 2 ** 3 ** 2;");
    let StmtKind::VarDecl { decls, .. } = &m.body[0].kind else {
        panic!()
    };
    let ExprKind::Binary { op: BinaryOp::Pow, right, .. } = &decls[0].init.as_ref().unwrap().kind else {
        panic!("べき乗を期待")
    };
    // 右結合なので right 自体もべき乗であるはず (2 ** (3 ** 2))。
    assert!(matches!(
        right.kind,
        ExprKind::Binary { op: BinaryOp::Pow, .. }
    ));
}

#[test]
fn unary_operators_typeof_instanceof_in_delete_void() {
    let m = parse(
        r#"
        typeof x === "string";
        x instanceof Foo;
        "k" in obj;
        delete obj.k;
        void 0;
        "#,
    );
    assert!(matches!(
        first_expr_stmt(&m).kind,
        ExprKind::Binary { op: BinaryOp::StrictEq, .. }
    ));
    assert!(matches!(
        m.body[1].kind,
        StmtKind::Expr(ref e) if matches!(e.kind, ExprKind::Binary { op: BinaryOp::Instanceof, .. })
    ));
    assert!(matches!(
        m.body[2].kind,
        StmtKind::Expr(ref e) if matches!(e.kind, ExprKind::Binary { op: BinaryOp::In, .. })
    ));
    assert!(matches!(
        m.body[3].kind,
        StmtKind::Expr(ref e) if matches!(e.kind, ExprKind::Unary { op: UnaryOp::Delete, .. })
    ));
    assert!(matches!(
        m.body[4].kind,
        StmtKind::Expr(ref e) if matches!(e.kind, ExprKind::Unary { op: UnaryOp::Void, .. })
    ));
}

#[test]
fn hex_numeric_literal() {
    let m = parse("const h = 0x7f;");
    let StmtKind::VarDecl { decls, .. } = &m.body[0].kind else {
        panic!()
    };
    assert!(matches!(
        decls[0].init.as_ref().unwrap().kind,
        ExprKind::NumberLit(n) if (n - 127.0).abs() < f64::EPSILON
    ));
}

#[test]
fn await_expression_in_async_function() {
    let m = parse("async function f() { await g(); }");
    let StmtKind::FunctionDecl(func) = &m.body[0].kind else {
        panic!()
    };
    assert!(func.is_async);
    let StmtKind::Expr(e) = &func.body.as_ref().unwrap()[0].kind else {
        panic!()
    };
    assert!(matches!(e.kind, ExprKind::Await(_)));
}

#[test]
fn top_level_await() {
    let m = parse(r#"await player.run("title.ks");"#);
    let e = first_expr_stmt(&m);
    assert!(matches!(e.kind, ExprKind::Await(_)));
}

#[test]
fn object_literal_method_shorthand_and_getter() {
    let m = parse(
        r#"const o = {
            plain: 1,
            method(a) { return a; },
            get value() { return 1; },
            ...spread,
        };"#,
    );
    let StmtKind::VarDecl { decls, .. } = &m.body[0].kind else {
        panic!()
    };
    let ExprKind::ObjectLit(props) = &decls[0].init.as_ref().unwrap().kind else {
        panic!("オブジェクトリテラルを期待")
    };
    assert_eq!(props.len(), 4);
    assert!(matches!(props[1], ObjectProp::Method { kind: MethodKind::Normal, .. }));
    assert!(matches!(props[2], ObjectProp::Method { kind: MethodKind::Get, .. }));
    assert!(matches!(props[3], ObjectProp::Spread(_)));
}

// ---------- エラー系（構文が壊れている場合は Err になること） ----------

#[test]
fn unterminated_string_is_error() {
    parse_err("const a = \"unterminated;");
}

#[test]
fn unterminated_template_is_error() {
    parse_err("const a = `unterminated;");
}

#[test]
fn try_without_catch_or_finally_is_error() {
    parse_err("try { f(); }");
}

#[test]
fn stray_closing_brace_is_error() {
    parse_err("function f() { } }");
}
