//! Process-independent snapshots of the Lisp global environment.
use crate::heap::{RootSlot, Word};
use crate::{EvalError, Root, Runtime};
use std::path::Path;
use std::rc::Rc;

const MAGIC: &[u8] = b"\x89ZKCLIMAGE";

/// Explicit boundary for process resources retained by a Lisp image.
/// Hooks must preserve their own resource identity and reject unavailable resources.
pub trait ImageHooks {
    fn save_external(
        &self,
        runtime: &Runtime,
        value: &Root<'_>,
    ) -> Result<Option<(String, Vec<u8>)>, EvalError>;
    fn restore_external<'rt>(
        &self,
        runtime: &'rt Runtime,
        key: &str,
        data: &[u8],
    ) -> Result<Root<'rt>, EvalError>;
}

struct NoHooks;
impl ImageHooks for NoHooks {
    fn save_external(
        &self,
        _: &Runtime,
        _: &Root<'_>,
    ) -> Result<Option<(String, Vec<u8>)>, EvalError> {
        Ok(None)
    }
    fn restore_external<'rt>(
        &self,
        _: &'rt Runtime,
        key: &str,
        _: &[u8],
    ) -> Result<Root<'rt>, EvalError> {
        Err(EvalError::new(
            0,
            format!("image requires a restore hook for external resource {key:?}"),
        ))
    }
}

/// Rebind a specific set of host callback bindings by their stable package/name.
/// The host must install those bindings in the candidate before restoring it.
pub struct NamedImageHooks {
    names: Vec<(String, String)>,
}
impl NamedImageHooks {
    pub fn new(names: &[(&str, &str)]) -> Self {
        Self {
            names: names.iter().map(|&(p, n)| (p.into(), n.into())).collect(),
        }
    }
    fn lookup<'rt>(
        &self,
        runtime: &'rt Runtime,
        package: &str,
        name: &str,
    ) -> Result<Option<Root<'rt>>, EvalError> {
        let Some(symbol) = runtime.lookup_symbol(package, name)? else {
            return Ok(None);
        };
        Ok(runtime
            .heap
            .symbol_cell(symbol.slot.get(), true)
            .map(|word| runtime.root(word)))
    }
}
impl ImageHooks for NamedImageHooks {
    fn save_external(
        &self,
        runtime: &Runtime,
        value: &Root<'_>,
    ) -> Result<Option<(String, Vec<u8>)>, EvalError> {
        for (package, name) in &self.names {
            if self
                .lookup(runtime, package, name)?
                .is_some_and(|function| function.same_identity(value))
            {
                return Ok(Some((
                    format!("named-callback:{package}"),
                    name.as_bytes().to_vec(),
                )));
            }
        }
        Ok(None)
    }
    fn restore_external<'rt>(
        &self,
        runtime: &'rt Runtime,
        key: &str,
        data: &[u8],
    ) -> Result<Root<'rt>, EvalError> {
        let package = key.strip_prefix("named-callback:").ok_or_else(invalid)?;
        let name = std::str::from_utf8(data).map_err(|_| invalid())?;
        if !self.names.iter().any(|(p, n)| p == package && n == name) {
            return Err(EvalError::new(
                0,
                "image callback is not permitted by the restore hook",
            ));
        }
        self.lookup(runtime, package, name)?.ok_or_else(|| {
            EvalError::new(
                0,
                format!("install image callback {package}::{name} before restoring"),
            )
        })
    }
}

pub(crate) struct ImageWriter<'a> {
    pub(crate) runtime: &'a Runtime,
    bytes: Vec<u8>,
    words: Vec<RootSlot>,
    nesting: Rc<std::cell::Cell<usize>>,
}
impl<'a> ImageWriter<'a> {
    fn new(runtime: &'a Runtime) -> Self {
        Self {
            runtime,
            bytes: Vec::new(),
            words: Vec::new(),
            nesting: Rc::default(),
        }
    }
    pub(crate) fn enter_nested(&self) -> Result<ImageNesting, EvalError> {
        ImageNesting::enter(&self.nesting)
    }
    pub(crate) fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }
    pub(crate) fn usize(&mut self, value: usize) {
        self.u64(value as u64);
    }
    pub(crate) fn bool(&mut self, value: bool) {
        self.bytes.push(u8::from(value));
    }
    pub(crate) fn string(&mut self, value: &str) {
        self.usize(value.len());
        self.bytes.extend_from_slice(value.as_bytes());
    }
    pub(crate) fn word(&mut self, value: Word) -> Result<(), EvalError> {
        self.usize(self.words.len());
        self.words.push(self.runtime.root_slot(value));
        Ok(())
    }
    pub(crate) fn form(&mut self, value: &crate::read::Form) -> Result<(), EvalError> {
        value.write_image(self)
    }
    pub(crate) fn name(&mut self, value: &crate::symbol::Name) -> Result<(), EvalError> {
        if let Some(word) = value.rooted_word() {
            self.usize(2);
            self.word(word)
        } else {
            match value.key() {
                crate::symbol::NameKey::CommonLisp(_) => self.usize(0),
                crate::symbol::NameKey::Keyword(_) => self.usize(1),
                _ => return Err(invalid()),
            };
            self.string(value.spelling());
            Ok(())
        }
    }
    pub(crate) fn key(&mut self, key: &crate::symbol::NameKey) {
        use crate::symbol::NameKey;
        match key {
            NameKey::CommonLisp(text) => {
                self.usize(0);
                self.string(text)
            }
            NameKey::Keyword(text) => {
                self.usize(1);
                self.string(text)
            }
            NameKey::Symbol(id) => {
                self.usize(2);
                self.usize(*id)
            }
        }
    }
}
pub(crate) struct ImageReader<'a> {
    pub(crate) runtime: &'a Runtime,
    bytes: &'a [u8],
    words: &'a [RootSlot],
    nesting: Rc<std::cell::Cell<usize>>,
}
impl<'a> ImageReader<'a> {
    fn new(runtime: &'a Runtime, bytes: &'a [u8], words: &'a [RootSlot]) -> Self {
        Self {
            runtime,
            bytes,
            words,
            nesting: Rc::default(),
        }
    }
    fn take(&mut self, len: usize) -> Result<&'a [u8], EvalError> {
        let value = self.bytes.get(..len).ok_or_else(invalid)?;
        self.bytes = &self.bytes[len..];
        Ok(value)
    }
    pub(crate) fn enter_nested(&self) -> Result<ImageNesting, EvalError> {
        ImageNesting::enter(&self.nesting)
    }
    pub(crate) fn u64(&mut self) -> Result<u64, EvalError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    pub(crate) fn usize(&mut self) -> Result<usize, EvalError> {
        usize::try_from(self.u64()?).map_err(|_| invalid())
    }
    pub(crate) fn bool(&mut self) -> Result<bool, EvalError> {
        match self.take(1)?[0] {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(invalid()),
        }
    }
    pub(crate) fn string(&mut self) -> Result<String, EvalError> {
        let len = self.usize()?;
        String::from_utf8(self.take(len)?.to_vec()).map_err(|_| invalid())
    }
    pub(crate) fn word(&mut self) -> Result<Word, EvalError> {
        let index = self.usize()?;
        self.words.get(index).map(RootSlot::get).ok_or_else(invalid)
    }
    pub(crate) fn form(&mut self) -> Result<crate::read::Form, EvalError> {
        crate::read::Form::read_image(self)
    }
    pub(crate) fn name(&mut self) -> Result<crate::symbol::Name, EvalError> {
        match self.usize()? {
            0 => Ok(crate::symbol::Name::common_lisp(&self.string()?)),
            1 => Ok(crate::symbol::Name::keyword(&self.string()?)),
            2 => {
                let word = self.word()?;
                crate::symbol::Name::from_word(self.runtime, word).ok_or_else(invalid)
            }
            _ => Err(invalid()),
        }
    }
    pub(crate) fn key(&mut self) -> Result<crate::symbol::NameKey, EvalError> {
        use crate::symbol::NameKey;
        match self.usize()? {
            0 => Ok(NameKey::CommonLisp(self.string()?.into())),
            1 => Ok(NameKey::Keyword(self.string()?.into())),
            2 => Ok(NameKey::Symbol(self.usize()?)),
            _ => Err(invalid()),
        }
    }
}
pub(crate) struct ImageNesting(Rc<std::cell::Cell<usize>>);
impl ImageNesting {
    fn enter(depth: &Rc<std::cell::Cell<usize>>) -> Result<Self, EvalError> {
        if depth.get() >= 512 {
            return Err(EvalError::new(
                0,
                "runtime image metadata exceeds the nesting limit",
            ));
        }
        depth.set(depth.get() + 1);
        Ok(Self(Rc::clone(depth)))
    }
}
impl Drop for ImageNesting {
    fn drop(&mut self) {
        self.0.set(self.0.get() - 1);
    }
}

// Images store runtime metadata and refer to builtin native functions by index.
fn native_abi_fingerprint() -> u64 {
    let sources = [
        include_str!("image.rs"),
        include_str!("lib.rs"),
        include_str!("macros/standard.rs"),
        include_str!("patterns/lexical.rs"),
        include_str!("patterns/mod.rs"),
        include_str!("vm/methods.rs"),
        include_str!("vm/mop.rs"),
        include_str!("vm/method_combination/mod.rs"),
        include_str!("vm/class_definition.rs"),
        include_str!("vm/modules/mod.rs"),
        include_str!("vm/mod.rs"),
        include_str!("vm/function_mop.rs"),
        include_str!("vm/instances/bootstrap.rs"),
    ];
    sources
        .iter()
        .flat_map(|source| source.bytes())
        .fold(0xcbf29ce484222325u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
        })
}
pub(crate) fn invalid() -> EvalError {
    EvalError::new(0, "invalid runtime image metadata")
}

impl Runtime {
    /// Save the complete global Lisp environment. Active evaluations, tasks and
    /// dynamic host scopes must finish first; call stacks are not persisted.
    pub fn save_image(&self, path: &Path) -> Result<(), EvalError> {
        self.save_image_with_hooks(path, &NoHooks)
    }
    pub fn save_image_with_hooks(
        &self,
        path: &Path,
        hooks: &dyn ImageHooks,
    ) -> Result<(), EvalError> {
        self.check_image_idle()?;
        self.initialize_packages()?;
        let mut writer = ImageWriter::new(self);
        self.write_image_metadata(&mut writer)?;
        use crate::vm::{ConstantTemplate, FunctionTemplate};
        let code = FunctionTemplate {
            name: Some("runtime image roots".into()),
            debug: Default::default(),
            instructions: vec![zkcl_bytecode::Instruction::SetValues(0..0)],
            constants: writer
                .words
                .iter()
                .map(|word| ConstantTemplate::Rooted(Rc::new(self.root_slot(word.get()))))
                .collect(),
            lambda_list: None,
            registers: 1,
            arity: zkcl_bytecode::Arity::ZERO,
            captures: Vec::new(),
            notinline: Vec::new(),
            vop_calls: Vec::new(),
            handlers: Vec::new(),
            restarts: Vec::new(),
            keywords: Vec::new(),
            jump_tables: Vec::new(),
            facts: Default::default(),
        };
        let mut builder = crate::fasl::CompiledFileBuilder::new(self, None);
        builder.push_image_entry(self, code, hooks)?;
        let graph = builder
            .finish(self)?
            .encode()
            .map_err(|e| EvalError::file_format(0, path, e))?;
        let mut bytes = Vec::with_capacity(MAGIC.len() + 8 + writer.bytes.len() + graph.len());
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&(writer.bytes.len() as u64).to_le_bytes());
        bytes.extend(writer.bytes);
        bytes.extend(graph);
        crate::fasl::write_fasl_atomically(path, &bytes).map_err(|e| EvalError::file_io(0, path, e))
    }
    /// Restore into an idle candidate runtime. Hosts should publish the candidate
    /// only after this succeeds, then reinstall their debugger and I/O services.
    pub fn restore_image(&mut self, path: &Path) -> Result<(), EvalError> {
        self.restore_image_with_hooks(path, &NoHooks)
    }
    pub fn restore_image_with_hooks(
        &mut self,
        path: &Path,
        hooks: &dyn ImageHooks,
    ) -> Result<(), EvalError> {
        self.check_image_idle()?;
        self.initialize_packages()?;
        let bytes = std::fs::read(path).map_err(|e| EvalError::file_io(0, path, e))?;
        if !bytes.starts_with(MAGIC) {
            return Err(invalid());
        }
        let head = MAGIC.len();
        let len = bytes.get(head..head + 8).ok_or_else(invalid)?;
        let len =
            usize::try_from(u64::from_le_bytes(len.try_into().unwrap())).map_err(|_| invalid())?;
        let end = (head + 8).checked_add(len).ok_or_else(invalid)?;
        let metadata = bytes.get(head + 8..end).ok_or_else(invalid)?;
        let mut header = ImageReader::new(self, metadata, &[]);
        if header.usize()? != self.builtin_native_count.get()
            || header.bool()? != self.foreign_calls
            || header.u64()? != native_abi_fingerprint()
        {
            return Err(EvalError::new(
                0,
                "runtime image requires the same builtin/FFI configuration",
            ));
        }
        let graph = zkcl_bytecode::CompiledFile::decode_for_target(
            bytes.get(end..).ok_or_else(invalid)?,
            self.compilation_target(),
        )
        .map_err(|e| EvalError::file_format(0, path, e))?;
        let words =
            crate::fasl::CompiledFileLoader::new(self, graph, None).image_roots(self, hooks)?;
        self.read_image_metadata(metadata, &words)
    }
    fn check_image_idle(&self) -> Result<(), EvalError> {
        if self
            .pause_requested
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(EvalError::new(
                0,
                "runtime image requires pending pause requests to be cleared",
            ));
        }
        self.host_io.check_image_idle()?;
        self.profiler.check_image_idle()?;
        if !self.foreign_symbols.is_empty() {
            return Err(EvalError::new(
                0,
                "foreign library symbol registrations require host reinitialization before an image can be saved",
            ));
        }
        if self.evaluation_active.get()
            || self.eval_depth.get() != 0
            || self.active_scheduler.get().is_some()
            || self.child_task_running.get()
            || self.cleanup_depth.get() != 0
            || self.rust_pin_depth.get() != 0
            || !self.host_scopes.borrow().is_empty()
            || !self.macro_environments.borrow().is_empty()
            || !self.modules_loading.borrow().is_empty()
        {
            return Err(EvalError::new(
                0,
                "runtime image requires no active evaluation, task, dynamic stack or host scope",
            ));
        }
        Ok(())
    }
}

fn write_optional_root(
    image: &mut ImageWriter<'_>,
    value: Option<&RootSlot>,
) -> Result<(), EvalError> {
    image.bool(value.is_some());
    if let Some(value) = value {
        image.word(value.get())?;
    }
    Ok(())
}
fn read_optional_root(image: &mut ImageReader<'_>) -> Result<Option<RootSlot>, EvalError> {
    Ok(if image.bool()? {
        Some(image.runtime.root_slot(image.word()?))
    } else {
        None
    })
}
fn write_id_roots(
    image: &mut ImageWriter<'_>,
    values: &std::collections::HashMap<usize, RootSlot>,
) -> Result<(), EvalError> {
    image.usize(values.len());
    for (id, value) in values {
        image.usize(*id);
        image.word(value.get())?;
    }
    Ok(())
}
fn read_id_roots(
    image: &mut ImageReader<'_>,
) -> Result<std::collections::HashMap<usize, RootSlot>, EvalError> {
    let mut values = std::collections::HashMap::new();
    for _ in 0..image.usize()? {
        values.insert(image.usize()?, image.runtime.root_slot(image.word()?));
    }
    Ok(values)
}
fn write_key_roots(
    image: &mut ImageWriter<'_>,
    values: &std::collections::HashMap<crate::symbol::NameKey, RootSlot>,
) -> Result<(), EvalError> {
    image.usize(values.len());
    for (key, value) in values {
        image.key(key);
        image.word(value.get())?;
    }
    Ok(())
}
fn read_key_roots(
    image: &mut ImageReader<'_>,
) -> Result<std::collections::HashMap<crate::symbol::NameKey, RootSlot>, EvalError> {
    let mut values = std::collections::HashMap::new();
    for _ in 0..image.usize()? {
        values.insert(image.key()?, image.runtime.root_slot(image.word()?));
    }
    Ok(values)
}
fn write_key_forms(
    image: &mut ImageWriter<'_>,
    values: &std::collections::HashMap<crate::symbol::NameKey, crate::read::Form>,
) -> Result<(), EvalError> {
    image.usize(values.len());
    for (key, value) in values {
        image.key(key);
        image.form(value)?;
    }
    Ok(())
}
fn read_key_forms(
    image: &mut ImageReader<'_>,
) -> Result<std::collections::HashMap<crate::symbol::NameKey, crate::read::Form>, EvalError> {
    let mut values = std::collections::HashMap::new();
    for _ in 0..image.usize()? {
        values.insert(image.key()?, image.form()?);
    }
    Ok(values)
}
fn write_strings(image: &mut ImageWriter<'_>, values: &[String]) {
    image.usize(values.len());
    for value in values {
        image.string(value);
    }
}
fn read_strings(image: &mut ImageReader<'_>) -> Result<Vec<String>, EvalError> {
    let mut values = Vec::new();
    for _ in 0..image.usize()? {
        values.push(image.string()?);
    }
    Ok(values)
}
pub(crate) fn write_path(
    image: &mut ImageWriter<'_>,
    value: &std::path::Path,
) -> Result<(), EvalError> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let bytes = value.as_os_str().as_bytes();
        image.usize(bytes.len());
        image.bytes.extend_from_slice(bytes);
    }
    #[cfg(not(unix))]
    {
        image.string(
            value
                .to_str()
                .ok_or_else(|| EvalError::new(0, "image path is not representable as Unicode"))?,
        );
    }
    Ok(())
}
pub(crate) fn read_path(image: &mut ImageReader<'_>) -> Result<std::path::PathBuf, EvalError> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        let len = image.usize()?;
        Ok(std::ffi::OsString::from_vec(image.take(len)?.to_vec()).into())
    }
    #[cfg(not(unix))]
    {
        Ok(image.string()?.into())
    }
}

struct ImageState {
    definition_sources:
        std::collections::HashMap<(usize, crate::DefinitionKind), crate::DefinitionInfo>,
    packages: crate::package::PackageRegistry,
    nil_name: Option<RootSlot>,
    t_name: Option<RootSlot>,
    next_symbol_id: usize,
    next_macro_environment: usize,
    declaration_handlers: std::collections::HashMap<crate::symbol::NameKey, RootSlot>,
    vops: crate::compile::vop::Registry,
    default_print_method: Word,
    debugger: Option<crate::vm::signaling::Debugger>,
    inspector: Option<RootSlot>,
    structures: crate::vm::structures::Registry,
    logical_hosts: crate::pathname::LogicalHosts,
    module_directories: Vec<std::path::PathBuf>,
    command_line_arguments: Vec<String>,
    environment: std::collections::HashMap<String, Option<String>>,
    working_directory: Option<std::path::PathBuf>,
    exit_code: u8,
    classes: crate::classes::Registry,
    invocation_protocol: Option<crate::vm::function_mop::InvocationProtocol>,
    eql_specializers: Option<RootSlot>,
    method_combinations: Option<crate::vm::method_combination::MethodCombinations>,
    next_exit: usize,
    repl_compiler_feedback: bool,
    constantly_code: Option<RootSlot>,
    complement_code: Option<RootSlot>,
    optimization_qualities: crate::core_ir::OptimizationQualities,
    proclaimed_inline: std::collections::HashMap<(usize, bool), crate::core_ir::InlineDeclaration>,
    proclaimed_types: std::collections::HashMap<crate::symbol::NameKey, crate::read::Form>,
    proclaimed_ftypes: std::collections::HashMap<(crate::symbol::NameKey, bool), crate::read::Form>,
    inline_defs: std::collections::HashMap<crate::symbol::NameKey, Rc<crate::core_ir::CoreFn>>,
    type_expanders: std::collections::HashMap<usize, RootSlot>,
    setf_expanders: std::collections::HashMap<usize, crate::SetfExpander>,
    setf_docs: std::collections::HashMap<usize, RootSlot>,
    traced: Vec<crate::TracedFunction>,
    profiler: crate::profiling::Profiler,
    finalizers: crate::finalizers::Finalizers,
    compiler_macros: std::collections::HashMap<(usize, bool), RootSlot>,
    symbol_plists: std::collections::HashMap<usize, RootSlot>,
}
impl Runtime {
    fn write_image_metadata(&self, image: &mut ImageWriter<'_>) -> Result<(), EvalError> {
        image.usize(self.builtin_native_count.get());
        image.bool(self.foreign_calls);
        image.u64(native_abi_fingerprint());
        image.usize(self.definition_sources.borrow().len());
        for ((id, _), info) in self.definition_sources.borrow().iter() {
            image.usize(*id);
            info.write_image(image)?;
        }
        self.packages.borrow().write_image(image)?;
        write_optional_root(image, self.nil_name.borrow().as_ref())?;
        write_optional_root(image, self.t_name.borrow().as_ref())?;
        image.usize(self.next_symbol_id.get());
        image.usize(self.next_macro_environment.get());
        write_key_roots(image, &self.declaration_handlers.borrow())?;
        self.vops.write_image(image)?;
        image.word(self.default_print_method.get())?;
        image.bool(self.debugger.get().is_some());
        if let Some(value) = self.debugger.get() {
            value.write_image(image)?;
        }
        write_optional_root(image, self.inspector.borrow().as_ref())?;
        self.structures.borrow().write_image(image)?;
        self.logical_hosts.borrow().write_image(image)?;
        image.usize(self.module_directories.borrow().len());
        for path in self.module_directories.borrow().iter() {
            write_path(image, path)?;
        }
        write_strings(image, &self.command_line_arguments.borrow());
        image.usize(self.environment.borrow().len());
        for (name, value) in self.environment.borrow().iter() {
            image.string(name);
            image.bool(value.is_some());
            if let Some(value) = value {
                image.string(value);
            }
        }
        image.bool(self.working_directory.borrow().is_some());
        if let Some(path) = self.working_directory.borrow().as_ref() {
            write_path(image, path)?;
        }
        image.usize(self.exit_code.get() as usize);
        self.classes.borrow().write_image(image)?;
        image.bool(self.invocation_protocol.get().is_some());
        if let Some(value) = self.invocation_protocol.get() {
            value.write_image(image)?;
        }
        write_optional_root(image, self.eql_specializers.get())?;
        image.bool(self.method_combinations.get().is_some());
        if let Some(value) = self.method_combinations.get() {
            value.write_image(image)?;
        }
        image.usize(self.next_exit.get());
        image.bool(self.repl_compiler_feedback.get());
        write_optional_root(image, self.constantly_code.get())?;
        write_optional_root(image, self.complement_code.get())?;
        let q = self.optimization_qualities.get();
        for value in [q.speed, q.safety, q.debug, q.space, q.compilation_speed] {
            image.usize(value as usize);
        }
        image.usize(self.proclaimed_inline.borrow().len());
        for ((id, setf), value) in self.proclaimed_inline.borrow().iter() {
            image.usize(*id);
            image.bool(*setf);
            image.bool(*value == crate::core_ir::InlineDeclaration::Inline);
        }
        write_key_forms(image, &self.proclaimed_types.borrow())?;
        image.usize(self.proclaimed_ftypes.borrow().len());
        for ((key, setf), form) in self.proclaimed_ftypes.borrow().iter() {
            image.key(key);
            image.bool(*setf);
            image.form(form)?;
        }
        image.usize(self.inline_defs.borrow().len());
        for (key, value) in self.inline_defs.borrow().iter() {
            image.key(key);
            value.write_image(image)?;
        }
        write_id_roots(image, &self.type_expanders.borrow())?;
        image.usize(self.setf_expanders.borrow().len());
        for (id, value) in self.setf_expanders.borrow().iter() {
            image.usize(*id);
            match value {
                crate::SetfExpander::Function(root) => {
                    image.usize(0);
                    image.word(root.get())?;
                }
                crate::SetfExpander::Short(root) => {
                    image.usize(1);
                    image.word(root.get())?;
                }
                crate::SetfExpander::Long {
                    function,
                    store_count,
                    positional_count,
                    has_keys,
                } => {
                    image.usize(2);
                    image.word(function.get())?;
                    image.usize(*store_count);
                    image.usize(*positional_count);
                    image.bool(*has_keys);
                }
            }
        }
        write_id_roots(image, &self.setf_docs.borrow())?;
        image.usize(self.traced.borrow().len());
        for value in self.traced.borrow().iter() {
            image.word(value.symbol.get())?;
            image.bool(value.is_setf);
            image.word(value.original.get())?;
            image.word(value.wrapper.get())?;
            image.bool(value.generic);
            value.options.write_image(image)?;
        }
        self.profiler.write_image(image)?;
        self.finalizers.write_image(image)?;
        image.usize(self.compiler_macros.borrow().len());
        for ((id, setf), value) in self.compiler_macros.borrow().iter() {
            image.usize(*id);
            image.bool(*setf);
            image.word(value.get())?;
        }
        write_id_roots(image, &self.symbol_plists.borrow())?;
        Ok(())
    }
    fn read_image_metadata(&mut self, bytes: &[u8], words: &[RootSlot]) -> Result<(), EvalError> {
        let state = {
            let image = &mut ImageReader::new(self, bytes, words);
            if image.usize()? != self.builtin_native_count.get()
                || image.bool()? != self.foreign_calls
                || image.u64()? != native_abi_fingerprint()
            {
                return Err(EvalError::new(
                    0,
                    "runtime image requires the same builtin/FFI configuration",
                ));
            }
            let state = ImageState {
                definition_sources: {
                    let mut values = std::collections::HashMap::new();
                    for _ in 0..image.usize()? {
                        let id = image.usize()?;
                        let info = crate::DefinitionInfo::read_image(image)?;
                        values.insert((id, info.kind), info);
                    }
                    values
                },
                packages: crate::package::PackageRegistry::read_image(image)?,
                nil_name: read_optional_root(image)?,
                t_name: read_optional_root(image)?,
                next_symbol_id: image.usize()?,
                next_macro_environment: image.usize()?,
                declaration_handlers: read_key_roots(image)?,
                vops: crate::compile::vop::Registry::read_image(self, image)?,
                default_print_method: image.word()?,
                debugger: if image.bool()? {
                    Some(crate::vm::signaling::Debugger::read_image(image)?)
                } else {
                    None
                },
                inspector: read_optional_root(image)?,
                structures: crate::vm::structures::Registry::read_image(image)?,
                logical_hosts: crate::pathname::LogicalHosts::read_image(image)?,
                module_directories: {
                    let mut paths = Vec::new();
                    for _ in 0..image.usize()? {
                        paths.push(read_path(image)?);
                    }
                    paths
                },
                command_line_arguments: read_strings(image)?,
                environment: {
                    let mut values = std::collections::HashMap::new();
                    for _ in 0..image.usize()? {
                        let key = image.string()?;
                        let value = if image.bool()? {
                            Some(image.string()?)
                        } else {
                            None
                        };
                        values.insert(key, value);
                    }
                    values
                },
                working_directory: if image.bool()? {
                    Some(read_path(image)?)
                } else {
                    None
                },
                exit_code: u8::try_from(image.usize()?).map_err(|_| invalid())?,
                classes: crate::classes::Registry::read_image(image)?,
                invocation_protocol: if image.bool()? {
                    Some(crate::vm::function_mop::InvocationProtocol::read_image(
                        image,
                    )?)
                } else {
                    None
                },
                eql_specializers: read_optional_root(image)?,
                method_combinations: if image.bool()? {
                    Some(crate::vm::method_combination::MethodCombinations::read_image(image)?)
                } else {
                    None
                },
                next_exit: image.usize()?,
                repl_compiler_feedback: image.bool()?,
                constantly_code: read_optional_root(image)?,
                complement_code: read_optional_root(image)?,
                optimization_qualities: {
                    let mut q = [0; 5];
                    for v in &mut q {
                        *v = u8::try_from(image.usize()?).map_err(|_| invalid())?;
                        if *v > 3 {
                            return Err(invalid());
                        }
                    }
                    crate::core_ir::OptimizationQualities {
                        speed: q[0],
                        safety: q[1],
                        debug: q[2],
                        space: q[3],
                        compilation_speed: q[4],
                    }
                },
                proclaimed_inline: {
                    let mut values = std::collections::HashMap::new();
                    for _ in 0..image.usize()? {
                        let key = (image.usize()?, image.bool()?);
                        let value = if image.bool()? {
                            crate::core_ir::InlineDeclaration::Inline
                        } else {
                            crate::core_ir::InlineDeclaration::NotInline
                        };
                        values.insert(key, value);
                    }
                    values
                },
                proclaimed_types: read_key_forms(image)?,
                proclaimed_ftypes: {
                    let mut values = std::collections::HashMap::new();
                    for _ in 0..image.usize()? {
                        let key = (image.key()?, image.bool()?);
                        values.insert(key, image.form()?);
                    }
                    values
                },
                inline_defs: {
                    let mut values = std::collections::HashMap::new();
                    for _ in 0..image.usize()? {
                        let key = image.key()?;
                        values.insert(key, Rc::new(crate::core_ir::CoreFn::read_image(image)?));
                    }
                    values
                },
                type_expanders: read_id_roots(image)?,
                setf_expanders: {
                    let mut values = std::collections::HashMap::new();
                    for _ in 0..image.usize()? {
                        let id = image.usize()?;
                        let value = match image.usize()? {
                            0 => crate::SetfExpander::Function(self.root_slot(image.word()?)),
                            1 => crate::SetfExpander::Short(self.root_slot(image.word()?)),
                            2 => crate::SetfExpander::Long {
                                function: self.root_slot(image.word()?),
                                store_count: image.usize()?,
                                positional_count: image.usize()?,
                                has_keys: image.bool()?,
                            },
                            _ => return Err(invalid()),
                        };
                        values.insert(id, value);
                    }
                    values
                },
                setf_docs: read_id_roots(image)?,
                traced: {
                    let mut values = Vec::new();
                    for _ in 0..image.usize()? {
                        values.push(crate::TracedFunction {
                            symbol: self.root_slot(image.word()?),
                            is_setf: image.bool()?,
                            original: self.root_slot(image.word()?),
                            wrapper: self.root_slot(image.word()?),
                            generic: image.bool()?,
                            options: crate::vm::environment::TraceOptions::read_image(image)?,
                        });
                    }
                    values
                },
                profiler: crate::profiling::Profiler::read_image(image)?,
                finalizers: crate::finalizers::Finalizers::read_image(image)?,
                compiler_macros: {
                    let mut values = std::collections::HashMap::new();
                    for _ in 0..image.usize()? {
                        let key = (image.usize()?, image.bool()?);
                        values.insert(key, self.root_slot(image.word()?));
                    }
                    values
                },
                symbol_plists: read_id_roots(image)?,
            };
            if !image.bytes.is_empty() {
                return Err(invalid());
            }
            state
        };
        *self.definition_sources.get_mut() = state.definition_sources;
        *self.packages.get_mut() = state.packages;
        *self.nil_name.get_mut() = state.nil_name;
        *self.t_name.get_mut() = state.t_name;
        self.next_symbol_id.set(state.next_symbol_id);
        self.next_macro_environment
            .set(state.next_macro_environment);
        *self.declaration_handlers.get_mut() = state.declaration_handlers;
        self.vops = state.vops;
        self.default_print_method.set(state.default_print_method);
        self.debugger.take();
        if let Some(value) = state.debugger {
            let _ = self.debugger.set(value);
        }
        *self.inspector.get_mut() = state.inspector;
        *self.structures.get_mut() = state.structures;
        *self.logical_hosts.get_mut() = state.logical_hosts;
        *self.module_directories.get_mut() = state.module_directories;
        *self.command_line_arguments.get_mut() = state.command_line_arguments;
        *self.environment.get_mut() = state.environment;
        *self.working_directory.get_mut() = state.working_directory;
        self.exit_code.set(state.exit_code);
        *self.classes.get_mut() = state.classes;
        self.invocation_protocol.take();
        if let Some(value) = state.invocation_protocol {
            let _ = self.invocation_protocol.set(value);
        }
        self.eql_specializers.take();
        if let Some(value) = state.eql_specializers {
            let _ = self.eql_specializers.set(value);
        }
        self.method_combinations.take();
        if let Some(value) = state.method_combinations {
            let _ = self.method_combinations.set(value);
        }
        self.next_exit.set(state.next_exit);
        self.repl_compiler_feedback
            .set(state.repl_compiler_feedback);
        self.constantly_code.take();
        if let Some(value) = state.constantly_code {
            let _ = self.constantly_code.set(value);
        }
        self.complement_code.take();
        if let Some(value) = state.complement_code {
            let _ = self.complement_code.set(value);
        }
        self.optimization_qualities
            .set(state.optimization_qualities);
        *self.proclaimed_inline.get_mut() = state.proclaimed_inline;
        *self.proclaimed_types.get_mut() = state.proclaimed_types;
        *self.proclaimed_ftypes.get_mut() = state.proclaimed_ftypes;
        *self.inline_defs.get_mut() = state.inline_defs;
        *self.type_expanders.get_mut() = state.type_expanders;
        *self.setf_expanders.get_mut() = state.setf_expanders;
        *self.setf_docs.get_mut() = state.setf_docs;
        *self.traced.get_mut() = state.traced;
        self.profiler = state.profiler;
        self.finalizers = state.finalizers;
        *self.compiler_macros.get_mut() = state.compiler_macros;
        *self.symbol_plists.get_mut() = state.symbol_plists;
        self.heap.namespace_changed();
        self.read_symbol_cache.get_mut().clear_image_cache();
        self.printer_controls.take();
        self.printer_keywords.take();
        self.simd_type_symbols.take();
        self.symbol_expander_code.take();
        Ok(())
    }
}
