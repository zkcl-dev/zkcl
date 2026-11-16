//! Breakpoints retain function objects or explicitly follow a rooted function name.
use crate::code_store::{CodeId, VmCode};
use crate::heap::{RootFrame, RootSlot, Word};
use crate::{EvalError, Root, Runtime, SourceDocument};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::ops::Range;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BreakpointFollow {
    /// Retain the exact compiled function, including after redefinition.
    #[default]
    Object,
    /// Follow the identity of the symbol passed to the installation API.
    Name,
    /// Follow that symbol's source line in subsequent revisions of the same file.
    Source,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BreakpointOptions {
    pub enabled: bool,
    /// A Lisp expression evaluated in the stopped frame. NIL skips the stop.
    pub condition: Option<String>,
    /// Start accepting encounters at this one-based count.
    pub hit_count: Option<u64>,
    /// Accept every Nth encounter, after applying hit_count.
    pub hit_every: Option<u64>,
    /// A Lisp expression whose primary value is printed to *TRACE-OUTPUT*.
    pub log: Option<String>,
    pub log_only: bool,
    pub follow: BreakpointFollow,
}
impl Default for BreakpointOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            condition: None,
            hit_count: None,
            hit_every: None,
            log: None,
            log_only: false,
            follow: BreakpointFollow::Object,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BreakpointStatus {
    Verified,
    Pending(String),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Breakpoint {
    pub id: u64,
    pub function: Option<String>,
    pub bytecode_pc: usize,
    pub source: Option<SourceDocument>,
    pub offset: Option<usize>,
    pub options: BreakpointOptions,
    pub hits: u64,
    pub status: BreakpointStatus,
    pub requested_path: Option<PathBuf>,
    pub requested_line: Option<usize>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BreakpointLocation {
    pub bytecode_pc: usize,
    pub source: Option<SourceDocument>,
    pub offset: Option<usize>,
    pub line: Option<usize>,
    pub column: Option<usize>,
}
struct BreakpointSite {
    info: Breakpoint,
    code: Option<CodeId>,
    vm_pc: usize,
    function: RootSlot,
    name: Option<RootSlot>,
    line: Option<usize>,
    requested_path: Option<PathBuf>,
    source_line: Option<Range<usize>>,
}
#[derive(Default)]
pub(super) struct Registry {
    sites: RefCell<BTreeMap<u64, BreakpointSite>>,
    next: Cell<u64>,
    refreshed: Cell<Option<(usize, u64)>>,
}
impl Registry {
    pub(super) fn observing(&self) -> bool {
        self.sites
            .borrow()
            .values()
            .any(|site| site.info.options.enabled)
    }
    pub(super) fn has_logpoints(&self) -> bool {
        self.sites.borrow().values().any(|site| {
            site.info.options.enabled
                && (site.info.options.log.is_some() || site.info.options.log_only)
        })
    }
    pub(super) fn refresh(&self, runtime: &Runtime) {
        let revision = (
            runtime.heap.function_epoch(),
            runtime.packages.borrow().revision(),
        );
        if self.refreshed.replace(Some(revision)) == Some(revision) {
            return;
        }
        for site in self.sites.borrow_mut().values_mut() {
            if site.name.is_none()
                && let Some(path) = &site.requested_path
            {
                let symbols = {
                    let packages = runtime.packages.borrow();
                    packages
                        .list()
                        .flat_map(|package| {
                            packages
                                .entries_snapshot(package, true, true, false)
                                .unwrap_or_default()
                        })
                        .map(|entry| entry.0)
                        .collect::<std::collections::HashSet<_>>()
                };
                let mut found = None;
                for symbol in symbols {
                    let Some(function) = runtime.heap.symbol_cell(symbol, true) else {
                        continue;
                    };
                    let Some(code) = runtime.heap.function_code(function) else {
                        continue;
                    };
                    let id = runtime
                        .heap
                        .function_info(code)
                        .expect("function code")
                        .code_id;
                    let code = runtime.heap.code_store.get(id);
                    if !code.source.as_ref().is_some_and(|source| {
                        source.path() == Some(path.as_path()) && source.base_offset() == 0
                    }) {
                        continue;
                    }
                    let Ok(pc) = select_line(&code, site.line.expect("file breakpoint line"))
                    else {
                        continue;
                    };
                    if found
                        .as_ref()
                        .is_some_and(|(_, existing, _, _, _, _)| *existing != id)
                    {
                        found = None;
                        break;
                    }
                    found = Some((
                        symbol,
                        id,
                        pc,
                        code.source.clone(),
                        breakpoint_offset(&code, pc),
                        code.name.clone(),
                    ));
                }
                if let Some((symbol, id, pc, source, offset, name)) = found {
                    let function = runtime
                        .heap
                        .symbol_cell(symbol, true)
                        .expect("matched function");
                    let code = runtime.heap.code_store.get(id);
                    site.name = Some(runtime.root_slot(symbol));
                    site.function = runtime.root_slot(function);
                    site.code = Some(id);
                    site.vm_pc = code.vm_pc(pc).expect("canonical executable PC");
                    site.info.bytecode_pc = pc;
                    site.info.source = source;
                    site.info.offset = offset;
                    site.info.function = name;
                    site.source_line = code
                        .source
                        .as_ref()
                        .and_then(SourceDocument::text)
                        .and_then(|text| {
                            line_range(text, site.line.expect("file breakpoint line")).ok()
                        });
                    site.info.status = BreakpointStatus::Verified;
                }
            }
            let Some(name) = &site.name else { continue };
            if site.info.options.follow == BreakpointFollow::Object {
                continue;
            }
            let Some(function) = runtime
                .heap
                .symbol_cell(name.get(), true)
                .filter(|&v| v != Word::UNBOUND)
            else {
                site.info.status = BreakpointStatus::Pending("function name is unbound".into());
                continue;
            };
            if function == site.function.get() {
                site.info.status = BreakpointStatus::Verified;
                continue;
            }
            let migrated = (|| -> Result<_, EvalError> {
                let code = runtime.heap.function_code(function).ok_or_else(|| {
                    EvalError::new(0, "new definition has no executable Lisp code")
                })?;
                let id = runtime
                    .heap
                    .function_info(code)
                    .expect("function code")
                    .code_id;
                let code = runtime.heap.code_store.get(id);
                let pc = if site.info.options.follow == BreakpointFollow::Source {
                    select_follow_source(site, &code)?
                } else {
                    site.info.bytecode_pc
                };
                let vm_pc = executable_pc(&code, pc)?;
                Ok((
                    id,
                    vm_pc,
                    pc,
                    code.source.clone(),
                    breakpoint_offset(&code, pc),
                    code.name.clone(),
                ))
            })();
            match migrated {
                Ok((id, vm_pc, pc, source, offset, name)) => {
                    site.code = Some(id);
                    site.vm_pc = vm_pc;
                    site.function = runtime.root_slot(function);
                    site.info.bytecode_pc = pc;
                    site.info.source = source;
                    site.info.offset = offset;
                    site.info.function = name;
                    site.source_line = site
                        .info
                        .source
                        .as_ref()
                        .and_then(SourceDocument::text)
                        .zip(offset)
                        .and_then(|(text, offset)| range_at_offset(text, offset));
                    site.info.status = BreakpointStatus::Verified;
                }
                Err(error) => site.info.status = BreakpointStatus::Pending(error.message().into()),
            }
        }
    }
    pub(super) fn hit(&self, runtime: &Runtime, code: CodeId, pc: usize) -> Option<u64> {
        self.refresh(runtime);
        let mut first = None;
        for (&id, site) in self.sites.borrow_mut().iter_mut() {
            if !site.info.options.enabled
                || site.info.status != BreakpointStatus::Verified
                || site.code != Some(code)
                || site.vm_pc != pc
            {
                continue;
            }
            site.info.hits = site.info.hits.saturating_add(1);
            if accepts_hit(site) && first.is_none() {
                first = Some(id);
            }
        }
        first
    }
}
fn accepts_hit(site: &BreakpointSite) -> bool {
    let first = site.info.options.hit_count.unwrap_or(1);
    site.info.hits >= first
        && (site.info.hits - first).is_multiple_of(site.info.options.hit_every.unwrap_or(1))
}
fn validate(options: &BreakpointOptions) -> Result<(), EvalError> {
    if options.hit_count == Some(0) || options.hit_every == Some(0) {
        return Err(EvalError::new(0, "breakpoint hit counts must be positive"));
    }
    Ok(())
}
fn executable_pc(code: &VmCode, pc: usize) -> Result<usize, EvalError> {
    let vm_pc = code
        .vm_pc(pc)
        .filter(|&vm_pc| vm_pc < code.vm_ops.len())
        .ok_or_else(|| EvalError::new(0, "breakpoint instruction is outside the function"))?;
    if pc != 0 && code.canonical_pc(vm_pc) != Some(pc) {
        return Err(EvalError::new(
            0,
            "this instruction is fused into a neighboring instruction",
        ));
    }
    Ok(vm_pc)
}
fn line_range(text: &str, line: usize) -> Result<Range<usize>, EvalError> {
    if line == 0 {
        return Err(EvalError::new(0, "source lines start at 1"));
    }
    let start = if line == 1 {
        0
    } else {
        text.match_indices('\n')
            .nth(line - 2)
            .map(|(offset, _)| offset + 1)
            .ok_or_else(|| EvalError::new(0, "source line is outside this document"))?
    };
    let end = text[start..]
        .find('\n')
        .map_or(text.len(), |end| start + end);
    Ok(start..end)
}
fn range_at_offset(text: &str, offset: usize) -> Option<Range<usize>> {
    if !text.is_char_boundary(offset) {
        return None;
    }
    let start = text[..offset].rfind('\n').map_or(0, |index| index + 1);
    let end = text[offset..]
        .find('\n')
        .map_or(text.len(), |index| offset + index);
    Some(start..end)
}
fn select_range(code: &VmCode, range: Range<usize>) -> Result<usize, EvalError> {
    (0..code.vm_ops.len())
        .filter_map(|pc| code.canonical_pc(pc))
        .find(|&pc| breakpoint_offset(code, pc).is_some_and(|offset| range.contains(&offset)))
        .ok_or_else(|| EvalError::new(0, "no executable location on this source line"))
}
fn select_line(code: &VmCode, line: usize) -> Result<usize, EvalError> {
    let text = code
        .source
        .as_ref()
        .and_then(SourceDocument::text)
        .ok_or_else(|| EvalError::new(0, "source text is unavailable for this function"))?;
    select_range(code, line_range(text, line)?)
}
fn select_follow_source(site: &BreakpointSite, code: &VmCode) -> Result<usize, EvalError> {
    let previous = site
        .info
        .source
        .as_ref()
        .ok_or_else(|| EvalError::new(0, "previous source document is unavailable"))?;
    let next = code
        .source
        .as_ref()
        .ok_or_else(|| EvalError::new(0, "new definition has no source document"))?;
    if !(previous.path().is_some() && previous.path() == next.path()
        || previous.uri().is_some() && previous.uri() == next.uri())
    {
        return Err(EvalError::new(
            0,
            "new definition has no matching source document",
        ));
    }
    if previous.base_offset() == 0 && next.base_offset() == 0 {
        return select_line(
            code,
            site.line.ok_or_else(|| {
                EvalError::new(0, "source-follow breakpoint requires a source line")
            })?,
        );
    }
    let previous_text = previous
        .text()
        .ok_or_else(|| EvalError::new(0, "previous source text is unavailable"))?;
    let next_text = next
        .text()
        .ok_or_else(|| EvalError::new(0, "new source text is unavailable"))?;
    let original = site
        .source_line
        .clone()
        .ok_or_else(|| EvalError::new(0, "previous source line range is unavailable"))?;
    let absolute = previous
        .absolute_range(original.clone())
        .ok_or_else(|| EvalError::new(0, "previous source line range overflowed"))?;
    let extent = next
        .absolute_range(0..next_text.len())
        .ok_or_else(|| EvalError::new(0, "new source range overflowed"))?;
    let start = absolute.start.max(extent.start);
    let end = absolute.end.min(extent.end);
    if start < end {
        let mapped = start - next.base_offset()..end - next.base_offset();
        let old = start - previous.base_offset()..end - previous.base_offset();
        let same_revision = previous.version().is_some() && previous.version() == next.version();
        let same_text = previous_text
            .get(old)
            .zip(next_text.get(mapped.clone()))
            .is_some_and(|(old, new)| old == new);
        if (same_revision || same_text)
            && let Ok(pc) = select_range(code, mapped)
        {
            return Ok(pc);
        }
    }
    // A unique unchanged complete line supplies a concrete textual delta when
    // another edit moved the fragment. Ambiguous or changed lines stay pending.
    let original_text = previous_text
        .get(original)
        .ok_or_else(|| EvalError::new(0, "previous source line range is unavailable"))?;
    let matches = next_text
        .match_indices(original_text)
        .filter(|(offset, _)| {
            (*offset == 0 || next_text.as_bytes()[offset - 1] == b'\n')
                && next_text
                    .as_bytes()
                    .get(offset + original_text.len())
                    .is_none_or(|byte| *byte == b'\n')
        })
        .map(|(offset, _)| offset..offset + original_text.len())
        .collect::<Vec<_>>();
    if let [range] = matches.as_slice() {
        return select_range(code, range.clone());
    }
    Err(EvalError::new(
        0,
        "source fragment cannot map the requested line across these revisions",
    ))
}

impl Runtime {
    pub fn add_breakpoint(
        &self,
        function: &Root<'_>,
        pc: Option<usize>,
    ) -> Result<Breakpoint, EvalError> {
        self.add_breakpoint_with_options(function, pc, BreakpointOptions::default())
    }
    pub fn add_source_breakpoint(
        &self,
        function: &Root<'_>,
        line: usize,
    ) -> Result<Breakpoint, EvalError> {
        self.add_source_breakpoint_with_options(function, line, BreakpointOptions::default())
    }
    pub fn add_breakpoint_with_options(
        &self,
        function: &Root<'_>,
        pc: Option<usize>,
        options: BreakpointOptions,
    ) -> Result<Breakpoint, EvalError> {
        self.install_breakpoint(function, None, options, |_| Ok(pc.unwrap_or(0)))
    }
    pub fn add_source_breakpoint_with_options(
        &self,
        function: &Root<'_>,
        line: usize,
        options: BreakpointOptions,
    ) -> Result<Breakpoint, EvalError> {
        self.install_breakpoint(function, Some(line), options, |code| {
            select_line(code, line)
        })
    }
    fn install_breakpoint(
        &self,
        input: &Root<'_>,
        line: Option<usize>,
        options: BreakpointOptions,
        select: impl FnOnce(&VmCode) -> Result<usize, EvalError>,
    ) -> Result<Breakpoint, EvalError> {
        validate(&options)?;
        let name = self
            .heap
            .is_symbol(input.slot.get())
            .then(|| self.root_slot(input.slot.get()));
        if options.follow != BreakpointFollow::Object && name.is_none() {
            return Err(EvalError::new(
                0,
                "following a definition requires a symbol, not a function object",
            ));
        }
        if options.follow == BreakpointFollow::Source && line.is_none() {
            return Err(EvalError::new(
                0,
                "source-follow mode requires a source-line breakpoint",
            ));
        }
        let function = self.resolve_function(input)?;
        let code = self
            .heap
            .function_code(function.slot.get())
            .ok_or_else(|| EvalError::new(0, "breakpoints require a compiled Lisp function"))?;
        let code_id = self
            .heap
            .function_info(code)
            .expect("function code")
            .code_id;
        let code = self.heap.code_store.get(code_id);
        let bytecode_pc = select(&code)?;
        let vm_pc = executable_pc(&code, bytecode_pc)?;
        let registry = &self
            .debugger
            .get()
            .expect("initialized runtime")
            .controls
            .breakpoints;
        let id = registry
            .next
            .get()
            .checked_add(1)
            .ok_or_else(|| EvalError::new(0, "breakpoint identities exhausted"))?;
        registry.next.set(id);
        registry.refreshed.set(None);
        let info = Breakpoint {
            id,
            function: code.name.clone(),
            bytecode_pc,
            source: code.source.clone(),
            offset: breakpoint_offset(&code, bytecode_pc),
            options,
            hits: 0,
            status: BreakpointStatus::Verified,
            requested_path: code
                .source
                .as_ref()
                .and_then(SourceDocument::path)
                .map(Path::to_owned),
            requested_line: line,
        };
        registry.sites.borrow_mut().insert(
            id,
            BreakpointSite {
                info: info.clone(),
                code: Some(code_id),
                vm_pc,
                function: function.slot,
                name,
                line,
                requested_path: None,
                source_line: line.and_then(|line| {
                    code.source
                        .as_ref()
                        .and_then(SourceDocument::text)
                        .and_then(|text| line_range(text, line).ok())
                }),
            },
        );
        self.heap.code_store.invalidate_native_calls();
        Ok(info)
    }
    /// Install a source breakpoint before its file is loaded. Resolution uses actual
    /// compiled source locations and roots the matching symbol identity once found.
    pub fn add_file_breakpoint(
        &self,
        path: impl AsRef<Path>,
        line: usize,
        mut options: BreakpointOptions,
    ) -> Result<Breakpoint, EvalError> {
        validate(&options)?;
        if line == 0 {
            return Err(EvalError::new(0, "source lines start at 1"));
        }
        self.initialize_packages()?;
        options.follow = BreakpointFollow::Source;
        let path = path.as_ref();
        let path = path.canonicalize().unwrap_or_else(|_| {
            if path.is_absolute() {
                path.to_owned()
            } else {
                std::env::current_dir().unwrap_or_default().join(path)
            }
        });
        let registry = &self
            .debugger
            .get()
            .expect("initialized runtime")
            .controls
            .breakpoints;
        let id = registry
            .next
            .get()
            .checked_add(1)
            .ok_or_else(|| EvalError::new(0, "breakpoint identities exhausted"))?;
        registry.next.set(id);
        registry.refreshed.set(None);
        let info = Breakpoint {
            id,
            function: None,
            bytecode_pc: 0,
            source: None,
            offset: None,
            options,
            hits: 0,
            status: BreakpointStatus::Pending(
                "source location has not been loaded or is not executable".into(),
            ),
            requested_path: Some(path.clone()),
            requested_line: Some(line),
        };
        registry.sites.borrow_mut().insert(
            id,
            BreakpointSite {
                info,
                code: None,
                vm_pc: 0,
                function: self.root_slot(Word::NIL),
                name: None,
                line: Some(line),
                requested_path: Some(path),
                source_line: None,
            },
        );
        registry.refresh(self);
        self.heap.code_store.invalidate_native_calls();
        Ok(registry
            .sites
            .borrow()
            .get(&id)
            .expect("installed breakpoint")
            .info
            .clone())
    }
    pub fn update_breakpoint(
        &self,
        id: u64,
        options: BreakpointOptions,
    ) -> Result<Breakpoint, EvalError> {
        validate(&options)?;
        let registry = &self
            .debugger
            .get()
            .ok_or_else(|| EvalError::new(0, "unknown breakpoint"))?
            .controls
            .breakpoints;
        let mut sites = registry.sites.borrow_mut();
        let site = sites
            .get_mut(&id)
            .ok_or_else(|| EvalError::new(0, "unknown breakpoint"))?;
        if options.follow != BreakpointFollow::Object
            && site.name.is_none()
            && site.requested_path.is_none()
        {
            return Err(EvalError::new(
                0,
                "this breakpoint was installed without a symbol identity",
            ));
        }
        if options.follow == BreakpointFollow::Source && site.line.is_none() {
            return Err(EvalError::new(
                0,
                "source-follow mode requires a source-line breakpoint",
            ));
        }
        if options.follow == BreakpointFollow::Object {
            if site.code.is_none() {
                return Err(EvalError::new(
                    0,
                    "an unresolved source breakpoint has no function object to retain",
                ));
            }
            site.info.status = BreakpointStatus::Verified;
        }
        site.info.options = options;
        registry.refreshed.set(None);
        self.heap.code_store.invalidate_native_calls();
        Ok(site.info.clone())
    }
    pub fn enable_breakpoint(&self, id: u64, enabled: bool) -> Result<Breakpoint, EvalError> {
        let mut options = self
            .breakpoints()
            .into_iter()
            .find(|bp| bp.id == id)
            .ok_or_else(|| EvalError::new(0, "unknown breakpoint"))?
            .options;
        options.enabled = enabled;
        self.update_breakpoint(id, options)
    }
    pub fn remove_breakpoint(&self, id: u64) -> bool {
        self.debugger.get().is_some_and(|d| {
            d.controls
                .breakpoints
                .sites
                .borrow_mut()
                .remove(&id)
                .is_some()
        })
    }
    pub fn breakpoints(&self) -> Vec<Breakpoint> {
        self.debugger.get().map_or_else(Vec::new, |d| {
            d.controls.breakpoints.refresh(self);
            d.controls
                .breakpoints
                .sites
                .borrow()
                .values()
                .map(|site| site.info.clone())
                .collect()
        })
    }
    pub fn executable_locations(
        &self,
        function: &Root<'_>,
    ) -> Result<Vec<BreakpointLocation>, EvalError> {
        let function = self.resolve_function(function)?;
        let code = self
            .heap
            .function_code(function.slot.get())
            .ok_or_else(|| {
                EvalError::new(0, "executable locations require a compiled Lisp function")
            })?;
        let code = self.heap.code_store.get(
            self.heap
                .function_info(code)
                .expect("function code")
                .code_id,
        );
        Ok((0..code.vm_ops.len())
            .filter_map(|pc| code.canonical_pc(pc))
            .map(|pc| {
                let offset = breakpoint_offset(&code, pc);
                let position = code
                    .source
                    .as_ref()
                    .and_then(SourceDocument::text)
                    .zip(offset)
                    .and_then(|(text, offset)| super::source_line(text, offset))
                    .map(|(line, column, _)| (line, column));
                BreakpointLocation {
                    bytecode_pc: pc,
                    source: code.source.clone(),
                    offset,
                    line: position.map(|p| p.0),
                    column: position.map(|p| p.1),
                }
            })
            .collect())
    }
}
fn breakpoint_offset(code: &VmCode, pc: usize) -> Option<usize> {
    code.canonical[pc]
        .source_offset()
        .or_else(|| if pc == 0 { code.debug.entry } else { None })
}

/// The body runs inside the normal debugger pause scope, preserving dynamic bindings,
/// real lexical reads/writes and ordinary Lisp side effects. Evaluation errors always
/// stop and report the error, including for log-only breakpoints.
pub(in crate::vm) fn breakpoint_entry(
    runtime: &Runtime,
    id: u64,
    condition: Word,
    hook: Word,
    frontend: Word,
) -> Result<Option<(RootSlot, RootFrame)>, EvalError> {
    let options = runtime
        .debugger
        .get()
        .map(|d| {
            let sites = d.controls.breakpoints.sites.borrow();
            let Some(selected) = sites.get(&id) else {
                return Vec::new();
            };
            sites
                .values()
                .filter(|site| {
                    site.code == selected.code
                        && site.vm_pc == selected.vm_pc
                        && site.info.options.enabled
                        && site.info.status == BreakpointStatus::Verified
                        && accepts_hit(site)
                })
                .map(|site| (site.info.id, site.info.options.clone()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if options.is_empty() {
        return Ok(None);
    }
    if options.len() == 1
        && options[0].1.condition.is_none()
        && options[0].1.log.is_none()
        && !options[0].1.log_only
    {
        return Ok(None);
    }
    let function = super::super::instances::compile_builtin_lambda(
        runtime,
        r#"(lambda (condition hook debugger entries)
      (labels ((in-frame (text)
                 (let* ((*package* (or (zk::%debug-eval-package 0) *package*))
                        (form (read-from-string text))
                        (bindings (mapcar (lambda (entry) (list (car entry) (list 'zk::%debug-local 0 (list 'quote (car entry))))) (zk::%debug-locals 0))))
                   (eval (list 'symbol-macrolet bindings form)))))
        (let ((stop nil))
          (dolist (entry entries)
            (destructuring-bind (id test log log-only) entry
              (handler-case
                  (when (or (null test) (in-frame test))
                    (when log (format *trace-output* "~&~A~%" (in-frame log)))
                    (unless log-only (setf stop t)))
                (error (error) (format *error-output* "~&Breakpoint ~D evaluation failed: ~A~%" id error) (setf stop t)))))
          (when stop
            (when hook (let ((*debugger-hook* nil)) (funcall hook condition hook)))
            (when debugger (funcall debugger condition)))
          stop)))"#,
        &[],
    )?;
    let function = runtime.root_slot(function);
    let args = runtime.root_frame(4);
    args.set(0, condition);
    args.set(1, hook);
    args.set(2, frontend);
    let mut entries = runtime.nil();
    for (id, options) in options.into_iter().rev() {
        let mut row = runtime.nil();
        row = runtime
            .cons(
                &if options.log_only {
                    runtime.t()
                } else {
                    runtime.nil()
                },
                &row,
            )
            .map_err(|e| EvalError::out_of_memory(0, e))?;
        for text in [options.log, options.condition] {
            let text = if let Some(text) = text {
                runtime
                    .make_string(&text)
                    .map_err(|e| EvalError::out_of_memory(0, e))?
            } else {
                runtime.nil()
            };
            row = runtime
                .cons(&text, &row)
                .map_err(|e| EvalError::out_of_memory(0, e))?;
        }
        let id = runtime
            .integer_from(id)
            .map_err(|e| EvalError::out_of_memory(0, e))?;
        row = runtime
            .cons(&id, &row)
            .map_err(|e| EvalError::out_of_memory(0, e))?;
        entries = runtime
            .cons(&row, &entries)
            .map_err(|e| EvalError::out_of_memory(0, e))?;
    }
    args.set(3, entries.slot.get());
    Ok(Some((function, args)))
}

impl Registry {
    pub(super) fn write_image(
        &self,
        image: &mut crate::image::ImageWriter<'_>,
    ) -> Result<(), EvalError> {
        self.refresh(image.runtime);
        image.u64(self.next.get());
        let sites = self.sites.borrow();
        image.usize(sites.len());
        for (&id, site) in sites.iter() {
            image.u64(id);
            image.usize(site.info.bytecode_pc);
            image.u64(site.info.hits);
            image.word(site.function.get())?;
            image.bool(site.name.is_some());
            if let Some(name) = &site.name {
                image.word(name.get())?;
            }
            write_options(image, &site.info.options);
            match &site.info.status {
                BreakpointStatus::Verified => image.usize(0),
                BreakpointStatus::Pending(message) => {
                    image.usize(1);
                    image.string(message);
                }
            }
            image.bool(site.line.is_some());
            if let Some(line) = site.line {
                image.usize(line);
            }
            write_optional_path(image, site.info.requested_path.as_deref())?;
            write_optional_path(image, site.requested_path.as_deref())?;
            image.bool(site.source_line.is_some());
            if let Some(range) = &site.source_line {
                image.usize(range.start);
                image.usize(range.end);
            }
        }
        Ok(())
    }
    pub(super) fn read_image(image: &mut crate::image::ImageReader<'_>) -> Result<Self, EvalError> {
        let next = image.u64()?;
        let mut sites = BTreeMap::new();
        for _ in 0..image.usize()? {
            let id = image.u64()?;
            let bytecode_pc = image.usize()?;
            let hits = image.u64()?;
            if id == 0 || id > next || sites.contains_key(&id) {
                return Err(crate::image::invalid());
            }
            let function = image.runtime.root_slot(image.word()?);
            let name = if image.bool()? {
                let word = image.word()?;
                if !image.runtime.heap.is_symbol(word) {
                    return Err(crate::image::invalid());
                }
                Some(image.runtime.root_slot(word))
            } else {
                None
            };
            let options = read_options(image)?;
            let status = match image.usize()? {
                0 => BreakpointStatus::Verified,
                1 => BreakpointStatus::Pending(image.string()?),
                _ => return Err(crate::image::invalid()),
            };
            let line = if image.bool()? {
                let line = image.usize()?;
                if line == 0 {
                    return Err(crate::image::invalid());
                }
                Some(line)
            } else {
                None
            };
            let info_path = read_optional_path(image)?;
            let requested_path = read_optional_path(image)?;
            let source_line = if image.bool()? {
                let start = image.usize()?;
                let end = image.usize()?;
                if start > end {
                    return Err(crate::image::invalid());
                }
                Some(start..end)
            } else {
                None
            };
            let (code, vm_pc, source, offset, function_name) = if function.get() == Word::NIL {
                if status == BreakpointStatus::Verified
                    || name.is_some()
                    || requested_path.is_none()
                {
                    return Err(crate::image::invalid());
                }
                (None, 0, None, None, None)
            } else {
                let function_code = image
                    .runtime
                    .heap
                    .function_code(function.get())
                    .ok_or_else(crate::image::invalid)?;
                let id = image
                    .runtime
                    .heap
                    .function_info(function_code)
                    .ok_or_else(crate::image::invalid)?
                    .code_id;
                let code = image.runtime.heap.code_store.get(id);
                let vm_pc = executable_pc(&code, bytecode_pc)?;
                if let Some(range) = &source_line {
                    code.source
                        .as_ref()
                        .ok_or_else(crate::image::invalid)?
                        .span(range.clone())
                        .map_err(|_| crate::image::invalid())?;
                }
                (
                    Some(id),
                    vm_pc,
                    code.source.clone(),
                    breakpoint_offset(&code, bytecode_pc),
                    code.name.clone(),
                )
            };
            if options.follow != BreakpointFollow::Object
                && name.is_none()
                && requested_path.is_none()
            {
                return Err(crate::image::invalid());
            }
            if options.follow == BreakpointFollow::Source && line.is_none() {
                return Err(crate::image::invalid());
            }
            let info = Breakpoint {
                id,
                function: function_name,
                bytecode_pc,
                source,
                offset,
                options,
                hits,
                status,
                requested_path: info_path,
                requested_line: line,
            };
            sites.insert(
                id,
                BreakpointSite {
                    info,
                    code,
                    vm_pc,
                    function,
                    name,
                    line,
                    requested_path,
                    source_line,
                },
            );
        }
        Ok(Self {
            sites: RefCell::new(sites),
            next: Cell::new(next),
            refreshed: Cell::new(None),
        })
    }
}
fn write_optional_path(
    image: &mut crate::image::ImageWriter<'_>,
    path: Option<&Path>,
) -> Result<(), EvalError> {
    image.bool(path.is_some());
    if let Some(path) = path {
        crate::image::write_path(image, path)?;
    }
    Ok(())
}
fn read_optional_path(
    image: &mut crate::image::ImageReader<'_>,
) -> Result<Option<PathBuf>, EvalError> {
    Ok(if image.bool()? {
        Some(crate::image::read_path(image)?)
    } else {
        None
    })
}
fn write_options(image: &mut crate::image::ImageWriter<'_>, options: &BreakpointOptions) {
    image.bool(options.enabled);
    image.bool(options.log_only);
    image.usize(match options.follow {
        BreakpointFollow::Object => 0,
        BreakpointFollow::Name => 1,
        BreakpointFollow::Source => 2,
    });
    for text in [&options.condition, &options.log] {
        image.bool(text.is_some());
        if let Some(text) = text {
            image.string(text);
        }
    }
    for count in [options.hit_count, options.hit_every] {
        image.bool(count.is_some());
        if let Some(count) = count {
            image.u64(count);
        }
    }
}
fn read_options(image: &mut crate::image::ImageReader<'_>) -> Result<BreakpointOptions, EvalError> {
    let enabled = image.bool()?;
    let log_only = image.bool()?;
    let follow = match image.usize()? {
        0 => BreakpointFollow::Object,
        1 => BreakpointFollow::Name,
        2 => BreakpointFollow::Source,
        _ => return Err(crate::image::invalid()),
    };
    let condition = if image.bool()? {
        Some(image.string()?)
    } else {
        None
    };
    let log = if image.bool()? {
        Some(image.string()?)
    } else {
        None
    };
    let hit_count = if image.bool()? {
        Some(image.u64()?)
    } else {
        None
    };
    let hit_every = if image.bool()? {
        Some(image.u64()?)
    } else {
        None
    };
    let options = BreakpointOptions {
        enabled,
        condition,
        hit_count,
        hit_every,
        log,
        log_only,
        follow,
    };
    validate(&options)?;
    Ok(options)
}
