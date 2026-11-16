use rustc_hash::FxHashMap;
use zkcl_syntax::cl_builtins::{BuiltinKind, EXTERNAL_NAMES, builtin_kind};
use zkcl_syntax::{Prefix, SymbolName};

use crate::heap::Word;
use crate::package::Package;
pub use crate::package::SymbolStatus;
use crate::{EvalError, Root, Runtime, classes};

/// A symbol rooted in its owning runtime, with its accessibility in the queried package.
pub struct SymbolLookup<'rt> {
    pub symbol: Root<'rt>,
    pub status: SymbolStatus,
}

/// A stamp for the package, bindings, and declaration metadata visible to tools.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NamespaceRevision {
    packages: u64,
    functions: usize,
    definitions: u64,
    current: Option<Package>,
    standard_macro_hook: bool,
}

/// Metadata for a live package.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamespacePackage {
    pub name: String,
    pub nicknames: Vec<String>,
    pub external_count: usize,
    pub total_count: usize,
    pub is_current: bool,
    pub use_list: Vec<String>,
    pub used_by: Vec<String>,
    pub local_nicknames: Vec<(String, String)>,
}

/// The namespace in which a symbol has a live binding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NamespaceBindingKind {
    Function,
    Macro,
    Variable,
    Constant,
    Type,
}

/// Metadata for one binding, independent of completion or display policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamespaceBinding {
    pub kind: NamespaceBindingKind,
    pub lambda_list: Option<String>,
    pub documentation: Option<String>,
}

/// A defined symbol as seen through one package's present entries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamespaceSymbol {
    /// Identity within the owning runtime, shared by imported occurrences.
    pub id: usize,
    /// Exact symbol name, without reader escaping or case conversion.
    pub name: String,
    /// The package containing this present entry, which may be an import.
    pub package: String,
    pub home_package: Option<String>,
    pub external: bool,
    pub accessible: bool,
    pub bindings: Vec<NamespaceBinding>,
}

/// Owned metadata for live package and symbol bindings. Taking a snapshot runs no Lisp.
#[derive(Clone, Debug, Default)]
pub struct NamespaceSnapshot {
    pub current_package: Option<String>,
    pub packages: Vec<NamespacePackage>,
    pub symbols: Vec<NamespaceSymbol>,
    pub local_nicknames: Vec<(String, String)>,
}

/// Access to standard symbols and callable bindings under the live package graph.
/// This metadata can be used by syntax analysis without reading or expanding Lisp.
#[derive(Default)]
pub struct StandardSourceSymbols {
    current: Option<Package>,
    packages_by_name: FxHashMap<String, Package>,
    accessible: FxHashMap<Package, FxHashMap<&'static str, SymbolStatus>>,
}

impl StandardSourceSymbols {
    /// Whether this spelling resolves to the corresponding standard symbol and,
    /// for a callable, retains its standard implementation.
    pub fn resolves(&self, symbol: &SymbolName) -> bool {
        let (package, external) = match &symbol.prefix {
            Prefix::None => (self.current, false),
            Prefix::Package { name, external } => {
                (self.packages_by_name.get(name).copied(), *external)
            }
            Prefix::Keyword | Prefix::Uninterned => return false,
        };
        package
            .and_then(|package| self.accessible.get(&package))
            .and_then(|names| names.get(symbol.name.as_str()))
            .is_some_and(|status| !external || *status == SymbolStatus::External)
    }
}

impl Runtime {
    /// Like Common Lisp `FIND-SYMBOL`, using exact names without invoking a reader.
    /// The package may be a global nickname. An unknown package is an error.
    pub fn find_symbol(
        &self,
        package: &str,
        name: &str,
    ) -> Result<Option<SymbolLookup<'_>>, EvalError> {
        self.initialize_packages()?;
        let packages = self.packages.borrow();
        let package = packages
            .named(package)
            .ok_or_else(|| EvalError::new(0, format!("unknown package {package:?}")))?;
        Ok(packages
            .find(package, name)
            .map(|(symbol, status)| SymbolLookup {
                symbol: self.root(symbol),
                status,
            }))
    }

    /// Invoke Common Lisp `REQUIRE` in this instance, including its module providers.
    pub fn require_module(&self, name: &str) -> Result<(), EvalError> {
        let require = self
            .lookup_symbol("COMMON-LISP", "REQUIRE")?
            .expect("standard REQUIRE");
        let name = self
            .make_string(name)
            .map_err(|e| EvalError::out_of_memory(0, e))?;
        self.call(&require, &[&name])?;
        Ok(())
    }

    /// Copy the current `*MODULES*` binding in its Lisp list order.
    pub fn loaded_modules(&self) -> Result<Vec<String>, EvalError> {
        let variable = self
            .lookup_symbol("COMMON-LISP", "*MODULES*")?
            .expect("standard module variable");
        let value = self
            .lookup_symbol("COMMON-LISP", "SYMBOL-VALUE")?
            .expect("standard symbol accessor");
        let value = self.call(&value, &[&variable])?;
        let mut spine = crate::heap::ListSpine::new(value[0].slot.get());
        let mut names = Vec::new();
        let invalid = || EvalError::new(0, "*MODULES* must be a proper list of strings");
        while let Some((name, _)) = spine.next(&self.heap).map_err(|_| invalid())? {
            names.push(self.heap.string_contents(name).ok_or_else(invalid)?);
        }
        if spine.tail() != Word::NIL {
            return Err(invalid());
        }
        Ok(names)
    }

    /// Check whether a previously collected namespace snapshot must be refreshed.
    pub fn namespace_revision(&self) -> NamespaceRevision {
        NamespaceRevision {
            packages: self.packages.borrow().revision(),
            functions: self.heap.function_epoch(),
            definitions: self.heap.namespace_epoch(),
            current: self.namespace_package(),
            standard_macro_hook: self.uses_standard_macroexpand_hook(),
        }
    }

    /// Collect standard symbol visibility for source analysis. This takes no Lisp actions.
    /// A caller may cache the result while [`Self::namespace_revision`] is unchanged.
    pub fn standard_source_symbols(&self) -> StandardSourceSymbols {
        let packages = self.packages.borrow();
        let current = self.namespace_package();
        let mut snapshot = StandardSourceSymbols {
            current,
            ..StandardSourceSymbols::default()
        };
        let standard_macro_hook = self.uses_standard_macroexpand_hook();
        let standard: FxHashMap<_, _> = EXTERNAL_NAMES
            .iter()
            .filter_map(|&name| {
                let (symbol, _) = packages.present(Package::COMMON_LISP, name)?;
                let attributes = self.heap.symbol_attributes(symbol)?;
                let known_binding = match builtin_kind(name) {
                    Some(BuiltinKind::Function) => {
                        self.heap.symbol_cell(symbol, true).is_some_and(|function| {
                            crate::vm::Primitive::named(name).map_or_else(
                                || crate::vm::is_function(self, function),
                                |primitive| function == primitive,
                            )
                        })
                    }
                    Some(BuiltinKind::Macro) => {
                        standard_macro_hook && self.heap.macro_expander(symbol).is_none()
                    }
                    Some(BuiltinKind::SpecialOperator) => {
                        self.heap.macro_expander(symbol).is_none()
                    }
                    _ => true,
                };
                (attributes.canonical && known_binding).then_some((symbol, name))
            })
            .collect();
        for package in packages.list() {
            let name = packages.name(package).expect("listed package");
            snapshot.packages_by_name.insert(name.to_owned(), package);
            for nickname in packages.nicknames(package).unwrap_or_default() {
                snapshot.packages_by_name.insert(nickname.clone(), package);
            }
            let names = packages
                .entries_snapshot(package, true, true, true)
                .expect("listed package")
                .into_iter()
                .filter_map(|(symbol, status, _)| standard.get(&symbol).map(|&name| (name, status)))
                .collect();
            snapshot.accessible.insert(package, names);
        }
        if let Some(current) = current.filter(|&package| packages.name(package).is_some()) {
            snapshot.packages_by_name.extend(
                packages
                    .local_nicknames(current)
                    .iter()
                    .map(|(name, &package)| (name.clone(), package)),
            );
        }
        snapshot
    }

    fn uses_standard_macroexpand_hook(&self) -> bool {
        self.find_common_lisp("*MACROEXPAND-HOOK*")
            .and_then(|symbol| self.heap.symbol_cell(symbol, false))
            .is_some_and(|hook| crate::vm::Primitive::named("FUNCALL") == Some(hook))
    }

    /// Collect package names, visible definitions, and available function metadata.
    pub fn namespace_snapshot(&self) -> NamespaceSnapshot {
        let packages = self.packages.borrow();
        let current = self.namespace_package();
        let mut snapshot = NamespaceSnapshot {
            current_package: current
                .and_then(|pkg| packages.name(pkg))
                .map(str::to_owned),
            ..NamespaceSnapshot::default()
        };
        if let Some(current) = current.filter(|id| packages.name(*id).is_some()) {
            snapshot.local_nicknames.extend(
                packages
                    .local_nicknames(current)
                    .iter()
                    .filter_map(|(nick, &pkg)| {
                        Some((nick.clone(), packages.name(pkg)?.to_owned()))
                    }),
            );
        }
        for pkg in packages.list() {
            let name = packages.name(pkg).expect("listed package");
            let mut summary = NamespacePackage {
                name: name.to_owned(),
                nicknames: packages.nicknames(pkg).unwrap_or_default().to_vec(),
                external_count: 0,
                total_count: 0,
                is_current: current == Some(pkg),
                use_list: packages
                    .use_list(pkg)
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|&id| packages.name(id).map(str::to_owned))
                    .collect(),
                used_by: packages
                    .used_by(pkg)
                    .into_iter()
                    .filter_map(|id| packages.name(id).map(str::to_owned))
                    .collect(),
                local_nicknames: packages
                    .local_nicknames(pkg)
                    .iter()
                    .filter_map(|(nick, &id)| Some((nick.clone(), packages.name(id)?.to_owned())))
                    .collect(),
            };
            for (spelling, symbol, status) in packages.entries(pkg) {
                let external = status == SymbolStatus::External;
                summary.external_count += usize::from(external);
                summary.total_count += 1;
                let attributes = self.heap.symbol_attributes(symbol).expect("package symbol");
                let mut bindings = Vec::new();
                let macro_function = self.heap.macro_expander(symbol);
                if let Some(function) =
                    macro_function.or_else(|| self.heap.symbol_cell(symbol, true))
                {
                    bindings.push(NamespaceBinding {
                        kind: if macro_function.is_some() {
                            NamespaceBindingKind::Macro
                        } else {
                            NamespaceBindingKind::Function
                        },
                        lambda_list: crate::vm::lambda_list_word(self, function)
                            .and_then(|list| self.repr(&self.root(list)).ok()),
                        documentation: self
                            .heap
                            .function_documentation(function)
                            .or_else(|| {
                                self.heap
                                    .generic_info(function)
                                    .map(|info| info.documentation)
                            })
                            .and_then(|doc| self.heap.string_contents(doc)),
                    });
                }
                if self.heap.symbol_cell(symbol, false).is_some() {
                    bindings.push(NamespaceBinding {
                        kind: if attributes.is_constant_variable() {
                            NamespaceBindingKind::Constant
                        } else {
                            NamespaceBindingKind::Variable
                        },
                        lambda_list: None,
                        documentation: self
                            .heap
                            .string_contents(self.heap.variable_documentation(symbol)),
                    });
                }
                if let Some(class) = classes::named_definition(self, symbol) {
                    bindings.push(NamespaceBinding {
                        kind: NamespaceBindingKind::Type,
                        lambda_list: None,
                        documentation: class
                            .documentation
                            .as_ref()
                            .and_then(|doc| self.heap.string_contents(doc.get())),
                    });
                } else if let Some(expander) = self.type_expander(symbol) {
                    bindings.push(NamespaceBinding {
                        kind: NamespaceBindingKind::Type,
                        lambda_list: None,
                        documentation: self
                            .heap
                            .function_documentation(expander)
                            .and_then(|doc| self.heap.string_contents(doc)),
                    });
                }
                if bindings.is_empty() {
                    continue;
                }
                let accessible = current
                    .and_then(|current| packages.find(current, spelling))
                    .is_some_and(|(visible, _)| visible == symbol);
                snapshot.symbols.push(NamespaceSymbol {
                    id: attributes.id,
                    name: spelling.to_owned(),
                    package: name.to_owned(),
                    home_package: attributes
                        .home
                        .and_then(|home| packages.name(home))
                        .map(str::to_owned),
                    external,
                    accessible,
                    bindings,
                });
            }
            snapshot.packages.push(summary);
        }
        snapshot
            .packages
            .sort_unstable_by(|a, b| a.name.cmp(&b.name));
        snapshot
            .symbols
            .sort_unstable_by(|a, b| a.name.cmp(&b.name).then_with(|| a.package.cmp(&b.package)));
        snapshot
    }

    fn namespace_package(&self) -> Option<Package> {
        if self.packages_initialized.get() {
            self.bound_package()
        } else {
            Some(Package::USER)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_source_symbols_follow_package_accessibility_and_live_callables() {
        let runtime = Runtime::start().unwrap();
        runtime
            .load_str(
                "(defpackage :preview-target (:use :cl) (:shadow :+ :if))
                 (defpackage :preview-caller (:use :cl)
                   (:local-nicknames (:std :cl) (:target :preview-target)))
                 (in-package :preview-caller)",
            )
            .unwrap();
        let resolves = |snapshot: &StandardSourceSymbols, spelling| {
            snapshot.resolves(&SymbolName::parse(spelling).unwrap())
        };
        let snapshot = runtime.standard_source_symbols();
        for spelling in ["+", "if", "std:+", "target::car"] {
            assert!(resolves(&snapshot, spelling), "{spelling}");
        }
        for spelling in ["target::+", "target::if", "target:car", "missing::+"] {
            assert!(!resolves(&snapshot, spelling), "{spelling}");
        }

        let revision = runtime.namespace_revision();
        runtime.eval("(trace cl:+)").unwrap();
        assert_ne!(runtime.namespace_revision(), revision);
        assert!(!resolves(&runtime.standard_source_symbols(), "std:+"));
        runtime.eval("(untrace cl:+)").unwrap();
        assert!(resolves(&runtime.standard_source_symbols(), "std:+"));

        let revision = runtime.namespace_revision();
        runtime
            .eval("(setq *macroexpand-hook* (lambda (fn form env) (funcall fn form env)))")
            .unwrap();
        assert_ne!(runtime.namespace_revision(), revision);
        let snapshot = runtime.standard_source_symbols();
        assert!(resolves(&snapshot, "if"));
        assert!(!resolves(&snapshot, "when"));
    }
}
