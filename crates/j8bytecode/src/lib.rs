extern crate alloc;

mod function;
mod v8;

pub use v8::{Bytecode, BytecodeError, Decoded, Decoder, Encoder, FeedbackSlotKind, FunctionKind, ImplicitRegisterUse, Intrinsic, NativeContextIndex, OperandScale, OperandSize, OperandType, Operands, OperandTypeInfo, OperandValue, Register, Runtime, ScopeType, VariableMode};
pub use function::{ArrayBoilerplate, BytecodeArray, CatchPrediction, ClassBoilerplate, ClassMember, Constant, Context, ContextVariable, EvalDeclaration, VariableRole, ContinuationToken, CreateArrayLiteralFlags, CreateClosureFlags, CreateObjectLiteralFlags, DebugTable, GlobalDeclaration, HandlerEntry, LiteralFlags, LiteralValue, LocalVariable, MemberKind, Module, ObjectBoilerplate, ScopeInfo, SharedFunction, SourcePosition, StoreLookupSlotFlags, TemplateObject, TypeOfLiteral};
