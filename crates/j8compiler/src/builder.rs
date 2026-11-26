#![allow(clippy::cast_possible_wrap, reason = "constant pool and feedback indices are far below i64::MAX")]
use alloc::boxed::Box;
use alloc::vec::Vec;

use j8bytecode::{Bytecode, BytecodeArray, CatchPrediction, Constant, DebugTable, Encoder, FeedbackSlotKind, HandlerEntry, Intrinsic, LocalVariable, NativeContextIndex, OperandScale, OperandValue, Register, Runtime, SourcePosition};
use j8framework::{BinaryOp, CompareOp};

use crate::lines::Lines;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegisterList {
    pub first: Register,
    pub count: u32,
}

impl RegisterList {
    pub const EMPTY: Self = Self { first: Register(0), count: 0 };

    pub fn at(self, i: u32) -> Register {
        Register(self.first.0 + i32::try_from(i).unwrap_or(i32::MAX))
    }
}

#[derive(Default)]
pub struct Label {
    bound: Option<usize>,
    refs: Vec<(usize, usize)>,
}

#[derive(Clone, Copy)]
pub struct LoopHeader(usize);

pub struct JumpTable {
    constant: usize,
    pub size: u32,
    pub case_base: i32,
    switch_at: usize,
}

pub struct Builder {
    enc: Encoder,
    constants: Vec<Constant>,
    handlers: Vec<HandlerEntry>,
    positions: Vec<(u32, u32)>,
    latent: Option<u32>,
    statements: Vec<(u32, u32)>,
    latent_statement: Option<u32>,
    variables: Vec<LocalVariable>,
    feedback: Vec<FeedbackSlotKind>,
    parameter_count: u16,
    locals: i32,
    next: i32,
    max: i32,
    unbound: usize,
    reserved: usize,
    slot_cache: Vec<(FeedbackSlotKind, alloc::string::String, usize)>,
    boolean: bool,
    stored: Option<Register>,
    dead: bool,
    pub new_target_or_generator: Option<i32>,
}

impl Builder {
    pub fn new(parameter_count: u16, locals: i32) -> Self {
        Self { enc: Encoder::default(), constants: Vec::new(), handlers: Vec::new(), positions: Vec::new(), latent: None, statements: Vec::new(), latent_statement: None, variables: Vec::new(), feedback: Vec::new(), parameter_count, locals, next: locals, max: locals, unbound: 0, reserved: 0, slot_cache: Vec::new(), boolean: false, stored: None, dead: false, new_target_or_generator: None }
    }

    pub fn finish(self, lines: &Lines<'_>) -> BytecodeArray {
        debug_assert_eq!(self.unbound, 0, "every label is bound");
        let locate = |&(offset, pos): &(u32, u32)| {
            let (line, column) = lines.locate(pos);
            SourcePosition { offset, line, column }
        };
        let positions = self.positions.iter().map(locate).collect();
        let debug = (!self.statements.is_empty() || !self.variables.is_empty()).then(|| Box::new(DebugTable { statements: self.statements.iter().map(locate).collect(), locals: self.variables }));
        BytecodeArray {
            code: self.enc.code.into_boxed_slice(),
            constants: self.constants,
            handlers: self.handlers,
            positions,
            debug,
            parameter_count: self.parameter_count,
            register_count: u32::try_from(self.max).unwrap_or(0),
            feedback: self.feedback,
            new_target_or_generator: self.new_target_or_generator,
        }
    }

    pub fn offset(&self) -> usize {
        self.enc.code.len()
    }

    fn out(&mut self, b: Bytecode, ops: &[OperandValue]) -> usize {
        if self.dead {
            return self.offset();
        }
        let at = self.enc.emit(b, ops).expect("operands match the bytecode");
        let offset = u32::try_from(at).unwrap_or(u32::MAX);
        if let Some(pos) = self.latent_statement.take() {
            self.statements.push((offset, pos));
        }
        if let Some(pos) = self.latent.take()
            && self.positions.last().is_none_or(|&(_, last)| last != pos)
        {
            self.positions.push((offset, pos));
        }
        self.boolean = matches!(b, Bytecode::TestEqual | Bytecode::TestEqualStrict | Bytecode::TestLessThan | Bytecode::TestGreaterThan | Bytecode::TestLessThanOrEqual | Bytecode::TestGreaterThanOrEqual | Bytecode::TestReferenceEqual | Bytecode::TestInstanceOf | Bytecode::TestIn | Bytecode::TestUndetectable | Bytecode::TestNull | Bytecode::TestUndefined | Bytecode::TestTypeOf | Bytecode::LogicalNot | Bytecode::ToBooleanLogicalNot | Bytecode::LdaTrue | Bytecode::LdaFalse);
        self.stored = None;
        self.dead = matches!(b, Bytecode::Return | Bytecode::Throw | Bytecode::ReThrow | Bytecode::Jump | Bytecode::JumpConstant | Bytecode::JumpLoop | Bytecode::Abort);
        at
    }

    pub fn at(&mut self, pos: u32) {
        self.latent = Some(pos);
    }

    pub fn at_statement(&mut self, pos: u32) {
        self.latent_statement = Some(pos);
        self.at(pos);
    }

    pub fn open_local(&mut self, name: &str, register: Register) {
        let start = u32::try_from(self.offset()).unwrap_or(u32::MAX);
        self.variables.push(LocalVariable { name: name.into(), register: register.0, start, end: u32::MAX });
    }

    pub fn locals_len(&self) -> usize {
        self.variables.len()
    }

    pub fn close_locals(&mut self, from: usize) {
        let end = u32::try_from(self.offset()).unwrap_or(u32::MAX);
        if let Some(open) = self.variables.get_mut(from..) {
            open.iter_mut().filter(|l| l.end == u32::MAX).for_each(|l| l.end = end);
        }
    }

    pub fn reachable(&self) -> bool {
        !self.dead
    }

    pub fn cached_slot(&mut self, kind: FeedbackSlotKind, key: &str) -> usize {
        if let Some(&(_, _, s)) = self.slot_cache.iter().find(|(k, n, _)| *k == kind && n == key) {
            return s;
        }
        let s = self.slot(kind);
        self.slot_cache.push((kind, key.into(), s));
        s
    }

    pub fn new_register_list(&mut self, count: u32) -> RegisterList {
        let first = Register(self.next);
        self.next += i32::try_from(count).unwrap_or(i32::MAX);
        self.max = self.max.max(self.next);
        RegisterList { first, count }
    }

    pub fn mark(&self) -> i32 {
        self.next
    }

    pub fn release_to(&mut self, mark: i32) {
        debug_assert!(mark >= self.locals && mark <= self.next);
        self.next = mark;
    }

    pub fn constant(&mut self, c: Constant) -> usize {
        let shared = matches!(c, Constant::Number(_) | Constant::String(_) | Constant::Utf16(_) | Constant::BigInt(_) | Constant::Function(_));
        if shared && let Some(i) = self.constants.iter().position(|x| *x == c) {
            return i;
        }
        self.constants.push(c);
        self.constants.len() - 1
    }

    pub fn name(&mut self, s: &str) -> usize {
        self.constant(Constant::String(s.into()))
    }

    pub fn slot(&mut self, kind: FeedbackSlotKind) -> usize {
        let at = self.feedback.len();
        self.feedback.push(kind);
        for _ in 1..kind.slot_size() {
            self.feedback.push(FeedbackSlotKind::Invalid);
        }
        at
    }

    pub fn load_smi(&mut self, v: i32) {
        if v == 0 {
            self.out(Bytecode::LdaZero, &[]);
        } else {
            self.out(Bytecode::LdaSmi, &[OperandValue::from(v)]);
        }
    }

    pub fn load_number(&mut self, v: f64) {
        if let Some(i) = Self::is_smi(v) {
            self.load_smi(i);
        } else {
            let k = self.constant(Constant::Number(v));
            self.out(Bytecode::LdaConstant, &[k as OperandValue]);
        }
    }

    pub fn load_string(&mut self, s: &str) {
        let k = self.name(s);
        self.out(Bytecode::LdaConstant, &[k as OperandValue]);
    }

    pub fn load_constant(&mut self, k: usize) {
        self.out(Bytecode::LdaConstant, &[k as OperandValue]);
    }

    pub fn load_undefined(&mut self) {
        self.out(Bytecode::LdaUndefined, &[]);
    }

    pub fn load_null(&mut self) {
        self.out(Bytecode::LdaNull, &[]);
    }

    pub fn load_the_hole(&mut self) {
        self.out(Bytecode::LdaTheHole, &[]);
    }

    pub fn load_bool(&mut self, b: bool) {
        self.out(if b { Bytecode::LdaTrue } else { Bytecode::LdaFalse }, &[]);
    }

    pub fn ldar(&mut self, r: Register) {
        if self.stored == Some(r) {
            return;
        }
        self.out(Bytecode::Ldar, &[i32::from(r).into()]);
    }

    pub fn star(&mut self, r: Register) {
        let boolean = self.boolean;
        match Bytecode::short_star(r) {
            Some(b) => self.out(b, &[]),
            None => self.out(Bytecode::Star, &[i32::from(r).into()]),
        };
        self.boolean = boolean;
        if !self.dead {
            self.stored = Some(r);
        }
    }

    pub fn mov(&mut self, from: Register, to: Register) {
        if from != to {
            self.out(Bytecode::Mov, &[i32::from(from).into(), i32::from(to).into()]);
        }
    }

    pub fn load_global(&mut self, name: &str, inside_typeof: bool) {
        let k = self.name(name);
        let s = self.cached_slot(if inside_typeof { FeedbackSlotKind::LoadGlobalInsideTypeof } else { FeedbackSlotKind::LoadGlobalNotInsideTypeof }, name);
        self.out(if inside_typeof { Bytecode::LdaGlobalInsideTypeof } else { Bytecode::LdaGlobal }, &[k as OperandValue, s as OperandValue]);
    }

    pub fn store_global(&mut self, name: &str, strict: bool) {
        let k = self.name(name);
        let s = self.cached_slot(if strict { FeedbackSlotKind::StoreGlobalStrict } else { FeedbackSlotKind::StoreGlobalSloppy }, name);
        self.out(Bytecode::StaGlobal, &[k as OperandValue, s as OperandValue]);
    }

    pub fn load_context_slot(&mut self, context: Register, slot: u32, depth: u32, immutable: bool, cells: bool) {
        let current = context == Register::current_context() && depth == 0;
        let c = i32::from(context).into();
        match (immutable, current, cells) {
            (true, true, _) => self.out(Bytecode::LdaImmutableCurrentContextSlot, &[slot.into()]),
            (true, false, _) => self.out(Bytecode::LdaImmutableContextSlot, &[c, slot.into(), depth.into()]),
            (false, true, true) => self.out(Bytecode::LdaCurrentContextSlot, &[slot.into()]),
            (false, false, true) => self.out(Bytecode::LdaContextSlot, &[c, slot.into(), depth.into()]),
            (false, true, false) => self.out(Bytecode::LdaCurrentContextSlotNoCell, &[slot.into()]),
            (false, false, false) => self.out(Bytecode::LdaContextSlotNoCell, &[c, slot.into(), depth.into()]),
        };
    }

    pub fn store_context_slot(&mut self, context: Register, slot: u32, depth: u32) {
        self.store_context_slot_cells(context, slot, depth, false);
    }

    pub fn store_context_slot_cells(&mut self, context: Register, slot: u32, depth: u32, cells: bool) {
        let current = context == Register::current_context() && depth == 0;
        match (current, cells) {
            (true, true) => self.out(Bytecode::StaCurrentContextSlot, &[slot.into()]),
            (false, true) => self.out(Bytecode::StaContextSlot, &[i32::from(context).into(), slot.into(), depth.into()]),
            (true, false) => self.out(Bytecode::StaCurrentContextSlotNoCell, &[slot.into()]),
            (false, false) => self.out(Bytecode::StaContextSlotNoCell, &[i32::from(context).into(), slot.into(), depth.into()]),
        };
    }

    pub fn is_smi(v: f64) -> Option<i32> {
        #[expect(clippy::cast_possible_truncation, reason = "checked to round-trip exactly")]
        let i = v as i32;
        (f64::from(i).to_bits() == v.to_bits() && (-(1 << 30)..(1 << 30)).contains(&i)).then_some(i)
    }

    pub fn push_context(&mut self, save: Register) {
        self.out(Bytecode::PushContext, &[i32::from(save).into()]);
    }

    pub fn pop_context(&mut self, from: Register) {
        self.out(Bytecode::PopContext, &[i32::from(from).into()]);
    }

    pub fn create_block_context(&mut self, scope: usize) {
        self.out(Bytecode::CreateBlockContext, &[scope as OperandValue]);
    }

    pub fn create_function_context(&mut self, scope: usize, slots: u32) {
        self.out(Bytecode::CreateFunctionContext, &[scope as OperandValue, slots.into()]);
    }

    pub fn create_catch_context(&mut self, exception: Register, scope: usize) {
        self.out(Bytecode::CreateCatchContext, &[i32::from(exception).into(), scope as OperandValue]);
    }

    pub fn create_with_context(&mut self, object: Register, scope: usize) {
        self.out(Bytecode::CreateWithContext, &[i32::from(object).into(), scope as OperandValue]);
    }

    pub fn load_lookup_slot(&mut self, name: &str, inside_typeof: bool) {
        let k = self.name(name);
        self.out(if inside_typeof { Bytecode::LdaLookupSlotInsideTypeof } else { Bytecode::LdaLookupSlot }, &[k as OperandValue]);
    }

    pub fn store_lookup_slot(&mut self, name: &str, flags: u8) {
        let k = self.name(name);
        self.out(Bytecode::StaLookupSlot, &[k as OperandValue, flags.into()]);
    }

    pub fn get_named(&mut self, object: Register, name: &str) {
        self.get_named_shared(object, name, None);
    }

    pub fn get_named_shared(&mut self, object: Register, name: &str, variable: Option<&str>) {
        let k = self.name(name);
        let s = match variable {
            Some(v) => self.cached_slot(FeedbackSlotKind::LoadProperty, &alloc::format!("{v}.{name}")),
            None => self.slot(FeedbackSlotKind::LoadProperty),
        };
        self.out(Bytecode::GetNamedProperty, &[i32::from(object).into(), k as OperandValue, s as OperandValue]);
    }

    pub fn get_named_from_super(&mut self, receiver: Register, name: &str) {
        let k = self.name(name);
        let s = self.slot(FeedbackSlotKind::LoadProperty);
        self.out(Bytecode::GetNamedPropertyFromSuper, &[i32::from(receiver).into(), k as OperandValue, s as OperandValue]);
    }

    pub fn set_named(&mut self, object: Register, name: &str, strict: bool) {
        self.set_named_shared(object, name, strict, None);
    }

    pub fn set_named_shared(&mut self, object: Register, name: &str, strict: bool, variable: Option<&str>) {
        let k = self.name(name);
        let kind = if strict { FeedbackSlotKind::SetNamedStrict } else { FeedbackSlotKind::SetNamedSloppy };
        let s = match variable {
            Some(v) => self.cached_slot(kind, &alloc::format!("{v}.{name}")),
            None => self.slot(kind),
        };
        self.out(Bytecode::SetNamedProperty, &[i32::from(object).into(), k as OperandValue, s as OperandValue]);
    }

    pub fn define_named_own(&mut self, object: Register, name: &str) {
        let k = self.name(name);
        let s = self.slot(FeedbackSlotKind::DefineNamedOwn);
        self.out(Bytecode::DefineNamedOwnProperty, &[i32::from(object).into(), k as OperandValue, s as OperandValue]);
    }

    pub fn get_keyed(&mut self, object: Register) {
        let s = self.slot(FeedbackSlotKind::LoadKeyed);
        self.out(Bytecode::GetKeyedProperty, &[i32::from(object).into(), s as OperandValue]);
    }

    pub fn set_keyed(&mut self, object: Register, key: Register, strict: bool) {
        let s = self.slot(if strict { FeedbackSlotKind::SetKeyedStrict } else { FeedbackSlotKind::SetKeyedSloppy });
        self.out(Bytecode::SetKeyedProperty, &[i32::from(object).into(), i32::from(key).into(), s as OperandValue]);
    }

    pub fn define_keyed_own(&mut self, object: Register, key: Register, flags: u8) {
        let s = self.slot(FeedbackSlotKind::DefineKeyedOwn);
        self.out(Bytecode::DefineKeyedOwnProperty, &[i32::from(object).into(), i32::from(key).into(), flags.into(), s as OperandValue]);
    }

    pub fn define_keyed_own_in_literal(&mut self, object: Register, key: Register, flags: u8) {
        let s = self.slot(FeedbackSlotKind::DefineKeyedOwnPropertyInLiteral);
        self.out(Bytecode::DefineKeyedOwnPropertyInLiteral, &[i32::from(object).into(), i32::from(key).into(), flags.into(), s as OperandValue]);
    }

    pub fn store_in_array_literal(&mut self, array: Register, index: Register) {
        let s = self.slot(FeedbackSlotKind::StoreInArrayLiteral);
        self.out(Bytecode::StaInArrayLiteral, &[i32::from(array).into(), i32::from(index).into(), s as OperandValue]);
    }

    pub fn delete_property(&mut self, object: Register, strict: bool) {
        self.out(if strict { Bytecode::DeletePropertyStrict } else { Bytecode::DeletePropertySloppy }, &[i32::from(object).into()]);
    }

    pub fn binary(&mut self, op: BinaryOp, reg: Register) {
        self.out(Self::binary_bytecode(op), &[i32::from(reg).into(), 0]);
    }

    pub fn binary_smi(&mut self, op: BinaryOp, smi: i32) {
        let b = match op {
            BinaryOp::Add => Bytecode::AddSmi,
            BinaryOp::Sub => Bytecode::SubSmi,
            BinaryOp::Mul => Bytecode::MulSmi,
            BinaryOp::Div => Bytecode::DivSmi,
            BinaryOp::Rem => Bytecode::ModSmi,
            BinaryOp::Exp => Bytecode::ExpSmi,
            BinaryOp::BitOr => Bytecode::BitwiseOrSmi,
            BinaryOp::BitXor => Bytecode::BitwiseXorSmi,
            BinaryOp::BitAnd => Bytecode::BitwiseAndSmi,
            BinaryOp::Shl => Bytecode::ShiftLeftSmi,
            BinaryOp::Shr => Bytecode::ShiftRightSmi,
            BinaryOp::UShr => Bytecode::ShiftRightLogicalSmi,
        };
        self.out(b, &[OperandValue::from(smi), 0]);
    }

    pub fn binary_bytecode(op: BinaryOp) -> Bytecode {
        match op {
            BinaryOp::Add => Bytecode::Add,
            BinaryOp::Sub => Bytecode::Sub,
            BinaryOp::Mul => Bytecode::Mul,
            BinaryOp::Div => Bytecode::Div,
            BinaryOp::Rem => Bytecode::Mod,
            BinaryOp::Exp => Bytecode::Exp,
            BinaryOp::BitOr => Bytecode::BitwiseOr,
            BinaryOp::BitXor => Bytecode::BitwiseXor,
            BinaryOp::BitAnd => Bytecode::BitwiseAnd,
            BinaryOp::Shl => Bytecode::ShiftLeft,
            BinaryOp::Shr => Bytecode::ShiftRight,
            BinaryOp::UShr => Bytecode::ShiftRightLogical,
        }
    }

    pub fn compare(&mut self, op: CompareOp, reg: Register) {
        let (b, negate) = match op {
            CompareOp::Eq => (Bytecode::TestEqual, false),
            CompareOp::Ne => (Bytecode::TestEqual, true),
            CompareOp::StrictEq => (Bytecode::TestEqualStrict, false),
            CompareOp::StrictNe => (Bytecode::TestEqualStrict, true),
            CompareOp::Lt => (Bytecode::TestLessThan, false),
            CompareOp::Gt => (Bytecode::TestGreaterThan, false),
            CompareOp::Le => (Bytecode::TestLessThanOrEqual, false),
            CompareOp::Ge => (Bytecode::TestGreaterThanOrEqual, false),
        };
        self.out(b, &[i32::from(reg).into(), 0]);
        if negate {
            self.out(Bytecode::LogicalNot, &[]);
        }
    }

    pub fn test_reference_equal(&mut self, reg: Register) {
        self.out(Bytecode::TestReferenceEqual, &[i32::from(reg).into()]);
    }

    pub fn test_instance_of(&mut self, reg: Register) {
        let s = self.slot(FeedbackSlotKind::InstanceOf);
        self.out(Bytecode::TestInstanceOf, &[i32::from(reg).into(), s as OperandValue]);
    }

    pub fn test_in(&mut self, reg: Register) {
        let s = self.slot(FeedbackSlotKind::HasKeyed);
        self.out(Bytecode::TestIn, &[i32::from(reg).into(), s as OperandValue]);
    }

    pub fn unary(&mut self, b: Bytecode) {
        let kind = if b == Bytecode::TypeOf { FeedbackSlotKind::TypeOf } else { FeedbackSlotKind::BinaryOp };
        let s = self.slot(kind);
        self.out(b, &[s as OperandValue]);
    }

    pub fn plain(&mut self, b: Bytecode) {
        self.out(b, &[]);
    }

    pub fn to_numeric(&mut self) {
        let s = self.slot(FeedbackSlotKind::BinaryOp);
        self.out(Bytecode::ToNumeric, &[s as OperandValue]);
    }

    pub fn to_number(&mut self) {
        let s = self.slot(FeedbackSlotKind::BinaryOp);
        self.out(Bytecode::ToNumber, &[s as OperandValue]);
    }

    pub fn to_object(&mut self, out: Register) {
        self.out(Bytecode::ToObject, &[i32::from(out).into()]);
    }

    pub fn jump(&mut self, b: Bytecode, label: &mut Label) {
        self.jump_with(b, label, &[]);
    }

    fn jump_with(&mut self, b: Bytecode, label: &mut Label, rest: &[OperandValue]) {
        debug_assert!(label.bound.is_none(), "backward jumps are JumpLoop");
        if self.dead {
            return;
        }
        let b = match (b, self.boolean) {
            (Bytecode::JumpIfToBooleanTrue, true) => Bytecode::JumpIfTrue,
            (Bytecode::JumpIfToBooleanFalse, true) => Bytecode::JumpIfFalse,
            (b, _) => b,
        };
        let entry = self.constants.len() + self.reserved;
        self.reserved += 1;
        let placeholder: OperandValue = match OperandScale::for_unsigned(u32::try_from(entry).unwrap_or(u32::MAX)) {
            OperandScale::Single => 0,
            OperandScale::Double => 0x100,
            OperandScale::Quadruple => 0x1_0000,
        };
        let mut ops = alloc::vec![placeholder];
        ops.extend_from_slice(rest);
        let boolean = self.boolean;
        let at = self.out(b, &ops);
        self.boolean = boolean;
        label.refs.push((at, entry));
        self.unbound += 1;
    }

    pub fn bind(&mut self, label: &mut Label) {
        let target = self.offset();
        label.bound = Some(target);
        if !label.refs.is_empty() {
            self.dead = false;
        }
        self.boolean = false;
        self.stored = None;
        for (at, entry) in core::mem::take(&mut label.refs) {
            self.patch(at, entry, target);
        }
    }

    fn patch(&mut self, at: usize, _entry: usize, target: usize) {
        let code = &mut self.enc.code;
        let (scale, start) = match Bytecode::from_u8(code[at]).and_then(Bytecode::prefix_scale) {
            Some(s) => (s, at + 1),
            None => (OperandScale::Single, at),
        };
        let delta = u32::try_from(target - start).unwrap_or(u32::MAX);
        let width = usize::from(j8bytecode::OperandSize::from(scale));
        self.reserved -= 1;
        let value = if OperandScale::for_unsigned(delta) <= scale {
            delta
        } else {
            let b = Bytecode::from_u8(code[start]).expect("a jump");
            code[start] = Self::with_constant_operand(b) as u8;
            self.constants.push(Constant::Offset(delta));
            u32::try_from(self.constants.len() - 1).unwrap_or(u32::MAX)
        };
        let code = &mut self.enc.code;
        code[start + 1..start + 1 + width].copy_from_slice(&value.to_le_bytes()[..width]);
        self.unbound -= 1;
    }

    fn with_constant_operand(b: Bytecode) -> Bytecode {
        match b {
            Bytecode::Jump => Bytecode::JumpConstant,
            Bytecode::JumpIfTrue => Bytecode::JumpIfTrueConstant,
            Bytecode::JumpIfFalse => Bytecode::JumpIfFalseConstant,
            Bytecode::JumpIfToBooleanTrue => Bytecode::JumpIfToBooleanTrueConstant,
            Bytecode::JumpIfToBooleanFalse => Bytecode::JumpIfToBooleanFalseConstant,
            Bytecode::JumpIfNull => Bytecode::JumpIfNullConstant,
            Bytecode::JumpIfNotNull => Bytecode::JumpIfNotNullConstant,
            Bytecode::JumpIfUndefined => Bytecode::JumpIfUndefinedConstant,
            Bytecode::JumpIfNotUndefined => Bytecode::JumpIfNotUndefinedConstant,
            Bytecode::JumpIfUndefinedOrNull => Bytecode::JumpIfUndefinedOrNullConstant,
            Bytecode::JumpIfJSReceiver => Bytecode::JumpIfJSReceiverConstant,
            Bytecode::JumpIfForInDone => Bytecode::JumpIfForInDoneConstant,
            other => unreachable!("{other:?} has no constant form"),
        }
    }

    pub fn loop_header(&mut self) -> LoopHeader {
        self.dead = false;
        self.boolean = false;
        self.stored = None;
        LoopHeader(self.offset())
    }

    pub fn jump_loop(&mut self, header: LoopHeader, depth: u32) {
        let s = self.slot(FeedbackSlotKind::JumpLoop);
        let back = self.offset() - header.0;
        let mut ops = [back as OperandValue, depth.into(), s as OperandValue];
        if Encoder::scale(Bytecode::JumpLoop, &ops) > OperandScale::Single {
            ops[0] = (back + 1) as OperandValue;
        }
        self.out(Bytecode::JumpLoop, &ops);
    }

    pub fn jump_table(&mut self, size: u32, case_base: i32) -> JumpTable {
        let constant = self.constants.len();
        for _ in 0..size {
            self.constants.push(Constant::Reserved);
        }
        JumpTable { constant, size, case_base, switch_at: 0 }
    }

    pub fn switch_on_smi(&mut self, table: &mut JumpTable) {
        if self.dead {
            return;
        }
        let at = self.out(Bytecode::SwitchOnSmiNoFeedback, &[table.constant as OperandValue, table.size.into(), table.case_base.into()]);
        table.switch_at = at + usize::from(self.enc.code[at] != Bytecode::SwitchOnSmiNoFeedback as u8);
    }

    pub fn switch_on_generator_state(&mut self, generator: Register, table: &mut JumpTable) {
        if self.dead {
            return;
        }
        let at = self.out(Bytecode::SwitchOnGeneratorState, &[i32::from(generator).into(), table.constant as OperandValue, table.size.into()]);
        table.switch_at = at + usize::from(self.enc.code[at] != Bytecode::SwitchOnGeneratorState as u8);
    }

    pub fn bind_case(&mut self, table: &JumpTable, case: i32) {
        self.dead = false;
        self.boolean = false;
        self.stored = None;
        let i = usize::try_from(case - table.case_base).expect("case in range");
        self.constants[table.constant + i] = Constant::Offset(u32::try_from(self.offset() - table.switch_at).unwrap_or(u32::MAX));
    }

    pub fn call_property(&mut self, callable: Register, args: RegisterList) {
        let s = self.slot(FeedbackSlotKind::Call);
        let c = i32::from(callable).into();
        let a = |i: u32| OperandValue::from(i32::from(args.at(i)));
        match args.count {
            1 => self.out(Bytecode::CallProperty0, &[c, a(0), s as OperandValue]),
            2 => self.out(Bytecode::CallProperty1, &[c, a(0), a(1), s as OperandValue]),
            3 => self.out(Bytecode::CallProperty2, &[c, a(0), a(1), a(2), s as OperandValue]),
            n => self.out(Bytecode::CallProperty, &[c, a(0), n.into(), s as OperandValue]),
        };
    }

    pub fn call_undefined_receiver(&mut self, callable: Register, args: RegisterList) {
        let s = self.slot(FeedbackSlotKind::Call);
        let c = i32::from(callable).into();
        let a = |i: u32| OperandValue::from(i32::from(args.at(i)));
        match args.count {
            0 => self.out(Bytecode::CallUndefinedReceiver0, &[c, s as OperandValue]),
            1 => self.out(Bytecode::CallUndefinedReceiver1, &[c, a(0), s as OperandValue]),
            2 => self.out(Bytecode::CallUndefinedReceiver2, &[c, a(0), a(1), s as OperandValue]),
            n => self.out(Bytecode::CallUndefinedReceiver, &[c, a(0), n.into(), s as OperandValue]),
        };
    }

    pub fn call_with_spread(&mut self, callable: Register, args: RegisterList) {
        let s = self.slot(FeedbackSlotKind::Call);
        self.out(Bytecode::CallWithSpread, &[i32::from(callable).into(), i32::from(args.first).into(), args.count.into(), s as OperandValue]);
    }

    pub fn construct(&mut self, constructor: Register, args: RegisterList) {
        let s = self.slot(FeedbackSlotKind::Call);
        self.out(Bytecode::Construct, &[i32::from(constructor).into(), i32::from(args.first).into(), args.count.into(), s as OperandValue]);
    }

    pub fn construct_with_spread(&mut self, constructor: Register, args: RegisterList) {
        let s = self.slot(FeedbackSlotKind::Call);
        self.out(Bytecode::ConstructWithSpread, &[i32::from(constructor).into(), i32::from(args.first).into(), args.count.into(), s as OperandValue]);
    }

    pub fn call_runtime(&mut self, f: Runtime, args: RegisterList) {
        match Self::intrinsic(f) {
            Some(i) => self.out(Bytecode::InvokeIntrinsic, &[OperandValue::from(i as u8), i32::from(args.first).into(), args.count.into()]),
            None => self.out(Bytecode::CallRuntime, &[OperandValue::from(f as u16), i32::from(args.first).into(), args.count.into()]),
        };
    }

    pub fn call_runtime_for_pair(&mut self, f: Runtime, args: RegisterList, out: Register) {
        self.out(Bytecode::CallRuntimeForPair, &[OperandValue::from(f as u16), i32::from(args.first).into(), args.count.into(), i32::from(out).into()]);
    }

    pub fn call_js_runtime(&mut self, f: NativeContextIndex, args: RegisterList) {
        self.out(Bytecode::CallJSRuntime, &[OperandValue::from(f as u16), i32::from(args.first).into(), args.count.into()]);
    }

    fn intrinsic(f: Runtime) -> Option<Intrinsic> {
        let name = alloc::format!("{f:?}");
        let inner = name.strip_prefix("Inline")?;
        Intrinsic::ALL.into_iter().find(|i| alloc::format!("{i:?}") == inner)
    }

    pub fn create_closure(&mut self, function: usize, flags: u8) {
        let s = self.slot(FeedbackSlotKind::Literal);
        self.out(Bytecode::CreateClosure, &[function as OperandValue, s as OperandValue, flags.into()]);
    }

    pub fn create_object_literal(&mut self, boilerplate: usize, flags: u8) {
        let s = self.slot(FeedbackSlotKind::Literal);
        self.out(Bytecode::CreateObjectLiteral, &[boilerplate as OperandValue, s as OperandValue, flags.into()]);
    }

    pub fn create_array_literal(&mut self, boilerplate: usize, flags: u8) {
        let s = self.slot(FeedbackSlotKind::Literal);
        self.out(Bytecode::CreateArrayLiteral, &[boilerplate as OperandValue, s as OperandValue, flags.into()]);
    }

    pub fn create_empty_array_literal(&mut self) {
        let s = self.slot(FeedbackSlotKind::Literal);
        self.out(Bytecode::CreateEmptyArrayLiteral, &[s as OperandValue]);
    }

    pub fn create_reg_exp_literal(&mut self, pattern: usize, flags: u16) {
        let s = self.slot(FeedbackSlotKind::Literal);
        self.out(Bytecode::CreateRegExpLiteral, &[pattern as OperandValue, s as OperandValue, flags.into()]);
    }

    pub fn get_template_object(&mut self, template: usize) {
        let s = self.slot(FeedbackSlotKind::Literal);
        self.out(Bytecode::GetTemplateObject, &[template as OperandValue, s as OperandValue]);
    }

    pub fn get_iterator(&mut self, object: Register) {
        let load = self.slot(FeedbackSlotKind::LoadProperty);
        let call = self.slot(FeedbackSlotKind::Call);
        self.out(Bytecode::GetIterator, &[i32::from(object).into(), load as OperandValue, call as OperandValue]);
    }

    pub fn for_in_enumerate(&mut self, receiver: Register) {
        self.out(Bytecode::ForInEnumerate, &[i32::from(receiver).into()]);
    }

    pub fn for_in_prepare(&mut self, out: RegisterList) -> usize {
        let s = self.slot(FeedbackSlotKind::ForIn);
        self.out(Bytecode::ForInPrepare, &[i32::from(out.first).into(), s as OperandValue]);
        s
    }

    pub fn jump_if_for_in_done(&mut self, label: &mut Label, index: Register, cache_length: Register) {
        self.jump_with(Bytecode::JumpIfForInDone, label, &[i32::from(index).into(), i32::from(cache_length).into()]);
    }

    pub fn for_in_next(&mut self, receiver: Register, index: Register, cache: RegisterList, slot: usize) {
        self.out(Bytecode::ForInNext, &[i32::from(receiver).into(), i32::from(index).into(), i32::from(cache.first).into(), slot as OperandValue]);
    }

    pub fn for_in_step(&mut self, index: Register) {
        self.out(Bytecode::ForInStep, &[i32::from(index).into()]);
    }

    pub fn live_registers(&self) -> RegisterList {
        RegisterList { first: Register(0), count: u32::try_from(self.next).unwrap_or(0) }
    }

    pub fn suspend_generator(&mut self, generator: Register, registers: RegisterList, suspend_id: u32) {
        self.out(Bytecode::SuspendGenerator, &[i32::from(generator).into(), i32::from(registers.first).into(), registers.count.into(), suspend_id.into()]);
    }

    pub fn resume_generator(&mut self, generator: Register, registers: RegisterList) {
        self.out(Bytecode::ResumeGenerator, &[i32::from(generator).into(), i32::from(registers.first).into(), registers.count.into()]);
    }

    pub fn new_handler(&mut self, context: Register, prediction: CatchPrediction) -> usize {
        self.handlers.push(HandlerEntry { start: 0, end: 0, handler: 0, context: context.0, prediction });
        self.handlers.len() - 1
    }

    pub fn try_start(&mut self, handler: usize) {
        self.handlers[handler].start = u32::try_from(self.offset()).unwrap_or(u32::MAX);
    }

    pub fn try_end(&mut self, handler: usize) {
        self.handlers[handler].end = u32::try_from(self.offset()).unwrap_or(u32::MAX);
    }

    pub fn handler_target(&mut self, handler: usize) {
        self.dead = false;
        self.boolean = false;
        self.stored = None;
        self.handlers[handler].handler = u32::try_from(self.offset()).unwrap_or(u32::MAX);
    }

    pub fn throw_reference_error_if_hole(&mut self, name: &str) {
        let k = self.name(name);
        self.out(Bytecode::ThrowReferenceErrorIfHole, &[k as OperandValue]);
    }

    pub fn create_rest_parameter(&mut self) {
        self.out(Bytecode::CreateRestParameter, &[]);
    }

    pub fn create_arguments(&mut self, mapped: bool) {
        self.out(if mapped { Bytecode::CreateMappedArguments } else { Bytecode::CreateUnmappedArguments }, &[]);
    }

    pub fn raw(&mut self, b: Bytecode, ops: &[OperandValue]) -> usize {
        self.out(b, ops)
    }
}
