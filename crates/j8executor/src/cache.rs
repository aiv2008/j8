use alloc::boxed::Box;
use alloc::sync::Arc;
use core::hash::BuildHasher;
use std::hash::RandomState;

use j8bytecode::Module;
use j8compiler::{Compile, DebugInfo};
use j8framework::{Error, ModuleInfo, Source};
use j8lockfree::Chain;

#[derive(PartialEq, Eq)]
struct Key {
    file: Box<str>,
    len: usize,
    hash: [u64; 2],
    debug: DebugInfo,
}

struct Entry<T> {
    key: Key,
    code: Module,
    info: T,
}

impl<T> Entry<T> {
    fn module(&self) -> Module {
        self.code.clone()
    }
}

#[derive(Default)]
pub struct CodeCache {
    seeds: [RandomState; 2],
    scripts: Chain<Entry<()>>,
    modules: Chain<Entry<Arc<ModuleInfo>>>,
}

impl CodeCache {
    pub fn shared() -> Arc<Self> {
        Arc::default()
    }

    pub fn script(&self, src: &Source<'_>, debug: DebugInfo) -> Result<Module, Error> {
        let key = self.key(src, debug);
        if let Some(e) = self.scripts.iter().find(|e| e.key == key) {
            return Ok(e.module());
        }
        let module = Module::compile(src, j8framework::CodeKind::Script, debug)?;
        self.scripts.push(Entry { key, code: module.clone(), info: () });
        Ok(module)
    }

    pub fn module(&self, src: &Source<'_>, debug: DebugInfo) -> Result<(Module, Arc<ModuleInfo>), Error> {
        let key = self.key(src, debug);
        if let Some(e) = self.modules.iter().find(|e| e.key == key) {
            return Ok((e.module(), Arc::clone(&e.info)));
        }
        let (module, info) = Module::compile_module(src, debug)?;
        let info = Arc::new(info);
        self.modules.push(Entry { key, code: module.clone(), info: Arc::clone(&info) });
        Ok((module, info))
    }

    fn key(&self, src: &Source<'_>, debug: DebugInfo) -> Key {
        Key { file: src.file.into(), len: src.text.len(), hash: self.seeds.each_ref().map(|s| s.hash_one(src.text)), debug }
    }
}
