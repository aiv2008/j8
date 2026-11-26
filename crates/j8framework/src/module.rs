use crate::ValueKind;
use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::{Collect, ScopeLocal};

use crate::{Scope, Error, ErrorKind, FuncId, JsEnv, JsObject, JsString, Key, ObjectKind, Property, PropertyKey, Slot, Value};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImportName {
    Named(Box<str>),
    Namespace,
}

#[derive(Clone, Debug)]
pub struct ImportEntry {
    pub request: usize,
    pub name: ImportName,
}

#[derive(Clone, Debug, Default)]
pub struct ModuleInfo {
    pub requests: Vec<Box<str>>,
    pub imports: Vec<ImportEntry>,
    pub local_exports: Vec<(Box<str>, u16)>,
    pub indirect_exports: Vec<(Box<str>, usize, ImportName)>,
    pub star_exports: Vec<usize>,
    pub slots: u16,
    pub lexical: Vec<u16>,
    pub top_level_await: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Collect)]
#[collect(require_static)]
pub enum ModuleStatus {
    Linked,
    Evaluating,
    EvaluatingAsync,
    Evaluated,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Collect)]
#[collect(require_static)]
pub struct Evaluation {
    pub dfs_index: u32,
    pub dfs_ancestor: u32,
    pub pending: u32,
    pub order: Option<u64>,
    pub cycle_root: Option<u32>,
}

#[derive(Clone, Copy, Debug, Collect)]
#[collect(no_drop)]
pub enum ResolvedBinding<'scope> {
    Slot(JsEnv<'scope>, u16),
    Namespace(u32),
}

#[derive(Clone, Collect)]
#[collect(no_drop)]
pub struct ModuleRecord<'scope> {
    pub path: JsString<'scope>,
    pub env: JsEnv<'scope>,
    #[collect(require_static)]
    pub info: alloc::sync::Arc<ModuleInfo>,
    pub main: FuncId,
    pub code: crate::Code,
    pub function: JsObject<'scope>,
    pub status: ModuleStatus,
    pub error: Option<Value<'scope>>,
    pub evaluation: Evaluation,
    pub async_parents: Vec<u32>,
    pub capability: Option<JsObject<'scope>>,
    pub requested: Vec<u32>,
    pub imports: Vec<ResolvedBinding<'scope>>,
    pub namespace: Option<JsObject<'scope>>,
    pub meta: Option<JsObject<'scope>>,
}

impl core::fmt::Debug for ModuleRecord<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ModuleRecord").field("path", &&**self.path).finish()
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Resolution<'scope> {
    Found(ResolvedBinding<'scope>),
    NotFound,
    Ambiguous,
}

impl ResolvedBinding<'_> {
    fn same(self, other: Self) -> bool {
        match (self, other) {
            (ResolvedBinding::Slot(e1, s1), ResolvedBinding::Slot(e2, s2)) => ScopeLocal::ptr_eq(e1, e2) && s1 == s2,
            (ResolvedBinding::Namespace(x), ResolvedBinding::Namespace(y)) => x == y,
            _ => false,
        }
    }
}

impl<'scope> Scope<'scope, '_> {
    pub fn modules(&self) -> &'scope [ModuleRecord<'scope>] {
        self.context.modules.get()
    }

    pub fn update_module(&self, m: u32, mut f: impl FnMut(&mut ModuleRecord<'scope>)) {
        self.context.modules.update(self.mc, |mods| {
            let mut mods = mods.clone();
            f(&mut mods[m as usize]);
            mods
        });
    }

    pub fn push_module(&self, record: &ModuleRecord<'scope>) -> u32 {
        self.context.modules.update_with(self.mc, |mods| {
            let mut mods = mods.clone();
            mods.push(record.clone());
            let id = u32::try_from(mods.len() - 1).unwrap_or(u32::MAX);
            (mods, id)
        })
    }

    fn module_info(&self, m: u32) -> (alloc::sync::Arc<ModuleInfo>, JsEnv<'scope>, Vec<u32>) {
        let r = &self.modules()[m as usize];
        (r.info.clone(), r.env, r.requested.clone())
    }

    pub fn resolve_export(&self, m: u32, name: &str, seen: &mut Vec<(u32, Box<str>)>) -> Resolution<'scope> {
        if seen.iter().any(|(x, n)| *x == m && **n == *name) {
            return Resolution::NotFound;
        }
        seen.push((m, name.into()));
        let (info, env, requested) = self.module_info(m);
        if let Some(&(_, slot)) = info.local_exports.iter().find(|(n, _)| **n == *name) {
            return Resolution::Found(ResolvedBinding::Slot(env, slot));
        }
        if let Some((_, req, import)) = info.indirect_exports.iter().find(|(n, _, _)| **n == *name) {
            let target = requested[*req];
            return match import {
                ImportName::Namespace => Resolution::Found(ResolvedBinding::Namespace(target)),
                ImportName::Named(n) => self.resolve_export(target, n, seen),
            };
        }
        if name == "default" {
            return Resolution::NotFound;
        }
        let mut found: Option<ResolvedBinding<'scope>> = None;
        for &req in &info.star_exports {
            match self.resolve_export(requested[req], name, seen) {
                Resolution::Ambiguous => return Resolution::Ambiguous,
                Resolution::NotFound => {}
                Resolution::Found(b) => match found {
                    None => found = Some(b),
                    Some(f) if f.same(b) => {}
                    Some(_) => return Resolution::Ambiguous,
                },
            }
        }
        found.map_or(Resolution::NotFound, Resolution::Found)
    }

    pub fn exported_names(&self, m: u32, seen: &mut Vec<u32>) -> Vec<Box<str>> {
        if seen.contains(&m) {
            return Vec::new();
        }
        seen.push(m);
        let (info, _, requested) = self.module_info(m);
        let mut out: Vec<Box<str>> = info.local_exports.iter().map(|(n, _)| n.clone()).chain(info.indirect_exports.iter().map(|(n, _, _)| n.clone())).collect();
        for &req in &info.star_exports {
            for n in self.exported_names(requested[req], seen) {
                if &*n != "default" && !out.contains(&n) {
                    out.push(n);
                }
            }
        }
        out
    }

    pub fn binding_value(&mut self, b: ResolvedBinding<'scope>, name: &str) -> Result<Value<'scope>, Error> {
        match b {
            ResolvedBinding::Slot(env, slot) => match env.load(usize::from(slot)).kind() {
                ValueKind::Uninit => Err(self.throw(ErrorKind::Reference, &alloc::format!("Cannot access '{name}' before initialization"))),
                v => Ok(v.into()),
            },
            ResolvedBinding::Namespace(m) => Ok(self.module_namespace(m).into()),
        }
    }

    pub fn module_namespace(&mut self, m: u32) -> JsObject<'scope> {
        if let Some(ns) = self.modules()[m as usize].namespace {
            return ns;
        }
        let mut names = self.exported_names(m, &mut Vec::new());
        names.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
        let mut exports = Vec::new();
        for n in names {
            if let Resolution::Found(b) = self.resolve_export(m, &n, &mut Vec::new()) {
                exports.push((self.string(&n), b));
            }
        }
        let ns = crate::ObjectData { kind: ObjectKind::ModuleNamespace(exports), proto: None, non_extensible: true, ..crate::ObjectData::default() }.alloc(self.mc);
        let tag = self.string("Module");
        ns.borrow_mut(self.mc).define_symbol(self.context.symbols.to_string_tag, Property { slot: Slot::Data(tag.into()), enumerable: false, writable: false, configurable: false });
        self.update_module(m, |r| r.namespace = Some(ns));
        ns
    }

    #[expect(clippy::option_option, reason = "outer: whether it is a string key on a namespace object; inner: whether that export exists")]
    pub(crate) fn namespace_export(o: JsObject<'scope>, key: Key<'_, 'scope>) -> Option<Option<ResolvedBinding<'scope>>> {
        let Key::Str(k) = key else { return None };
        if o.is_plain() {
            return None;
        }
        match &o.borrow().kind {
            ObjectKind::ModuleNamespace(exports) => Some(exports.iter().find(|(n, _)| ***n == *k).map(|&(_, b)| b)),
            _ => None,
        }
    }

    pub(crate) fn namespace_own(&mut self, b: Option<ResolvedBinding<'scope>>, key: &str) -> Result<Option<Property<'scope>>, Error> {
        let Some(b) = b else { return Ok(None) };
        let v = self.binding_value(b, key)?;
        Ok(Some(Property { slot: Slot::Data(v), enumerable: true, writable: true, configurable: false }))
    }

    pub(crate) fn namespace_keys(o: JsObject<'scope>) -> Option<Vec<PropertyKey<'scope>>> {
        let d = o.borrow();
        let ObjectKind::ModuleNamespace(exports) = &d.kind else { return None };
        let mut keys: Vec<PropertyKey<'scope>> = exports.iter().map(|&(n, _)| PropertyKey::String(n)).collect();
        keys.extend(d.symbols.iter().map(|&(s, _)| PropertyKey::Symbol(s)));
        Some(keys)
    }
}
