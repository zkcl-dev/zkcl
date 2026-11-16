use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::vec::IntoIter;

use super::{
    Continuation, ContinuationStep, Invocation, Resume, Values, designator, evaluate::Evaluation,
    proper_list, symbol_value,
};
use crate::condition::{Class, Fault};
use crate::heap::{ListSpine, RootSlot, Word};
use crate::loader::{self, LoadFile};
use crate::package::Package;
use crate::pathname::{COMPILED_FILE_TYPE, Pathname};
use crate::symbol::Name;
use crate::{EvalError, NativeFunction, Runtime};

mod atomic_word;
mod bundled;
mod compiled;
#[cfg(feature = "syntax-editing")]
mod syntax;

/// The bundled source registered for `name`, if this binary carries one.
pub(crate) fn bundled_source(name: &str) -> Option<&'static str> {
    bundled::find(name).map(|(_, source)| source)
}

/// Compile a bundled module into this process's FASL cache.
pub(crate) fn warm_bundled(
    source: &'static str,
    name: &str,
    foreign_calls: bool,
) -> Result<(), EvalError> {
    compiled::warm(source, name, foreign_calls)
}

pub(crate) fn initialize(runtime: &Runtime) -> Result<(), EvalError> {
    atomic_word::initialize(runtime)?;
    super::atomic::initialize(runtime)?;
    #[cfg(feature = "syntax-editing")]
    syntax::initialize(runtime)?;
    let providers = export_symbol(runtime, "*MODULE-PROVIDER-FUNCTIONS*")?;
    runtime.heap.proclaim_special(providers);
    runtime.heap.set_symbol_cell(providers, false, Word::NIL);

    let arguments: Arc<NativeFunction> = Arc::new(|runtime, args, at| {
        if !args.is_empty() {
            return Err(arity_error(
                at,
                "COMMAND-LINE-ARGUMENTS expects no arguments",
            ));
        }
        let arguments = runtime.command_line_arguments.borrow();
        let mut result = runtime.nil();
        for text in arguments.iter().rev() {
            let value = runtime
                .make_string(text)
                .map_err(|error| EvalError::out_of_memory(at, error))?;
            result = runtime
                .cons(&value, &result)
                .map_err(|error| EvalError::out_of_memory(at, error))?;
        }
        Ok(vec![result])
    });
    let compilation_target: Arc<NativeFunction> = Arc::new(|runtime, args, at| {
        if !args.is_empty() {
            return Err(arity_error(at, "COMPILATION-TARGET expects no arguments"));
        }
        let triple = runtime.compilation_target().to_string();
        Ok(vec![runtime.root(allocate_string(runtime, &triple, at)?)])
    });
    let exit: Arc<NativeFunction> = Arc::new(|runtime, args, at| {
        let [status] = args else {
            return Err(arity_error(at, "SET-EXIT-CODE expects one argument"));
        };
        let Some(code) = status
            .as_fixnum()
            .and_then(|value| u8::try_from(value).ok())
        else {
            let integer = runtime.intern(&Name::common_lisp("INTEGER"), at)?;
            let expected = super::methods::list_from_words(
                runtime,
                vec![
                    integer,
                    Word::fixnum(0).unwrap(),
                    Word::fixnum(255).unwrap(),
                ],
                at,
            )?;
            return Err(Fault::type_error_specifier(runtime, 
                status.slot.get(),
                expected,
                EvalError::new(at, "exit code must be an integer from 0 to 255"),
            ));
        };
        runtime.exit_code.set(code);
        Ok(vec![status.clone()])
    });
    let getenv: Arc<NativeFunction> = Arc::new(|runtime, args, at| {
        let [name] = args else {
            return Err(arity_error(at, "GETENV expects one argument"));
        };
        let name = runtime.string(name).ok_or_else(|| {
            Fault::type_error(runtime, 
                name.slot.get(),
                "STRING",
                EvalError::new(at, "expected an environment variable name"),
            )
        })?;
        let value = match runtime.environment_variable(&name) {
            Ok(Some(value)) => allocate_string(runtime, &value, at)?,
            Ok(None) | Err(std::env::VarError::NotPresent) => Word::NIL,
            Err(std::env::VarError::NotUnicode(_)) => {
                return Err(Fault::plain(
                    Class::Error,
                    EvalError::new(at, format!("environment variable {name:?} is not Unicode")),
                ));
            }
        };
        Ok(vec![runtime.root(value)])
    });
    let getcwd: Arc<NativeFunction> = Arc::new(|runtime, args, at| {
        if !args.is_empty() {
            return Err(arity_error(at, "GETCWD expects no arguments"));
        }
        let path = runtime.working_directory().map_err(|error| {
            Fault::plain(
                Class::FileError,
                EvalError::file_io(at, Path::new("."), error),
            )
        })?;
        let directory = Pathname::from_directory_path(&path).map_err(|error| {
            Fault::plain(Class::FileError, EvalError::file_format(at, &path, error))
        })?;
        Ok(vec![directory.to_lisp_value(runtime, at)?])
    });
    for (name, callback) in [
        ("COMMAND-LINE-ARGUMENTS", arguments),
        ("COMPILATION-TARGET", compilation_target),
        ("FINALIZE", Arc::new(crate::finalizers::register)),
        ("CANCEL-FINALIZATION", Arc::new(crate::finalizers::cancel)),
        ("GC", Arc::new(crate::finalizers::collect)),
        ("SET-EXIT-CODE", exit),
        ("GETENV", getenv),
        ("HASH-TABLE-WEAKNESS", Arc::new(super::hash_tables::weakness)),
        ("GETCWD", getcwd),
        ("MAKE-WEAK-POINTER", Arc::new(make_weak_pointer)),
        ("WEAK-POINTER-P", Arc::new(weak_pointer_p)),
        ("WEAK-POINTER-VALUE", Arc::new(weak_pointer_value)),
    ] {
        let symbol = export_symbol(runtime, name)?;
        let mut natives = runtime.natives.borrow_mut();
        let function = Word::native(natives.len());
        natives.push(callback);
        runtime.heap.set_symbol_cell(symbol, true, function);
    }
    #[cfg(feature = "asdf-ports")]
    bundled::initialize(runtime)?;
    Ok(())
}

fn make_weak_pointer<'rt>(
    runtime: &'rt Runtime,
    args: &[crate::Root<'rt>],
    at: usize,
) -> Result<Vec<crate::Root<'rt>>, Fault> {
    let [target] = args else {
        return Err(arity_error(at, "MAKE-WEAK-POINTER expects one argument"));
    };
    let pointer = runtime
        .heap
        .weak_pointer(&target.slot)
        .map_err(|error| EvalError::out_of_memory(at, error))?;
    Ok(vec![runtime.root(pointer)])
}

fn weak_pointer_p<'rt>(
    runtime: &'rt Runtime,
    args: &[crate::Root<'rt>],
    at: usize,
) -> Result<Vec<crate::Root<'rt>>, Fault> {
    let [object] = args else {
        return Err(arity_error(at, "WEAK-POINTER-P expects one argument"));
    };
    Ok(vec![runtime.root(
        if runtime.heap.is_weak_pointer(object.slot.get()) {
            Word::T
        } else {
            Word::NIL
        },
    )])
}

fn weak_pointer_value<'rt>(
    runtime: &'rt Runtime,
    args: &[crate::Root<'rt>],
    at: usize,
) -> Result<Vec<crate::Root<'rt>>, Fault> {
    let [pointer] = args else {
        return Err(arity_error(at, "WEAK-POINTER-VALUE expects one argument"));
    };
    if let Some(target) = runtime.heap.weak_pointer_value(pointer.slot.get()) {
        return Ok(vec![runtime.root(target), runtime.root(Word::T)]);
    }
    if runtime.heap.is_weak_pointer(pointer.slot.get()) {
        return Ok(vec![runtime.nil(), runtime.nil()]);
    }
    let satisfies = runtime.intern(&Name::common_lisp("SATISFIES"), at)?;
    let predicate = runtime
        .packages
        .borrow()
        .present(Package::ZK, "WEAK-POINTER-P")
        .expect("weak pointer predicate initialized")
        .0;
    let expected = super::methods::list_from_words(runtime, vec![satisfies, predicate], at)?;
    Err(Fault::type_error_specifier(runtime, 
        pointer.slot.get(),
        expected,
        EvalError::new(at, "expected a weak pointer"),
    ))
}

fn export_symbol(runtime: &Runtime, name: &str) -> Result<Word, EvalError> {
    let symbol = runtime.intern_package(Package::ZK, name, 0)?.0;
    runtime
        .packages
        .borrow_mut()
        .export(Package::ZK, name, symbol)
        .map_err(|error| EvalError::from_error(0, error))?;
    Ok(symbol)
}

pub(crate) fn arity_error(at: usize, message: &str) -> Fault {
    Fault::plain(Class::ProgramError, EvalError::new(at, message))
}

pub(super) fn provide(runtime: &Runtime, value: Word, at: usize) -> Result<Word, Fault> {
    let name = module_name(runtime, value, at)?;
    let variable = modules_variable(runtime);
    let modules = symbol_value(runtime, variable, at)?;
    if !contains(runtime, modules, &name, at)? {
        let variable = runtime.root_slot(variable);
        let modules = runtime.root_slot(modules);
        let name = if runtime.heap.is_string(value) {
            value
        } else {
            allocate_string(runtime, &name, at)?
        };
        let name = runtime.root_slot(name);
        let modules = runtime
            .heap
            .cons(|| (name.get(), modules.get()))
            .map_err(|error| EvalError::out_of_memory(at, error))?;
        runtime.heap.set_symbol_cell(variable.get(), false, modules);
    }
    Ok(Word::T)
}

pub(super) struct Require {
    files: IntoIter<RootSlot>,
    _loading: LoadingModule,
    pub(super) at: usize,
}

struct LoadingModule {
    name: String,
    loading: Rc<RefCell<HashSet<String>>>,
}

impl Require {
    pub(super) fn start(
        runtime: &Runtime,
        name: Word,
        pathnames: Word,
        resume: Resume,
        at: usize,
    ) -> Result<Option<Invocation<'_>>, Fault> {
        let module = module_name(runtime, name, at)?;
        let modules = symbol_value(runtime, modules_variable(runtime), at)?;
        if contains(runtime, modules, &module, at)? {
            return Ok(None);
        }
        let loading = Rc::clone(&runtime.modules_loading);
        if !loading.borrow_mut().insert(module.clone()) {
            return Err(Fault::plain(
                Class::Error,
                EvalError::new(at, format!("circular REQUIRE of module {module:?}")),
            ));
        }
        let loading = LoadingModule {
            name: module,
            loading,
        };
        let files = if pathnames != Word::NIL {
            if runtime.heap.cons_fields(pathnames).is_some() {
                rooted_list(runtime, pathnames, at)?
            } else {
                vec![runtime.root_slot(pathnames)]
            }
        } else if let Some(path) = resolve_module(runtime, &loading.name, at)? {
            // The search result is an OS path, so wildcard characters are literal.
            let pathname = Pathname::from_path(&path).map_err(|error| {
                Fault::plain(Class::FileError, EvalError::file_format(at, &path, error))
            })?;
            vec![pathname.to_lisp_value(runtime, at)?.slot]
        } else if let Some((registered, source)) = bundled::find(&loading.name) {
            // Bundled lookup folds ASCII case. The source only provides its
            // registered names, so a different spelling can already be loaded.
            if registered != loading.name && contains(runtime, modules, registered, at)? {
                return Ok(None);
            }
            let require = Box::new(Self {
                files: Vec::new().into_iter(),
                _loading: loading,
                at,
            });
            // A warmed module decodes its FASL; everything else still reads source.
            let file = compiled::fasl(runtime, source)
                .unwrap_or_else(|| LoadFile::embedded_source(source));
            return Ok(Some(Invocation::Evaluate(Box::new(Evaluation::module(
                file, require, resume,
            )))));
        } else {
            let providers = runtime
                .packages
                .borrow()
                .present(Package::ZK, "*MODULE-PROVIDER-FUNCTIONS*");
            let providers = match providers {
                Some((symbol, _)) => rooted_list(runtime, symbol_value(runtime, symbol, at)?, at)?,
                None => Vec::new(),
            };
            return Ok(Some(Invocation::Continuation(Box::new(Providers {
                providers: providers.into_iter(),
                designator: runtime.root_slot(name),
                loading,
                resume,
                at,
            }))));
        };
        let mut require = Box::new(Self {
            files: files.into_iter(),
            _loading: loading,
            at,
        });
        let file = require
            .next_file(runtime)?
            .expect("REQUIRE had a nonempty pathname list");
        Ok(Some(Invocation::Evaluate(Box::new(Evaluation::module(
            file, require, resume,
        )))))
    }

    pub(super) fn next_file(&mut self, runtime: &Runtime) -> Result<Option<LoadFile>, Fault> {
        let Some(pathname) = self.files.next() else {
            return Ok(None);
        };
        let args = runtime.root_frame(1);
        args.set(0, pathname.get());
        loader::prepare_lisp_load(runtime, &args, 0..1, self.at)
    }
}

impl Drop for LoadingModule {
    fn drop(&mut self) {
        self.loading.borrow_mut().remove(&self.name);
    }
}

struct Providers {
    providers: IntoIter<RootSlot>,
    designator: RootSlot,
    loading: LoadingModule,
    resume: Resume,
    at: usize,
}

impl Continuation for Providers {
    fn step(
        &mut self,
        runtime: &Runtime,
        values: &Values,
        returned: bool,
    ) -> Result<ContinuationStep, Fault> {
        if returned && values.get(0) != Word::NIL {
            return Ok(ContinuationStep::Done(Word::NIL));
        }
        let Some(provider) = self.providers.next() else {
            return Err(Fault::plain(
                Class::Error,
                EvalError::new(
                    self.at,
                    format!(
                        "module {:?} was not found in {} configured directories or by a module provider",
                        self.loading.name,
                        runtime.module_directories.borrow().len()
                    ),
                ),
            ));
        };
        let args = runtime.root_frame(1);
        args.set(0, self.designator.get());
        Ok(ContinuationStep::Call {
            target: designator(runtime, provider.get(), self.at)?,
            args,
            at: self.at,
        })
    }

    fn resume_to(&self) -> Resume {
        self.resume
    }
}

fn rooted_list(runtime: &Runtime, list: Word, at: usize) -> Result<Vec<RootSlot>, Fault> {
    let words = proper_list(runtime, list, at)
        .map_err(|error| Fault::type_error(runtime, list, "LIST", error))?;
    Ok(words
        .into_iter()
        .map(|word| runtime.root_slot(word))
        .collect())
}

fn resolve_module(runtime: &Runtime, name: &str, at: usize) -> Result<Option<PathBuf>, Fault> {
    let mut components = Path::new(name).components();
    if !matches!(components.next(), Some(Component::Normal(part)) if part == name)
        || components.next().is_some()
    {
        return Ok(None);
    }
    let directories = runtime.module_directories.borrow();
    let folded = name.to_ascii_lowercase();
    let filenames = ["lisp", COMPILED_FILE_TYPE]
        .into_iter()
        .flat_map(|extension| {
            std::iter::once(name)
                .chain((folded != name).then_some(folded.as_str()))
                .map(move |stem| format!("{stem}.{extension}"))
        })
        .collect::<Vec<_>>();
    for directory in directories.iter() {
        for filename in &filenames {
            let path = directory.join(filename);
            match path.metadata() {
                Ok(metadata) if metadata.is_file() => return Ok(Some(path)),
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(Fault::plain(
                        Class::FileError,
                        EvalError::file_io(at, &path, error),
                    ));
                }
            }
        }
    }
    Ok(None)
}

pub(super) fn implementation_type(runtime: &Runtime, at: usize) -> Result<Word, Fault> {
    allocate_string(runtime, "zkcl", at)
}

pub(super) fn implementation_version(runtime: &Runtime, at: usize) -> Result<Word, Fault> {
    allocate_string(runtime, env!("CARGO_PKG_VERSION"), at)
}

fn allocate_string(runtime: &Runtime, text: &str, at: usize) -> Result<Word, Fault> {
    runtime
        .heap
        .string(text)
        .map_err(|error| EvalError::out_of_memory(at, error).into())
}

fn module_name(runtime: &Runtime, value: Word, at: usize) -> Result<String, Fault> {
    runtime.string_designator_text(value).ok_or_else(|| {
        Fault::string_designator(runtime, value, EvalError::new(at, "expected a module name"))
    })
}

fn modules_variable(runtime: &Runtime) -> Word {
    runtime
        .find_named_symbol(&Name::common_lisp("*MODULES*"))
        .expect("*MODULES* is initialized by package bootstrap")
}

fn contains(runtime: &Runtime, modules: Word, name: &str, at: usize) -> Result<bool, Fault> {
    let invalid_list = || {
        Fault::type_error(runtime, 
            modules,
            "LIST",
            EvalError::new(at, "*MODULES* must be a proper list"),
        )
    };
    let mut spine = ListSpine::new(modules);
    while let Some((element, _)) = spine.next(&runtime.heap).map_err(|_| invalid_list())? {
        let matched = runtime
            .heap
            .with_string_slice(element, |candidate| candidate == name)
            .ok_or_else(|| {
                Fault::type_error(runtime, 
                    element,
                    "STRING",
                    EvalError::new(at, "*MODULES* entries must be strings"),
                )
            })?;
        if matched {
            return Ok(true);
        }
    }
    if spine.tail() != Word::NIL {
        return Err(invalid_list());
    }
    Ok(false)
}
