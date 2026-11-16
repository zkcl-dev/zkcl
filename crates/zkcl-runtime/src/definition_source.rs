use std::ops::Range;

use crate::heap::Word;
use crate::read::Form;
use crate::symbol::Name;
use crate::{EvalError, Root, Runtime, SourceDocument, classes};
use zkcl_bytecode::DefinitionDebug;
pub use zkcl_bytecode::DefinitionKind;

/// Owned metadata for a currently installed runtime definition.
#[derive(Clone, Debug)]
pub struct DefinitionInfo {
    pub name: String,
    pub kind: DefinitionKind,
    pub source: Option<SourceDocument>,
    /// A half-open UTF-8 byte range in `source.text()`. Empty ranges identify
    /// a known starting position when the reader did not retain an ending one.
    pub byte_range: Range<usize>,
    pub lambda_list: Option<String>,
    pub declared_type: Option<String>,
    pub documentation: Option<String>,
}

impl Runtime {
    /// Query the running namespace, without invoking reader macros or Lisp code.
    /// Accepts a symbol, `(SETF symbol)` name, function, or method object.
    pub fn definition_info(&self, value: &Root<'_>) -> Result<Vec<DefinitionInfo>, EvalError> {
        if !value.belongs_to(self) {
            return Err(EvalError::new(
                0,
                "definition value belongs to a different runtime",
            ));
        }
        let value = value.slot.get();
        if self.heap.is_symbol(value) {
            return self.symbol_definition_info(value);
        }
        if let Some(class) = classes::class_declaration(self, value) {
            let symbol = class.name.get();
            let name = self.repr(&self.root(symbol))?;
            let mut info = self
                .heap
                .symbol_attributes(symbol)
                .and_then(|attributes| {
                    self.definition_sources
                        .borrow()
                        .get(&(attributes.id, DefinitionKind::Class))
                        .cloned()
                })
                .unwrap_or_else(|| empty_info(name.clone(), DefinitionKind::Class));
            info.name = name;
            info.documentation = class
                .documentation
                .as_ref()
                .and_then(|doc| self.heap.string_contents(doc.get()));
            return Ok(vec![info]);
        }
        if let Some(method) = self.heap.method_info(value) {
            let name = self
                .heap
                .generic_info(method.owner)
                .and_then(|generic| self.repr(&self.root(generic.name)).ok());
            let mut info =
                self.callable_definition(method.function, DefinitionKind::Method, name)?;
            info.lambda_list = self.repr(&self.root(method.lambda_list)).ok();
            info.documentation = self.heap.string_contents(method.documentation);
            return Ok(vec![info]);
        }
        if let Ok(name) = crate::vm::FunctionName::parse(self, value, 0)
            && let Some(function) = name.get(self)
        {
            return Ok(vec![self.callable_definition(
                function,
                DefinitionKind::Function,
                Some(self.repr(&self.root(value))?),
            )?]);
        }
        if crate::vm::is_function(self, value) {
            let kind = if self.heap.generic_info(value).is_some() {
                DefinitionKind::GenericFunction
            } else {
                DefinitionKind::Function
            };
            return Ok(vec![self.callable_definition(value, kind, None)?]);
        }
        Ok(Vec::new())
    }

    /// Find current definitions of a symbol spelling, resolved in the current
    /// package. This reads only symbol syntax; it never evaluates submitted text.
    pub fn find_definitions(&self, spelling: &str) -> Result<Vec<DefinitionInfo>, EvalError> {
        self.initialize_packages()?;
        let spelling = spelling.trim();
        if spelling.starts_with('(') {
            // The sole compound function name is (SETF symbol). Parse the two
            // tokens directly so custom readtables cannot execute editor queries.
            let inner = spelling
                .strip_prefix('(')
                .and_then(|s| s.strip_suffix(')'))
                .ok_or_else(|| EvalError::new(0, "expected a symbol or (SETF symbol) name"))?;
            let tokens = symbol_tokens(inner)?;
            let [operator, accessor] = tokens.as_slice() else {
                return Err(EvalError::new(0, "expected a symbol or (SETF symbol) name"));
            };
            let operator = Name::read(self, operator, 0)?;
            if self.find_named_symbol(&operator)
                != self.find_named_symbol(&Name::common_lisp("SETF"))
            {
                return Err(EvalError::new(
                    0,
                    "expected the COMMON-LISP:SETF function name operator",
                ));
            }
            let name = Name::read(self, accessor, 0)?;
            let Some(symbol) = self.find_named_symbol(&name) else {
                return Ok(Vec::new());
            };
            return match crate::vm::FunctionName::Setf(symbol).get(self) {
                Some(function) => Ok(vec![self.callable_definition(
                    function,
                    DefinitionKind::Function,
                    Some(format!("(SETF {})", self.repr(&self.root(symbol))?)),
                )?]),
                None => Ok(Vec::new()),
            };
        }
        let tokens = symbol_tokens(spelling)?;
        let [spelling] = tokens.as_slice() else {
            return Err(EvalError::new(0, "expected a single symbol name"));
        };
        let name = Name::read(self, spelling, 0)?;
        let Some(symbol) = self.find_named_symbol(&name) else {
            return Ok(Vec::new());
        };
        self.symbol_definition_info(symbol)
    }

    fn symbol_definition_info(&self, symbol: Word) -> Result<Vec<DefinitionInfo>, EvalError> {
        let mut definitions = Vec::new();
        let attributes = self
            .heap
            .symbol_attributes(symbol)
            .expect("definition symbol");
        let display = self.repr(&self.root(symbol))?;
        let recorded = self.definition_sources.borrow();
        let current = |kind| recorded.get(&(attributes.id, kind));
        if let Some(function) = self.heap.macro_expander(symbol) {
            let mut info =
                self.callable_definition(function, DefinitionKind::Macro, Some(display.clone()))?;
            if let Some(origin) = current(DefinitionKind::Macro) {
                merge_origin(&mut info, origin);
            }
            definitions.push(info);
        } else if let Some(function) = self.heap.symbol_cell(symbol, true) {
            let kind = if self.heap.generic_info(function).is_some() {
                DefinitionKind::GenericFunction
            } else {
                DefinitionKind::Function
            };
            let mut info = self.callable_definition(function, kind, Some(display.clone()))?;
            if let Some(origin) = current(kind) {
                merge_origin(&mut info, origin);
            }
            if let Some(name) = Name::from_word(self, symbol) {
                info.declared_type = self
                    .proclaimed_ftypes
                    .borrow()
                    .get(&(name.key().clone(), false))
                    .and_then(|form| crate::form_value::to_value(self, form).ok())
                    .and_then(|value| self.repr(&value).ok());
            }
            definitions.push(info);
            if let Some(generic) = self.heap.generic_info(function) {
                let mut methods = generic.methods;
                while let Some((method, rest)) = self.heap.cons_fields(methods) {
                    if let Some(method) = self.heap.method_info(method) {
                        let mut info = self.callable_definition(
                            method.function,
                            DefinitionKind::Method,
                            Some(display.clone()),
                        )?;
                        info.lambda_list = self.repr(&self.root(method.lambda_list)).ok();
                        info.documentation = self.heap.string_contents(method.documentation);
                        definitions.push(info);
                    }
                    methods = rest;
                }
            }
        }
        if classes::named_definition(self, symbol).is_some() {
            let mut info = current(DefinitionKind::Class)
                .cloned()
                .unwrap_or_else(|| empty_info(display.clone(), DefinitionKind::Class));
            info.name = display.clone();
            definitions.push(info);
        }
        if self.heap.symbol_cell(symbol, false).is_some() {
            let kind = if attributes.is_constant_variable() {
                DefinitionKind::Constant
            } else {
                DefinitionKind::Variable
            };
            let mut info = current(kind)
                .cloned()
                .unwrap_or_else(|| empty_info(display, kind));
            if let Some(name) = Name::from_word(self, symbol) {
                info.declared_type = self
                    .proclaimed_types
                    .borrow()
                    .get(name.key())
                    .and_then(|form| crate::form_value::to_value(self, form).ok())
                    .and_then(|value| self.repr(&value).ok());
            }
            info.documentation = self
                .heap
                .string_contents(self.heap.variable_documentation(symbol));
            definitions.push(info);
        }
        Ok(definitions)
    }

    fn callable_definition(
        &self,
        function: Word,
        kind: DefinitionKind,
        name: Option<String>,
    ) -> Result<DefinitionInfo, EvalError> {
        // TRACE follows the live definition. Report its original rather than
        // the implementation lambda which prints tracing messages.
        let function = self
            .traced
            .borrow()
            .iter()
            .find(|trace| trace.wrapper.get() == function && !trace.generic)
            .map_or(function, |trace| trace.original.get());
        let mut info = empty_info(name.unwrap_or_default(), kind);
        if info.name.is_empty() {
            if let Some(generic) = self.heap.generic_info(function) {
                info.name = self.repr(&self.root(generic.name))?;
            } else if let Some(name) = crate::vm::Primitive::compiled_name(function) {
                info.name = name.to_owned();
            }
        }
        info.lambda_list = crate::vm::lambda_list_word(self, function)
            .and_then(|list| self.repr(&self.root(list)).ok());
        info.documentation = self
            .heap
            .function_documentation(function)
            .or_else(|| {
                self.heap
                    .generic_info(function)
                    .map(|generic| generic.documentation)
            })
            .and_then(|doc| self.heap.string_contents(doc));
        if let Some(code) = self.heap.function_code(function)
            && let Some(metadata) = self.heap.function_info(code)
        {
            let code = self.heap.code_store.get(metadata.code_id);
            info.source = code.source.clone();
            let at = code.debug.entry.unwrap_or(0);
            info.byte_range = code
                .debug
                .source_at(at)
                .and_then(|source| source.range.clone())
                .unwrap_or(at..at);
            if info.name.is_empty() {
                info.name = code.name.clone().unwrap_or_default();
            }
        }
        Ok(info)
    }

    pub(crate) fn record_definition_debug(
        &self,
        document: &SourceDocument,
        definitions: impl IntoIterator<Item = DefinitionDebug>,
    ) {
        for definition in definitions {
            let symbol = {
                let packages = self.packages.borrow();
                packages
                    .list()
                    .find(|&package| packages.name(package) == Some(definition.package.as_str()))
                    .and_then(|package| packages.present(package, &definition.name))
                    .map(|entry| entry.0)
            };
            let Some(symbol) = symbol else {
                continue;
            };
            let Some(attributes) = self.heap.symbol_attributes(symbol) else {
                continue;
            };
            let info = DefinitionInfo {
                name: definition.name,
                kind: definition.kind,
                source: Some(document.clone()),
                byte_range: definition.range.unwrap_or(definition.at..definition.at),
                lambda_list: definition.lambda_list,
                declared_type: None,
                documentation: None,
            };
            self.definition_sources
                .borrow_mut()
                .insert((attributes.id, definition.kind), info);
        }
    }
}

fn empty_info(name: String, kind: DefinitionKind) -> DefinitionInfo {
    DefinitionInfo {
        name,
        kind,
        source: None,
        byte_range: 0..0,
        lambda_list: None,
        declared_type: None,
        documentation: None,
    }
}

fn merge_origin(info: &mut DefinitionInfo, origin: &DefinitionInfo) {
    // A saved function object continues to report its own older snapshot.
    if (info.kind == DefinitionKind::GenericFunction && info.source.is_none())
        || info.source.as_ref().is_some_and(|source| {
            origin
                .source
                .as_ref()
                .is_some_and(|origin| same_revision_content(origin, source))
        })
    {
        if info.source.is_none() {
            info.source = origin.source.clone();
        }
        info.byte_range = origin.byte_range.clone();
        if let Some(lambda_list) = &origin.lambda_list {
            info.lambda_list = Some(lambda_list.clone());
        }
    }
}

fn same_revision_content(left: &SourceDocument, right: &SourceDocument) -> bool {
    left == right
        || (left.path() == right.path()
            && left.uri() == right.uri()
            && left.version() == right.version()
            && left.base_offset() == right.base_offset()
            && left.text().is_some()
            && left.text() == right.text())
}

pub(crate) fn definition_debug(
    runtime: &Runtime,
    form: &Form,
) -> Result<Vec<DefinitionDebug>, EvalError> {
    if let Some([operator, body @ ..]) = form.list() {
        let forms = match operator.name().map(Name::operator) {
            Some("PROGN" | "LOCALLY") => Some(body),
            Some("MACROLET" | "SYMBOL-MACROLET") => body.get(1..),
            _ => None,
        };
        if let Some(forms) = forms {
            let mut definitions = Vec::new();
            for form in forms {
                definitions.extend(definition_debug(runtime, form)?);
            }
            return Ok(definitions);
        }
    }
    let Some([operator, target, rest @ ..]) = form.list() else {
        return Ok(Vec::new());
    };
    let kind = match operator.name().map(Name::operator) {
        Some("DEFUN") => DefinitionKind::Function,
        Some("DEFMACRO") => DefinitionKind::Macro,
        Some("DEFGENERIC") => DefinitionKind::GenericFunction,
        Some("DEFCLASS" | "DEFSTRUCT" | "DEFINE-CONDITION") => DefinitionKind::Class,
        Some("DEFVAR" | "DEFPARAMETER") => DefinitionKind::Variable,
        Some("DEFCONSTANT") => DefinitionKind::Constant,
        _ => return Ok(Vec::new()),
    };
    let target = if kind == DefinitionKind::Class && target.name().is_none() {
        target
            .list()
            .and_then(|list| list.first())
            .unwrap_or(target)
    } else {
        target
    };
    let Some(name) = target.name() else {
        return Ok(Vec::new());
    };
    let Some(package) = name
        .package()
        .and_then(|package| runtime.packages.borrow().name(package).map(str::to_owned))
    else {
        return Ok(Vec::new());
    };
    let lambda_list = if matches!(
        kind,
        DefinitionKind::Function | DefinitionKind::Macro | DefinitionKind::GenericFunction
    ) {
        rest.first()
            .map(|parameters| {
                crate::form_value::to_value(runtime, parameters)
                    .and_then(|value| runtime.repr(&value))
            })
            .transpose()?
    } else {
        None
    };
    Ok(vec![DefinitionDebug {
        package,
        name: name.spelling().to_owned(),
        kind,
        at: form.at,
        range: form.source.range.clone(),
        lambda_list,
    }])
}

impl DefinitionInfo {
    pub(crate) fn write_image(
        &self,
        writer: &mut crate::image::ImageWriter<'_>,
    ) -> Result<(), EvalError> {
        writer.string(&self.name);
        writer.usize(match self.kind {
            DefinitionKind::Function => 0,
            DefinitionKind::Macro => 1,
            DefinitionKind::Method => 2,
            DefinitionKind::Class => 3,
            DefinitionKind::Variable => 4,
            DefinitionKind::Constant => 5,
            DefinitionKind::GenericFunction => 6,
        });
        writer.bool(self.source.is_some());
        if let Some(source) = &self.source {
            let path = source
                .path()
                .map(|path| {
                    path.to_str()
                        .ok_or_else(|| EvalError::new(0, "image source path must be UTF-8"))
                })
                .transpose()?;
            write_optional_string(writer, path);
            write_optional_string(writer, source.text());
            write_optional_string(writer, source.uri());
            writer.bool(source.version().is_some());
            if let Some(version) = source.version() {
                writer.string(&version.to_string());
            }
            writer.usize(source.base_offset());
        }
        writer.usize(self.byte_range.start);
        writer.usize(self.byte_range.end);
        write_optional_string(writer, self.lambda_list.as_deref());
        write_optional_string(writer, self.declared_type.as_deref());
        write_optional_string(writer, self.documentation.as_deref());
        Ok(())
    }

    pub(crate) fn read_image(
        reader: &mut crate::image::ImageReader<'_>,
    ) -> Result<Self, EvalError> {
        let name = reader.string()?;
        let kind = match reader.usize()? {
            0 => DefinitionKind::Function,
            1 => DefinitionKind::Macro,
            2 => DefinitionKind::Method,
            3 => DefinitionKind::Class,
            4 => DefinitionKind::Variable,
            5 => DefinitionKind::Constant,
            6 => DefinitionKind::GenericFunction,
            _ => return Err(crate::image::invalid()),
        };
        let source = if reader.bool()? {
            let path = read_optional_string(reader)?.map(std::path::PathBuf::from);
            let text = read_optional_string(reader)?;
            let uri = read_optional_string(reader)?;
            let version = if reader.bool()? {
                Some(
                    reader
                        .string()?
                        .parse::<i64>()
                        .map_err(|_| crate::image::invalid())?,
                )
            } else {
                None
            };
            let base = reader.usize()?;
            let mut source = SourceDocument::new(path, text).with_base_offset(base);
            if let Some(uri) = uri {
                source = source.with_uri(uri);
            }
            if let Some(version) = version {
                source = source.with_version(version);
            }
            Some(source)
        } else {
            None
        };
        let byte_range = reader.usize()?..reader.usize()?;
        if byte_range.start > byte_range.end
            || source
                .as_ref()
                .is_some_and(|source| source.span(byte_range.clone()).is_err())
        {
            return Err(crate::image::invalid());
        }
        Ok(Self {
            name,
            kind,
            source,
            byte_range,
            lambda_list: read_optional_string(reader)?,
            declared_type: read_optional_string(reader)?,
            documentation: read_optional_string(reader)?,
        })
    }
}

fn write_optional_string(writer: &mut crate::image::ImageWriter<'_>, value: Option<&str>) {
    writer.bool(value.is_some());
    if let Some(value) = value {
        writer.string(value);
    }
}
fn read_optional_string(
    reader: &mut crate::image::ImageReader<'_>,
) -> Result<Option<String>, EvalError> {
    if reader.bool()? {
        Ok(Some(reader.string()?))
    } else {
        Ok(None)
    }
}

/// Split symbol tokens without consulting the Lisp readtable or executing code.
fn symbol_tokens(source: &str) -> Result<Vec<&str>, EvalError> {
    let mut tokens = Vec::new();
    let mut start = None;
    let mut multiple_escape = false;
    let mut escaped = false;
    for (index, ch) in source.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            start.get_or_insert(index);
            escaped = true;
            continue;
        }
        if ch == '|' {
            start.get_or_insert(index);
            multiple_escape = !multiple_escape;
            continue;
        }
        if !multiple_escape && ch.is_whitespace() {
            if let Some(start) = start.take() {
                tokens.push(&source[start..index]);
            }
        } else {
            if !multiple_escape && matches!(ch, '(' | ')' | ';' | '"' | '\'' | '`' | ',') {
                return Err(EvalError::new(index, "expected escaped symbol syntax"));
            }
            start.get_or_insert(index);
        }
    }
    if escaped || multiple_escape {
        return Err(EvalError::new(source.len(), "unterminated symbol escape"));
    }
    if let Some(start) = start {
        tokens.push(&source[start..]);
    }
    Ok(tokens)
}
