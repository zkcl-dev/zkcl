use std::collections::HashSet;

use num_bigint::Sign;

use crate::condition::{Class, Fault};
use crate::package::Package;
use crate::{EvalError, Root, Runtime};

use super::{
    DEFINE_PACKAGE, Expansion, SET_PACKAGE_DOCUMENTATION, call, fresh, invalid, list, native_call,
    proper_list, symbol,
};

enum Designator {
    Name(String),
    Package(Package),
}

impl Designator {
    fn parse(runtime: &Runtime, value: &Root<'_>) -> Result<Self, EvalError> {
        if let Some(package) = Package::from_word(value.slot.get()) {
            return Ok(Self::Package(package));
        }
        string_designator(runtime, value).map(Self::Name)
    }

    fn literal<'rt>(&self, runtime: &'rt Runtime) -> Expansion<'rt> {
        match self {
            Self::Name(spelling) => literal_string(runtime, spelling),
            Self::Package(package) => Ok(runtime.root(package.word())),
        }
    }
}

struct Import {
    source: Designator,
    names: Vec<String>,
}

#[derive(Default)]
struct DefpackageOptions<'rt> {
    nicknames: Vec<String>,
    local_nicknames: Vec<(String, String)>,
    documentation: Option<Root<'rt>>,
    uses: Vec<Designator>,
    shadows: Vec<String>,
    shadowing_imports: Vec<Import>,
    imports: Vec<Import>,
    interns: Vec<String>,
    exports: Vec<String>,
}

impl<'rt> DefpackageOptions<'rt> {
    fn parse(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Result<Self, EvalError> {
        let mut options = Self::default();
        let mut has_size = false;
        for form in forms {
            let option = proper_list(runtime, form.clone(), "DEFPACKAGE option")?;
            let Some((operator, operands)) = option.split_first() else {
                return Err(invalid("DEFPACKAGE", "expects an option keyword"));
            };
            let word = operator.slot.get();
            let name = (runtime.heap.symbol_home(word) == Some(Package::KEYWORD))
                .then(|| runtime.symbol_name(operator))
                .flatten();
            match name.as_deref() {
                Some("NICKNAMES") => options.nicknames.extend(names(runtime, operands)?),
                Some("LOCAL-NICKNAMES") => {
                    for spec in operands {
                        let pair =
                            proper_list(runtime, spec.clone(), "DEFPACKAGE :LOCAL-NICKNAMES")?;
                        let [nickname, target] = pair.as_slice() else {
                            return Err(invalid(
                                "DEFPACKAGE :LOCAL-NICKNAMES",
                                "expected (nickname target-package)",
                            ));
                        };
                        options.local_nicknames.push((
                            package_spelling(runtime, nickname)?,
                            package_spelling(runtime, target)?,
                        ));
                    }
                }
                Some("SHADOW") => options.shadows.extend(names(runtime, operands)?),
                Some("INTERN") => options.interns.extend(names(runtime, operands)?),
                Some("EXPORT") => options.exports.extend(names(runtime, operands)?),
                Some("USE") => {
                    for operand in operands {
                        options.uses.push(Designator::parse(runtime, operand)?);
                    }
                }
                Some("IMPORT-FROM" | "SHADOWING-IMPORT-FROM") => {
                    let Some((source, symbols)) = operands.split_first() else {
                        return Err(invalid("DEFPACKAGE", "import requires a source package"));
                    };
                    let import = Import {
                        source: Designator::parse(runtime, source)?,
                        names: names(runtime, symbols)?,
                    };
                    if name.as_deref() == Some("IMPORT-FROM") {
                        options.imports.push(import);
                    } else {
                        options.shadowing_imports.push(import);
                    }
                }
                Some("DOCUMENTATION") => {
                    let [doc] = operands else {
                        return Err(invalid("DEFPACKAGE :DOCUMENTATION", "expects one string"));
                    };
                    if options.documentation.is_some() || !runtime.heap.is_string(doc.slot.get()) {
                        return Err(invalid(
                            "DEFPACKAGE :DOCUMENTATION",
                            "expects a single occurrence with a string",
                        ));
                    }
                    options.documentation = Some(doc.clone());
                }
                Some("SIZE") => {
                    if has_size
                        || !matches!(operands, [value]
                            if runtime.heap.integer(value.slot.get())
                                .is_some_and(|value| value.sign() != Sign::Minus))
                    {
                        return Err(invalid(
                            "DEFPACKAGE :SIZE",
                            "expects a single occurrence with a non-negative integer",
                        ));
                    }
                    has_size = true;
                }
                _ => return Err(invalid("DEFPACKAGE", "encountered an unknown option")),
            }
        }
        options.check_disjoint()?;
        Ok(options)
    }

    fn check_disjoint(&self) -> Result<(), EvalError> {
        let shadows: HashSet<_> = self.shadows.iter().collect();
        let interns: HashSet<_> = self.interns.iter().collect();
        let imports: HashSet<_> = self
            .imports
            .iter()
            .flat_map(|import| &import.names)
            .collect();
        let shadowing_imports: HashSet<_> = self
            .shadowing_imports
            .iter()
            .flat_map(|import| &import.names)
            .collect();
        let sets = [&shadows, &interns, &imports, &shadowing_imports];
        let names_overlap = sets.iter().enumerate().any(|(index, group)| {
            sets[index + 1..]
                .iter()
                .any(|other| !group.is_disjoint(other))
        });
        if names_overlap || self.exports.iter().any(|name| interns.contains(name)) {
            return Err(invalid(
                "DEFPACKAGE",
                "symbol name options must be disjoint",
            ));
        }
        Ok(())
    }
}

fn string_designator(runtime: &Runtime, value: &Root<'_>) -> Result<String, EvalError> {
    runtime
        .string_designator_text(value.slot.get())
        .ok_or_else(|| invalid("DEFPACKAGE", "expects a string designator"))
}

fn package_spelling(runtime: &Runtime, value: &Root<'_>) -> Result<String, EvalError> {
    match Designator::parse(runtime, value)? {
        Designator::Name(spelling) => Ok(spelling),
        Designator::Package(package) => runtime
            .packages
            .borrow()
            .name(package)
            .map(str::to_owned)
            .ok_or_else(|| invalid("DEFPACKAGE :LOCAL-NICKNAMES", "expected a live package")),
    }
}

fn names(runtime: &Runtime, values: &[Root<'_>]) -> Result<Vec<String>, EvalError> {
    values
        .iter()
        .map(|value| string_designator(runtime, value))
        .collect()
}

fn literal_string<'rt>(runtime: &'rt Runtime, spelling: &str) -> Expansion<'rt> {
    runtime
        .heap
        .string(spelling)
        .map(|word| runtime.root(word))
        .map_err(|error| EvalError::out_of_memory(0, error))
}

fn keyword<'rt>(runtime: &'rt Runtime, spelling: &str) -> Expansion<'rt> {
    let (word, _) = runtime.intern_package(Package::KEYWORD, spelling, 0)?;
    Ok(runtime.root(word))
}

fn quoted_list<'rt>(runtime: &'rt Runtime, values: &[Root<'rt>]) -> Expansion<'rt> {
    let values = list(runtime, values.iter())?;
    call(runtime, "QUOTE", std::iter::once(&values))
}

fn import_form<'rt>(
    runtime: &'rt Runtime,
    package: &Root<'rt>,
    source: &Root<'rt>,
    spelling: &str,
    operation: &str,
) -> Expansion<'rt> {
    let spelling = literal_string(runtime, spelling)?;
    let found = fresh(runtime, "DEFPACKAGE IMPORT")?;
    let status = fresh(runtime, "DEFPACKAGE STATUS")?;
    let bindings = list(runtime, [&found, &status].into_iter())?;
    let lookup = call(runtime, "FIND-SYMBOL", [&spelling, source].into_iter())?;
    let found = call(runtime, "LIST", std::iter::once(&found))?;
    let import = call(runtime, operation, [&found, package].into_iter())?;
    let class = call(
        runtime,
        "QUOTE",
        std::iter::once(&symbol(runtime, "PACKAGE-ERROR")?),
    )?;
    let package_key = keyword(runtime, "PACKAGE")?;
    let error = call(runtime, "ERROR", [&class, &package_key, source].into_iter())?;
    let nil = runtime.nil();
    let import_if_found = call(runtime, "IF", [&status, &import, &nil].into_iter())?;
    let check_again = call(
        runtime,
        "MULTIPLE-VALUE-BIND",
        [&bindings, &lookup, &import_if_found].into_iter(),
    )?;
    let continue_clause = list(
        runtime,
        [&symbol(runtime, "CONTINUE")?, &nil, &check_again].into_iter(),
    )?;
    let missing = call(
        runtime,
        "RESTART-CASE",
        [&error, &continue_clause].into_iter(),
    )?;
    let branch = call(runtime, "IF", [&status, &import, &missing].into_iter())?;
    call(
        runtime,
        "MULTIPLE-VALUE-BIND",
        [&bindings, &lookup, &branch].into_iter(),
    )
}

pub(super) fn expand<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    let Some((name, forms)) = forms.split_first() else {
        return Err(invalid("DEFPACKAGE", "requires a package name"));
    };
    let name = literal_string(runtime, &string_designator(runtime, name)?)?;
    let options = DefpackageOptions::parse(runtime, forms)?;
    let package = fresh(runtime, "DEFPACKAGE PACKAGE")?;
    let mut body = Vec::new();

    let nicknames: Vec<_> = options
        .nicknames
        .iter()
        .map(|name| literal_string(runtime, name))
        .collect::<Result<_, _>>()?;
    let proposed_nicknames = quoted_list(runtime, &nicknames)?;
    let define = native_call(runtime, DEFINE_PACKAGE, [&name, &proposed_nicknames])?;
    let binding = list(runtime, [&package, &define].into_iter())?;
    let bindings = list(runtime, std::iter::once(&binding))?;

    if !options.shadows.is_empty() {
        let shadows: Vec<_> = options
            .shadows
            .iter()
            .map(|name| literal_string(runtime, name))
            .collect::<Result<_, _>>()?;
        let shadows = quoted_list(runtime, &shadows)?;
        body.push(call(runtime, "SHADOW", [&shadows, &package].into_iter())?);
    }
    for import in options.shadowing_imports {
        let source = import.source.literal(runtime)?;
        for name in import.names {
            body.push(import_form(
                runtime,
                &package,
                &source,
                &name,
                "SHADOWING-IMPORT",
            )?);
        }
    }
    if !options.uses.is_empty() {
        let uses: Vec<_> = options
            .uses
            .iter()
            .map(|package| package.literal(runtime))
            .collect::<Result<_, _>>()?;
        let uses = quoted_list(runtime, &uses)?;
        body.push(call(runtime, "USE-PACKAGE", [&uses, &package].into_iter())?);
    }
    for import in options.imports {
        let source = import.source.literal(runtime)?;
        for name in import.names {
            body.push(import_form(runtime, &package, &source, &name, "IMPORT")?);
        }
    }
    for name in options.interns {
        let name = literal_string(runtime, &name)?;
        body.push(call(runtime, "INTERN", [&name, &package].into_iter())?);
    }
    for name in options.exports {
        let name = literal_string(runtime, &name)?;
        let intern = call(runtime, "INTERN", [&name, &package].into_iter())?;
        // EXPORT accepts a list of symbols to distinguish NIL from an empty list.
        let singleton = call(runtime, "LIST", std::iter::once(&intern))?;
        body.push(call(runtime, "EXPORT", [&singleton, &package].into_iter())?);
    }
    if options.local_nicknames.is_empty() {
        // This primitive also serves DEFPACKAGE during runtime bootstrap.
        let replace =
            runtime.root(crate::vm::Primitive::named("ZK::%REPLACE-LOCAL-NICKNAMES").unwrap());
        body.push(call(
            runtime,
            "FUNCALL",
            [&replace, &package, &runtime.nil()].into_iter(),
        )?);
    } else {
        let mut pairs = Vec::new();
        for (nickname, target) in options.local_nicknames {
            let name = literal_string(runtime, &nickname)?;
            let target = literal_string(runtime, &target)?;
            pairs.push(list(runtime, [&name, &target].into_iter())?);
        }
        let specs = quoted_list(runtime, &pairs)?;
        let function = runtime.root(
            runtime
                .intern_package(Package::ZK, "%DEFINE-LOCAL-NICKNAMES", 0)?
                .0,
        );
        body.push(list(runtime, [&function, &package, &specs].into_iter())?);
    }
    if let Some(doc) = options.documentation {
        body.push(native_call(
            runtime,
            SET_PACKAGE_DOCUMENTATION,
            [&package, &doc],
        )?);
    }
    let form = call(
        runtime,
        "LET",
        std::iter::once(&bindings)
            .chain(&body)
            .chain(std::iter::once(&package)),
    )?;
    super::define_at_toplevel(runtime, form)
}

pub(super) fn define_package<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let [name, nicknames] = args else {
        return Err(Fault::plain(
            Class::ProgramError,
            EvalError::new(at, "expected a package name and nicknames"),
        ));
    };
    let name = string_designator(runtime, name)?;
    let nicknames = proper_list(runtime, nicknames.clone(), "package nicknames")?;
    let nicknames = names(runtime, &nicknames)?;
    // A conflicting nickname must fail the definition. Continuing MAKE-PACKAGE
    // could otherwise return the nickname's owner, and later options would
    // modify that package instead of the one being defined.
    let current = runtime.bound_package();
    let package = runtime
        .packages
        .borrow_mut()
        .define(current, name, nicknames)
        .map_err(|error| Fault::from_package_error(error, at))?;
    Ok(vec![runtime.root(package.word())])
}

pub(super) fn set_documentation<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let [package, documentation] = args else {
        return Err(Fault::plain(
            Class::ProgramError,
            EvalError::new(at, "expected a package and documentation string"),
        ));
    };
    let package = Package::from_word(package.slot.get()).ok_or_else(|| {
        Fault::type_error(runtime, 
            package.slot.get(),
            "PACKAGE",
            EvalError::new(at, "expected a package"),
        )
    })?;
    let doc = documentation.slot.get();
    if !runtime.heap.is_string(doc) {
        return Err(Fault::type_error(runtime, 
            doc,
            "STRING",
            EvalError::new(at, "expected a documentation string"),
        ));
    }
    runtime
        .packages
        .borrow_mut()
        .set_documentation(package, doc)
        .map_err(|error| Fault::from_package_error(error, at))?;
    Ok(vec![documentation.clone()])
}
