use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::{FeedbackSlotKind, FunctionKind, ScopeType, VariableMode};

#[derive(Clone, Debug)]
pub enum Constant {
    Number(f64),
    String(Box<str>),
    Utf16(Box<[u16]>),
    BigInt(Box<str>),
    Function(u32),
    Scope(ScopeInfo),
    Object(ObjectBoilerplate),
    Array(ArrayBoilerplate),
    Template(TemplateObject),
    Class(ClassBoilerplate),
    Declarations(Vec<GlobalDeclaration>),
    Offset(u32),
    AsyncIteratorSymbol,
    Reserved,
}

impl PartialEq for Constant {
    fn eq(&self, o: &Self) -> bool {
        match (self, o) {
            (Self::Number(a), Self::Number(b)) => a.to_bits() == b.to_bits(),
            (Self::String(a), Self::String(b)) | (Self::BigInt(a), Self::BigInt(b)) => a == b,
            (Self::Utf16(a), Self::Utf16(b)) => a == b,
            (Self::Function(a), Self::Function(b)) | (Self::Offset(a), Self::Offset(b)) => a == b,
            _ => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GlobalDeclaration {
    pub name: Box<str>,
    pub function: Option<u32>,
}

#[derive(Clone, Debug, Default)]
pub struct ScopeInfo {
    pub scope_type: Option<ScopeType>,
    pub variables: Vec<ContextVariable>,
    pub has_extension: bool,
}

#[derive(Clone, Debug)]
pub struct ContextVariable {
    pub name: Box<str>,
    pub mode: VariableMode,
    pub role: VariableRole,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum EvalDeclaration {
    Var = 1,
    Function = 2,
}

impl TryFrom<i32> for EvalDeclaration {
    type Error = i32;

    fn try_from(v: i32) -> Result<Self, i32> {
        match v {
            1 => Ok(EvalDeclaration::Var),
            2 => Ok(EvalDeclaration::Function),
            v => Err(v),
        }
    }
}

impl From<EvalDeclaration> for i32 {
    fn from(d: EvalDeclaration) -> i32 {
        d as i32
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VariableRole {
    Local,
    Parameter(u16),
    FunctionName,
}

pub struct Context;

impl Context {
    pub const MIN_SLOTS: u32 = 2;
}

#[derive(Clone, Debug)]
pub struct ObjectBoilerplate {
    pub properties: Vec<(Box<str>, Option<LiteralValue>)>,
}

#[derive(Clone, Debug)]
pub struct ArrayBoilerplate {
    pub elements: Vec<Option<LiteralValue>>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum LiteralValue {
    Number(f64),
    String(Box<str>),
    Bool(bool),
    Null,
    Undefined,
    Hole,
}

#[derive(Clone, Debug)]
pub struct TemplateObject {
    pub cooked: Vec<Option<Box<str>>>,
    pub raw: Vec<Box<str>>,
}

#[derive(Clone, Debug, Default)]
pub struct ClassBoilerplate {
    pub members: Vec<ClassMember>,
}

#[derive(Clone, Debug)]
pub struct ClassMember {
    pub name: Option<Box<str>>,
    pub kind: MemberKind,
    pub statik: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemberKind {
    Method,
    Getter,
    Setter,
    Field,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HandlerEntry {
    pub start: u32,
    pub end: u32,
    pub handler: u32,
    pub context: i32,
    pub prediction: CatchPrediction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatchPrediction {
    Uncaught,
    Caught,
    Promise,
    AsyncAwait,
    UncaughtAsyncAwait,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourcePosition {
    pub offset: u32,
    pub line: u32,
    pub column: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalVariable {
    pub name: Box<str>,
    pub register: i32,
    pub start: u32,
    pub end: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DebugTable {
    pub statements: Vec<SourcePosition>,
    pub locals: Vec<LocalVariable>,
}

#[derive(Clone, Debug, Default)]
pub struct BytecodeArray {
    pub code: Box<[u8]>,
    pub constants: Vec<Constant>,
    pub handlers: Vec<HandlerEntry>,
    pub positions: Vec<SourcePosition>,
    pub debug: Option<Box<DebugTable>>,
    pub parameter_count: u16,
    pub register_count: u32,
    pub feedback: Vec<FeedbackSlotKind>,
    pub new_target_or_generator: Option<i32>,
}

impl BytecodeArray {
    pub fn position_at(&self, offset: u32) -> Option<SourcePosition> {
        let i = self.positions.partition_point(|p| p.offset <= offset);
        i.checked_sub(1).map(|i| self.positions[i])
    }

    pub fn statements(&self) -> &[SourcePosition] {
        self.debug.as_ref().map_or(&[], |d| &d.statements)
    }

    pub fn locals_at(&self, offset: u32) -> impl Iterator<Item = &LocalVariable> {
        self.debug.iter().flat_map(|d| &d.locals).filter(move |l| (l.start..l.end).contains(&offset))
    }
}

#[derive(Clone, Debug)]
pub struct SharedFunction {
    pub name: Box<str>,
    pub kind: FunctionKind,
    pub strict: bool,
    pub length: u16,
    pub formal_parameters: u16,
    pub scope: Option<ScopeInfo>,
    pub bytecode: BytecodeArray,
}

#[derive(Clone, Debug, Default)]
pub struct Module {
    pub functions: Vec<SharedFunction>,
}

impl Module {
    pub const MAIN: u32 = 0;
}

pub struct CreateArrayLiteralFlags;

impl CreateArrayLiteralFlags {
    pub const FLAGS_MASK: u8 = 0x1f;
    pub const FAST_CLONE_SUPPORTED: u8 = 1 << 5;

    pub const fn encode(fast_clone: bool, runtime_flags: u8) -> u8 {
        (runtime_flags & Self::FLAGS_MASK) | if fast_clone { Self::FAST_CLONE_SUPPORTED } else { 0 }
    }
}

pub struct CreateObjectLiteralFlags;

impl CreateObjectLiteralFlags {
    pub const FLAGS_MASK: u8 = 0x1f;
    pub const FAST_CLONE_SUPPORTED: u8 = 1 << 5;

    pub const fn encode(runtime_flags: u8, fast_clone: bool) -> u8 {
        (runtime_flags & Self::FLAGS_MASK) | if fast_clone { Self::FAST_CLONE_SUPPORTED } else { 0 }
    }
}

pub struct LiteralFlags;

impl LiteralFlags {
    pub const NO_FLAGS: u8 = 0;
    pub const IS_SHALLOW: u8 = 1;
}

pub struct CreateClosureFlags;

impl CreateClosureFlags {
    pub const PRETENURED: u8 = 1;
    pub const FAST_NEW_CLOSURE: u8 = 1 << 1;

    pub const fn encode(pretenure: bool, is_function_scope: bool) -> u8 {
        let fast = !pretenure && is_function_scope;
        (if pretenure { Self::PRETENURED } else { 0 }) | if fast { Self::FAST_NEW_CLOSURE } else { 0 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum TypeOfLiteral {
    Number,
    String,
    Symbol,
    Boolean,
    BigInt,
    Undefined,
    Function,
    Object,
    Other,
}

impl From<&str> for TypeOfLiteral {
    fn from(s: &str) -> Self {
        match s {
            "number" => Self::Number,
            "string" => Self::String,
            "symbol" => Self::Symbol,
            "boolean" => Self::Boolean,
            "bigint" => Self::BigInt,
            "undefined" => Self::Undefined,
            "function" => Self::Function,
            "object" => Self::Object,
            _ => Self::Other,
        }
    }
}

pub struct StoreLookupSlotFlags;

impl StoreLookupSlotFlags {
    pub const STRICT: u8 = 1;
    pub const LOOKUP_HOISTING: u8 = 1 << 1;
}

pub struct ContinuationToken;

impl ContinuationToken {
    pub const FALLTHROUGH: i32 = -1;
    pub const RETHROW: i32 = 0;
}
