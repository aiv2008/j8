use alloc::string::String;
use j8compiler::Compile;
use alloc::vec::Vec;

use j8framework::{Scope, Closure, Error, ErrorKind, ImportName, JsObject, ModuleStatus, ObjectData, Resolution, ResolvedBinding, Value};

use crate::Executor;

pub const EVAL_FILE: &str = "[eval]";
const SOURCE_URL: &str = "//# sourceURL=";

impl Executor<'_> {
    pub fn load_module(&self, a: &mut Scope<'_, '_>, path: &str, text: &str) -> Result<u32, Error> {
        if let Some(i) = a.modules().iter().position(|m| **m.path == *path) {
            return Ok(u32::try_from(i).unwrap_or(u32::MAX));
        }
        let src = j8framework::Source { text, file: path };
        let (module, info) = match &self.cache {
            Some(cache) => cache.module(&src, self.debug_info())?,
            None => <j8bytecode::Module as Compile>::compile_module(&src, self.debug_info()).map(|(m, info)| (m, alloc::sync::Arc::new(info)))?,
        };
        let main = self.load(a.mc, module);
        let mut slots = alloc::vec![Value::Undefined; usize::from(info.slots)];
        for &s in &info.lexical {
            slots[usize::from(s)] = Value::Uninit;
        }
        let env = j8framework::Env::new(slots, None).alloc(a.mc);
        let function = self.function_object(a, Closure::new(main, Some(env)));
        let record = j8framework::ModuleRecord {
            path: a.string(path),
            env,
            info: alloc::sync::Arc::clone(&info),
            main,
            code: self.function(main).code(),
            function,
            status: ModuleStatus::Linked,
            error: None,
            evaluation: j8framework::Evaluation::default(),
            async_parents: Vec::new(),
            capability: None,
            requested: Vec::new(),
            imports: Vec::new(),
            namespace: None,
            meta: None,
        };
        let index = a.push_module(&record);
        self.module_of.borrow_mut().insert(main.0 - j8bytecode::Module::MAIN, index);
        let mut requested = Vec::with_capacity(info.requests.len());
        for spec in &info.requests {
            let Some((p, t)) = a.platform.load_module(path, spec) else {
                return Err(Error::Syntax(alloc::format!("{path}: Cannot find module '{spec}'")));
            };
            requested.push(self.load_module(a, &p, &t)?);
        }
        a.update_module(index, |r| r.requested.clone_from(&requested));
        Ok(index)
    }

    pub fn link(a: &mut Scope<'_, '_>, m: u32, seen: &mut Vec<u32>) -> Result<(), Error> {
        if seen.contains(&m) {
            return Ok(());
        }
        seen.push(m);
        let (info, requested, path, done) = {
            let r = &a.modules()[m as usize];
            (alloc::sync::Arc::clone(&r.info), r.requested.clone(), String::from(&**r.path), r.imports.len() == r.info.imports.len() && !r.info.imports.is_empty())
        };
        for &r in &requested {
            Self::link(a, r, seen)?;
        }
        if done {
            return Ok(());
        }
        let mut imports = Vec::with_capacity(info.imports.len());
        for e in &info.imports {
            let target = requested[e.request];
            let b = match &e.name {
                ImportName::Namespace => ResolvedBinding::Namespace(target),
                ImportName::Named(n) => match a.resolve_export(target, n, &mut Vec::new()) {
                    Resolution::Found(b) => b,
                    Resolution::NotFound => return Err(Error::Syntax(alloc::format!("{path}: The requested module '{}' does not provide an export named '{n}'", info.requests[e.request]))),
                    Resolution::Ambiguous => return Err(Error::Syntax(alloc::format!("{path}: The requested module '{}' contains conflicting star exports for name '{n}'", info.requests[e.request]))),
                },
            };
            imports.push(b);
        }
        for (name, _, _) in &info.indirect_exports {
            if !matches!(a.resolve_export(m, name, &mut Vec::new()), Resolution::Found(_)) {
                return Err(Error::Syntax(alloc::format!("{path}: cannot resolve export '{name}'")));
            }
        }
        a.update_module(m, |r| r.imports.clone_from(&imports));
        Ok(())
    }

    pub fn load_script(&self, mc: &j8framework::Mutation<'_>, module: j8bytecode::Module, file: &str) -> j8framework::FuncId {
        let main = self.load(mc, module);
        self.script_files.borrow_mut().insert(main.0 - j8bytecode::Module::MAIN, file.into());
        main
    }

    pub fn source_url(src: &str) -> Option<&str> {
        let at = src.rfind(SOURCE_URL)? + SOURCE_URL.len();
        let url = src[at..].split(char::is_whitespace).next()?;
        (!url.is_empty()).then_some(url)
    }

    pub fn referrer(&self, a: &Scope<'_, '_>, base: u32) -> String {
        match self.module_index(base) {
            Some(m) => String::from(&**a.modules()[m as usize].path),
            None => self.script_files.borrow().get(&base).cloned().unwrap_or_default(),
        }
    }

    pub fn module_index(&self, base: u32) -> Option<u32> {
        self.module_of.borrow().get(&base).copied()
    }

    pub fn import_value<'scope>(&self, a: &mut Scope<'scope, '_>, base: u32, index: u16) -> Result<Value<'scope>, Error> {
        let Some(m) = self.module_index(base) else { return Err(a.throw(ErrorKind::Reference, "import binding outside a module")) };
        let b = a.modules()[m as usize].imports[usize::from(index)];
        a.binding_value(b, "import")
    }

    pub fn import_meta<'scope>(&self, a: &mut Scope<'scope, '_>, base: u32) -> Result<Value<'scope>, Error> {
        let Some(m) = self.module_index(base) else { return Err(a.throw(ErrorKind::Syntax, "Cannot use 'import.meta' outside a module")) };
        if let Some(meta) = a.modules()[m as usize].meta {
            return Ok(meta.into());
        }
        let meta = ObjectData::default().alloc(a.mc);
        let url = a.modules()[m as usize].path;
        meta.borrow_mut(a.mc).set("url", url.into());
        a.update_module(m, |r| r.meta = Some(meta));
        Ok(meta.into())
    }

    pub fn import<'scope>(&self, a: &mut Scope<'scope, '_>, path: &str, text: &str) -> Result<JsObject<'scope>, Error> {
        let module = self.load_module(a, path, text)?;
        Self::link(a, module, &mut Vec::new())?;
        let promise = a.new_promise();
        a.enqueue_job(j8framework::Job::Import { module, promise });
        Ok(promise)
    }

    pub fn dynamic_import<'scope>(&self, a: &mut Scope<'scope, '_>, referrer: &str, specifier: Value<'scope>) -> Result<Value<'scope>, Error> {
        let r = (|| -> Result<JsObject<'scope>, Error> {
            let spec = a.to_string(specifier)?;
            let Some((path, text)) = a.platform.load_module(referrer, &spec) else {
                return Err(a.throw(ErrorKind::Type, &alloc::format!("Cannot find module '{}'", &**spec)));
            };
            self.import(a, &path, &text)
        })();
        let reason = match r {
            Ok(promise) => return Ok(promise.into()),
            Err(Error::Exception) => a.take_exception(),
            Err(Error::Syntax(msg)) => a.error(ErrorKind::Syntax, &msg).into(),
            Err(e) => return Err(e),
        };
        let promise = a.new_promise();
        a.reject_promise(promise, reason);
        Ok(promise.into())
    }
}
