use std::ops::Range;

use super::{
    Continuation, ContinuationStep, Invocation, Resume, Values, keyword_arguments, proper_list,
};
use crate::condition::{Class, Fault};
use crate::heap::{RootFrame, RootSlot, RootSlots, Word};
use crate::package::{Package, PackageError, SymbolStatus};
use crate::symbol::{Name, designator_text};
use crate::{EvalError, Root, Runtime};

pub(crate) fn iterator_entries<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let [packages_arg, mode_arg] = args else {
        return Err(Fault::plain(
            Class::ProgramError,
            EvalError::new(at, "expected package iterator arguments"),
        ));
    };
    let mode = mode_arg.as_fixnum().ok_or_else(|| {
        Fault::plain(
            Class::ProgramError,
            EvalError::new(at, "expected iterator mode"),
        )
    })?;
    let env = Call { runtime, at };
    if mode & 8 != 0 {
        let words = runtime.packages.borrow().all_present_symbols();
        let list = env.word_list(words)?;
        return Ok(vec![runtime.root(list)]);
    }
    let internal = mode & 1 != 0;
    let external = mode & 2 != 0;
    let inherited = mode & 4 != 0;
    if mode & 16 != 0 {
        let package = env.live_package(packages_arg.slot.get())?;
        let entries = runtime
            .packages
            .borrow()
            .entries_snapshot(package, internal, external, inherited)
            .map_err(|error| env.registry_error(error))?;
        let words = entries.into_iter().map(|(sym, _, _)| sym).collect();
        let list = env.word_list(words)?;
        return Ok(vec![runtime.root(list)]);
    }
    let packages = env
        .list_or_one(packages_arg.slot.get())?
        .into_iter()
        .map(|word| env.live_package(word))
        .collect::<Result<Vec<_>, _>>()?;
    let internal_kw = runtime.root(runtime.intern(&Name::keyword("INTERNAL"), at)?);
    let external_kw = runtime.root(runtime.intern(&Name::keyword("EXTERNAL"), at)?);
    let inherited_kw = runtime.root(runtime.intern(&Name::keyword("INHERITED"), at)?);
    let mut flat = Vec::new();
    let registry = runtime.packages.borrow();
    for package in packages {
        let entries = registry
            .entries_snapshot(package, internal, external, inherited)
            .map_err(|error| env.registry_error(error))?;
        for (sym, status, pkg) in entries {
            let status_kw = match status {
                SymbolStatus::Internal => internal_kw.slot.get(),
                SymbolStatus::External => external_kw.slot.get(),
                SymbolStatus::Inherited => inherited_kw.slot.get(),
            };
            flat.push(sym);
            flat.push(status_kw);
            flat.push(pkg.word());
        }
    }
    drop(registry);
    let list = env.word_list(flat)?;
    Ok(vec![runtime.root(list)])
}

pub(super) fn resolve_package(
    runtime: &Runtime,
    value: Option<Word>,
    at: usize,
) -> Result<Package, Fault> {
    Call { runtime, at }.package_or_current(value)
}

#[derive(Clone, Copy)]
pub(crate) enum Op {
    Make,
    Find,
    Predicate,
    Name,
    Nicknames,
    GlobalPackage,
    LocalNicknames,
    LocallyNicknamedBy,
    ReplaceLocalNicknames,
    RemoveLocalNickname,
    UseList,
    UsedByList,
    ShadowingSymbols,
    ListAll,
    Intern,
    FindSymbol,
    FindAllSymbols,
    Use,
    Unuse,
    Export,
    Unexport,
    Import,
    Shadow,
    ShadowingImport,
    Unintern,
    Rename,
    Delete,
}

pub(super) const PRIMITIVES: &[(&str, Op)] = &[
    ("MAKE-PACKAGE", Op::Make),
    ("FIND-PACKAGE", Op::Find),
    ("PACKAGEP", Op::Predicate),
    ("PACKAGE-NAME", Op::Name),
    ("PACKAGE-NICKNAMES", Op::Nicknames),
    ("ZK::%GLOBAL-PACKAGE", Op::GlobalPackage),
    ("ZK:PACKAGE-LOCAL-NICKNAMES", Op::LocalNicknames),
    (
        "ZK:PACKAGE-LOCALLY-NICKNAMED-BY-LIST",
        Op::LocallyNicknamedBy,
    ),
    ("ZK::%REPLACE-LOCAL-NICKNAMES", Op::ReplaceLocalNicknames),
    ("ZK:REMOVE-PACKAGE-LOCAL-NICKNAME", Op::RemoveLocalNickname),
    ("PACKAGE-USE-LIST", Op::UseList),
    ("PACKAGE-USED-BY-LIST", Op::UsedByList),
    ("PACKAGE-SHADOWING-SYMBOLS", Op::ShadowingSymbols),
    ("LIST-ALL-PACKAGES", Op::ListAll),
    ("INTERN", Op::Intern),
    ("FIND-SYMBOL", Op::FindSymbol),
    ("FIND-ALL-SYMBOLS", Op::FindAllSymbols),
    ("USE-PACKAGE", Op::Use),
    ("UNUSE-PACKAGE", Op::Unuse),
    ("EXPORT", Op::Export),
    ("UNEXPORT", Op::Unexport),
    ("IMPORT", Op::Import),
    ("SHADOW", Op::Shadow),
    ("SHADOWING-IMPORT", Op::ShadowingImport),
    ("UNINTERN", Op::Unintern),
    ("RENAME-PACKAGE", Op::Rename),
    ("DELETE-PACKAGE", Op::Delete),
];

pub(crate) fn initialize_local_nicknames(runtime: &Runtime) -> Result<(), EvalError> {
    for &(qualified, _) in PRIMITIVES {
        let Some(name) = qualified.strip_prefix("ZK:") else {
            continue;
        };
        let spelling = name.trim_prefix(':');
        let symbol = runtime.intern_package(Package::ZK, spelling, 0)?.0;
        let function = super::Primitive::named(qualified).expect("registered package primitive");
        runtime.heap.set_symbol_cell(symbol, true, function);
        if !name.starts_with(':') {
            runtime
                .packages
                .borrow_mut()
                .export(Package::ZK, spelling, symbol)
                .map_err(|error| EvalError::from_error(0, error))?;
        }
    }
    runtime.load_builtin_source(include_str!(
        "../../lisp/runtime/packages/package-local-nicknames.lisp"
    ))
}

impl Op {
    pub(super) fn accepts(self, count: usize) -> bool {
        match self {
            Self::ListAll => count == 0,
            Self::ReplaceLocalNicknames => count == 2,
            Self::GlobalPackage | Self::LocalNicknames | Self::LocallyNicknamedBy => count == 1,
            Self::Make => count >= 1,
            Self::Rename => (2..=3).contains(&count),
            Self::Find
            | Self::Predicate
            | Self::Name
            | Self::Nicknames
            | Self::UseList
            | Self::UsedByList
            | Self::ShadowingSymbols
            | Self::FindAllSymbols
            | Self::Delete => count == 1,
            _ => (1..=2).contains(&count),
        }
    }
}

/// The continuation of a correctable package error.
enum Recovery {
    UseExisting(Package),
    IgnoreMissingPackage,
    /// Continuing removes the package from its users and deletes it.
    Delete(Package),
    Symbols(PendingSymbols),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SymbolOperation {
    Import,
    Export,
}

struct SymbolConflict {
    operation: SymbolOperation,
    error: PackageError,
}

impl SymbolOperation {
    /// A failed EXPORT can require an IMPORT that has its own name conflict.
    /// CONTINUE acknowledges only the operation that signaled.
    fn apply(
        self,
        call: &Call<'_>,
        package: Package,
        symbol: Word,
        continuing: Option<Self>,
    ) -> Result<Option<SymbolConflict>, Fault> {
        let name = call.symbol_name(symbol)?;
        let mut registry = call.runtime.packages.borrow_mut();
        let (operation, result) = match self {
            Self::Import => (Self::Import, registry.import(package, name, symbol)),
            Self::Export if continuing.is_some() => (
                Self::Import,
                registry.import_and_export(package, &name, symbol),
            ),
            Self::Export => (Self::Export, registry.export(package, &name, symbol)),
        };
        match result {
            Ok(added) => {
                if added && call.runtime.heap.symbol_home(symbol).is_none() {
                    call.runtime.heap.set_symbol_home(symbol, Some(package));
                }
                Ok(None)
            }
            Err(error) if !error.correctable => Err(call.registry_error(error)),
            // An unresolved IMPORT conflict can be continued by skipping this symbol.
            Err(_) if continuing == Some(Self::Import) => Ok(None),
            Err(error) => Ok(Some(SymbolConflict { operation, error })),
        }
    }
}

/// Only a suspended batch needs roots of its own. The first root is the
/// conflicting symbol; preceding symbols must not be processed again.
struct PendingSymbols {
    operation: SymbolOperation,
    package: Package,
    symbols: RootFrame,
    next: usize,
    conflict: SymbolOperation,
}

impl PendingSymbols {
    fn new(
        runtime: &Runtime,
        operation: SymbolOperation,
        package: Package,
        remaining: &[Word],
        conflict: SymbolOperation,
    ) -> Self {
        let symbols = runtime.root_frame(remaining.len());
        for (index, &symbol) in remaining.iter().enumerate() {
            symbols.set(index, symbol);
        }
        Self {
            operation,
            package,
            symbols,
            next: 0,
            conflict,
        }
    }

    fn resume(&mut self, call: &Call<'_>) -> Result<Option<Fault>, Fault> {
        let mut continuing = Some(self.conflict);
        while self.next < self.symbols.len() {
            let symbol = self.symbols.get(self.next);
            if let Some(conflict) =
                self.operation
                    .apply(call, self.package, symbol, continuing.take())?
            {
                self.conflict = conflict.operation;
                return Ok(Some(call.registry_error(conflict.error)));
            }
            self.next += 1;
        }
        Ok(None)
    }
}

struct CorrectablePackageError {
    fault: Fault,
    recovery: Recovery,
}

enum Failure {
    Fault(Fault),
    Correctable(Box<CorrectablePackageError>),
}

impl From<Fault> for Failure {
    fn from(fault: Fault) -> Self {
        Self::Fault(fault)
    }
}

impl From<EvalError> for Failure {
    fn from(error: EvalError) -> Self {
        Self::Fault(error.into())
    }
}

impl Failure {
    fn correctable(fault: Fault, recovery: Recovery) -> Self {
        Self::Correctable(Box::new(CorrectablePackageError { fault, recovery }))
    }

    fn into_fault(self) -> Fault {
        match self {
            Self::Fault(fault) => fault,
            Self::Correctable(error) => error.fault,
        }
    }
}

pub(super) fn run(
    runtime: &Runtime,
    frame: &RootSlots,
    op: Op,
    args: Range<usize>,
    values: &mut Values,
    at: usize,
) -> Result<(), Fault> {
    dispatch(runtime, frame, op, args, values, at).map_err(Failure::into_fault)
}

/// Run a package primitive from a call site that can signal a condition with
/// restarts. Correctable errors become a continuation that signals the
/// package-error with a CONTINUE restart; invoking the restart resumes this
/// continuation, which completes the operation and returns its value.
pub(super) fn invoke<'rt>(
    runtime: &'rt Runtime,
    frame: &RootSlots,
    op: Op,
    args: Range<usize>,
    values: &mut Values,
    resume: Resume,
    at: usize,
) -> Result<Invocation<'rt>, Fault> {
    match dispatch(runtime, frame, op, args, values, at) {
        Ok(()) => Ok(Invocation::Done(resume)),
        Err(Failure::Fault(fault)) => Err(fault),
        Err(Failure::Correctable(error)) => {
            let CorrectablePackageError { fault, recovery } = *error;
            let condition = match fault.make_condition(runtime) {
                Some(condition) => runtime.root_slot(condition),
                None => return Err(fault),
            };
            let host = fault.into_eval_error();
            Ok(Invocation::Continuation(Box::new(PackageContinuation {
                condition,
                host: Some(host),
                recovery,
                at,
                resume,
            })))
        }
    }
}

/// Signals the package-error, then completes the interrupted operation when
/// its CONTINUE restart is invoked. The restart itself is established by the
/// VM around the signal, using the same resumable-restart scope as
/// MUFFLE-WARNING.
struct PackageContinuation {
    condition: RootSlot,
    host: Option<EvalError>,
    recovery: Recovery,
    at: usize,
    resume: Resume,
}

impl Continuation for PackageContinuation {
    fn step(
        &mut self,
        runtime: &Runtime,
        _values: &Values,
        _returned: bool,
    ) -> Result<ContinuationStep, Fault> {
        if let Some(host) = self.host.take() {
            return Ok(ContinuationStep::Signal {
                condition: self.condition.get(),
                at: self.at,
                unhandled: Some(host),
            });
        }
        apply_recovery(runtime, &mut self.recovery, self.at)
    }

    fn resume_to(&self) -> Resume {
        self.resume
    }
}

fn apply_recovery(
    runtime: &Runtime,
    recovery: &mut Recovery,
    at: usize,
) -> Result<ContinuationStep, Fault> {
    let call = Call { runtime, at };
    match recovery {
        Recovery::UseExisting(package) => Ok(ContinuationStep::Done(package.word())),
        Recovery::IgnoreMissingPackage => Ok(ContinuationStep::Done(Word::NIL)),
        Recovery::Delete(package) => {
            let removed = runtime
                .packages
                .borrow_mut()
                .force_delete(*package)
                .map_err(|error| call.registry_error(error))?;
            if let Some(members) = removed {
                for symbol in members {
                    call.clear_home(*package, symbol.get());
                }
            }
            Ok(ContinuationStep::Done(Word::T))
        }
        Recovery::Symbols(symbols) => {
            let Some(fault) = symbols.resume(&call)? else {
                return Ok(ContinuationStep::Done(Word::T));
            };
            let Some(condition) = fault.make_condition(runtime) else {
                return Err(fault);
            };
            Ok(ContinuationStep::Signal {
                condition,
                at,
                unhandled: Some(fault.into_eval_error()),
            })
        }
    }
}

fn dispatch(
    runtime: &Runtime,
    frame: &RootSlots,
    op: Op,
    args: Range<usize>,
    values: &mut Values,
    at: usize,
) -> Result<(), Failure> {
    let call = Call { runtime, at };
    let arg = |index| frame.get(args.start + index);
    let second = || (args.len() == 2).then(|| arg(1));
    let value = match op {
        Op::GlobalPackage => {
            let value = arg(0);
            let target = if let Some(package) = Package::from_word(value) {
                Some(package)
            } else {
                let name = designator_text(runtime, value).ok_or_else(|| {
                    Fault::package_designator(runtime, value, call.error("expected a package designator"))
                })?;
                runtime.packages.borrow().named(&name)
            };
            target
                .filter(|pkg| runtime.packages.borrow().name(*pkg).is_some())
                .ok_or_else(|| {
                    Fault::package_error(runtime, 
                        value,
                        call.error(
                            "local nickname target does not designate a live global package",
                        ),
                    )
                })?
                .word()
        }
        Op::LocalNicknames => {
            let owner = call.live_package(arg(0))?;
            let mut list = runtime.root_slot(Word::NIL);
            let nicknames = runtime.packages.borrow().local_nicknames(owner).clone();
            for (name, target) in nicknames.into_iter().rev() {
                let name = runtime.root_slot(call.make_string(&name)?);
                let pair = call.cons(&name, &runtime.root_slot(target.word()))?;
                list = call.cons(&pair, &list)?;
            }
            list.get()
        }
        Op::LocallyNicknamedBy => {
            let target = call.live_package(arg(0))?;
            let owners = runtime.packages.borrow().locally_nicknamed_by(target);
            call.package_list(owners)?
        }
        Op::ReplaceLocalNicknames => {
            let owner = call.live_package(arg(0))?;
            let mut entries = std::collections::BTreeMap::new();
            for pair in call.list(arg(1))? {
                let (name, target) = runtime.heap.cons_fields(pair).ok_or_else(|| {
                    Fault::type_error(runtime, 
                        pair,
                        "CONS",
                        call.error("expected a local nickname and package pair"),
                    )
                })?;
                entries.insert(call.string_designator(name)?, call.live_package(target)?);
            }
            runtime
                .packages
                .borrow_mut()
                .replace_local_nicknames(owner, entries);
            owner.word()
        }
        Op::RemoveLocalNickname => {
            let name = call.string_designator(arg(0))?;
            let owner = call.package_or_current(second())?;
            truth(
                runtime
                    .packages
                    .borrow_mut()
                    .remove_local_nickname(owner, &name),
            )
        }
        Op::Make => call.make_package(frame, args)?.word(),
        Op::Find => call.find_package(arg(0))?.map_or(Word::NIL, Package::word),
        Op::Predicate => truth(Package::from_word(arg(0)).is_some()),
        Op::Name => {
            let package = call.require_package(arg(0))?;
            let name = runtime.packages.borrow().name(package).map(str::to_owned);
            match name {
                Some(name) => call.make_string(&name)?,
                None => Word::NIL,
            }
        }
        Op::Nicknames => {
            let package = call.live_package(arg(0))?;
            let nicknames = runtime
                .packages
                .borrow()
                .nicknames(package)
                .unwrap()
                .to_vec();
            let mut list = runtime.root_slot(Word::NIL);
            for name in nicknames.iter().rev() {
                let string = runtime.root_slot(call.make_string(name)?);
                list = call.cons(&string, &list)?;
            }
            list.get()
        }
        Op::UseList | Op::UsedByList => {
            let package = call.live_package(arg(0))?;
            let registry = runtime.packages.borrow();
            let packages = match op {
                Op::UseList => registry.use_list(package).unwrap().to_vec(),
                _ => registry.used_by(package),
            };
            drop(registry);
            call.package_list(packages)?
        }
        Op::ShadowingSymbols => {
            let package = call.live_package(arg(0))?;
            let symbols = runtime.packages.borrow().shadowing_symbols(package);
            call.word_list(symbols)?
        }
        Op::ListAll => {
            let packages = runtime.packages.borrow().list().collect();
            call.package_list(packages)?
        }
        Op::FindAllSymbols => {
            let name = call.string_designator(arg(0))?;
            let symbols = runtime.packages.borrow().present_symbols(&name);
            call.word_list(symbols)?
        }
        Op::Intern | Op::FindSymbol => {
            let name = call.string_designator(arg(0))?;
            let package = call.package_or_current(second())?;
            let (symbol, status) = if matches!(op, Op::Intern) {
                runtime.intern_package(package, &name, at)?
            } else {
                runtime
                    .packages
                    .borrow()
                    .find(package, &name)
                    .map_or((Word::NIL, None), |(symbol, status)| (symbol, Some(status)))
            };
            let symbol = runtime.root_slot(symbol);
            let status = match status {
                Some(SymbolStatus::Internal) => call.keyword("INTERNAL")?,
                Some(SymbolStatus::External) => call.keyword("EXTERNAL")?,
                Some(SymbolStatus::Inherited) => call.keyword("INHERITED")?,
                None => Word::NIL,
            };
            values.resize(2);
            values.slots.set(0, symbol.get());
            values.slots.set(1, status);
            return Ok(());
        }
        Op::Use | Op::Unuse => {
            let package = call.package_or_current(second())?;
            for designator in call.list_or_one(arg(0))? {
                let used = call.live_package(designator)?;
                let mut registry = runtime.packages.borrow_mut();
                match op {
                    Op::Use => registry.use_package(package, used),
                    _ => registry.unuse_package(package, used),
                }
                .map_err(|error| call.registry_error(error))?;
            }
            Word::T
        }
        Op::Import | Op::Export => {
            let package = call.package_or_current(second())?;
            let operation = if matches!(op, Op::Import) {
                SymbolOperation::Import
            } else {
                SymbolOperation::Export
            };
            let symbols = call.list_or_one(arg(0))?;
            for (index, &symbol) in symbols.iter().enumerate() {
                if let Some(conflict) = operation.apply(&call, package, symbol, None)? {
                    let pending = PendingSymbols::new(
                        runtime,
                        operation,
                        package,
                        &symbols[index..],
                        conflict.operation,
                    );
                    return Err(Failure::correctable(
                        call.registry_error(conflict.error),
                        Recovery::Symbols(pending),
                    ));
                }
            }
            Word::T
        }
        Op::Unexport | Op::ShadowingImport => {
            let package = call.package_or_current(second())?;
            for symbol in call.list_or_one(arg(0))? {
                call.update_membership(op, package, symbol)?;
            }
            Word::T
        }
        Op::Shadow => {
            let package = call.package_or_current(second())?;
            // Materialize names before allocating a fresh symbol for any shadow.
            let names = call
                .list_or_one(arg(0))?
                .into_iter()
                .map(|word| call.string_designator(word))
                .collect::<Result<Vec<_>, _>>()?;
            for name in names {
                let present = runtime.packages.borrow().present(package, &name);
                let symbol = match present {
                    Some((symbol, _)) => symbol,
                    None => {
                        let (_, symbol) = runtime.allocate_symbol(&name, None, at)?;
                        runtime.heap.set_symbol_home(symbol, Some(package));
                        symbol
                    }
                };
                runtime
                    .packages
                    .borrow_mut()
                    .shadow(package, name, symbol)
                    .map_err(|error| call.registry_error(error))?;
            }
            Word::T
        }
        Op::Unintern => {
            let package = call.package_or_current(second())?;
            let symbol = arg(0);
            let name = call.symbol_name(symbol)?;
            let removed = runtime
                .packages
                .borrow_mut()
                .unintern(package, &name, symbol)
                .map_err(|error| call.registry_error(error))?;
            if let Some(removed) = removed {
                call.clear_home(package, removed.get());
                Word::T
            } else {
                Word::NIL
            }
        }
        Op::Rename => {
            let package = call.live_package(arg(0))?;
            let name = call.rename_name(arg(1))?;
            let nicknames = if args.len() == 3 {
                call.string_list(arg(2))?
            } else {
                Vec::new()
            };
            runtime
                .packages
                .borrow_mut()
                .rename(package, name, nicknames)
                .map_err(|error| call.registry_error(error))?;
            package.word()
        }
        Op::Delete => {
            let Some(package) = call.find_package(arg(0))? else {
                // A nonexistent package is a correctable error; continuing
                // completes DELETE-PACKAGE with NIL.
                let fault =
                    Fault::package_error(runtime, arg(0), EvalError::new(at, "package does not exist"));
                return Err(Failure::correctable(fault, Recovery::IgnoreMissingPackage));
            };
            match runtime.packages.borrow_mut().delete(package) {
                Ok(Some(members)) => {
                    for symbol in members {
                        call.clear_home(package, symbol.get());
                    }
                    Word::T
                }
                Ok(None) => Word::NIL,
                Err(error) => {
                    return Err(call.package_failure(error, Recovery::Delete(package)));
                }
            }
        }
    };
    values.one(value);
    Ok(())
}

struct Call<'rt> {
    runtime: &'rt Runtime,
    at: usize,
}

impl Call<'_> {
    fn make_package(&self, frame: &RootSlots, args: Range<usize>) -> Result<Package, Failure> {
        let name = self.string_designator(frame.get(args.start))?;
        let mut nicknames = Vec::new();
        // This implementation defaults new packages to an empty use list.
        let mut uses = Vec::new();
        if args.len() > 1 {
            let keys = self.runtime.root_frame(3);
            for (index, name) in ["NICKNAMES", "USE", "ALLOW-OTHER-KEYS"]
                .into_iter()
                .enumerate()
            {
                keys.set(index, self.keyword(name)?);
            }
            let options = keyword_arguments(
                self.runtime,
                &[keys.get(0), keys.get(1)],
                args.start + 1..args.end,
                |index| frame.get(index),
                false,
                keys.get(2),
            )
            .map_err(|error| error.program_error(self.at))?;
            if let Some(index) = options[0] {
                nicknames = self.string_list(frame.get(index))?;
            }
            if let Some(index) = options[1] {
                uses = self
                    .list(frame.get(index))?
                    .into_iter()
                    .map(|word| self.live_package(word))
                    .collect::<Result<_, _>>()?;
            }
        }
        match self
            .runtime
            .packages
            .borrow_mut()
            .create(name, nicknames, uses)
        {
            Ok(package) => Ok(package),
            Err(error) => {
                let existing = error.package;
                Err(self.package_failure(error, Recovery::UseExisting(existing)))
            }
        }
    }

    fn update_membership(&self, op: Op, package: Package, symbol: Word) -> Result<(), Fault> {
        let name = self.symbol_name(symbol)?;
        let mut registry = self.runtime.packages.borrow_mut();
        match op {
            Op::Unexport => registry
                .unexport(package, &name, symbol)
                .map_err(|error| self.registry_error(error)),
            Op::ShadowingImport => {
                let removed = registry
                    .shadowing_import(package, name, symbol)
                    .map_err(|error| self.registry_error(error))?;
                if let Some(removed) = removed {
                    self.clear_home(package, removed.get());
                }
                Ok(())
            }
            _ => unreachable!("VM selects package membership operations before dispatch"),
        }
    }

    fn clear_home(&self, package: Package, symbol: Word) {
        if self.runtime.heap.symbol_home(symbol) == Some(package) {
            self.runtime.heap.set_symbol_home(symbol, None);
        }
    }

    fn registry_error(&self, error: PackageError) -> Fault {
        Fault::from_package_error(error, self.at)
    }

    fn package_failure(&self, error: PackageError, recovery: Recovery) -> Failure {
        let correctable = error.correctable;
        let fault = Fault::from_package_error(error, self.at);
        if correctable {
            Failure::correctable(fault, recovery)
        } else {
            Failure::Fault(fault)
        }
    }

    fn find_package(&self, value: Word) -> Result<Option<Package>, Fault> {
        if let Some(package) = Package::from_word(value) {
            return Ok(Some(package));
        }
        let name = designator_text(self.runtime, value).ok_or_else(|| {
            Fault::package_designator(self.runtime, value, self.error("expected a package designator"))
        })?;
        Ok(self.runtime.find_package_named(&name))
    }

    fn require_package(&self, value: Word) -> Result<Package, Fault> {
        self.find_package(value)?
            .ok_or_else(|| Fault::package_error(self.runtime, value, self.error("package does not exist")))
    }

    fn live_package(&self, value: Word) -> Result<Package, Fault> {
        let package = self.require_package(value)?;
        if self.runtime.packages.borrow().name(package).is_some() {
            Ok(package)
        } else {
            Err(Fault::package_error(self.runtime, 
                value,
                self.error("package has been deleted"),
            ))
        }
    }

    fn package_or_current(&self, value: Option<Word>) -> Result<Package, Fault> {
        match value {
            Some(value) => self.live_package(value),
            None => Ok(self.runtime.current_package_at(self.at)?),
        }
    }

    fn rename_name(&self, value: Word) -> Result<String, Fault> {
        if let Some(package) = Package::from_word(value) {
            self.runtime
                .packages
                .borrow()
                .name(package)
                .map(str::to_owned)
                .ok_or_else(|| Fault::package_error(self.runtime, value, self.error("package has been deleted")))
        } else {
            self.string_designator(value)
        }
    }

    fn string_designator(&self, value: Word) -> Result<String, Fault> {
        designator_text(self.runtime, value).ok_or_else(|| {
            Fault::string_designator(self.runtime, value, self.error("expected a string designator"))
        })
    }

    fn symbol_name(&self, value: Word) -> Result<String, Fault> {
        self.runtime
            .heap
            .symbol_name(value)
            .ok_or_else(|| Fault::type_error(self.runtime, value, "SYMBOL", self.error("expected a symbol")))
    }

    fn keyword(&self, name: &str) -> Result<Word, EvalError> {
        Ok(self
            .runtime
            .intern_package(Package::KEYWORD, name, self.at)?
            .0)
    }

    fn list(&self, value: Word) -> Result<Vec<Word>, Fault> {
        proper_list(self.runtime, value, self.at)
            .map_err(|error| Fault::type_error(self.runtime, value, "LIST", error))
    }

    fn list_or_one(&self, value: Word) -> Result<Vec<Word>, Fault> {
        if value == Word::NIL || self.runtime.heap.cons_fields(value).is_some() {
            self.list(value)
        } else {
            Ok(vec![value])
        }
    }

    fn string_list(&self, value: Word) -> Result<Vec<String>, Fault> {
        self.list(value)?
            .into_iter()
            .map(|word| self.string_designator(word))
            .collect()
    }

    fn package_list(&self, packages: Vec<Package>) -> Result<Word, EvalError> {
        let mut list = RootSlot::nil();
        for package in packages.into_iter().rev() {
            list = self.cons(&self.runtime.root_slot(package.word()), &list)?;
        }
        Ok(list.get())
    }

    fn word_list(&self, words: Vec<Word>) -> Result<Word, EvalError> {
        let words: Vec<_> = words
            .into_iter()
            .map(|word| self.runtime.root_slot(word))
            .collect();
        let mut list = RootSlot::nil();
        for word in words.into_iter().rev() {
            list = self.cons(&word, &list)?;
        }
        Ok(list.get())
    }

    fn make_string(&self, text: &str) -> Result<Word, EvalError> {
        self.runtime
            .heap
            .string(text)
            .map_err(|error| EvalError::out_of_memory(self.at, error))
    }

    fn cons(&self, car: &RootSlot, cdr: &RootSlot) -> Result<RootSlot, EvalError> {
        self.runtime
            .heap
            .cons(|| (car.get(), cdr.get()))
            .map(|word| self.runtime.root_slot(word))
            .map_err(|error| EvalError::out_of_memory(self.at, error))
    }

    fn error(&self, message: &'static str) -> EvalError {
        EvalError::new(self.at, message)
    }
}

fn truth(value: bool) -> Word {
    if value { Word::T } else { Word::NIL }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heap::Heap;
    use crate::vm::{ExecState, Pending, PendingContinuation, StepControl, dispatch_continuation};

    #[test]
    fn package_condition_survives_restart_allocation() {
        let mut config = Heap::builder();
        config.always_defrag = true;
        config.stress_bytes = Some(0);
        let runtime = Runtime::with_heap(Heap::start(config).unwrap());
        runtime
            .intern_package(Package::COMMON_LISP, "CONTINUE", 0)
            .unwrap();
        let fault = Fault::package_error(
            &runtime,
            Package::USER.word(),
            EvalError::new(0, "package error"),
        );
        let condition = runtime.root_slot(fault.make_condition(&runtime).unwrap());
        let before = condition.get();
        let continuation = PackageContinuation {
            condition: runtime.root_slot(before),
            host: Some(fault.host),
            recovery: Recovery::IgnoreMissingPackage,
            at: 0,
            resume: Resume::Call(None),
        };
        let mut exec = ExecState::new(Values::new(&runtime));
        let step =
            dispatch_continuation(&runtime, &mut exec, Box::new(continuation), false).unwrap();
        let StepControl::Pending(Some(PendingContinuation(Pending::Signal(search)))) = step else {
            panic!("expected a pending package error signal");
        };
        assert_ne!(condition.get(), before);
        assert_eq!(search.signal.condition.get(0), condition.get());
    }
}
