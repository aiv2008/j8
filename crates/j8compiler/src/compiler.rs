use std::collections::HashSet;

use oxc_allocator::Allocator;
use oxc_ast::ast::{Directive, BindingIdentifier, BindingPattern, CatchClause, BlockStatement, Class, ClassElement, Expression, MethodDefinitionKind, PropertyKey, ForStatementInit, FormalParameters, Function as AstFunction, ForInStatement, ForOfStatement, ForStatementLeft, Program, Statement, SwitchStatement, TryStatement, VariableDeclaration, VariableDeclarationKind};
use oxc_parser::{ParseOptions, Parser};
use oxc_semantic::SemanticBuilder;
use oxc_span::{GetSpan, SourceType, Span};
use j8framework::{CodeKind, CompareOp, Error, ImportEntry, ImportName, ModuleInfo, Source};

use crate::lines::Lines;
use crate::builder::{Builder, JumpTable, Label, RegisterList};
use crate::scope::{Decl, DeclKind, Decls, Hidden, Key, Resolver, NEW_TARGET, THIS};
use j8bytecode::{Bytecode, ClassBoilerplate, ClassMember, Constant, Context, ContextVariable, MemberKind, CreateClosureFlags, FunctionKind, GlobalDeclaration, Module, Register, Runtime, ScopeInfo, ScopeType, SharedFunction, VariableMode, VariableRole, EvalDeclaration};

const COMPLETION_THROW: i32 = j8bytecode::ContinuationToken::RETHROW;

const COMPLETION_RETURN: i32 = 1;

const COMPLETION_ROUTE: i32 = 2;

const COMPLETION_NORMAL: i32 = j8bytecode::ContinuationToken::FALLTHROUGH;

#[derive(Clone, Copy)]
enum Binding {
    Reg(Register),
    Context(u32),
    Import(i32),
}

struct Bound<'a> {
    name: &'a str,
    binding: Binding,
    constant: bool,
    ready: Option<u32>,
    self_name: bool,
}

const DEFAULT_BINDING: &str = "*default*";

struct Level<'a> {
    func: usize,
    names: Vec<Bound<'a>>,
    context: Option<Register>,
    scope: Option<usize>,
    cells: bool,
    dynamic: bool,
    mark: i32,
    switch: bool,
    locals: usize,
}

#[derive(Clone, Copy)]
pub struct Resolved<'a> {
    pub place: Place<'a>,
    pub constant: bool,
    pub tdz: Option<(u32, bool)>,
}

impl Resolved<'_> {
    pub fn needs_check(&self, pos: u32) -> bool {
        match self.tdz {
            Some((ready, always)) => always || pos < ready,
            None => false,
        }
    }
}

#[derive(Clone, Copy)]
pub enum Place<'a> {
    Reg(Register),
    Context { depth: u32, slot: u32, cells: bool },
    Global(&'a str),
    Import(i32),
    Lookup(&'a str),
}

struct Jump<'a> {
    label: Option<&'a str>,
    breakable: bool,
    continuable: bool,
    breaks: Label,
    continues: Label,
    contexts: Vec<Register>,
}

#[derive(Clone, Copy)]
struct Route<'a> {
    span: Span,
    label: Option<&'a str>,
    cont: bool,
}

struct Finally<'a> {
    token: Register,
    value: Register,
    contexts: Vec<Register>,
    entry: Label,
    routes: Vec<Route<'a>>,
}

enum Control<'a> {
    Jump(Jump<'a>),
    Handler,
    Finally(Finally<'a>),
    Iterator(Register),
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum Kind {
    #[default]
    Normal,
    Arrow,
    Method,
    Getter,
    Setter,
    Constructor { derived: bool },
    Initializer { statik: bool },
}

#[derive(Clone, Copy, Default)]
pub struct Flavor {
    pub kind: Kind,
    pub generator: bool,
    pub asynchronous: bool,
    pub strict: bool,
    pub statik: bool,
}

impl From<(&AstFunction<'_>, Kind)> for Flavor {
    fn from((f, kind): (&AstFunction<'_>, Kind)) -> Self {
        let strict = f.body.as_ref().is_some_and(|b| Self::use_strict(&b.directives));
        Self { kind, generator: f.generator, asynchronous: f.r#async, strict, statik: false }
    }
}

impl From<Kind> for Flavor {
    fn from(kind: Kind) -> Self {
        Self { kind, ..Self::default() }
    }
}

impl Flavor {
    pub fn use_strict(directives: &[Directive<'_>]) -> bool {
        directives.iter().any(|d| d.directive.as_str() == "use strict")
    }

    pub fn v8_kind(self, default: bool) -> FunctionKind {
        let (g, a, s) = (self.generator, self.asynchronous, self.statik);
        match self.kind {
            Kind::Arrow if a => FunctionKind::AsyncArrowFunction,
            Kind::Arrow => FunctionKind::ArrowFunction,
            Kind::Getter if s => FunctionKind::StaticGetterFunction,
            Kind::Getter => FunctionKind::GetterFunction,
            Kind::Setter if s => FunctionKind::StaticSetterFunction,
            Kind::Setter => FunctionKind::SetterFunction,
            Kind::Constructor { derived: false } if default => FunctionKind::DefaultBaseConstructor,
            Kind::Constructor { derived: true } if default => FunctionKind::DefaultDerivedConstructor,
            Kind::Constructor { derived: false } => FunctionKind::BaseConstructor,
            Kind::Constructor { derived: true } => FunctionKind::DerivedConstructor,
            Kind::Initializer { statik: false } => FunctionKind::ClassMembersInitializerFunction,
            Kind::Initializer { statik: true } => FunctionKind::ClassStaticInitializerFunction,
            Kind::Method => match (g, a, s) {
                (true, true, false) => FunctionKind::AsyncConciseGeneratorMethod,
                (true, true, true) => FunctionKind::StaticAsyncConciseGeneratorMethod,
                (true, false, false) => FunctionKind::ConciseGeneratorMethod,
                (true, false, true) => FunctionKind::StaticConciseGeneratorMethod,
                (false, true, false) => FunctionKind::AsyncConciseMethod,
                (false, true, true) => FunctionKind::StaticAsyncConciseMethod,
                (false, false, false) => FunctionKind::ConciseMethod,
                (false, false, true) => FunctionKind::StaticConciseMethod,
            },
            Kind::Normal => match (g, a) {
                (true, true) => FunctionKind::AsyncGeneratorFunction,
                (true, false) => FunctionKind::GeneratorFunction,
                (false, true) => FunctionKind::AsyncFunction,
                (false, false) => FunctionKind::NormalFunction,
            },
        }
    }
}

enum Exit {
    Jump(usize),
    Finally(usize),
    Function,
}

#[expect(clippy::struct_excessive_bools, reason = "state of a function being compiled: several unrelated flags")]
struct Frame<'a> {
    index: usize,
    b: Builder,
    name: Box<str>,
    flavor: Flavor,
    default_constructor: bool,
    length: u16,
    formals: u16,
    scope: Option<ScopeInfo>,
    contexts: Vec<Register>,
    loop_depth: u32,
    controls: Vec<Control<'a>>,
    label: Option<&'a str>,
    completion: Option<Register>,
    generator: Option<Register>,
    suspends: u32,
    home: Option<&'static str>,
    resume_table: Option<JumpTable>,
    async_generator: bool,
    strict: bool,
    annexb: Vec<u32>,
    var_level: Option<usize>,
    eval: bool,
    prologue: bool,
    simple_params: bool,
}

impl Frame<'_> {
    fn finish(self, lines: &Lines<'_>) -> SharedFunction {
        SharedFunction {
            name: self.name,
            kind: self.flavor.v8_kind(self.default_constructor),
            strict: self.strict,
            length: self.length,
            formal_parameters: self.formals,
            scope: self.scope,
            bytecode: self.b.finish(lines),
        }
    }
}

#[derive(Clone, Copy)]
pub enum Body<'a> {
    Statements(&'a [Statement<'a>]),
    Expression(&'a Expression<'a>),
    Fields { class: &'a Class<'a>, statik: bool },
    DefaultConstructor { derived: bool },
}

const RESUME_NEXT: i32 = 0;
const RESUME_RETURN: i32 = 1;
const RESUME_THROW: i32 = 2;

pub struct Compiler<'a, 's> {
    src: &'s Source<'s>,
    lines: Lines<'s>,
    pub alloc: &'a Allocator,
    used: HashSet<Key>,
    class_depth: u32,
    captured: HashSet<Key>,
    module: Vec<SharedFunction>,
    frames: Vec<Frame<'a>>,
    levels: Vec<Level<'a>>,
    pub chains: Vec<Label>,
    eval_fns: HashSet<u32>,
    suspends: std::collections::HashMap<u32, u32>,
    completion_assign: HashSet<u32>,
    completion_reset: HashSet<u32>,
    for_init_text: Option<&'a str>,
    pub classes: Vec<ClassInfo<'a>>,
    pub next_home: Option<&'static str>,
    pub hidden: bool,
    debug: DebugInfo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Private {
    Field,
    Method { statik: bool },
    Accessor { statik: bool, get: bool, set: bool },
}

pub struct ClassInfo<'a> {
    pub privates: Vec<(&'a str, Private)>,
    pub brands: [&'a str; 2],
    pub brand: bool,
    pub fields: bool,
}

#[derive(Default)]
struct Rewriter {
    is_set: bool,
    breakable: bool,
    assign: HashSet<u32>,
    reset: HashSet<u32>,
}

impl Rewriter {
    fn process<'a>(&mut self, stmts: impl DoubleEndedIterator<Item = &'a Statement<'a>>) {
        for s in stmts.rev() {
            if !(self.breakable || !self.is_set) {
                break;
            }
            self.visit(s);
        }
    }

    fn breakable(&mut self, f: impl FnOnce(&mut Self)) {
        let saved = core::mem::replace(&mut self.breakable, true);
        f(self);
        self.breakable = saved;
    }

    fn visit(&mut self, s: &Statement<'_>) {
        let start = s.span().start;
        match s {
            Statement::ExpressionStatement(_) => {
                if !self.is_set {
                    self.assign.insert(start);
                    self.is_set = true;
                }
            }
            Statement::BlockStatement(b) => self.process(b.body.iter()),
            Statement::IfStatement(i) => {
                let after = self.is_set;
                self.visit(&i.consequent);
                let in_then = self.is_set;
                self.is_set = after;
                if let Some(a) = &i.alternate {
                    self.visit(a);
                }
                if !(in_then && self.is_set) {
                    self.reset.insert(start);
                }
                self.is_set = true;
            }
            Statement::WhileStatement(_) | Statement::DoWhileStatement(_) | Statement::ForStatement(_) | Statement::ForInStatement(_) | Statement::ForOfStatement(_) => {
                let body = match s {
                    Statement::WhileStatement(w) => &w.body,
                    Statement::DoWhileStatement(w) => &w.body,
                    Statement::ForStatement(f) => &f.body,
                    Statement::ForInStatement(f) => &f.body,
                    Statement::ForOfStatement(f) => &f.body,
                    _ => unreachable!("a loop"),
                };
                self.breakable(|r| r.visit(body));
                self.reset.insert(start);
                self.is_set = true;
            }
            Statement::SwitchStatement(sw) => {
                self.breakable(|r| {
                    for c in sw.cases.iter().rev() {
                        r.process(c.consequent.iter());
                    }
                });
                self.reset.insert(start);
                self.is_set = true;
            }
            Statement::TryStatement(t) => {
                let after = self.is_set;
                self.process(t.block.body.iter());
                let in_try = self.is_set;
                if let Some(h) = &t.handler {
                    self.is_set = after;
                    self.process(h.body.body.iter());
                }
                if !(in_try && self.is_set) {
                    self.reset.insert(start);
                }
                self.is_set = true;
            }
            Statement::WithStatement(w) => {
                self.visit(&w.body);
                if !self.is_set {
                    self.reset.insert(start);
                }
                self.is_set = true;
            }
            Statement::LabeledStatement(l) => self.breakable(|r| r.visit(&l.body)),
            Statement::BreakStatement(_) | Statement::ContinueStatement(_) => self.is_set = false,
            _ => {}
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DebugInfo {
    #[default]
    Omit,
    Record,
}

pub trait Compile: Sized {
    fn compile(src: &Source<'_>, kind: CodeKind, debug: DebugInfo) -> Result<Self, Error>;

    fn compile_direct(src: &Source<'_>, strict: bool, hidden: bool, debug: DebugInfo) -> Result<Self, Error>;

    fn compile_eval(src: &Source<'_>, kind: CodeKind, hidden: bool, debug: DebugInfo) -> Result<Self, Error>;

    fn compile_module(src: &Source<'_>, debug: DebugInfo) -> Result<(Self, ModuleInfo), Error>;
}

impl Compile for Module {
    fn compile(src: &Source<'_>, kind: CodeKind, debug: DebugInfo) -> Result<Self, Error> {
        Compiler::compile_in(src, kind, false, false, debug)
    }

    fn compile_direct(src: &Source<'_>, strict: bool, hidden: bool, debug: DebugInfo) -> Result<Self, Error> {
        Compiler::compile_in(src, if strict { CodeKind::StrictEval } else { CodeKind::Eval }, true, hidden, debug)
    }

    fn compile_eval(src: &Source<'_>, kind: CodeKind, hidden: bool, debug: DebugInfo) -> Result<Self, Error> {
        Compiler::compile_in(src, kind, false, hidden, debug)
    }

    fn compile_module(src: &Source<'_>, debug: DebugInfo) -> Result<(Self, ModuleInfo), Error> {
        Compiler::compile_module(src, debug)
    }
}

impl Compiler<'_, '_> {
    fn compile_in(src: &Source<'_>, kind: CodeKind, direct: bool, hidden: bool, debug: DebugInfo) -> Result<Module, Error> {
        let allocator = Allocator::default();
        let options = ParseOptions { parse_regular_expression: true, ..ParseOptions::default() };
        let ret = Parser::new(&allocator, src.text, SourceType::script()).with_options(options).parse();
        if let Some(d) = ret.diagnostics.first() {
            return Err(Error::Syntax(format!("{}: {d}", src.file)));
        }
        let program = &ret.program;
        let checked = SemanticBuilder::new().with_check_syntax_error(true).build(program);
        if let Some(d) = checked.diagnostics.errors().next() {
            return Err(Error::Syntax(format!("{}: {d}", src.file)));
        }
        let scoping = checked.semantic.scoping();
        let eval_scopes: HashSet<oxc_syntax::scope::ScopeId> = scoping.scope_descendants_from_root().filter(|&id| scoping.scope_flags(id).contains_direct_eval()).collect();
        let strict = kind == CodeKind::StrictEval || Flavor::use_strict(&program.directives);
        let local = (kind != CodeKind::Script).then(|| Decls::eval_local(&program.body, strict));
        let Resolver { captured, used, eval_fns, suspends, .. } = Resolver::script(&program.body, Decls::this_key(program.span), &allocator, local.as_ref(), strict, eval_scopes, None);
        let mut c = Compiler { src, lines: Lines::from(src.text), alloc: &allocator, captured, used, class_depth: 0, module: Vec::new(), frames: Vec::new(), levels: Vec::new(), chains: Vec::new(), eval_fns, suspends, completion_assign: HashSet::new(), completion_reset: HashSet::new(), for_init_text: None, classes: Vec::new(), next_home: None, hidden, debug };
        c.script(program, local.as_ref(), strict, direct)?;
        Ok(Module { functions: c.module })
    }
}

#[derive(Clone, Copy)]
enum ModuleName {
    Slot { slot: u16, lexical: bool, constant: bool },
    Import(u16),
}

struct Layout<'a> {
    names: Vec<(&'a str, ModuleName)>,
    info: ModuleInfo,
    slots: u16,
    local_exports: Vec<(&'a str, &'a str)>,
    imports: Vec<(&'a str, usize)>,
}

impl<'a> Layout<'a> {
    fn export_name(n: &oxc_ast::ast::ModuleExportName<'a>) -> &'a str {
        use oxc_ast::ast::ModuleExportName;
        match n {
            ModuleExportName::IdentifierName(i) => i.name.as_str(),
            ModuleExportName::IdentifierReference(i) => i.name.as_str(),
            ModuleExportName::StringLiteral(s) => s.value.as_str().unwrap_or_default(),
        }
    }

    fn request(&mut self, spec: &str) -> usize {
        match self.info.requests.iter().position(|r| **r == *spec) {
            Some(i) => i,
            None => {
                self.info.requests.push(spec.into());
                self.info.requests.len() - 1
            }
        }
    }

    fn declare(&mut self, name: &'a str, lexical: bool, constant: bool) {
        if self.names.iter().any(|(n, _)| *n == name) {
            return;
        }
        self.names.push((name, ModuleName::Slot { slot: self.slots, lexical, constant }));
        if lexical {
            self.info.lexical.push(self.slots);
        }
        self.slots += 1;
    }

    fn variables(&mut self, v: &'a VariableDeclaration<'a>) -> Vec<&'a str> {
        let mut ds = Decls::default();
        ds.declarators(v, DeclKind::Var);
        let lexical = v.kind != VariableDeclarationKind::Var;
        let names: Vec<&'a str> = ds.0.iter().map(|x| x.name).collect();
        for &n in &names {
            self.declare(n, lexical, v.kind == VariableDeclarationKind::Const);
        }
        names
    }

    fn declaration(&mut self, d: &'a oxc_ast::ast::Declaration<'a>) -> Vec<&'a str> {
        use oxc_ast::ast::Declaration;
        match d {
            Declaration::VariableDeclaration(v) => self.variables(v),
            Declaration::FunctionDeclaration(f) => f.id.iter().map(|id| {
                self.declare(id.name.as_str(), false, false);
                id.name.as_str()
            }).collect(),
            Declaration::ClassDeclaration(c) => c.id.iter().map(|id| {
                self.declare(id.name.as_str(), true, false);
                id.name.as_str()
            }).collect(),
            _ => Vec::new(),
        }
    }

    fn finish(mut self) -> (Vec<(&'a str, ModuleName)>, ModuleInfo) {
        for (exported, local) in core::mem::take(&mut self.local_exports) {
            if let Some(&(_, idx)) = self.imports.iter().find(|(n, _)| *n == local) {
                let e = self.info.imports[idx].clone();
                self.info.indirect_exports.push((exported.into(), e.request, e.name));
            } else if let Some(&(_, ModuleName::Slot { slot, .. })) = self.names.iter().find(|(n, _)| *n == local) {
                self.info.local_exports.push((exported.into(), slot));
            }
        }
        self.info.slots = self.slots;
        (self.names, self.info)
    }
}

impl Compiler<'_, '_> {
    fn compile_module(src: &Source<'_>, debug: DebugInfo) -> Result<(Module, ModuleInfo), Error> {
        let allocator = Allocator::default();
        let options = ParseOptions { parse_regular_expression: true, ..ParseOptions::default() };
        let ret = Parser::new(&allocator, src.text, SourceType::mjs()).with_options(options).parse();
        if let Some(d) = ret.diagnostics.first() {
            return Err(Error::Syntax(format!("{}: {d}", src.file)));
        }
        let program = &ret.program;
        let checked = SemanticBuilder::new().with_check_syntax_error(true).build(program);
        if let Some(d) = checked.diagnostics.errors().next() {
            return Err(Error::Syntax(format!("{}: {d}", src.file)));
        }
        let scoping = checked.semantic.scoping();
        let eval_scopes: HashSet<oxc_syntax::scope::ScopeId> = scoping.scope_descendants_from_root().filter(|&id| scoping.scope_flags(id).contains_direct_eval()).collect();
        let (names, info) = Compiler::module_layout(program);
        let order: Vec<&str> = names.iter().map(|&(n, _)| n).collect();
        let Resolver { captured, used, eval_fns, suspends, suspend_count, .. } = Resolver::script(&program.body, Decls::this_key(program.span), &allocator, None, true, eval_scopes, Some(&[order]));
        let mut c = Compiler { src, lines: Lines::from(src.text), alloc: &allocator, captured, used, class_depth: 0, module: Vec::new(), frames: Vec::new(), levels: Vec::new(), chains: Vec::new(), eval_fns, suspends, completion_assign: HashSet::new(), completion_reset: HashSet::new(), for_init_text: None, classes: Vec::new(), next_home: None, hidden: false, debug };
        let bound = names
            .iter()
            .map(|&(name, n)| match n {
                ModuleName::Slot { slot, lexical, constant } => Bound { name, binding: Binding::Context(Context::MIN_SLOTS + u32::from(slot)), constant, ready: lexical.then_some(u32::MAX), self_name: false },
                ModuleName::Import(i) => Bound { name, binding: Binding::Import(-1 - i32::from(i)), constant: true, ready: None, self_name: false },
            })
            .collect();
        c.levels.push(Level { func: usize::MAX, names: bound, context: Some(Register::current_context()), scope: None, dynamic: false, mark: 0, switch: false, cells: false, locals: usize::MAX });
        c.module_body(program, suspend_count)?;
        Ok((Module { functions: c.module }, ModuleInfo { top_level_await: suspend_count > 0, ..info }))
    }

    fn module_layout<'a>(program: &'a Program<'a>) -> (Vec<(&'a str, ModuleName)>, ModuleInfo) {
        use oxc_ast::ast::{ExportDefaultDeclarationKind, ImportDeclarationSpecifier};
        let mut l = Layout { names: alloc::vec![(THIS, ModuleName::Slot { slot: 0, lexical: false, constant: true })], info: ModuleInfo::default(), slots: 1, local_exports: Vec::new(), imports: Vec::new() };
        for s in &program.body {
            match s {
                Statement::ImportDeclaration(i) => {
                    let req = l.request(i.source.value.as_str().unwrap_or_default());
                    for sp in i.specifiers.iter().flatten() {
                        let (local, name) = match sp {
                            ImportDeclarationSpecifier::ImportSpecifier(x) => (x.local.name.as_str(), ImportName::Named(Layout::export_name(&x.imported).into())),
                            ImportDeclarationSpecifier::ImportDefaultSpecifier(x) => (x.local.name.as_str(), ImportName::Named("default".into())),
                            ImportDeclarationSpecifier::ImportNamespaceSpecifier(x) => (x.local.name.as_str(), ImportName::Namespace),
                        };
                        let idx = u16::try_from(l.info.imports.len()).unwrap_or(u16::MAX);
                        l.info.imports.push(ImportEntry { request: req, name });
                        l.names.push((local, ModuleName::Import(idx)));
                        l.imports.push((local, usize::from(idx)));
                    }
                }
                Statement::ExportDeclaration(e) => {
                    for n in l.declaration(&e.declaration) {
                        l.local_exports.push((n, n));
                    }
                }
                Statement::ExportFromDeclaration(e) => {
                    let req = l.request(e.source.value.as_str().unwrap_or_default());
                    for sp in &e.specifiers {
                        l.info.indirect_exports.push((Layout::export_name(&sp.exported).into(), req, ImportName::Named(Layout::export_name(&sp.local).into())));
                    }
                }
                Statement::ExportNamedDeclaration(e) => {
                    for sp in &e.specifiers {
                        l.local_exports.push((Layout::export_name(&sp.exported), Layout::export_name(&sp.local)));
                    }
                }
                Statement::ExportDefaultDeclaration(e) => {
                    let local = match &e.declaration {
                        ExportDefaultDeclarationKind::FunctionDeclaration(f) => {
                            let n = f.id.as_ref().map_or(DEFAULT_BINDING, |id| id.name.as_str());
                            l.declare(n, false, false);
                            n
                        }
                        ExportDefaultDeclarationKind::ClassDeclaration(c) => {
                            let n = c.id.as_ref().map_or(DEFAULT_BINDING, |id| id.name.as_str());
                            l.declare(n, true, false);
                            n
                        }
                        _ => {
                            l.declare(DEFAULT_BINDING, true, false);
                            DEFAULT_BINDING
                        }
                    };
                    l.local_exports.push(("default", local));
                }
                Statement::ExportAllDeclaration(e) => {
                    let req = l.request(e.source.value.as_str().unwrap_or_default());
                    match &e.exported {
                        Some(n) => l.info.indirect_exports.push((Layout::export_name(n).into(), req, ImportName::Namespace)),
                        None => l.info.star_exports.push(req),
                    }
                }
                Statement::VariableDeclaration(v) => _ = l.variables(v),
                Statement::FunctionDeclaration(f) => {
                    if let Some(id) = &f.id {
                        l.declare(id.name.as_str(), false, false);
                    }
                }
                Statement::ClassDeclaration(c) => {
                    if let Some(id) = &c.id {
                        l.declare(id.name.as_str(), true, false);
                    }
                }
                _ => {}
            }
        }
        let mut vars = Decls::default();
        vars.vars(&program.body);
        for d in &vars.0 {
            l.declare(d.name, false, false);
        }
        l.finish()
    }
}

impl<'a> Compiler<'a, '_> {
    pub fn unsupported(&self, span: Span, what: &str) -> Error {
        self.src.unsupported(span.start, what)
    }

    fn annexb_copy(&mut self, f: &'a oxc_ast::ast::Function<'a>) {
        let Some(id) = &f.id else { return };
        if !self.frame().annexb.contains(&f.span.start) {
            return;
        }
        let name = id.name.as_str();
        self.load(name);
        let (var_level, eval) = (self.frame().var_level, self.frame().eval);
        match var_level {
            Some(level) => {
                let mut depth = 0;
                for (i, l) in self.levels.iter().enumerate().rev() {
                    if i <= level
                        && let Some(n) = l.names.iter().find(|n| n.name == name)
                    {
                        match n.binding {
                            Binding::Reg(r) => self.b().star(r),
                            Binding::Context(slot) => self.b().store_context_slot(Register::current_context(), slot, depth),
                            Binding::Import(_) => self.throw_const_assign(),
                        }
                        return;
                    }
                    if l.context.is_some() {
                        depth += 1;
                    }
                }
            }
            None if eval => {
                let flags = j8bytecode::StoreLookupSlotFlags::LOOKUP_HOISTING;
                self.b().store_lookup_slot(name, flags);
            }
            None => self.b().store_global(name, false),
        }
    }

    pub fn throw_const_assign(&mut self) {
        self.b().call_runtime(Runtime::ThrowConstAssignError, RegisterList::EMPTY);
    }

    fn is_self_name(&self, name: &str) -> bool {
        self.levels.iter().rev().find_map(|l| l.names.iter().find(|n| n.name == name)).is_some_and(|n| n.self_name)
    }

    pub fn strict_here(&self) -> bool {
        self.strict() || self.class_depth > 0
    }

    pub fn strict(&self) -> bool {
        self.frames.last().is_some_and(|f| f.strict)
    }

    fn frame(&mut self) -> &mut Frame<'a> {
        self.frames.last_mut().expect("inside a function")
    }

    pub fn b(&mut self) -> &mut Builder {
        &mut self.frame().b
    }

    pub fn constant(&mut self, c: Constant) -> usize {
        self.b().constant(c)
    }

    pub fn name(&mut self, s: &str) -> usize {
        self.b().name(s)
    }

    pub fn temps(&mut self, n: u32) -> Register {
        self.b().new_register_list(n).first
    }

    pub fn temp(&mut self) -> Register {
        self.temps(1)
    }

    pub fn release(&mut self, r: Register) {
        self.b().release_to(r.0);
    }

    pub fn closure(&mut self, index: u32) {
        let k = self.constant(Constant::Function(index));
        let in_function = self.frames.len() > 1 || self.levels.first().is_some_and(|l| l.func == usize::MAX);
        self.b().create_closure(k, CreateClosureFlags::encode(false, in_function));
    }

    fn enter(&mut self, decls: &Decls<'a>, scope_type: ScopeType) -> Result<(), Error> {
        let func = self.frames.len() - 1;
        let mark = self.b().mark();
        let mut slots = 0u32;
        let mut names = Vec::with_capacity(decls.0.len());
        let mut variables = Vec::new();
        for d in &decls.0 {
            if matches!(d.kind, DeclKind::NewTarget | DeclKind::Arguments) && !self.used.contains(&d.key) {
                continue;
            }
            let binding = if self.captured.contains(&d.key) {
                slots += 1;
                let mode = match d.kind {
                    DeclKind::Let => VariableMode::Let,
                    DeclKind::Const | DeclKind::SelfName => VariableMode::Const,
                    _ => VariableMode::Var,
                };
                let role = match d.kind {
                    DeclKind::Param(i) => VariableRole::Parameter(i),
                    DeclKind::SelfName => VariableRole::FunctionName,
                    _ => VariableRole::Local,
                };
                variables.push(ContextVariable { name: d.name.into(), mode, role });
                Binding::Context(Context::MIN_SLOTS + slots - 1)
            } else {
                match d.kind {
                    DeclKind::This => Binding::Reg(Register::receiver()),
                    DeclKind::SelfName => Binding::Reg(Register::function_closure()),
                    DeclKind::Param(i) => Binding::Reg(Register::parameter(i32::from(i) + 1)),
                    _ => Binding::Reg(self.temp()),
                }
            };
            names.push(Bound { name: d.name, binding, constant: matches!(d.kind, DeclKind::Const), ready: d.ready, self_name: matches!(d.kind, DeclKind::SelfName) });
        }
        let mut scope = None;
        let context = if slots > 0 {
            let info = ScopeInfo { scope_type: Some(scope_type), variables, has_extension: false };
            let k = self.constant(Constant::Scope(info.clone()));
            scope = Some(k);
            match scope_type {
                ScopeType::FunctionScope => {
                    self.frame().scope = Some(info);
                    self.b().create_function_context(k, slots);
                }
                _ => self.b().create_block_context(k),
            }
            let saved = self.temp();
            self.b().push_context(saved);
            self.frame().contexts.push(saved);
            Some(saved)
        } else {
            None
        };
        let bindings: Vec<Binding> = names.iter().map(|n| n.binding).collect();
        let decls: Vec<&Decl<'a>> = decls.0.iter().filter(|d| !(matches!(d.kind, DeclKind::NewTarget | DeclKind::Arguments) && !self.used.contains(&d.key))).collect();
        let locals = self.open_locals(&decls, &bindings);
        self.levels.push(Level { func, names, context, scope, dynamic: false, mark, switch: false, cells: false, locals });
        for (d, &b) in decls.iter().zip(&bindings) {
            let skip = self.for_init_text.is_some_and(|t| !t.contains(d.name)) && matches!(b, Binding::Reg(_));
            if d.ready.is_some() && !skip {
                self.b().load_the_hole();
                self.bind(b);
            }
        }
        for (d, b) in decls.iter().zip(bindings) {
            let fixed = match d.kind {
                DeclKind::This => Some(Register::receiver()),
                DeclKind::SelfName => Some(Register::function_closure()),
                DeclKind::Param(i) => Some(Register::parameter(i32::from(i) + 1)),
                DeclKind::Function(f) => {
                    let n = self.function_declaration(f)?;
                    self.closure(n);
                    self.bind(b);
                    None
                }
                DeclKind::NewTarget if self.frame().generator.is_some() => None,
                DeclKind::NewTarget if self.used.contains(&d.key) => {
                    let r = self.temp();
                    self.b().new_target_or_generator = Some(r.0);
                    self.b().ldar(r);
                    self.bind(b);
                    None
                }
                DeclKind::Arguments if self.used.contains(&d.key) => {
                    let mapped = self.frame().simple_params && !self.frame().strict;
                    self.b().create_arguments(mapped);
                    self.bind(b);
                    None
                }
                DeclKind::Var | DeclKind::Let | DeclKind::Const | DeclKind::NewTarget | DeclKind::Arguments => None,
            };
            if let (Some(r), Binding::Context(_)) = (fixed, b) {
                self.b().ldar(r);
                self.bind(b);
            }
        }
        Ok(())
    }

    fn bind(&mut self, b: Binding) {
        match b {
            Binding::Reg(r) => self.b().star(r),
            Binding::Context(slot) => self.b().store_context_slot(Register::current_context(), slot, 0),
            Binding::Import(_) => self.throw_const_assign(),
        }
    }

    fn leave(&mut self) {
        let l = self.levels.pop().expect("a scope to leave");
        if let Some(saved) = l.context {
            self.b().pop_context(saved);
            self.frame().contexts.pop();
        }
        self.b().close_locals(l.locals);
        self.b().release_to(l.mark);
    }

    fn open_locals(&mut self, decls: &[&Decl<'a>], bindings: &[Binding]) -> usize {
        let first = self.b().locals_len();
        if self.debug == DebugInfo::Omit {
            return first;
        }
        for (d, b) in decls.iter().zip(bindings) {
            if let (Binding::Reg(r), false) = (b, matches!(d.kind, DeclKind::This | DeclKind::NewTarget)) {
                self.b().open_local(d.name, *r);
            }
        }
        first
    }

    fn scoped(&mut self, decls: &Decls<'a>, f: impl FnOnce(&mut Self) -> Result<(), Error>) -> Result<(), Error> {
        self.enter(decls, ScopeType::BlockScope)?;
        f(self)?;
        self.leave();
        Ok(())
    }

    pub fn resolve(&self, name: &'a str) -> Resolved<'a> {
        let cur = self.frames.len() - 1;
        let mut depth = 0;
        for l in self.levels.iter().rev() {
            if let Some(n) = l.names.iter().find(|n| n.name == name) {
                let (place, in_context) = match n.binding {
                    Binding::Reg(_) if l.func != cur && name == THIS && l.func == 0 && self.frames.first().is_some_and(|f| !f.eval) => (Place::Global(THIS), false),
                    Binding::Reg(r) => {
                        assert_eq!(l.func, cur, "an outer function's binding `{name}` used from inside must be captured");
                        (Place::Reg(r), false)
                    }
                    Binding::Context(slot) => (Place::Context { depth, slot, cells: l.cells }, true),
                    Binding::Import(i) => (Place::Import(i), false),
                };
                return Resolved { place, constant: n.constant, tdz: n.ready.map(|r| (r, (in_context && l.func != cur) || l.switch)) };
            }
            if l.dynamic {
                return Resolved { place: Place::Lookup(name), constant: false, tdz: None };
            }
            if l.context.is_some() {
                depth += 1;
            }
        }
        if self.frames.first().is_some_and(|f| f.eval) {
            return Resolved { place: Place::Lookup(name), constant: false, tdz: None };
        }
        Resolved { place: Place::Global(name), constant: false, tdz: None }
    }

    pub fn register_of(&self, name: &'a str, pos: u32) -> Option<Register> {
        let r = self.resolve(name);
        match r.place {
            Place::Reg(reg) if !r.needs_check(pos) => Some(reg),
            _ => None,
        }
    }

    pub fn lookup(&self, name: &'a str) -> (Place<'a>, bool) {
        let r = self.resolve(name);
        (r.place, r.constant)
    }

    pub fn load(&mut self, name: &'a str) {
        self.load_at(name, 0);
    }

    pub fn load_typeof(&mut self, name: &'a str) {
        match self.resolve(name).place {
            Place::Global(g) if !matches!(g, "undefined" | "NaN" | "Infinity") => self.b().load_global(g, true),
            Place::Lookup(g) => self.b().load_lookup_slot(g, true),
            _ => self.load_at(name, 0),
        }
    }

    pub fn load_at(&mut self, name: &'a str, pos: u32) {
        let r = self.resolve(name);
        match r.place {
            Place::Reg(reg) => self.b().ldar(reg),
            Place::Context { depth, slot, cells } => {
                let immutable = r.constant && !r.needs_check(pos);
                self.b().load_context_slot(Register::current_context(), slot, depth, immutable, cells);
            }
            Place::Global("undefined") => self.b().load_undefined(),
            Place::Global("NaN") => self.b().load_number(f64::NAN),
            Place::Global("Infinity") => self.b().load_number(f64::INFINITY),
            Place::Global(g) => self.b().load_global(g, false),
            Place::Import(cell) => _ = self.b().raw(Bytecode::LdaModuleVariable, &[cell.into(), 0]),
            Place::Lookup(g) => self.b().load_lookup_slot(g, false),
        }
        if r.needs_check(pos) {
            if name == THIS {
                self.b().plain(Bytecode::ThrowSuperNotCalledIfHole);
            } else {
                self.b().throw_reference_error_if_hole(name);
            }
        }
    }

    pub fn store(&mut self, name: &'a str, init: bool) {
        self.store_at(name, init, 0);
    }

    pub fn store_at(&mut self, name: &'a str, init: bool, pos: u32) {
        if !init && self.is_self_name(name) {
            if self.strict_here() {
                self.throw_const_assign();
            }
            return;
        }
        let r = self.resolve(name);
        let check = !init && r.needs_check(pos);
        if check || (r.constant && !init) {
            let value = self.temp();
            self.b().star(value);
            if check {
                self.load_at(name, 0);
            }
            if r.constant && !init {
                self.throw_const_assign();
            }
            self.b().ldar(value);
            self.release(value);
            if r.constant && !init {
                return;
            }
        }
        match r.place {
            Place::Reg(reg) => self.b().star(reg),
            Place::Context { depth, slot, cells } => self.b().store_context_slot_cells(Register::current_context(), slot, depth, cells),
            Place::Global(g) => {
                let strict = self.strict_here();
                self.b().store_global(g, strict);
            }
            Place::Import(_) => self.throw_const_assign(),
            Place::Lookup(g) => {
                let flags = if self.strict_here() { j8bytecode::StoreLookupSlotFlags::STRICT } else { 0 };
                self.b().store_lookup_slot(g, flags);
            }
        }
    }

    #[expect(clippy::too_many_lines, reason = "function prologue follows V8's GenerateBytecodeBody step by step")]
    pub fn function(&mut self, name: &str, flavor: Flavor, this: Option<Key>, self_name: Option<&'a BindingIdentifier<'a>>, params: Option<&'a FormalParameters<'a>>, body: Body<'a>) -> Result<u32, Error> {
        let count = params.map_or(0, |p| p.items.len());
        let n = u16::try_from(count).map_err(|_| self.unsupported(params.map_or(Span::default(), |p| p.span), "more than 65535 parameters"))?;
        let index = self.module.len();
        self.module.push(SharedFunction { name: "".into(), kind: FunctionKind::NormalFunction, strict: false, length: 0, formal_parameters: 0, scope: None, bytecode: j8bytecode::BytecodeArray::default() });
        let length = u16::try_from(params.map_or(0, |p| p.items.iter().take_while(|i| i.initializer.is_none()).count())).unwrap_or(u16::MAX);
        let strict = flavor.strict || self.class_depth > 0 || self.frames.last().is_some_and(|f| f.strict);
        let default_constructor = matches!(body, Body::DefaultConstructor { .. });
        let simple_params = params.is_some_and(|p| p.rest.is_none() && p.items.iter().all(|i| i.initializer.is_none() && matches!(i.pattern, BindingPattern::BindingIdentifier(_))));
        let next_home = self.next_home.take();
        let home = if flavor.kind == Kind::Arrow { self.frames.last().and_then(|f| f.home) } else { next_home };
        self.frames.push(Frame {
            index,
            b: Builder::new(n + 1, 0),
            name: name.into(),
            flavor,
            default_constructor,
            length,
            formals: n,
            scope: None,
            contexts: Vec::new(),
            loop_depth: 0,
            controls: Vec::new(),
            label: None,
            completion: None,
            generator: None,
            suspends: 0,
            resume_table: None,
            home,
            async_generator: flavor.generator && flavor.asynchronous,
            strict,
            annexb: Vec::new(),
            var_level: None,
            eval: false,
            prologue: false,
            simple_params,
        });
        if flavor.generator || flavor.asynchronous {
            let g = self.temp();
            self.b().new_target_or_generator = Some(g.0);
            self.frame().generator = Some(g);
            let count = params.and_then(|p| self.suspends.get(&p.span.start)).copied().unwrap_or(0);
            if count > 0 {
                let mut table = self.b().jump_table(count, 0);
                self.b().switch_on_generator_state(g, &mut table);
                self.frame().resume_table = Some(table);
            }
        }
        let stmts = match body {
            Body::Statements(s) => s,
            _ => &[],
        };
        let sloppy_eval = params.is_some_and(|p| !strict && self.eval_fns.contains(&p.span.start));
        let mut decls = Decls::function(this, self_name, params, stmts, !strict, None);
        if simple_params && !strict && decls.0.iter().any(|d| matches!(d.kind, DeclKind::Arguments) && self.used.contains(&d.key)) {
            for d in &decls.0 {
                if let DeclKind::Param(_) = d.kind {
                    self.captured.insert(d.key);
                }
            }
        }
        let super_in_arrow = flavor.kind == (Kind::Constructor { derived: true })
            && params.is_some_and(|p| {
                let text = &self.src.text[p.span.start as usize..];
                let body = stmts.last().map_or(0, |st| st.span().end as usize).saturating_sub(p.span.start as usize);
                let text = &text[..body.min(text.len())];
                text.contains("=>") && text.contains("super")
            });
        if flavor.kind == (Kind::Constructor { derived: true }) {
            for d in decls.0.iter().filter(|d| matches!(d.kind, DeclKind::NewTarget | DeclKind::This)) {
                self.used.insert(d.key);
                if super_in_arrow {
                    self.captured.insert(d.key);
                }
            }
            if super_in_arrow && let Some(p) = params {
                let key = Decls::key(p.span);
                self.captured.insert(key);
                decls.0.push(Decl { name: Hidden::THIS_FUNCTION, key, kind: DeclKind::Const, ready: None });
            }
        }
        self.enter(&decls, ScopeType::FunctionScope)?;
        if super_in_arrow {
            self.b().ldar(Register::function_closure());
            self.store(Hidden::THIS_FUNCTION, true);
        }
        if sloppy_eval && let Some(l) = self.levels.last_mut() {
            l.dynamic = true;
            if let Some(s) = self.frames.last_mut().and_then(|f| f.scope.as_mut()) {
                s.has_extension = true;
            }
        }
        if !strict {
            self.frame().var_level = Some(self.levels.len() - 1);
            self.frame().annexb = Decls::annexb_functions(stmts, params).iter().map(|f| f.span.start).collect();
        }
        if flavor.kind == (Kind::Constructor { derived: true })
            && let Some(t) = self.levels.last_mut().and_then(|l| l.names.iter_mut().find(|n| n.name == THIS))
        {
            t.ready = Some(u32::MAX);
        }
        if flavor.kind == (Kind::Constructor { derived: false }) {
            self.load(THIS);
            let this = self.temp();
            self.b().star(this);
            self.instance_init(this);
            self.release(this);
        }
        let async_function = flavor.asynchronous && !flavor.generator;
        if let Some(p) = params
            && !async_function
        {
            self.frame().prologue = true;
            self.prologue(p)?;
            self.frame().prologue = false;
        }
        if flavor.generator && !flavor.asynchronous {
            self.generator_object(Runtime::InlineCreateJSGeneratorObject);
            self.yield_value();
        }
        if flavor.generator && flavor.asynchronous {
            self.generator_object(Runtime::InlineCreateJSGeneratorObject);
            let g = self.frame().generator.expect("a generator register");
            self.try_finally(j8bytecode::CatchPrediction::AsyncAwait, |c| c.try_catch(j8bytecode::CatchPrediction::AsyncAwait, |c| {
                c.yield_value();
                c.function_body(body)
            }, |c, _| {
                let args = c.b().new_register_list(2);
                c.b().mov(g, args.first);
                c.b().star(args.at(1));
                c.b().load_the_hole();
                c.b().plain(Bytecode::SetPendingMessage);
                c.b().call_runtime(Runtime::InlineAsyncGeneratorReject, args);
                c.release(args.first);
                c.ret();
                Ok(())
            }), |c, _| {
                let arg = c.temp();
                c.b().mov(g, arg);
                c.b().call_runtime(Runtime::InlineGeneratorClose, RegisterList { first: arg, count: 1 });
                c.release(arg);
                Ok(())
            })?;
        } else if flavor.asynchronous {
            self.generator_object(Runtime::InlineAsyncFunctionEnter);
            self.try_catch(j8bytecode::CatchPrediction::AsyncAwait, |c| {
                if let Some(p) = params {
                    c.frame().prologue = true;
                    c.prologue(p)?;
                    c.frame().prologue = false;
                }
                c.function_body(body)
            }, |c, _| {
                c.reject_async_function();
                Ok(())
            })?;
        } else {
            self.function_body(body)?;
        }
        self.levels.pop();
        let f = self.frames.pop().expect("the function's frame");
        let at = f.index;
        self.module[at] = f.finish(&self.lines);
        Ok(u32::try_from(index).expect("fewer than 2^32 functions"))
    }

    fn reject_async_function(&mut self) {
        let g = self.frame().generator.expect("a generator register");
        let args = self.b().new_register_list(2);
        self.b().mov(g, args.first);
        self.b().star(args.at(1));
        self.b().load_the_hole();
        self.b().plain(Bytecode::SetPendingMessage);
        self.b().call_runtime(Runtime::InlineAsyncFunctionReject, args);
        self.b().plain(Bytecode::Return);
        self.release(args.first);
    }

    fn function_body(&mut self, body: Body<'a>) -> Result<(), Error> {
        match body {
            Body::Statements(s) => self.statements(s)?,
            Body::Expression(e) => {
                self.expr(e)?;
                self.ret();
            }
            Body::Fields { class, statik } => self.fields(class, statik)?,
            Body::DefaultConstructor { derived } => {
                if derived {
                    self.super_call_forward();
                }
            }
        }
        self.b().load_undefined();
        self.ret();
        Ok(())
    }

    fn prologue(&mut self, params: &'a FormalParameters<'a>) -> Result<(), Error> {
        let saved = self.enter_parameters(params);
        for (i, p) in (0u16..).zip(&params.items) {
            let reg = Register::parameter(i32::from(i) + 1);
            if let Some(init) = &p.initializer {
                match saved.get(usize::from(i)) {
                    Some(&t) => self.b().ldar(t),
                    None => self.b().ldar(reg),
                }
                let mut skip = Label::default();
                self.b().jump(Bytecode::JumpIfNotUndefined, &mut skip);
                match &p.pattern {
                    BindingPattern::BindingIdentifier(id) => self.expr_named(init, id.name.as_str())?,
                    _ => self.expr(init)?,
                }
                self.b().star(reg);
                self.b().bind(&mut skip);
            } else if let Some(&t) = saved.get(usize::from(i)) {
                self.b().ldar(t);
                self.b().star(reg);
            }
            match &p.pattern {
                BindingPattern::BindingIdentifier(id) => {
                    let name = id.name.as_str();
                    let copied_on_entry = saved.is_empty() && p.initializer.is_none();
                    if !copied_on_entry && !matches!(self.lookup(name).0, Place::Reg(r) if r == reg) {
                        self.b().ldar(reg);
                        self.store(name, true);
                    }
                }
                pat => self.bind_pattern(pat, reg, true)?,
            }
        }
        if let Some(r) = &params.rest {
            self.b().create_rest_parameter();
            let t = self.temp();
            self.b().star(t);
            self.bind_pattern(&r.rest.argument, t, true)?;
            self.release(t);
        }
        for &t in saved.iter().rev() {
            self.release(t);
        }
        Ok(())
    }

    /// Non-simple parameter list: the incoming arguments are copied into temporaries, every parameter binding is set
    /// to the hole and marked ready at the end of its own parameter, so a default value that reads a later parameter
    /// (or itself) carries a hole check. Initialization then runs left to right, as the spec requires. A simple list
    /// needs none of this: nothing is evaluated between two initializations, so nothing can observe the dead zone.
    fn enter_parameters(&mut self, params: &'a FormalParameters<'a>) -> Vec<Register>{
        if self.frame().simple_params{
            return Vec::new();
        }
        let saved: Vec<Register> = params.items.iter().map(|_|{self.temp()}).collect();
        for (i, &t) in (0u16..).zip(&saved){
            self.b().ldar(Register::parameter(i32::from(i)+1));
            self.b().star(t);
        }
        if let Some(level) = self.levels.last_mut() {
            for p in &params.items {
                let BindingPattern::BindingIdentifier(id) = &p.pattern else {
                    continue;
                };
                let end = p.initializer.as_ref().map_or(p.pattern.span().end, |init| init.span().end.max(p.pattern.span().end));
                if let Some(b) = level.names.iter_mut().find(|b| b.name == id.name.as_str()) {
                    b.ready = Some(end);
                }
            }
        }
        for p in &params.items {
            if let BindingPattern::BindingIdentifier(id) = &p.pattern{

                self.b().load_the_hole();
                self.store(&id.name, true);
            }
        }
        saved
        // unimplemented!()
    }

    pub fn bind_pattern(&mut self, p: &'a BindingPattern<'a>, value: Register, init: bool) -> Result<(), Error> {
        match p {
            BindingPattern::BindingIdentifier(id) => {
                self.b().ldar(value);
                self.store(id.name.as_str(), init);
            }
            BindingPattern::AssignmentPattern(a) => {
                let name = match &a.left {
                    BindingPattern::BindingIdentifier(id) => Some(id.name.as_str()),
                    _ => None,
                };
                let t = self.with_default(value, &a.right, name)?;
                self.bind_pattern(&a.left, t, init)?;
                self.release(t);
            }
            BindingPattern::ObjectPattern(o) => {
                let n = u32::try_from(o.properties.len()).map_err(|_| self.unsupported(o.span, "too many properties"))?;
                if o.properties.is_empty() {
                    self.require_object(value);
                }
                let list = o.rest.as_ref().map(|_| {
                    let l = self.b().new_register_list(n + 1);
                    self.b().mov(value, l.first);
                    l
                });
                for (i, b) in (0u32..).zip(&o.properties) {
                    let t = self.temp();
                    match (self.static_name(&b.key, b.computed), list) {
                        (Some(name), None) => self.b().get_named(value, name),
                        (_, l) => {
                            let k = l.map_or_else(|| self.temp(), |l| l.at(i + 1));
                            self.property_key_into(&b.key, b.computed, k)?;
                            self.b().ldar(k);
                            self.b().get_keyed(value);
                            if l.is_none() {
                                self.release(k);
                            }
                        }
                    }
                    self.b().star(t);
                    self.bind_pattern(&b.value, t, init)?;
                    self.release(t);
                }
                if let (Some(r), Some(l)) = (&o.rest, list) {
                    self.b().call_runtime(Runtime::InlineCopyDataPropertiesWithExcludedPropertiesOnStack, l);
                    let t = self.temp();
                    self.b().star(t);
                    self.bind_pattern(&r.argument, t, init)?;
                    self.release(t);
                    self.release(l.first);
                }
            }
            BindingPattern::ArrayPattern(a) => {
                let iter = self.iterate(value);
                self.closing_on_throw(iter, |c| {
                    for e in &a.elements {
                        c.iterator_next(iter);
                        let Some(e) = e else { continue };
                        let t = c.temp();
                        c.b().star(t);
                        c.bind_pattern(e, t, init)?;
                        c.release(t);
                    }
                    match &a.rest {
                        Some(r) => {
                            c.iterator_rest(iter);
                            let t = c.temp();
                            c.b().star(t);
                            c.bind_pattern(&r.argument, t, init)?;
                            c.release(t);
                        }
                        None => c.iterator_close(iter),
                    }
                    Ok(())
                })?;
                self.release(iter);
            }
        }
        Ok(())
    }

    pub fn try_catch(&mut self, prediction: j8bytecode::CatchPrediction, body: impl FnOnce(&mut Self) -> Result<(), Error>, catch: impl FnOnce(&mut Self, Register) -> Result<(), Error>) -> Result<(), Error> {
        let ctx = self.temp();
        self.b().mov(Register::current_context(), ctx);
        let h = self.b().new_handler(ctx, prediction);
        self.b().try_start(h);
        self.frame().controls.push(Control::Handler);
        body(self)?;
        self.frame().controls.pop();
        self.b().try_end(h);
        let mut done = Label::default();
        self.b().jump(Bytecode::Jump, &mut done);
        self.b().handler_target(h);
        catch(self, ctx)?;
        self.b().bind(&mut done);
        self.release(ctx);
        Ok(())
    }

    pub fn closing_on_throw(&mut self, iter: Register, f: impl FnOnce(&mut Self) -> Result<(), Error>) -> Result<(), Error> {
        self.try_catch(j8bytecode::CatchPrediction::Uncaught, f, |c, _| {
            let e = c.temp();
            c.b().star(e);
            c.iterator_close_quiet(iter);
            c.b().ldar(e);
            c.b().plain(Bytecode::ReThrow);
            c.release(e);
            Ok(())
        })
    }

    pub fn iterate(&mut self, value: Register) -> Register {
        let rec = self.temps(3);
        self.b().get_iterator(value);
        self.b().star(rec);
        self.b().get_named(rec, "next");
        self.b().star(Register(rec.0 + 1));
        self.b().load_bool(false);
        self.b().star(Register(rec.0 + 2));
        rec
    }

    fn step_result(&mut self, rec: Register) {
        let args = RegisterList { first: rec, count: 1 };
        self.b().call_property(Register(rec.0 + 1), args);
        let mut ok = Label::default();
        self.b().jump(Bytecode::JumpIfJSReceiver, &mut ok);
        let r = self.temp();
        self.b().star(r);
        self.b().call_runtime(Runtime::ThrowIteratorResultNotAnObject, RegisterList { first: r, count: 1 });
        self.release(r);
        self.b().bind(&mut ok);
    }

    pub fn iterator_next(&mut self, rec: Register) {
        let done = Register(rec.0 + 2);
        let mut finished = Label::default();
        let mut end = Label::default();
        self.b().ldar(done);
        self.b().jump(Bytecode::JumpIfTrue, &mut finished);
        self.b().load_bool(true);
        self.b().star(done);
        self.step_result(rec);
        let r = self.temp();
        self.b().star(r);
        self.b().get_named(r, "done");
        self.b().jump(Bytecode::JumpIfToBooleanTrue, &mut finished);
        self.b().get_named(r, "value");
        self.b().star(r);
        self.b().load_bool(false);
        self.b().star(done);
        self.b().ldar(r);
        self.release(r);
        self.b().jump(Bytecode::Jump, &mut end);
        self.b().bind(&mut finished);
        self.b().load_undefined();
        self.b().bind(&mut end);
    }

    pub fn iterator_rest(&mut self, rec: Register) {
        let done = Register(rec.0 + 2);
        let arr = self.temp();
        let index = self.temp();
        let r = self.temp();
        self.b().create_empty_array_literal();
        self.b().star(arr);
        self.b().load_smi(0);
        self.b().star(index);
        let mut end = Label::default();
        self.b().ldar(done);
        self.b().jump(Bytecode::JumpIfTrue, &mut end);
        self.b().load_bool(true);
        self.b().star(done);
        let header = self.b().loop_header();
        self.frame().loop_depth += 1;
        self.step_result(rec);
        self.b().star(r);
        self.b().get_named(r, "done");
        self.b().jump(Bytecode::JumpIfToBooleanTrue, &mut end);
        self.b().get_named(r, "value");
        self.b().store_in_array_literal(arr, index);
        self.b().ldar(index);
        self.b().unary(Bytecode::Inc);
        self.b().star(index);
        let depth = self.frame().loop_depth - 1;
        self.b().jump_loop(header, depth);
        self.frame().loop_depth -= 1;
        self.b().bind(&mut end);
        self.b().ldar(arr);
        self.release(arr);
    }

    pub fn iterator_close(&mut self, rec: Register) {
        let mut end = Label::default();
        self.b().ldar(Register(rec.0 + 2));
        self.b().jump(Bytecode::JumpIfTrue, &mut end);
        self.b().load_bool(true);
        self.b().star(Register(rec.0 + 2));
        self.b().get_named(rec, "return");
        self.b().jump(Bytecode::JumpIfUndefinedOrNull, &mut end);
        let m = self.temp();
        self.b().star(m);
        self.b().call_property(m, RegisterList { first: rec, count: 1 });
        self.b().jump(Bytecode::JumpIfJSReceiver, &mut end);
        self.b().star(m);
        self.b().call_runtime(Runtime::ThrowIteratorResultNotAnObject, RegisterList { first: m, count: 1 });
        self.release(m);
        self.b().bind(&mut end);
    }

    pub fn iterator_close_quiet(&mut self, rec: Register) {
        let mut end = Label::default();
        self.b().ldar(Register(rec.0 + 2));
        self.b().jump(Bytecode::JumpIfTrue, &mut end);
        let _ = self.try_catch(j8bytecode::CatchPrediction::Caught, |c| {
            let mut skip = Label::default();
            c.b().get_named(rec, "return");
            c.b().jump(Bytecode::JumpIfUndefinedOrNull, &mut skip);
            let m = c.temp();
            c.b().star(m);
            c.b().call_property(m, RegisterList { first: rec, count: 1 });
            c.release(m);
            c.b().bind(&mut skip);
            Ok(())
        }, |_, _| Ok(()));
        self.b().bind(&mut end);
    }

    fn for_left(&mut self, left: &'a ForStatementLeft<'a>, v: Register) -> Result<(), Error> {
        match left {
            ForStatementLeft::VariableDeclaration(d) => {
                let decl = d.declarations.first().ok_or_else(|| self.unsupported(d.span, "an empty declaration"))?;
                self.bind_pattern(&decl.id, v, true)
            }
            t => self.assign_target(t.as_assignment_target().ok_or_else(|| self.unsupported(t.span(), "for-in / for-of target"))?, v),
        }
    }

    fn for_of(&mut self, f: &'a ForOfStatement<'a>) -> Result<(), Error> {
        if f.r#await {
            return self.for_await(f);
        }
        let label = self.take_label();
        self.expr(&f.right)?;
        let src = self.temp();
        self.b().star(src);
        let iter = self.iterate(src);
        self.frame().controls.push(Control::Iterator(iter));
        let v = self.temp();
        let header = self.b().loop_header();
        self.frame().loop_depth += 1;
        let mut exit = Label::default();
        self.b().ldar(Register(iter.0 + 2));
        self.b().jump(Bytecode::JumpIfTrue, &mut exit);
        self.iterator_next(iter);
        self.b().star(v);
        self.b().ldar(Register(iter.0 + 2));
        self.b().jump(Bytecode::JumpIfTrue, &mut exit);
        self.push_jump(label, true, true);
        self.closing_on_throw(iter, |c| {
            c.scoped(&Decls::for_head(&f.left), |c| {
                c.for_left(&f.left, v)?;
                c.statement(&f.body)
            })
        })?;
        self.bind_continues();
        let depth = self.frame().loop_depth - 1;
        self.b().jump_loop(header, depth);
        self.frame().loop_depth -= 1;
        self.pop_jump();
        self.iterator_close(iter);
        self.b().bind(&mut exit);
        self.frame().controls.pop();
        self.release(src);
        Ok(())
    }

    fn for_await(&mut self, f: &'a ForOfStatement<'a>) -> Result<(), Error> {
        let label = self.take_label();
        self.expr(&f.right)?;
        let src = self.temp();
        self.b().star(src);
        let rec = self.async_iterate(src);
        let done = Register(rec.0 + 2);
        let next_result = self.temp();
        self.b().load_bool(false);
        self.b().star(done);
        let prediction = j8bytecode::CatchPrediction::AsyncAwait;
        self.try_finally(prediction, |c| {
            let header = c.b().loop_header();
            c.frame().loop_depth += 1;
            let mut exit = Label::default();
            c.b().load_bool(true);
            c.b().star(done);
            c.b().call_property(Register(rec.0 + 1), RegisterList { first: rec, count: 1 });
            c.await_value();
            let mut ok = Label::default();
            c.b().jump(Bytecode::JumpIfJSReceiver, &mut ok);
            c.b().star(next_result);
            c.b().call_runtime(Runtime::ThrowIteratorResultNotAnObject, RegisterList { first: next_result, count: 1 });
            c.b().bind(&mut ok);
            c.b().star(next_result);
            c.b().get_named(next_result, "done");
            c.b().jump(Bytecode::JumpIfToBooleanTrue, &mut exit);
            c.b().get_named(next_result, "value");
            c.b().star(next_result);
            c.b().load_bool(false);
            c.b().star(done);
            c.push_jump(label, true, true);
            c.scoped(&Decls::for_head(&f.left), |c| {
                c.for_left(&f.left, next_result)?;
                c.statement(&f.body)
            })?;
            c.bind_continues();
            let depth = c.frame().loop_depth - 1;
            c.b().jump_loop(header, depth);
            c.frame().loop_depth -= 1;
            c.pop_jump();
            c.b().bind(&mut exit);
            Ok(())
        }, |c, token| {
            c.finalize_iteration(rec, done, token, prediction);
            Ok(())
        })?;
        self.release(src);
        Ok(())
    }

    fn async_iterate(&mut self, src: Register) -> Register {
        let rec = self.temps(3);
        let mut sync = Label::default();
        let mut got = Label::default();
        let k = self.constant(Constant::AsyncIteratorSymbol);
        self.b().load_constant(k);
        self.b().get_keyed(src);
        self.b().jump(Bytecode::JumpIfUndefinedOrNull, &mut sync);
        let method = Register(rec.0 + 1);
        self.b().star(method);
        self.b().call_property(method, RegisterList { first: src, count: 1 });
        self.b().jump(Bytecode::JumpIfJSReceiver, &mut got);
        self.b().call_runtime(Runtime::ThrowSymbolAsyncIteratorInvalid, RegisterList::EMPTY);
        self.b().bind(&mut sync);
        self.b().get_iterator(src);
        self.b().star(rec);
        self.b().call_runtime(Runtime::InlineCreateAsyncFromSyncIterator, RegisterList { first: rec, count: 1 });
        self.b().bind(&mut got);
        self.b().star(rec);
        self.b().get_named(rec, "next");
        self.b().star(Register(rec.0 + 1));
        rec
    }

    fn finalize_iteration(&mut self, rec: Register, done: Register, token: Register, prediction: j8bytecode::CatchPrediction) {
        let mut is_done = Label::default();
        self.b().ldar(done);
        self.b().jump(Bytecode::JumpIfToBooleanTrue, &mut is_done);
        let mut closed = Label::default();
        let _ = self.try_catch(prediction, |c| {
            let method = c.temp();
            c.b().get_named(rec, "return");
            c.b().jump(Bytecode::JumpIfUndefinedOrNull, &mut closed);
            c.b().star(method);
            c.b().call_property(method, RegisterList { first: rec, count: 1 });
            c.await_value();
            c.b().jump(Bytecode::JumpIfJSReceiver, &mut closed);
            c.b().star(method);
            c.b().call_runtime(Runtime::ThrowIteratorResultNotAnObject, RegisterList { first: method, count: 1 });
            c.release(method);
            Ok(())
        }, |c, e| {
            c.b().star(e);
            let mut suppress = Label::default();
            c.b().load_smi(COMPLETION_THROW);
            c.b().test_reference_equal(token);
            c.b().jump(Bytecode::JumpIfTrue, &mut suppress);
            c.b().ldar(e);
            c.b().plain(Bytecode::ReThrow);
            c.b().bind(&mut suppress);
            Ok(())
        });
        self.b().bind(&mut closed);
        self.b().bind(&mut is_done);
    }

    fn for_in(&mut self, f: &'a ForInStatement<'a>) -> Result<(), Error> {
        let label = self.take_label();
        let mut exit = Label::default();
        self.expr(&f.right)?;
        self.b().jump(Bytecode::JumpIfUndefinedOrNull, &mut exit);
        let receiver = self.temp();
        self.b().to_object(receiver);
        self.b().for_in_enumerate(receiver);
        let cache = self.b().new_register_list(3);
        let slot = self.b().for_in_prepare(cache);
        let index = self.temp();
        self.b().load_smi(0);
        self.b().star(index);
        let v = self.temp();
        let header = self.b().loop_header();
        self.frame().loop_depth += 1;
        self.b().jump_if_for_in_done(&mut exit, index, cache.at(2));
        self.b().for_in_next(receiver, index, RegisterList { first: cache.first, count: 2 }, slot);
        self.push_jump(label, true, true);
        let mut skip = Label::default();
        self.b().jump(Bytecode::JumpIfUndefined, &mut skip);
        self.b().star(v);
        self.scoped(&Decls::for_head(&f.left), |c| {
            c.for_left(&f.left, v)?;
            c.statement(&f.body)
        })?;
        self.b().bind(&mut skip);
        self.bind_continues();
        self.b().for_in_step(index);
        let depth = self.frame().loop_depth - 1;
        self.b().jump_loop(header, depth);
        self.frame().loop_depth -= 1;
        self.pop_jump();
        self.b().bind(&mut exit);
        self.release(receiver);
        Ok(())
    }

    fn generator_object(&mut self, create: Runtime) {
        let g = self.frame().generator.expect("a generator register");
        let args = self.b().new_register_list(2);
        self.b().mov(Register::function_closure(), args.first);
        self.b().mov(Register::receiver(), args.at(1));
        self.b().call_runtime(create, args);
        self.b().star(g);
        self.release(args.first);
    }

    fn suspend_point(&mut self) {
        if !self.b().reachable() {
            return;
        }
        let g = self.frame().generator.expect("a generator register");
        let id = self.frame().suspends;
        self.frame().suspends += 1;
        let live = self.b().live_registers();
        self.b().suspend_generator(g, live, id);
        let table = self.frame().resume_table.take().expect("the resume table");
        self.b().bind_case(&table, i32::try_from(id).unwrap_or(i32::MAX));
        self.frame().resume_table = Some(table);
        self.b().resume_generator(g, live);
    }

    #[expect(clippy::too_many_lines, reason = "follows V8's VisitYieldStar step by step")]
    pub fn yield_star(&mut self, operand: &'a Expression<'a>) -> Result<(), Error> {
        let asynchronous = self.frame().async_generator;
        let g = self.frame().generator.expect("a generator register");
        let output = self.temp();
        let resume_mode = self.temp();
        let iterator_and_input = self.b().new_register_list(2);
        let (iterator, input) = (iterator_and_input.first, iterator_and_input.at(1));
        self.expr(operand)?;
        let next = self.temp();
        self.b().star(iterator);
        if asynchronous {
            let rec = self.async_iterate(iterator);
            self.b().mov(rec, iterator);
            self.b().mov(Register(rec.0 + 1), next);
            self.release(rec);
        } else {
            self.b().get_iterator(iterator);
            self.b().star(iterator);
            self.b().get_named(iterator, "next");
            self.b().star(next);
        }
        self.b().load_undefined();
        self.b().star(input);
        self.b().load_smi(RESUME_NEXT);
        self.b().star(resume_mode);
        let header = self.b().loop_header();
        self.frame().loop_depth += 1;
        let mut after_switch = Label::default();
        let mut table = self.b().jump_table(2, RESUME_RETURN);
        self.b().ldar(resume_mode);
        self.b().switch_on_smi(&mut table);
        self.b().call_property(next, iterator_and_input);
        self.b().jump(Bytecode::Jump, &mut after_switch);
        self.b().bind_case(&table, RESUME_RETURN);
        let mut no_return = Label::default();
        self.call_iterator_method(iterator, "return", iterator_and_input, &mut after_switch, &mut no_return);
        self.b().bind(&mut no_return);
        self.b().ldar(input);
        if asynchronous {
            self.await_value();
        }
        self.ret();
        self.b().bind_case(&table, RESUME_THROW);
        let mut no_throw = Label::default();
        self.call_iterator_method(iterator, "throw", iterator_and_input, &mut after_switch, &mut no_throw);
        self.b().bind(&mut no_throw);
        let mut closed = Label::default();
        let mut called = Label::default();
        self.call_iterator_method(iterator, "return", RegisterList { first: iterator, count: 1 }, &mut called, &mut closed);
        self.b().bind(&mut called);
        if asynchronous {
            self.await_value();
        }
        self.b().jump(Bytecode::JumpIfJSReceiver, &mut closed);
        let r = self.temp();
        self.b().star(r);
        self.b().call_runtime(Runtime::ThrowIteratorResultNotAnObject, RegisterList { first: r, count: 1 });
        self.release(r);
        self.b().bind(&mut closed);
        self.b().call_runtime(Runtime::ThrowThrowMethodMissing, RegisterList::EMPTY);
        self.b().bind(&mut after_switch);
        if asynchronous {
            self.await_value();
        }
        let mut check_if_done = Label::default();
        self.b().star(output);
        self.b().jump(Bytecode::JumpIfJSReceiver, &mut check_if_done);
        self.b().call_runtime(Runtime::ThrowIteratorResultNotAnObject, RegisterList { first: output, count: 1 });
        self.b().bind(&mut check_if_done);
        self.b().get_named(output, "done");
        let mut exit = Label::default();
        self.b().jump(Bytecode::JumpIfToBooleanTrue, &mut exit);
        if asynchronous {
            self.b().get_named(output, "value");
            let args = self.b().new_register_list(3);
            self.b().mov(g, args.first);
            self.b().star(args.at(1));
            self.b().load_bool(false);
            self.b().star(args.at(2));
            self.b().call_runtime(Runtime::InlineAsyncGeneratorResolve, args);
            self.release(args.first);
        } else {
            self.b().ldar(output);
        }
        self.suspend_point();
        self.b().star(input);
        self.b().call_runtime(Runtime::InlineGeneratorGetResumeMode, RegisterList { first: g, count: 1 });
        self.b().star(resume_mode);
        let depth = self.frame().loop_depth - 1;
        self.b().jump_loop(header, depth);
        self.frame().loop_depth -= 1;
        self.b().bind(&mut exit);
        self.release(iterator);
        let output_value = self.temp();
        self.b().get_named(output, "value");
        self.b().star(output_value);
        self.b().load_smi(RESUME_RETURN);
        self.b().test_reference_equal(resume_mode);
        let mut completion_is_output_value = Label::default();
        self.b().jump(Bytecode::JumpIfFalse, &mut completion_is_output_value);
        self.b().ldar(output_value);
        self.ret();
        self.b().bind(&mut completion_is_output_value);
        self.b().ldar(output_value);
        self.release(output);
        Ok(())
    }

    fn call_iterator_method(&mut self, iterator: Register, name: &str, receiver_and_args: RegisterList, called: &mut Label, missing: &mut Label) {
        let method = self.temp();
        self.b().get_named(iterator, name);
        self.b().jump(Bytecode::JumpIfUndefinedOrNull, missing);
        self.b().star(method);
        self.b().call_property(method, receiver_and_args);
        self.b().jump(Bytecode::Jump, called);
        self.release(method);
    }

    pub fn await_value(&mut self) {
        let g = self.frame().generator.expect("a generator register");
        let args = self.b().new_register_list(2);
        self.b().mov(g, args.first);
        self.b().star(args.at(1));
        let intrinsic = if self.frame().async_generator { Runtime::InlineAsyncGeneratorAwait } else { Runtime::InlineAsyncFunctionAwait };
        self.b().call_runtime(intrinsic, args);
        self.release(args.first);
        self.suspend_point();
        let input = self.temp();
        let resume_mode = self.temp();
        self.b().star(input);
        self.b().call_runtime(Runtime::InlineGeneratorGetResumeMode, RegisterList { first: g, count: 1 });
        self.b().star(resume_mode);
        self.b().load_smi(RESUME_NEXT);
        self.b().test_reference_equal(resume_mode);
        let mut resume_next = Label::default();
        self.b().jump(Bytecode::JumpIfTrue, &mut resume_next);
        self.b().ldar(input);
        self.b().plain(Bytecode::ReThrow);
        self.b().bind(&mut resume_next);
        self.b().ldar(input);
        self.release(input);
    }

    pub fn yield_value(&mut self) {
        let g = self.frame().generator.expect("a generator register");
        let asynchronous = self.frame().async_generator;
        if self.frame().suspends > 0 {
            let args = self.b().new_register_list(2);
            if asynchronous {
                self.b().mov(g, args.first);
                self.b().star(args.at(1));
                self.b().call_runtime(Runtime::InlineAsyncGeneratorYieldWithAwait, args);
            } else {
                self.b().star(args.first);
                self.b().mov(g, args.at(1));
                self.b().call_runtime(Runtime::InlineGeneratorYieldResult, args);
            }
            self.release(args.first);
        }
        self.suspend_point();
        let input = self.temp();
        self.b().star(input);
        self.b().call_runtime(Runtime::InlineGeneratorGetResumeMode, RegisterList { first: g, count: 1 });
        let mut table = self.b().jump_table(if asynchronous { 3 } else { 2 }, RESUME_NEXT);
        self.b().switch_on_smi(&mut table);
        if asynchronous {
            self.b().ldar(input);
            self.b().plain(Bytecode::ReThrow);
            self.b().bind_case(&table, RESUME_THROW);
        }
        self.b().ldar(input);
        self.b().plain(Bytecode::Throw);
        self.b().bind_case(&table, RESUME_RETURN);
        self.b().ldar(input);
        self.ret();
        self.b().bind_case(&table, RESUME_NEXT);
        self.b().ldar(input);
        self.release(input);
    }

    pub fn frame_loop_depth(&mut self) -> u32 {
        self.frame().loop_depth
    }

    pub fn with_default(&mut self, value: Register, default: &'a Expression<'a>, name: Option<&'a str>) -> Result<Register, Error> {
        let t = self.temp();
        self.b().ldar(value);
        self.b().star(t);
        let mut skip = Label::default();
        self.b().jump(Bytecode::JumpIfNotUndefined, &mut skip);
        match name {
            Some(n) => self.expr_named(default, n)?,
            None => self.expr(default)?,
        }
        self.b().star(t);
        self.b().bind(&mut skip);
        Ok(t)
    }

    pub fn require_object(&mut self, value: Register) {
        let mut ok = Label::default();
        self.b().ldar(value);
        self.b().jump(Bytecode::JumpIfUndefinedOrNull, &mut ok);
        let mut done = Label::default();
        self.b().jump(Bytecode::Jump, &mut done);
        self.b().bind(&mut ok);
        self.b().call_runtime(Runtime::ThrowPatternAssignmentNonCoercible, RegisterList { first: value, count: 1 });
        self.b().bind(&mut done);
    }

    pub fn property_key_into(&mut self, key: &'a PropertyKey<'a>, computed: bool, k: Register) -> Result<(), Error> {
        match self.static_key(key, computed) {
            Some(c) => {
                let c = c?;
                self.b().load_constant(c);
            }
            None => {
                self.expr(key.as_expression().ok_or_else(|| self.unsupported(key.span(), "property key"))?)?;
                self.b().plain(Bytecode::ToName);
            }
        }
        self.b().star(k);
        Ok(())
    }

    pub fn plain_function(&mut self, f: &'a AstFunction<'a>, self_name: bool, inferred: &str) -> Result<u32, Error> {
        let flavor = Flavor::from((f, Kind::Normal));
        let body = f.body.as_ref().ok_or_else(|| self.unsupported(f.span, "a function without a body"))?;
        let name = f.id.as_ref().map_or(inferred, |id| id.name.as_str());
        self.function(name, flavor, Some(Decls::this_key(f.span)), if self_name { f.id.as_ref() } else { None }, Some(&f.params), Body::Statements(&body.statements))
    }

    pub fn method(&mut self, name: &str, f: &'a AstFunction<'a>, kind: Kind) -> Result<u32, Error> {
        let flavor = Flavor::from((f, kind));
        let body = f.body.as_ref().ok_or_else(|| self.unsupported(f.span, "a method without a body"))?;
        self.function(name, flavor, Some(Decls::this_key(f.span)), None, Some(&f.params), Body::Statements(&body.statements))
    }

    pub fn static_name(&self, key: &'a PropertyKey<'a>, computed: bool) -> Option<&'a str> {
        if computed {
            return None;
        }
        match key {
            PropertyKey::StaticIdentifier(id) => Some(id.name.as_str()),
            PropertyKey::StringLiteral(s) => s.value.as_str(),
            PropertyKey::NumericLiteral(n) => Some(self.alloc.alloc_str(ryu_js::Buffer::new().format(n.value))),
            PropertyKey::PrivateIdentifier(id) => Some(Hidden::private(self.alloc, id)),
            _ => None,
        }
    }

    pub fn method_name(&self, key: &'a PropertyKey<'a>, computed: bool, kind: MethodDefinitionKind) -> &'a str {
        let Some(n) = self.static_name(key, computed) else { return "" };
        match kind {
            MethodDefinitionKind::Get => self.alloc.alloc_str(&format!("get {n}")),
            MethodDefinitionKind::Set => self.alloc.alloc_str(&format!("set {n}")),
            MethodDefinitionKind::Method | MethodDefinitionKind::Constructor => n,
        }
    }

    pub fn static_key(&mut self, key: &'a PropertyKey<'a>, computed: bool) -> Option<Result<usize, Error>> {
        if computed {
            return None;
        }
        Some(match key {
            PropertyKey::StaticIdentifier(id) => Ok(self.name(id.name.as_str())),
            PropertyKey::StringLiteral(s) => Ok(match s.value.as_str() {
                Some(t) => self.name(t),
                None => self.string_const(s.value),
            }),
            PropertyKey::NumericLiteral(n) => Ok(self.name(ryu_js::Buffer::new().format(n.value))),
            PropertyKey::PrivateIdentifier(id) => Err(self.unsupported(id.span, "private names")),
            _ => return None,
        })
    }

    pub fn class(&mut self, c: &'a Class<'a>, inferred: &'a str) -> Result<(), Error> {
        let info = self.class_info(c)?;
        let decls = Decls::class_scope(c, self.alloc);
        for d in &decls.0 {
            if d.name.starts_with(['.', '#', '%']) {
                self.captured.insert(d.key);
            }
        }
        self.class_depth += 1;
        self.enter(&decls, ScopeType::ClassScope)?;
        self.classes.push(info);
        let r = self.class_members(c, inferred);
        self.classes.pop();
        self.leave();
        self.class_depth -= 1;
        r
    }

    fn class_info(&self, c: &'a Class<'a>) -> Result<ClassInfo<'a>, Error> {
        let mut info = ClassInfo { privates: Vec::new(), brands: [Hidden::brand(self.alloc, c.span, false), Hidden::brand(self.alloc, c.span, true)], brand: false, fields: false };
        for e in &c.body.body {
            match e {
                ClassElement::MethodDefinition(m) => {
                    let PropertyKey::PrivateIdentifier(id) = &m.key else { continue };
                    let name = Hidden::private(self.alloc, id);
                    let statik = m.r#static;
                    info.brand |= !statik;
                    let (get, set) = (m.kind == MethodDefinitionKind::Get, m.kind == MethodDefinitionKind::Set);
                    match info.privates.iter_mut().find(|(n, _)| *n == name) {
                        Some((_, Private::Accessor { get: g, set: s, .. })) => {
                            *g |= get;
                            *s |= set;
                        }
                        Some(_) => {}
                        None => info.privates.push((name, if get || set { Private::Accessor { statik, get, set } } else { Private::Method { statik } })),
                    }
                }
                ClassElement::PropertyDefinition(p) => {
                    info.fields |= !p.r#static;
                    if let PropertyKey::PrivateIdentifier(id) = &p.key {
                        info.privates.push((Hidden::private(self.alloc, id), Private::Field));
                    }
                }
                ClassElement::StaticBlock(_) => {}
                e => return Err(self.unsupported(e.span(), "class element")),
            }
        }
        Ok(info)
    }

    fn class_method(&mut self, name: &str, f: &'a AstFunction<'a>, kind: Kind, statik: bool) -> Result<u32, Error> {
        let flavor = Flavor { statik, ..Flavor::from((f, kind)) };
        let body = f.body.as_ref().ok_or_else(|| self.unsupported(f.span, "a method without a body"))?;
        self.next_home = Some(if statik { Hidden::STATIC_HOME } else { Hidden::HOME });
        self.function(name, flavor, Some(Decls::this_key(f.span)), None, Some(&f.params), Body::Statements(&body.statements))
    }

    fn private_symbol(&mut self, create: Runtime, text: &str, binding: &'a str) {
        let k = self.name(text);
        let arg = self.temp();
        self.b().load_constant(k);
        self.b().star(arg);
        self.b().call_runtime(create, RegisterList { first: arg, count: 1 });
        self.release(arg);
        self.store(binding, true);
    }

    fn static_prototype_check(&mut self, key: Register) {
        let mut done = Label::default();
        self.b().load_string("prototype");
        self.b().compare(CompareOp::StrictEq, key);
        self.b().jump(Bytecode::JumpIfFalse, &mut done);
        self.b().call_runtime(Runtime::ThrowStaticPrototypeError, RegisterList::EMPTY);
        self.b().bind(&mut done);
    }

    #[expect(clippy::too_many_lines, reason = "follows V8's BuildClassLiteral step by step")]
    fn class_members(&mut self, c: &'a Class<'a>, inferred: &'a str) -> Result<(), Error> {
        let name = c.id.as_ref().map_or(inferred, |id| id.name.as_str());
        let derived = c.heritage.is_some();
        let info = self.classes.last().expect("this class");
        let privates = info.privates.clone();
        let [brand_binding, static_brand_binding] = info.brands;
        let brand = info.brand;
        let static_brand = privates.iter().any(|(_, p)| matches!(p, Private::Method { statik: true } | Private::Accessor { statik: true, .. }));
        if brand {
            self.private_symbol(Runtime::CreatePrivateBrandSymbol, name, brand_binding);
        }
        if static_brand {
            self.private_symbol(Runtime::CreatePrivateBrandSymbol, name, static_brand_binding);
        }
        for e in &c.body.body {
            match e {
                ClassElement::PropertyDefinition(p) => {
                    if let PropertyKey::PrivateIdentifier(id) = &p.key {
                        let n = Hidden::private(self.alloc, id);
                        self.private_symbol(Runtime::CreatePrivateNameSymbol, n, n);
                    }
                }
                ClassElement::MethodDefinition(m) if matches!(m.kind, MethodDefinitionKind::Method) => {
                    if let PropertyKey::PrivateIdentifier(id) = &m.key {
                        let n = Hidden::private(self.alloc, id);
                        let f = self.class_method(n, &m.value, Kind::Method, m.r#static)?;
                        self.closure(f);
                        self.store(n, true);
                    }
                }
                _ => {}
            }
        }
        let ctor = self.temp();
        let boilerplate = self.temp();
        let ctor_in_args = self.temp();
        let superclass = self.temp();
        match &c.heritage {
            Some(h) => self.expr(&h.expression)?,
            None => self.b().load_the_hole(),
        }
        self.b().star(superclass);
        let kind = Kind::Constructor { derived };
        let f = match c.body.body.iter().find_map(|e| match e {
            ClassElement::MethodDefinition(m) if m.kind == MethodDefinitionKind::Constructor => Some(m),
            _ => None,
        }) {
            Some(m) => {
                self.next_home = Some(Hidden::HOME);
                self.method(name, &m.value, kind)?
            }
            None => {
                self.next_home = Some(Hidden::HOME);
                let this = Decls::this_key(Span::new(c.body.span.end.saturating_sub(1), c.body.span.end));
                self.function(name, kind.into(), Some(this), None, None, Body::DefaultConstructor { derived })?
            }
        };
        self.closure(f);
        self.b().star(ctor);
        self.b().mov(ctor, ctor_in_args);
        let mut members = Vec::new();
        let mut statics = false;
        for e in &c.body.body {
            match e {
                ClassElement::MethodDefinition(m) if m.kind != MethodDefinitionKind::Constructor && !matches!(m.key, PropertyKey::PrivateIdentifier(_)) => {
                    let key = match self.static_name(&m.key, m.computed) {
                        Some(n) => Some(n),
                        None => {
                            let k = self.temp();
                            self.property_key_into(&m.key, true, k)?;
                            if m.r#static {
                                self.static_prototype_check(k);
                            }
                            None
                        }
                    };
                    let (mkind, fkind) = match m.kind {
                        MethodDefinitionKind::Get => (MemberKind::Getter, Kind::Getter),
                        MethodDefinitionKind::Set => (MemberKind::Setter, Kind::Setter),
                        _ => (MemberKind::Method, Kind::Method),
                    };
                    let fname = self.method_name(&m.key, m.computed, m.kind);
                    let v = self.temp();
                    let n = self.class_method(fname, &m.value, fkind, m.r#static)?;
                    self.closure(n);
                    self.b().star(v);
                    members.push(ClassMember { name: key.map(Into::into), kind: mkind, statik: m.r#static });
                }
                ClassElement::PropertyDefinition(p) => {
                    statics |= p.r#static;
                    if p.computed && !matches!(p.key, PropertyKey::PrivateIdentifier(_)) {
                        let k = self.temp();
                        self.property_key_into(&p.key, true, k)?;
                        if p.r#static {
                            self.static_prototype_check(k);
                        }
                        self.b().ldar(k);
                        self.store(Hidden::field_key(self.alloc, p.span), true);
                        members.push(ClassMember { name: None, kind: MemberKind::Field, statik: p.r#static });
                    }
                }
                ClassElement::StaticBlock(_) => statics = true,
                _ => {}
            }
        }
        let k = self.constant(Constant::Class(ClassBoilerplate { members }));
        self.b().load_constant(k);
        self.b().star(boilerplate);
        let count = u32::try_from(self.b().mark() - boilerplate.0).unwrap_or(0);
        self.b().call_runtime(Runtime::DefineClass, RegisterList { first: boilerplate, count });
        self.release(boilerplate);
        self.store(Hidden::HOME, true);
        self.b().ldar(ctor);
        self.store(Hidden::STATIC_HOME, true);
        if c.id.is_some() {
            self.b().ldar(ctor);
            self.store(name, true);
        }
        for (n, p) in &privates {
            let Private::Accessor { statik, .. } = *p else { continue };
            let pair = self.b().new_register_list(2);
            for (slot, want) in [(pair.first, MethodDefinitionKind::Get), (pair.at(1), MethodDefinitionKind::Set)] {
                let found = c.body.body.iter().find_map(|e| match e {
                    ClassElement::MethodDefinition(m) if m.kind == want && matches!(&m.key, PropertyKey::PrivateIdentifier(id) if Hidden::private(self.alloc, id) == *n) => Some(m),
                    _ => None,
                });
                match found {
                    Some(m) => {
                        let fname = self.method_name(&m.key, false, m.kind);
                        let kind = if want == MethodDefinitionKind::Get { Kind::Getter } else { Kind::Setter };
                        let f = self.class_method(fname, &m.value, kind, statik)?;
                        self.closure(f);
                    }
                    None => self.b().load_null(),
                }
                self.b().star(slot);
            }
            self.b().call_runtime(Runtime::CreatePrivateAccessors, pair);
            self.release(pair.first);
            self.store(n, true);
        }
        if static_brand {
            let key = self.temp();
            self.load(static_brand_binding);
            self.b().star(key);
            self.b().ldar(ctor);
            self.b().define_keyed_own(ctor, key, 0);
            self.release(key);
        }
        if self.classes.last().is_some_and(|i| i.fields) {
            self.next_home = Some(Hidden::HOME);
            let f = self.function("", Kind::Initializer { statik: false }.into(), Some(Decls::this_key(c.body.span)), None, None, Body::Fields { class: c, statik: false })?;
            self.closure(f);
            self.store(Hidden::FIELDS, true);
        }
        if statics {
            self.next_home = Some(Hidden::STATIC_HOME);
            let f = self.function("", Kind::Initializer { statik: true }.into(), Some(Decls::this_key(c.span)), None, None, Body::Fields { class: c, statik: true })?;
            let init = self.temp();
            self.closure(f);
            self.b().star(init);
            self.b().call_property(init, RegisterList { first: ctor, count: 1 });
            self.release(init);
        }
        self.b().ldar(ctor);
        self.release(ctor);
        Ok(())
    }

    fn fields(&mut self, class: &'a Class<'a>, statik: bool) -> Result<(), Error> {
        self.load(THIS);
        let this = self.temp();
        self.b().star(this);
        for e in &class.body.body {
            match e {
                ClassElement::PropertyDefinition(p) if p.r#static == statik => {
                    let fname = self.static_name(&p.key, p.computed);
                    let key = match &p.key {
                        PropertyKey::PrivateIdentifier(id) => Some(Hidden::private(self.alloc, id)),
                        _ if p.computed => Some(Hidden::field_key(self.alloc, p.span)),
                        _ => None,
                    };
                    let k = key.map(|binding| {
                        let k = self.temp();
                        self.load(binding);
                        self.b().star(k);
                        k
                    });
                    let anonymous = match &p.value {
                        Some(v) => {
                            match fname {
                                Some(n) => self.expr_named(v, n)?,
                                None => self.expr(v)?,
                            }
                            Self::anonymous_function(v)
                        }
                        None => {
                            self.b().load_undefined();
                            false
                        }
                    };
                    match k {
                        Some(k) => {
                            let flags = u8::from(anonymous && fname.is_none());
                            self.b().define_keyed_own(this, k, flags);
                            self.release(k);
                        }
                        None => self.b().define_named_own(this, fname.unwrap_or_default()),
                    }
                }
                ClassElement::StaticBlock(b) if statik => self.scoped(&Decls::static_block(&b.body), |c| c.statements(&b.body))?,
                _ => {}
            }
        }
        self.release(this);
        Ok(())
    }

    pub fn anonymous_function(e: &Expression<'_>) -> bool {
        match e {
            Expression::FunctionExpression(f) => f.id.is_none(),
            Expression::ArrowFunctionExpression(_) => true,
            Expression::ClassExpression(c) => c.id.is_none(),
            Expression::ParenthesizedExpression(p) => Self::anonymous_function(&p.expression),
            _ => false,
        }
    }

    pub fn source_text(&self) -> &str {
        self.src.text
    }

    pub fn captured_insert(&mut self, key: Key) {
        self.captured.insert(key);
    }

    pub fn scoped_block(&mut self, decls: &Decls<'a>, f: impl FnOnce(&mut Self) -> Result<(), Error>) -> Result<(), Error> {
        self.scoped(decls, f)
    }

    pub fn private_kind(&self, name: &str) -> Option<Private> {
        self.classes.iter().rev().find_map(|c| c.privates.iter().find(|(n, _)| *n == name).map(|&(_, p)| p))
    }

    pub fn brand_of(&self, name: &str, statik: bool) -> &'a str {
        let class = self.classes.iter().rev().find(|c| c.privates.iter().any(|(n, _)| *n == name));
        class.map_or("", |c| c.brands[usize::from(statik)])
    }

    pub fn brand_check(&mut self, obj: Register, name: &str, statik: bool) {
        let brand = self.brand_of(name, statik);
        self.load(brand);
        self.b().get_keyed(obj);
    }

    pub fn throw_type_error(&mut self, message: &str) {
        let k = self.name(message);
        let arg = self.temp();
        self.b().load_constant(k);
        self.b().star(arg);
        self.b().call_runtime(Runtime::ThrowTypeError, RegisterList { first: arg, count: 1 });
        self.release(arg);
    }

    pub fn load_home(&mut self) {
        let home = self.frame().home.unwrap_or(Hidden::HOME);
        self.load(home);
    }

    pub fn in_derived_constructor(&self) -> bool {
        self.frames.iter().rev().find(|f| f.flavor.kind != Kind::Arrow).is_some_and(|f| f.flavor.kind == (Kind::Constructor { derived: true }))
    }

    pub fn frame_kind(&mut self) -> Kind {
        self.frame().flavor.kind
    }

    fn load_raw(&mut self, name: &'a str) {
        match self.resolve(name).place {
            Place::Reg(reg) => self.b().ldar(reg),
            Place::Context { depth, slot, cells } => self.b().load_context_slot(Register::current_context(), slot, depth, false, cells),
            _ => self.load(name),
        }
    }

    pub fn bind_this(&mut self, instance: Register) {
        self.load_raw(THIS);
        self.b().plain(Bytecode::ThrowSuperAlreadyCalledIfNotHole);
        self.b().ldar(instance);
        self.store(THIS, true);
        self.instance_init(instance);
        self.b().ldar(instance);
    }

    fn instance_init(&mut self, this: Register) {
        let Some(info) = self.classes.last() else { return };
        let (brand, fields) = (info.brand, info.fields);
        if brand {
            let key = self.temp();
            let binding = self.classes.last().map_or("", |c| c.brands[0]);
            self.load(binding);
            self.b().star(key);
            self.b().ldar(this);
            self.b().define_keyed_own(this, key, 0);
            self.release(key);
        }
        if fields {
            let mut skip = Label::default();
            self.load(Hidden::FIELDS);
            self.b().jump(Bytecode::JumpIfUndefined, &mut skip);
            let init = self.temp();
            self.b().star(init);
            self.b().call_property(init, RegisterList { first: this, count: 1 });
            self.release(init);
            self.b().bind(&mut skip);
        }
    }

    pub fn super_constructor(&mut self) -> (RegisterList, Register) {
        let this_function = self.temp();
        if self.frame_kind() == Kind::Arrow {
            self.load(Hidden::THIS_FUNCTION);
            self.b().star(this_function);
        } else {
            self.b().mov(Register::function_closure(), this_function);
        }
        let new_target = self.temp();
        self.load(NEW_TARGET);
        self.b().star(new_target);
        let pair = self.b().new_register_list(2);
        self.b().raw(Bytecode::FindNonDefaultConstructorOrConstruct, &[i32::from(this_function).into(), i32::from(new_target).into(), i32::from(pair.first).into()]);
        (pair, new_target)
    }

    fn super_call_forward(&mut self) {
        let first = self.b().mark();
        let (pair, new_target) = self.super_constructor();
        let mut done = Label::default();
        self.b().ldar(pair.first);
        self.b().jump(Bytecode::JumpIfTrue, &mut done);
        self.b().raw(Bytecode::ThrowIfNotSuperConstructor, &[i32::from(pair.at(1)).into()]);
        self.b().ldar(new_target);
        let slot = j8bytecode::OperandValue::try_from(self.b().slot(j8bytecode::FeedbackSlotKind::Call)).unwrap_or(0);
        self.b().raw(Bytecode::ConstructForwardAllArgs, &[i32::from(pair.at(1)).into(), slot]);
        self.b().star(pair.at(1));
        self.b().bind(&mut done);
        self.bind_this(pair.at(1));
        self.b().release_to(first);
    }

    fn function_declaration(&mut self, f: &'a AstFunction<'a>) -> Result<u32, Error> {
        self.plain_function(f, false, "")
    }

    fn module_body(&mut self, program: &'a Program<'a>, suspends: u32) -> Result<(), Error> {
        let asynchronous = suspends > 0;
        let kind = if asynchronous { FunctionKind::AsyncFunction } else { FunctionKind::Module };
        self.module.push(SharedFunction { name: "".into(), kind, strict: true, length: 0, formal_parameters: 0, scope: None, bytecode: j8bytecode::BytecodeArray::default() });
        self.frames.push(Frame {
            index: 0,
            b: Builder::new(1, 0),
            name: "".into(),
            flavor: Flavor { strict: true, asynchronous, ..Flavor::default() },
            default_constructor: false,
            length: 0,
            formals: 0,
            scope: None,
            contexts: Vec::new(),
            loop_depth: 0,
            controls: Vec::new(),
            label: None,
            completion: None,
            generator: None,
            suspends: 0,
            resume_table: None,
            home: None,
            async_generator: false,
            strict: true,
            annexb: Vec::new(),
            var_level: None,
            eval: false,
            prologue: false,
            simple_params: false,
        });
        if !asynchronous {
            self.module_statements(program)?;
            self.b().load_undefined();
            self.b().plain(Bytecode::Return);
            self.finish_module();
            return Ok(());
        }
        let g = self.temp();
        self.b().new_target_or_generator = Some(g.0);
        self.frame().generator = Some(g);
        let mut table = self.b().jump_table(suspends, 0);
        self.b().switch_on_generator_state(g, &mut table);
        self.frame().resume_table = Some(table);
        self.generator_object(Runtime::InlineAsyncFunctionEnter);
        self.try_catch(j8bytecode::CatchPrediction::AsyncAwait, |c| {
            c.module_statements(program)?;
            c.b().load_undefined();
            c.ret();
            Ok(())
        }, |c, _| {
            c.reject_async_function();
            Ok(())
        })?;
        self.finish_module();
        Ok(())
    }

    fn finish_module(&mut self) {
        let f = self.frames.pop().expect("the module's frame");
        self.levels.clear();
        self.module[0] = f.finish(&self.lines);
    }

    fn module_statements(&mut self, program: &'a Program<'a>) -> Result<(), Error> {
        use oxc_ast::ast::{Declaration, ExportDefaultDeclarationKind};
        for s in &program.body {
            let (f, name, inferred) = match s {
                Statement::FunctionDeclaration(f) => (f, f.id.as_ref().map(|i| i.name.as_str()), ""),
                Statement::ExportDeclaration(e) => match &e.declaration {
                    Declaration::FunctionDeclaration(f) => (f, f.id.as_ref().map(|i| i.name.as_str()), ""),
                    _ => continue,
                },
                Statement::ExportDefaultDeclaration(e) => match &e.declaration {
                    ExportDefaultDeclarationKind::FunctionDeclaration(f) => (f, Some(f.id.as_ref().map_or(DEFAULT_BINDING, |i| i.name.as_str())), "default"),
                    _ => continue,
                },
                _ => continue,
            };
            let Some(name) = name else { continue };
            let n = self.plain_function(f, false, inferred)?;
            self.closure(n);
            self.store(name, true);
        }
        for s in &program.body {
            match s {
                Statement::ImportDeclaration(_) | Statement::ExportAllDeclaration(_) | Statement::FunctionDeclaration(_) | Statement::ExportNamedDeclaration(_) | Statement::ExportFromDeclaration(_) => {}
                Statement::ExportDeclaration(e) => match &e.declaration {
                    Declaration::VariableDeclaration(v) => self.declaration(v)?,
                    Declaration::ClassDeclaration(c) => {
                        self.class(c, "")?;
                        if let Some(id) = &c.id {
                            self.store(id.name.as_str(), true);
                        }
                    }
                    Declaration::FunctionDeclaration(_) => {}
                    _ => return Err(self.unsupported(e.span, "declaration")),
                },
                Statement::ExportDefaultDeclaration(e) => match &e.declaration {
                    ExportDefaultDeclarationKind::FunctionDeclaration(_) => {}
                    ExportDefaultDeclarationKind::ClassDeclaration(c) => {
                        self.class(c, "default")?;
                        self.store(c.id.as_ref().map_or(DEFAULT_BINDING, |i| i.name.as_str()), true);
                    }
                    other => {
                        let e = other.as_expression().ok_or_else(|| self.unsupported(e.span, "default export"))?;
                        self.expr_named(e, "default")?;
                        self.store(DEFAULT_BINDING, true);
                    }
                },
                s => self.statement(s)?,
            }
        }
        Ok(())
    }

    #[expect(clippy::too_many_lines, reason = "follows V8's GenerateBytecodeBody for a script / eval scope step by step")]
    fn script(&mut self, program: &'a Program<'a>, local: Option<&Decls<'a>>, strict: bool, direct: bool) -> Result<(), Error> {
        self.module.push(SharedFunction { name: "".into(), kind: FunctionKind::NormalFunction, strict, length: 0, formal_parameters: 0, scope: None, bytecode: j8bytecode::BytecodeArray::default() });
        let eval = local.is_some();
        self.frames.push(Frame {
            index: 0,
            b: Builder::new(1, 0),
            name: "".into(),
            flavor: Flavor { strict, ..Flavor::default() },
            default_constructor: false,
            length: 0,
            formals: 0,
            scope: None,
            contexts: Vec::new(),
            loop_depth: 0,
            controls: Vec::new(),
            label: None,
            completion: None,
            generator: None,
            suspends: 0,
            resume_table: None,
            home: None,
            async_generator: false,
            strict,
            annexb: Vec::new(),
            var_level: None,
            eval: eval && direct,
            prologue: false,
            simple_params: false,
        });
        let completion = self.temp();
        self.frame().completion = Some(completion);
        let mut rw = Rewriter::default();
        rw.process(program.body.iter());
        self.completion_assign = rw.assign;
        self.completion_reset = rw.reset;
        if !(eval && direct) {
            if !eval {
                self.captured.remove(&Decls::this_key(program.span));
            }
            self.enter(&Decls(vec![Decl { name: THIS, key: Decls::this_key(program.span), kind: DeclKind::This, ready: None }]), ScopeType::ScriptScope)?;
        }
        let mut globals = Decls::default();
        if !(eval && strict) {
            globals.vars(&program.body);
            globals.lexical(&program.body);
        }
        if !strict {
            let hoisted = Decls::annexb_functions(&program.body, None);
            self.frame().annexb = hoisted.iter().map(|f| f.span.start).collect();
            globals.annexb_globals(&hoisted);
        }
        if eval {
            if let Some(d) = local {
                self.enter(d, ScopeType::EvalScope)?;
            }
            if !strict {
                for d in &globals.0 {
                    let check = match d.kind {
                        DeclKind::Var => EvalDeclaration::Var,
                        DeclKind::Function(_) => EvalDeclaration::Function,
                        _ => continue,
                    };
                    let args = self.b().new_register_list(2);
                    self.b().load_string(d.name);
                    self.b().star(args.first);
                    self.b().load_smi(check.into());
                    self.b().star(args.at(1));
                    self.b().call_runtime(Runtime::DeclareEvalVar, args);
                    self.release(args.first);
                }
                for d in &globals.0 {
                    match d.kind {
                        DeclKind::Var => {
                            let r = self.temp();
                            self.b().load_string(d.name);
                            self.b().star(r);
                            self.b().call_runtime(Runtime::DeclareEvalVar, RegisterList { first: r, count: 1 });
                            self.release(r);
                        }
                        DeclKind::Function(f) => {
                            let n = self.function_declaration(f)?;
                            let args = self.b().new_register_list(2);
                            self.b().load_string(d.name);
                            self.b().star(args.first);
                            self.closure(n);
                            self.b().star(args.at(1));
                            self.b().call_runtime(Runtime::DeclareEvalFunction, args);
                            self.release(args.first);
                        }
                        _ => {}
                    }
                }
            }
        } else {
            let mut declarations = Vec::new();
            let mut variables = Vec::new();
            let mut lexical = Vec::new();
            for d in &globals.0 {
                match d.kind {
                    DeclKind::Var => declarations.push(GlobalDeclaration { name: d.name.into(), function: None }),
                    DeclKind::Function(f) => {
                        let n = self.function_declaration(f)?;
                        declarations.retain(|g: &GlobalDeclaration| *g.name != *d.name);
                        declarations.push(GlobalDeclaration { name: d.name.into(), function: Some(n) });
                    }
                    DeclKind::Let | DeclKind::Const => {
                        let slot = Context::MIN_SLOTS + u32::try_from(variables.len()).unwrap_or(u32::MAX);
                        let mode = if matches!(d.kind, DeclKind::Const) { VariableMode::Const } else { VariableMode::Let };
                        variables.push(ContextVariable { name: d.name.into(), mode, role: VariableRole::Local });
                        lexical.push(Bound { name: d.name, binding: Binding::Context(slot), constant: matches!(d.kind, DeclKind::Const), ready: d.ready.or(Some(u32::MAX)), self_name: false });
                    }
                    _ => {}
                }
            }
            if !variables.is_empty() {
                self.frame().scope = Some(ScopeInfo { scope_type: Some(ScopeType::ScriptScope), variables, has_extension: false });
            }
            let mark = self.b().mark();
            self.levels.push(Level { func: 0, names: lexical, context: None, scope: None, dynamic: false, mark, switch: false, cells: true, locals: usize::MAX });
            if !declarations.is_empty() {
                let k = self.constant(Constant::Declarations(declarations));
                let args = self.b().new_register_list(2);
                self.b().load_constant(k);
                self.b().star(args.first);
                self.b().mov(Register::function_closure(), args.at(1));
                self.b().call_runtime(Runtime::DeclareGlobals, args);
                self.release(args.first);
            }
        }
        for d in &program.directives {
            let k = self.string_const(d.expression.value);
            self.b().load_constant(k);
            self.b().star(completion);
        }
        self.statements(&program.body)?;
        self.b().ldar(completion);
        self.b().plain(Bytecode::Return);
        let f = self.frames.pop().expect("the script's frame");
        self.levels.clear();
        self.module[0] = f.finish(&self.lines);
        Ok(())
    }

    fn statements(&mut self, stmts: &'a [Statement<'a>]) -> Result<(), Error> {
        stmts.iter().try_for_each(|s| self.statement(s))
    }

    fn push_jump(&mut self, label: Option<&'a str>, breakable: bool, continuable: bool) {
        let f = self.frame();
        let contexts = f.contexts.clone();
        f.controls.push(Control::Jump(Jump { label, breakable, continuable, breaks: Label::default(), continues: Label::default(), contexts }));
    }

    fn bind_continues(&mut self) {
        let f = self.frame();
        let Some(Control::Jump(j)) = f.controls.iter_mut().rev().find(|c| matches!(c, Control::Jump(_))) else { unreachable!("a loop is the innermost jump target") };
        let mut l = core::mem::take(&mut j.continues);
        f.b.bind(&mut l);
    }

    fn pop_jump(&mut self) {
        let Some(Control::Jump(mut j)) = self.frame().controls.pop() else { unreachable!("the innermost control is this jump target") };
        self.b().bind(&mut j.breaks);
    }

    fn take_label(&mut self) -> Option<&'a str> {
        self.frame().label.take()
    }

    fn exit_plan(&self, target: Option<(Option<&'a str>, bool)>) -> Exit {
        let f = self.frames.last().expect("inside a function");
        for (i, c) in f.controls.iter().enumerate().rev() {
            match c {
                Control::Finally(_) => return Exit::Finally(i),
                Control::Jump(j) => {
                    let hit = match target {
                        Some((Some(l), false)) => j.label == Some(l),
                        Some((Some(l), true)) => j.label == Some(l) && j.continuable,
                        Some((None, false)) => j.breakable,
                        Some((None, true)) => j.continuable,
                        None => false,
                    };
                    if hit {
                        return Exit::Jump(i);
                    }
                }
                Control::Handler | Control::Iterator(_) => {}
            }
        }
        Exit::Function
    }

    fn restore_contexts(&mut self, contexts: &[Register]) {
        let cur = self.frame().contexts.clone();
        if cur.len() > contexts.len() {
            self.b().pop_context(cur[contexts.len()]);
        }
    }

    fn leave_controls(&mut self, from: usize) {
        let iters: Vec<Register> = self.frame().controls[from..]
            .iter()
            .rev()
            .filter_map(|c| match c {
                Control::Iterator(i) => Some(*i),
                _ => None,
            })
            .collect();
        if iters.is_empty() {
            return;
        }
        let saved = self.temp();
        self.b().star(saved);
        for iter in iters {
            self.iterator_close(iter);
        }
        self.b().ldar(saved);
        self.release(saved);
    }

    fn enter_finally(&mut self, i: usize, token: i32) -> Result<(), Error> {
        let Control::Finally(fin) = &self.frame().controls[i] else { unreachable!("a finally") };
        let (contexts, token_reg) = (fin.contexts.clone(), fin.token);
        self.leave_controls(i + 1);
        self.restore_contexts(&contexts);
        let saved = self.temp();
        self.b().star(saved);
        self.b().load_smi(token);
        self.b().star(token_reg);
        self.b().ldar(saved);
        self.release(saved);
        let f = self.frame();
        let Control::Finally(fin) = &mut f.controls[i] else { unreachable!("a finally") };
        let mut l = core::mem::take(&mut fin.entry);
        f.b.jump(Bytecode::Jump, &mut l);
        let Control::Finally(fin) = &mut f.controls[i] else { unreachable!("a finally") };
        fin.entry = l;
        Ok(())
    }

    fn exit(&mut self, span: Span, label: Option<&'a str>, cont: bool) -> Result<(), Error> {
        match self.exit_plan(Some((label, cont))) {
            Exit::Jump(i) => {
                let Control::Jump(j) = &self.frame().controls[i] else { unreachable!("a jump target") };
                let contexts = j.contexts.clone();
                self.leave_controls(i + 1);
                self.restore_contexts(&contexts);
                let f = self.frame();
                let Control::Jump(j) = &mut f.controls[i] else { unreachable!("a jump target") };
                let mut l = core::mem::take(if cont { &mut j.continues } else { &mut j.breaks });
                f.b.jump(Bytecode::Jump, &mut l);
                let Control::Jump(j) = &mut f.controls[i] else { unreachable!("a jump target") };
                if cont { j.continues = l } else { j.breaks = l }
                Ok(())
            }
            Exit::Finally(i) => {
                let Control::Finally(fin) = &mut self.frame().controls[i] else { unreachable!("a finally") };
                let n = fin.routes.len();
                fin.routes.push(Route { span, label, cont });
                self.enter_finally(i, COMPLETION_ROUTE + i32::try_from(n).expect("fewer than 2^31 routes"))
            }
            Exit::Function => Err(self.unsupported(span, "a jump with no target")),
        }
    }

    pub fn ret(&mut self) {
        match self.exit_plan(None) {
            Exit::Finally(i) => {
                let Control::Finally(fin) = &self.frame().controls[i] else { unreachable!("a finally") };
                let value = fin.value;
                self.b().star(value);
                let _ = self.enter_finally(i, COMPLETION_RETURN);
            }
            _ => {
                self.leave_controls(0);
                let flavor = self.frame().flavor;
                if flavor.asynchronous {
                    let g = self.frame().generator.expect("a generator register");
                    let args = self.b().new_register_list(if flavor.generator { 3 } else { 2 });
                    self.b().mov(g, args.first);
                    self.b().star(args.at(1));
                    if flavor.generator {
                        self.b().load_bool(true);
                        self.b().star(args.at(2));
                        self.b().call_runtime(Runtime::InlineAsyncGeneratorResolve, args);
                    } else {
                        self.b().call_runtime(Runtime::InlineAsyncFunctionResolve, args);
                    }
                    self.release(args.first);
                }
                if flavor.kind == (Kind::Constructor { derived: true }) {
                    let mut this = Label::default();
                    self.b().jump(Bytecode::JumpIfUndefined, &mut this);
                    self.b().plain(Bytecode::Return);
                    self.b().bind(&mut this);
                    self.load(THIS);
                }
                self.b().plain(Bytecode::Return);
            }
        }
    }

    fn clause(&mut self, s: &'a Statement<'a>) -> Result<(), Error> {
        if matches!(s, Statement::FunctionDeclaration(_)) {
            let one = core::slice::from_ref(s);
            self.scoped(&Decls::block([one]), |c| c.statements(one))
        } else {
            self.statement(s)
        }
    }

    fn block(&mut self, b: &'a BlockStatement<'a>) -> Result<(), Error> {
        self.scoped(&Decls::block([b.body.as_slice()]), |c| c.statements(&b.body))
    }

    pub fn on_completion(&mut self, reg: Register, token: i32, then: impl FnOnce(&mut Self) -> Result<(), Error>) -> Result<(), Error> {
        self.b().load_smi(token);
        self.b().compare(CompareOp::StrictEq, reg);
        let mut skip = Label::default();
        self.b().jump(Bytecode::JumpIfFalse, &mut skip);
        then(self)?;
        self.b().bind(&mut skip);
        Ok(())
    }

    fn renew_context(&mut self) {
        let Some(level) = self.levels.last() else { return };
        let Some(saved) = level.context else { return };
        let slots: Vec<u32> = level.names.iter().filter_map(|n| match n.binding {
            Binding::Context(s) => Some(s),
            _ => None,
        }).collect();
        let Some(scope) = level.scope else { return };
        let n = u32::try_from(slots.len()).unwrap_or(0);
        let tmp = self.b().new_register_list(n);
        for (i, &s) in (0u32..).zip(&slots) {
            self.b().load_context_slot(Register::current_context(), s, 0, false, false);
            self.b().star(tmp.at(i));
        }
        self.b().pop_context(saved);
        self.b().create_block_context(scope);
        self.b().push_context(saved);
        for (i, &s) in (0u32..).zip(&slots) {
            self.b().ldar(tmp.at(i));
            self.b().store_context_slot(Register::current_context(), s, 0);
        }
        self.release(tmp.first);
    }

    fn try_statement(&mut self, t: &'a TryStatement<'a>) -> Result<(), Error> {
        let body = |s: &mut Self| match &t.handler {
            Some(c) => s.try_catch(j8bytecode::CatchPrediction::Caught, |s| s.block(&t.block), |s, ctx| s.catch_clause(c, ctx)),
            None => s.block(&t.block),
        };
        match &t.finalizer {
            Some(f) => self.try_finally(j8bytecode::CatchPrediction::Uncaught, body, |s, _| s.block(f)),
            None => body(self),
        }
    }

    pub fn try_finally(&mut self, prediction: j8bytecode::CatchPrediction, body: impl FnOnce(&mut Self) -> Result<(), Error>, finalizer: impl FnOnce(&mut Self, Register) -> Result<(), Error>) -> Result<(), Error> {
        let token = self.temp();
        let value = self.temp();
        let contexts = self.frame().contexts.clone();
        self.frame().controls.push(Control::Finally(Finally { token, value, contexts, entry: Label::default(), routes: Vec::new() }));
        let ctx = self.temp();
        self.b().mov(Register::current_context(), ctx);
        let h = self.b().new_handler(ctx, prediction);
        self.b().try_start(h);
        self.frame().controls.push(Control::Handler);
        body(self)?;
        self.frame().controls.pop();
        self.b().try_end(h);
        let mut fall = Label::default();
        self.b().load_smi(COMPLETION_NORMAL);
        self.b().star(token);
        self.b().jump(Bytecode::Jump, &mut fall);
        self.b().handler_target(h);
        self.b().star(value);
        self.b().load_smi(COMPLETION_THROW);
        self.b().star(token);
        self.b().bind(&mut fall);
        let Some(Control::Finally(mut f)) = self.frame().controls.pop() else { unreachable!("the innermost control is this finally") };
        self.b().bind(&mut f.entry);
        finalizer(self, token)?;
        self.on_completion(token, COMPLETION_THROW, |s| {
            s.b().ldar(value);
            s.b().plain(Bytecode::ReThrow);
            Ok(())
        })?;
        self.on_completion(token, COMPLETION_RETURN, |s| {
            s.b().ldar(value);
            s.ret();
            Ok(())
        })?;
        for (n, r) in (0i32..).zip(f.routes) {
            self.on_completion(token, COMPLETION_ROUTE + n, |s| s.exit(r.span, r.label, r.cont))?;
        }
        self.release(token);
        Ok(())
    }

    fn catch_clause(&mut self, c: &'a CatchClause<'a>, ctx: Register) -> Result<(), Error> {
        let Some(p) = &c.param else {
            self.b().load_the_hole();
            self.b().plain(Bytecode::SetPendingMessage);
            self.b().ldar(ctx);
            return self.block(&c.body);
        };
        let name = match &p.pattern {
            BindingPattern::BindingIdentifier(id) => id.name.as_str(),
            _ => Hidden::CATCH,
        };
        let func = self.frames.len() - 1;
        let mark = self.b().mark();
        let info = ScopeInfo { scope_type: Some(ScopeType::CatchScope), variables: alloc::vec![ContextVariable { name: name.into(), mode: VariableMode::Var, role: VariableRole::Local }], has_extension: false };
        let k = self.constant(Constant::Scope(info));
        let e = self.temp();
        self.b().star(e);
        self.b().create_catch_context(e, k);
        self.release(e);
        self.b().star(ctx);
        self.b().load_the_hole();
        self.b().plain(Bytecode::SetPendingMessage);
        self.b().ldar(ctx);
        let saved = self.temp();
        self.b().push_context(saved);
        self.frame().contexts.push(saved);
        let names = alloc::vec![Bound { name, binding: Binding::Context(Context::MIN_SLOTS), constant: false, ready: None, self_name: false }];
        self.levels.push(Level { func, names, context: Some(saved), scope: Some(k), dynamic: false, mark, switch: false, cells: false, locals: usize::MAX });
        if matches!(p.pattern, BindingPattern::BindingIdentifier(_)) {
            self.block(&c.body)?;
        } else {
            self.scoped(&Decls::catch_param(c), |s| {
                s.load_at(Hidden::CATCH, c.span.start);
                let v = s.temp();
                s.b().star(v);
                s.bind_pattern(&p.pattern, v, true)?;
                s.release(v);
                s.block(&c.body)
            })?;
        }
        self.leave();
        Ok(())
    }

    fn loop_back(&mut self, header: crate::builder::LoopHeader) {
        self.bind_continues();
        let depth = self.frame().loop_depth - 1;
        self.b().jump_loop(header, depth);
    }

    #[expect(clippy::too_many_lines, reason = "one arm per statement kind; splitting it would hurt readability")]
    fn statement(&mut self, s: &'a Statement<'a>) -> Result<(), Error> {
        if self.debug == DebugInfo::Record {
            self.b().at_statement(s.span().start);
        } else {
            self.b().at(s.span().start);
        }
        if let Some(c) = self.frame().completion
            && self.completion_reset.contains(&s.span().start)
        {
            self.b().load_undefined();
            self.b().star(c);
        }
        match s {
            Statement::ExpressionStatement(e) => match self.frame().completion.filter(|_| self.completion_assign.contains(&e.span.start)) {
                Some(c) => {
                    self.expr(&e.expression)?;
                    self.b().star(c);
                }
                None => self.effect(&e.expression)?,
            },
            Statement::VariableDeclaration(d) => self.declaration(d)?,
            Statement::FunctionDeclaration(f) => self.annexb_copy(f),
            Statement::EmptyStatement(_) => {}
            Statement::DebuggerStatement(_) => self.b().plain(Bytecode::Debugger),
            Statement::ClassDeclaration(c) => {
                self.class(c, "")?;
                if let Some(id) = &c.id {
                    self.store(id.name.as_str(), true);
                }
            }
            Statement::BlockStatement(b) => self.block(b)?,
            Statement::ReturnStatement(r) => {
                match &r.argument {
                    Some(e) => {
                        self.expr(e)?;
                        if self.frame().async_generator {
                            self.await_value();
                        }
                    }
                    None => self.b().load_undefined(),
                }
                self.ret();
            }
            Statement::TryStatement(t) => self.try_statement(t)?,
            Statement::ThrowStatement(t) => {
                self.expr(&t.argument)?;
                self.b().at(t.span.start);
                self.b().plain(Bytecode::Throw);
            }
            Statement::IfStatement(i) => {
                self.expr(&i.test)?;
                let mut skip = Label::default();
                self.b().jump(Bytecode::JumpIfToBooleanFalse, &mut skip);
                self.clause(&i.consequent)?;
                if let Some(a) = &i.alternate {
                    let mut end = Label::default();
                    self.b().jump(Bytecode::Jump, &mut end);
                    self.b().bind(&mut skip);
                    self.clause(a)?;
                    self.b().bind(&mut end);
                } else {
                    self.b().bind(&mut skip);
                }
            }
            Statement::WhileStatement(w) => {
                let label = self.take_label();
                let header = self.b().loop_header();
                self.frame().loop_depth += 1;
                self.push_jump(label, true, true);
                self.expr(&w.test)?;
                let mut exit = Label::default();
                self.b().jump(Bytecode::JumpIfToBooleanFalse, &mut exit);
                self.statement(&w.body)?;
                self.loop_back(header);
                self.frame().loop_depth -= 1;
                self.b().bind(&mut exit);
                self.pop_jump();
            }
            Statement::DoWhileStatement(w) => {
                let label = self.take_label();
                let header = self.b().loop_header();
                self.frame().loop_depth += 1;
                self.push_jump(label, true, true);
                self.statement(&w.body)?;
                self.bind_continues();
                self.expr(&w.test)?;
                let mut exit = Label::default();
                self.b().jump(Bytecode::JumpIfToBooleanFalse, &mut exit);
                let depth = self.frame().loop_depth - 1;
                self.b().jump_loop(header, depth);
                self.frame().loop_depth -= 1;
                self.b().bind(&mut exit);
                self.pop_jump();
            }
            Statement::ForStatement(f) => {
                let label = self.take_label();
                let init_text = match &f.init {
                    Some(ForStatementInit::VariableDeclaration(d)) => {
                        let inits: Vec<&str> = d.declarations.iter().filter_map(|v| v.init.as_ref()).map(|e| &self.src.text[e.span().start as usize..e.span().end as usize]).collect();
                        Some(self.alloc.alloc_str(&inits.join(" ")))
                    }
                    _ => None,
                };
                self.for_init_text = init_text.map(|t| &*t);
                let decls = Decls::for_init(f.init.as_ref());
                self.enter(&decls, ScopeType::BlockScope)?;
                self.for_init_text = None;
                let r = (|c: &mut Self| -> Result<(), Error> {
                    match &f.init {
                        Some(ForStatementInit::VariableDeclaration(d)) => c.declaration(d)?,
                        Some(init) => c.effect(init.as_expression().expect("a for-init is a declaration or an expression"))?,
                        None => {}
                    }
                    let header = c.b().loop_header();
                    c.frame().loop_depth += 1;
                    c.push_jump(label, true, true);
                    let mut exit = Label::default();
                    if let Some(t) = &f.test {
                        c.expr(t)?;
                        c.b().jump(Bytecode::JumpIfToBooleanFalse, &mut exit);
                    }
                    c.statement(&f.body)?;
                    c.bind_continues();
                    c.renew_context();
                    if let Some(u) = &f.update {
                        c.effect(u)?;
                    }
                    let depth = c.frame().loop_depth - 1;
                    c.b().jump_loop(header, depth);
                    c.frame().loop_depth -= 1;
                    c.b().bind(&mut exit);
                    c.pop_jump();
                    Ok(())
                })(self);
                r?;
                self.leave();
            }
            Statement::WithStatement(w) => {
                self.expr(&w.object)?;
                let object = self.temp();
                self.b().to_object(object);
                let k = self.constant(Constant::Scope(ScopeInfo { scope_type: Some(ScopeType::WithScope), variables: Vec::new(), has_extension: false }));
                self.b().create_with_context(object, k);
                let saved = self.temp();
                self.b().push_context(saved);
                self.frame().contexts.push(saved);
                let mark = self.b().mark();
                let func = self.frames.len() - 1;
                self.levels.push(Level { func, names: Vec::new(), context: Some(saved), scope: None, dynamic: true, mark, switch: false, cells: false, locals: usize::MAX });
                self.statement(&w.body)?;
                self.leave();
                self.release(object);
            }
            Statement::LabeledStatement(l) => {
                let name = l.label.name.as_str();
                if matches!(l.body, Statement::WhileStatement(_) | Statement::DoWhileStatement(_) | Statement::ForStatement(_) | Statement::ForOfStatement(_) | Statement::ForInStatement(_) | Statement::SwitchStatement(_)) {
                    self.frame().label = Some(name);
                    self.statement(&l.body)?;
                } else {
                    self.push_jump(Some(name), false, false);
                    self.statement(&l.body)?;
                    self.pop_jump();
                }
            }
            Statement::BreakStatement(b) => self.exit(b.span, b.label.as_ref().map(|l| l.name.as_str()), false)?,
            Statement::ContinueStatement(c) => self.exit(c.span, c.label.as_ref().map(|l| l.name.as_str()), true)?,
            Statement::SwitchStatement(sw) => self.switch(sw)?,
            Statement::ForOfStatement(f) => self.for_of(f)?,
            Statement::ForInStatement(f) => self.for_in(f)?,
            s => return Err(self.unsupported(s.span(), "statement")),
        }
        Ok(())
    }

    fn declaration(&mut self, d: &'a VariableDeclaration<'a>) -> Result<(), Error> {
        if !matches!(d.kind, VariableDeclarationKind::Var | VariableDeclarationKind::Let | VariableDeclarationKind::Const) {
            return Err(self.unsupported(d.span, "`using` declarations"));
        }
        for v in &d.declarations {
            let BindingPattern::BindingIdentifier(id) = &v.id else {
                let init = v.init.as_ref().ok_or_else(|| self.unsupported(v.span, "a destructuring declaration without an initializer"))?;
                self.expr(init)?;
                let t = self.temp();
                self.b().star(t);
                self.bind_pattern(&v.id, t, true)?;
                self.release(t);
                continue;
            };
            match (&v.init, d.kind) {
                (Some(e), _) => self.expr_named(e, id.name.as_str())?,
                (None, VariableDeclarationKind::Let) => self.b().load_undefined(),
                (None, _) => continue,
            }
            self.store(id.name.as_str(), true);
        }
        Ok(())
    }

    fn switch(&mut self, sw: &'a SwitchStatement<'a>) -> Result<(), Error> {
        let label = self.take_label();
        self.expr(&sw.discriminant)?;
        let d = self.temp();
        self.b().star(d);
        self.scoped(&Decls::block(sw.cases.iter().map(|c| c.consequent.as_slice())), |c| {
            c.levels.last_mut().expect("the switch scope").switch = true;
            let mut entries: Vec<Option<Label>> = Vec::with_capacity(sw.cases.len());
            for case in &sw.cases {
                if let Some(t) = &case.test {
                    c.expr(t)?;
                    c.b().compare(CompareOp::StrictEq, d);
                    let mut l = Label::default();
                    c.b().jump(Bytecode::JumpIfTrue, &mut l);
                    entries.push(Some(l));
                } else {
                    entries.push(None);
                }
            }
            let mut fallback = Label::default();
            c.b().jump(Bytecode::Jump, &mut fallback);
            c.push_jump(label, true, false);
            let mut has_default = false;
            for (case, entry) in sw.cases.iter().zip(entries) {
                match entry {
                    Some(mut l) => c.b().bind(&mut l),
                    None => {
                        has_default = true;
                        c.b().bind(&mut fallback);
                    }
                }
                c.statements(&case.consequent)?;
            }
            if !has_default {
                c.b().bind(&mut fallback);
            }
            c.pop_jump();
            Ok(())
        })?;
        self.release(d);
        Ok(())
    }
}
