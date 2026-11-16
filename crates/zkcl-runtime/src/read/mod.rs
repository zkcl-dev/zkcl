mod image;
use std::borrow::Cow;
use std::cell::RefCell;
use std::mem::DropGuard;
use std::ops::Range;
use std::rc::Rc;

use crate::heap::{ReadtableCase, RootSlot, Word};
use crate::symbol::{Name, Package, SymbolTokenBuffer};
use crate::{EvalError, Root, Runtime};
use crate::{form_value, vm};
use zkcl_syntax::syntax::SyntaxKind;

mod array;
mod features;
mod input;
use input::ReaderInput;
pub(crate) use input::{StreamInput, StreamRead};
mod labels;
mod lexical;
mod numbers;
mod quasiquote;
mod readtable;
mod structure;

pub(crate) use numbers::numeric_or_reserved;
pub(crate) use quasiquote::MARKERS as QUASIQUOTE_MARKERS;
pub(crate) use readtable::{
    BuiltinMacro, CharSyntax, MacroDefinition, builtin_function, dispatch_function,
    effective_syntax, macro_definition, macro_function, syntax_keyword,
};

const MAX_NESTING: usize = 256;

pub(crate) fn raw_strings_enabled(runtime: &Runtime) -> bool {
    let reader = Reader::new(runtime, "");
    let Some(MacroDefinition::Dispatch { entries, .. }) = reader
        .readtable()
        .ok()
        .and_then(|table| macro_definition(runtime, table, '#').ok().flatten())
    else {
        return false;
    };
    let Some(function) = dispatch_function(runtime, entries, '>') else {
        return false;
    };
    runtime
        .packages
        .borrow()
        .present(Package::ZK, "%READ-RAW-STRING")
        .is_some_and(|(symbol, _)| runtime.heap.symbol_cell(symbol, true) == Some(function))
}

pub(crate) fn custom_readtable(runtime: &Runtime) -> bool {
    let Ok(table) = Reader::new(runtime, "").readtable() else {
        return true;
    };
    if runtime.heap.readtable_case(table) != Some(ReadtableCase::Upcase)
        || runtime.heap.readtable_syntax(table) != Some(Word::NIL)
    {
        return true;
    }
    let Some(macros) = runtime.heap.readtable_macros(table) else {
        return true;
    };
    if macros == Word::NIL {
        return false;
    }
    if !raw_strings_enabled(runtime) {
        return true;
    }
    // The only custom syntax understood by the editor is an otherwise
    // standard `#` dispatch table with exactly the raw-string entry.
    let Some((stored, Word::NIL)) = runtime.heap.cons_fields(macros) else {
        return true;
    };
    let Some((ch, _)) = runtime.heap.cons_fields(stored) else {
        return true;
    };
    if ch != Word::character('#') {
        return true;
    }
    match macro_definition(runtime, table, '#') {
        Ok(Some(MacroDefinition::Dispatch {
            entries,
            standard: true,
            ..
        })) => !matches!(runtime.heap.cons_fields(entries), Some((_, Word::NIL))),
        _ => true,
    }
}

impl Runtime {
    /// Whether the live reader bindings allow decimal, unsuppressed source analysis.
    /// The editor also checks [`Self::custom_readtable`] for unsupported readtable syntax.
    pub fn standard_source_reader_bindings(&self) -> bool {
        let value = |name| features::special_value(self, name, 0).ok();
        if value("*READ-BASE*") != Word::fixnum(10) || value("*READ-SUPPRESS*") != Some(Word::NIL) {
            return false;
        }
        let Some(float_format) = value("*READ-DEFAULT-FLOAT-FORMAT*") else {
            return false;
        };
        ["SHORT-FLOAT", "SINGLE-FLOAT", "DOUBLE-FLOAT", "LONG-FLOAT"]
            .into_iter()
            .any(|name| self.find_common_lisp(name) == Some(float_format))
    }
}

#[derive(Clone, Copy, Default)]
struct ReadNesting {
    depth: usize,
    backquotes: usize,
}

pub(crate) struct ReadContext {
    nesting: ReadNesting,
    preserve: bool,
    labels: Rc<RefCell<labels::Labels>>,
}

#[derive(Clone)]
pub(crate) struct Form {
    pub(crate) at: usize,
    pub(crate) kind: FormKind,
    original_object: Option<FormObject>,
    pub(crate) source: FormSource,
}

pub(crate) use zkcl_bytecode::{SourceLocation as FormSource, SourceOrigin as FormOrigin};

#[derive(Clone)]
enum FormObject {
    /// Convert the current object to syntax before compiling it as code.
    NeedsSyntax(Rc<RootSlot>),
    /// The form already has syntax. Retain the object for literal contexts.
    WithSyntax(Rc<RootSlot>),
}

#[derive(Clone)]
pub(crate) enum FormKind {
    Integer(isize),
    Character(char),
    String(String),
    Literal(std::rc::Rc<crate::heap::RootSlot>),
    Name(Name),
    List(Vec<Form>, Option<Box<Form>>),
}

impl Form {
    pub(crate) fn new(at: usize, kind: FormKind) -> Self {
        Self {
            at,
            kind,
            original_object: None,
            source: FormSource {
                at,
                ..FormSource::default()
            },
        }
    }

    pub(crate) fn with_reader_range(mut self, range: Range<usize>, generated: bool) -> Self {
        if generated {
            self.generated_from(FormSource {
                at: range.start,
                range: Some(range),
                origin: FormOrigin::Reader,
                expansion: Vec::new(),
            });
        } else {
            self.source = FormSource {
                at: self.at,
                range: Some(range.clone()),
                origin: FormOrigin::Original,
                expansion: Vec::new(),
            };
            // Abbreviations and quasiquote can synthesize children. Attribute only
            // those children to the reader; already read operands keep their spans.
            self.attribute_generated_children(&range);
        }
        self
    }

    fn attribute_generated_children(&mut self, range: &Range<usize>) {
        if self.source.range.is_none() {
            self.source = FormSource {
                at: self.at,
                range: Some(range.clone()),
                origin: FormOrigin::Reader,
                expansion: Vec::new(),
            };
        }
        if let FormKind::List(items, tail) = &mut self.kind {
            for child in items.iter_mut().chain(tail.iter_mut().map(Box::as_mut)) {
                if child.source.range.is_none() {
                    child.attribute_generated_children(range);
                }
            }
        }
    }

    pub(crate) fn generated_from(&mut self, mut source: FormSource) {
        self.at = source.range.as_ref().map_or(self.at, |range| range.start);
        source.at = self.at;
        self.source = source.clone();
        if let FormKind::List(items, tail) = &mut self.kind {
            for child in items.iter_mut().chain(tail.iter_mut().map(Box::as_mut)) {
                child.generated_from(source.clone());
            }
        }
    }

    pub(crate) fn macro_expansion(mut self, call: &Form) -> Self {
        let mut source = call.source.clone();
        if let Some(range) = &source.range {
            source.expansion.push(range.clone());
        }
        source.origin = FormOrigin::Macro;
        self.generated_from(source);
        self
    }

    pub(crate) fn source_at(&self, at: usize) -> Option<&FormSource> {
        if let FormKind::List(items, tail) = &self.kind {
            for child in items.iter().chain(tail.iter().map(Box::as_ref)) {
                if let Some(source) = child.source_at(at) {
                    return Some(source);
                }
            }
        }
        (self.at == at && self.source.range.is_some()).then_some(&self.source)
    }

    /// Executable forms use syntax. Literal contexts reuse the Lisp object
    /// supplied by the reader or preserved while converting a Lisp form.
    pub(crate) fn original_object(&self) -> Option<&Rc<RootSlot>> {
        self.original_object.as_ref().map(|value| match value {
            FormObject::NeedsSyntax(root) | FormObject::WithSyntax(root) => root,
        })
    }

    pub(crate) fn object_needing_syntax(&self) -> Option<&Rc<RootSlot>> {
        match &self.original_object {
            Some(FormObject::NeedsSyntax(root)) => Some(root),
            _ => None,
        }
    }

    pub(crate) fn retain_object(mut self, root: Rc<RootSlot>) -> Self {
        self.original_object = Some(FormObject::WithSyntax(root));
        self
    }

    pub(crate) fn name(&self) -> Option<&Name> {
        match &self.kind {
            FormKind::Name(name) => Some(name),
            _ => None,
        }
    }

    pub(crate) fn list(&self) -> Option<&[Self]> {
        match &self.kind {
            FormKind::List(items, None) => Some(items),
            FormKind::Name(name) if name.operator() == "NIL" => Some(&[]),
            _ => None,
        }
    }
}

pub(crate) fn read(runtime: &Runtime, source: &str) -> Result<Form, EvalError> {
    let mut reader = Reader::new(runtime, source);
    let mode = reader.mode_from_read_suppress()?;
    let form = reader
        .read_next(mode, ReadNesting::default())?
        .ok_or_else(|| reader.unexpected_eof("expected a form"))?;
    if let Some(token) = reader.next_object_token(0, 0, mode)? {
        return Err(token.error("expected exactly one form"));
    }
    Ok(form)
}

/// Host registrations accept a standard symbol token in the current package.
/// Reading host names never invokes dispatch or a reader macro.
pub(crate) fn read_symbol_name(
    runtime: &Runtime,
    source: &str,
) -> Result<(Name, usize), EvalError> {
    let mut reader = Reader::new(runtime, source);
    let token = reader
        .next_token(ReadMode::Normal, ReadNesting::default())?
        .ok_or_else(|| EvalError::read(reader.at, "expected a symbol name"))?;
    if token.kind != SyntaxKind::Atom || token.text.starts_with('#') {
        return Err(token.error("expected a symbol name"));
    }
    if let Some(extra) = reader.next_token(ReadMode::Normal, ReadNesting::default())? {
        return Err(extra.error("expected exactly one symbol name"));
    }
    Ok((Name::read(runtime, &token.text, token.at)?, token.at))
}

struct Token<'a> {
    kind: SyntaxKind,
    /// Text used for numeric and symbol semantics: escapes installed by the
    /// readtable are spelled the standard way, and dispatch prefixes are
    /// normalized to '#'.
    text: Cow<'a, str>,
    end: usize,
    at: usize,
    evaluated: Option<Box<Form>>,
}

impl Token<'_> {
    fn error(&self, message: &str) -> EvalError {
        EvalError::read_span(self.at..self.end, message)
    }

    fn dispatch(&self) -> Option<u8> {
        dispatch_character(&self.text)
    }

    fn has_dispatch_argument(&self) -> bool {
        self.text.as_bytes().get(1).is_some_and(u8::is_ascii_digit)
    }

    fn is_dot(&self) -> bool {
        self.kind == SyntaxKind::Atom && self.text == "."
    }
}

fn dispatch_character(text: &str) -> Option<u8> {
    text.strip_prefix('#')?
        .bytes()
        .find(|ch| !ch.is_ascii_digit())
        .map(|ch| ch.to_ascii_uppercase())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReadMode {
    Normal,
    Suppressed,
}

fn label_number(token: &Token<'_>) -> Option<u64> {
    let rest = token.text.strip_prefix('#')?;
    let digits = rest.get(..rest.len().checked_sub(1)?)?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

pub(crate) type ReadCall<'a> = dyn Fn(&Root<'_>, &[&Root<'_>], Option<(Word, Word)>, usize) -> Result<Vec<RootSlot>, EvalError>
    + 'a;

pub(crate) fn reader_call<'rt>(
    runtime: &'rt Runtime,
    calls: Option<&ReadCall<'_>>,
    function: &Root<'_>,
    arguments: &[&Root<'_>],
    binding: Option<(Word, Word)>,
    at: usize,
) -> Result<Vec<Root<'rt>>, EvalError> {
    let Some(call) = calls else {
        let invoke = || vm::call_host(runtime, function, arguments);
        return match binding {
            Some(binding) => vm::with_special_bindings(runtime, [binding], invoke),
            None => invoke(),
        };
    };
    let values = call(function, arguments, binding, at)?;
    Ok(values
        .into_iter()
        .map(|slot| Root {
            owner: runtime,
            slot,
        })
        .collect())
}

/// Reuse reader storage between top-level forms when the VM owns the source.
/// The active readtable is checked again for every form.
#[derive(Default)]
pub(crate) struct SourceReadCursor {
    offset: usize,
    table: Option<RootSlot>,
    symbol_token: SymbolTokenBuffer,
}

impl SourceReadCursor {
    pub(crate) fn next_form(
        &mut self,
        runtime: &Runtime,
        source: &str,
    ) -> Result<Option<Form>, EvalError> {
        let mut reader = Reader::resume_at(runtime, source, self.offset);
        reader.table = self.table.take();
        reader.symbol_token = std::mem::take(&mut self.symbol_token);
        let form = reader.next_form();
        self.offset = reader.at;
        self.table = reader.table;
        self.symbol_token = reader.symbol_token;
        form
    }
}

pub(crate) struct Reader<'a> {
    runtime: &'a Runtime,
    calls: Option<&'a ReadCall<'a>>,
    input: ReaderInput<'a>,
    at: usize,
    package: Option<Package>,
    // Only labels and calls into a Lisp reader macro need a shared context.
    labels: Option<Rc<RefCell<labels::Labels>>>,
    preserve_whitespace: bool,
    macro_consumed_input: bool,
    /// READ-DELIMITED-LIST stops only between objects at its own nesting depth.
    delimiter: Option<(usize, char)>,
    /// Rooted readtable cached for the active token scan.
    table: Option<RootSlot>,
    table_needs_sync: bool,
    bindings: Option<ReadBindings>,
    symbol_token: SymbolTokenBuffer,
    submission: bool,
}

#[derive(Default)]
struct ReadBindings {
    package: Option<Package>,
    base: Option<u32>,
    // Present when the table has standard character and macro syntax. Keep
    // its case mode alongside it, since Lisp can change that independently.
    standard_case: Option<ReadtableCase>,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(runtime: &'a Runtime, source: &'a str) -> Self {
        Self::resume_at(runtime, source, 0)
    }

    pub(crate) fn stream(runtime: &'a Runtime, stream: &'a mut dyn StreamInput) -> Self {
        Self::with_input(runtime, ReaderInput::Stream(stream), 0)
    }

    pub(crate) fn submission(runtime: &'a Runtime, stream: &'a mut dyn StreamInput) -> Self {
        let mut reader = Self::stream(runtime, stream);
        reader.submission = true;
        reader
    }

    pub(crate) fn with_calls(mut self, calls: Option<&'a ReadCall<'a>>) -> Self {
        self.calls = calls;
        self
    }

    pub(crate) fn resume_at(runtime: &'a Runtime, source: &'a str, at: usize) -> Self {
        Self::with_input(runtime, ReaderInput::Text(source), at)
    }

    fn with_input(runtime: &'a Runtime, input: ReaderInput<'a>, at: usize) -> Self {
        let cache_bindings = match &input {
            ReaderInput::Text(_) => true,
            ReaderInput::Stream(input) => !input.refill_invokes_callbacks(),
        };
        let bindings = cache_bindings.then(ReadBindings::default);
        Self {
            runtime,
            calls: None,
            input,
            at,
            package: None,
            labels: None,
            preserve_whitespace: false,
            macro_consumed_input: false,
            delimiter: None,
            table: None,
            table_needs_sync: true,
            bindings,
            symbol_token: SymbolTokenBuffer::default(),
            submission: false,
        }
    }

    fn readtable(&self) -> Result<Word, EvalError> {
        let (symbol, _) = self
            .runtime
            .packages
            .borrow()
            .present(Package::COMMON_LISP, "*READTABLE*")
            .ok_or_else(|| {
                EvalError::read(
                    self.input.location(self.at),
                    "unbound reader variable *READTABLE*",
                )
            })?;
        self.runtime.heap.symbol_cell(symbol, false).ok_or_else(|| {
            EvalError::read(
                self.input.location(self.at),
                "unbound reader variable *READTABLE*",
            )
        })
    }

    fn sync_readtable(&mut self) -> Result<(), EvalError> {
        let table = self.readtable()?;
        if self.table.as_ref().map(RootSlot::get) != Some(table) {
            self.table = Some(self.runtime.root_slot(table));
        }
        if let Some(bindings) = &mut self.bindings {
            bindings.standard_case = self.runtime.heap.standard_readtable_case(table);
        }
        self.table_needs_sync = false;
        Ok(())
    }

    fn current_table(&self) -> Word {
        self.table.as_ref().expect("synced readtable").get()
    }

    fn is_standard_readtable(&self, table: Word) -> bool {
        self.runtime.heap.standard_readtable_case(table).is_some()
    }

    fn standard_case(&self) -> Option<ReadtableCase> {
        self.bindings.as_ref()?.standard_case
    }

    pub(crate) fn next_form(&mut self) -> Result<Option<Form>, EvalError> {
        self.next_form_for_read(false, false)
    }

    pub(crate) fn next_form_for_read(
        &mut self,
        recursive: bool,
        preserve: bool,
    ) -> Result<Option<Form>, EvalError> {
        let nesting = self.begin_read(recursive, preserve);
        self.read_next(self.mode_from_read_suppress()?, nesting)
    }

    fn begin_read(&mut self, recursive: bool, preserve: bool) -> ReadNesting {
        self.invalidate_read_bindings();
        if recursive && let Some(context) = self.runtime.reader_macro_context.borrow().as_ref() {
            self.labels = Some(Rc::clone(&context.labels));
            self.preserve_whitespace = context.preserve;
            return context.nesting;
        }
        self.labels = None;
        self.preserve_whitespace = preserve;
        ReadNesting::default()
    }

    fn invalidate_read_bindings(&mut self) {
        self.table_needs_sync = true;
        if let Some(bindings) = &mut self.bindings {
            *bindings = ReadBindings::default();
        }
    }

    fn symbol_package(&mut self, at: usize) -> Result<Package, EvalError> {
        if let Some(package) = self.package {
            return Ok(package);
        }
        let current = || self.runtime.current_package_at(at);
        let Some(bindings) = &mut self.bindings else {
            return current();
        };
        if let Some(package) = bindings.package {
            return Ok(package);
        }
        let package = current()?;
        bindings.package = Some(package);
        Ok(package)
    }

    fn read_base(&mut self, token: &Token<'_>) -> Result<u32, EvalError> {
        let current = || numbers::read_base(self.runtime, token);
        let Some(bindings) = &mut self.bindings else {
            return current();
        };
        if let Some(base) = bindings.base {
            return Ok(base);
        }
        let base = current()?;
        bindings.base = Some(base);
        Ok(base)
    }

    pub(crate) fn read_position(&mut self, preserve_whitespace: bool) -> usize {
        if preserve_whitespace || self.preserve_whitespace || self.macro_consumed_input {
            return self.at;
        }
        if self.at == self.input.text().len()
            && let ReaderInput::Stream(input) = &mut self.input
        {
            input.refill_available(self.runtime, self.at);
        }
        let following = self.input.text()[self.at..].chars().next();
        let is_ws = |ch: char| {
            if let Some(table) = self.table.as_ref() {
                effective_syntax(self.runtime, table.get(), ch)
                    .is_ok_and(|syntax| syntax == CharSyntax::Whitespace)
            } else {
                matches!(ch, ' ' | '\t' | '\n' | '\r' | '\x0c')
            }
        };
        self.at + following.filter(|&ch| is_ws(ch)).map_or(0, char::len_utf8)
    }

    fn mode_from_read_suppress(&self) -> Result<ReadMode, EvalError> {
        let suppress = features::special_value(
            self.runtime,
            "*READ-SUPPRESS*",
            self.input.location(self.at),
        )?;
        Ok(if suppress == Word::NIL {
            ReadMode::Normal
        } else {
            ReadMode::Suppressed
        })
    }

    fn read_next(
        &mut self,
        mode: ReadMode,
        nesting: ReadNesting,
    ) -> Result<Option<Form>, EvalError> {
        self.next_object_token(nesting.depth, nesting.backquotes, mode)?
            .map(|token| self.form_in_mode(token, nesting, mode))
            .transpose()
    }

    fn form_in_mode(
        &mut self,
        token: Token<'a>,
        nesting: ReadNesting,
        mode: ReadMode,
    ) -> Result<Form, EvalError> {
        let ReadNesting { depth, backquotes } = nesting;
        match mode {
            ReadMode::Normal => self.form(token, depth, backquotes),
            ReadMode::Suppressed => {
                let at = token.at;
                self.skip_from_token(token, depth, backquotes)?;
                Ok(Form::new(at, nil()))
            }
        }
    }

    fn next_token(
        &mut self,
        mode: ReadMode,
        nesting: ReadNesting,
    ) -> Result<Option<Token<'a>>, EvalError> {
        self.next_token_with_pending(mode, nesting, false)
    }

    fn next_token_with_pending(
        &mut self,
        mode: ReadMode,
        nesting: ReadNesting,
        pending: bool,
    ) -> Result<Option<Token<'a>>, EvalError> {
        // Text and passive built-in input run Lisp only through reader callbacks.
        // Other stream refills can invoke host code and need fresh bindings.
        if self.table_needs_sync || self.bindings.is_none() {
            self.sync_readtable()?;
        }
        loop {
            self.macro_consumed_input = false;
            if let ReaderInput::Text(text) = self.input
                && self.standard_case().is_some()
            {
                return self.next_standard_text_token(text, mode, nesting);
            }
            let at = self.at;
            if self.input.text().len() == at {
                if self.submission && nesting.depth == 0 && !pending {
                    if let ReaderInput::Stream(input) = &mut self.input {
                        input.refill_available(self.runtime, at);
                    }
                } else {
                    self.input.refill(self.runtime, at);
                }
            }
            let Some(ch) = self.input.text()[at..].chars().next() else {
                return Ok(None);
            };
            if self.delimiter == Some((nesting.depth, ch)) {
                return Ok(None);
            }
            let table = self.current_table();
            let standard_syntax = self.bindings.as_ref().map_or_else(
                || self.is_standard_readtable(table),
                |bindings| bindings.standard_case.is_some(),
            );
            // An invalid character is a read error wherever the reader meets
            // it as input, not merely when it starts a token.
            let syntax = if standard_syntax {
                readtable::default_syntax(ch)
            } else {
                effective_syntax(self.runtime, table, ch)?
            };
            if syntax == CharSyntax::Invalid {
                return Err(EvalError::read(
                    self.input.location(at),
                    "invalid character in input",
                ));
            }
            let mut builtin = None;
            if !standard_syntax && syntax == CharSyntax::Macro {
                let after = at + ch.len_utf8();
                let invocation = match self.macro_definition_at(ch)? {
                    Some(MacroDefinition::Builtin(kind)) => {
                        builtin = Some(kind);
                        None
                    }
                    Some(MacroDefinition::Custom { function, .. }) => {
                        Some((function, after, ch, None))
                    }
                    Some(MacroDefinition::Dispatch {
                        entries,
                        standard: standard_dispatch,
                        ..
                    }) => {
                        let (digits, sub, end) = self.dispatch_prefix(after)?;
                        if let Some(function) = dispatch_function(self.runtime, entries, sub) {
                            Some((function, end, sub, Some(digits)))
                        } else if !standard_dispatch {
                            return Err(EvalError::read(
                                self.input.location(at),
                                format!("no dispatch macro function for {sub:?}"),
                            ));
                        } else {
                            builtin = Some(BuiltinMacro::Dispatch);
                            None
                        }
                    }
                    None => unreachable!("macro syntax has a definition"),
                };
                if let Some((function, start, ch, digits)) = invocation {
                    if let Some(token) =
                        self.call_macro(function, start, ch, digits.as_deref(), mode, nesting)?
                    {
                        return Ok(Some(token));
                    }
                    self.sync_readtable()?;
                    continue;
                }
            }
            if let Some(token) =
                self.scan_buffered_token(table, syntax, standard_syntax, builtin, mode)?
                && !(token.kind == SyntaxKind::UnknownDispatch && mode == ReadMode::Suppressed)
            {
                return Ok(Some(token));
            }
        }
    }

    fn macro_definition_at(&self, ch: char) -> Result<Option<MacroDefinition>, EvalError> {
        macro_definition(self.runtime, self.current_table(), ch)
    }

    fn call_macro(
        &mut self,
        function: Word,
        start: usize,
        ch: char,
        digits: Option<&str>,
        mode: ReadMode,
        nesting: ReadNesting,
    ) -> Result<Option<Token<'a>>, EvalError> {
        let at = self.input.location(self.at);
        if nesting.depth >= MAX_NESTING {
            return Err(EvalError::read(at, "form nesting exceeds the reader limit"));
        }
        let function = self.runtime.root(function);
        let numeric = digits
            .map(|digits| {
                if digits.is_empty() {
                    return Ok(self.runtime.nil());
                }
                let number = num_bigint::BigInt::parse_bytes(digits.as_bytes(), 10)
                    .expect("validated decimal dispatch argument");
                self.runtime
                    .make_integer(&number)
                    .map_err(|error| EvalError::out_of_memory(at, error))
            })
            .transpose()?;
        let runtime = self.runtime;
        let stream = match &self.input {
            ReaderInput::Stream(input) => input.stream(),
            ReaderInput::Text(source) => {
                let rest = &source[start..];
                let contents = self.runtime.root_slot(
                    self.runtime
                        .heap
                        .string(rest)
                        .map_err(|error| EvalError::out_of_memory(at, error))?,
                );
                self.runtime
                    .heap
                    .stream(
                        crate::heap::StreamKind::StringInput,
                        crate::heap::StreamElement::Character,
                        0,
                        rest.len(),
                        || contents.get(),
                    )
                    .map_err(|error| EvalError::out_of_memory(at, error))?
            }
        };
        let stream = self.runtime.root(stream);
        let character = self.runtime.character(ch);
        let labels = Rc::clone(self.labels.get_or_insert_with(Default::default));
        let slot = &self.runtime.reader_macro_context;
        let previous = slot.replace(Some(ReadContext {
            nesting: ReadNesting {
                depth: nesting.depth + 1,
                backquotes: nesting.backquotes,
            },
            preserve: self.preserve_whitespace,
            labels,
        }));
        let _scope = DropGuard::new((slot, previous), |(slot, previous)| {
            slot.replace(previous);
        });
        let calls = self.calls;
        let suppress =
            mode == ReadMode::Suppressed && self.mode_from_read_suppress()? == ReadMode::Normal;
        let values = self.call_at_cursor(start, at, || {
            let mut args = vec![&stream, &character];
            args.extend(numeric.iter());
            let binding = if suppress {
                Some((
                    runtime.intern(&Name::common_lisp("*READ-SUPPRESS*"), at)?,
                    Word::T,
                ))
            } else {
                None
            };
            reader_call(runtime, calls, &function, &args, binding, at)
        })?;
        if matches!(self.input, ReaderInput::Text(_)) {
            self.at = start
                + runtime
                    .heap
                    .stream_info(stream.slot.get())
                    .expect("reader macro stream")
                    .cursor;
        }
        self.macro_consumed_input = true;
        Ok(values.into_iter().next().map(|value| Token {
            kind: SyntaxKind::Atom,
            text: Cow::Borrowed(""),
            end: at,
            at,
            evaluated: (mode == ReadMode::Normal).then(|| {
                Box::new(evaluated_form(
                    self.runtime,
                    value,
                    at,
                    nesting.backquotes != 0,
                ))
            }),
        }))
    }

    fn call_at_cursor<T>(
        &mut self,
        cursor: usize,
        at: usize,
        call: impl FnOnce() -> Result<T, EvalError>,
    ) -> Result<T, EvalError> {
        self.invalidate_read_bindings();
        let ReaderInput::Stream(input) = &mut self.input else {
            return call();
        };
        input
            .commit(self.runtime, cursor, at)
            .map_err(|fault| fault.host)?;
        let result = call();
        self.at = 0;
        let resumed = input.resume(self.runtime, at);
        let value = result?;
        resumed.map_err(|fault| fault.host)?;
        Ok(value)
    }

    fn next_object_token(
        &mut self,
        depth: usize,
        backquotes: usize,
        mode: ReadMode,
    ) -> Result<Option<Token<'a>>, EvalError> {
        let nesting = ReadNesting { depth, backquotes };
        while let Some(token) = self.next_token(mode, nesting)? {
            if let Some(token) = self.object_token(token, nesting, mode)? {
                return Ok(Some(token));
            }
        }
        Ok(None)
    }

    fn object_token(
        &mut self,
        mut token: Token<'a>,
        nesting: ReadNesting,
        mode: ReadMode,
    ) -> Result<Option<Token<'a>>, EvalError> {
        let ReadNesting { depth, backquotes } = nesting;
        // A selected conditional is a prefix on the next object. Read a
        // chain of prefixes iteratively at the same structural depth.
        let mut needs_operand = false;
        loop {
            if token.kind == SyntaxKind::ReaderConditional {
                if depth >= MAX_NESTING {
                    return Err(token.error("form nesting exceeds the reader limit"));
                }
                if mode == ReadMode::Normal && token.has_dispatch_argument() {
                    return Err(token.error("reader dispatch does not accept a numeric argument"));
                }
                let enabled = self.feature_test(depth + 1, backquotes)?;
                if enabled == token.text.ends_with('+') {
                    needs_operand = true;
                } else {
                    self.skip_required_form(depth + 1, backquotes)?;
                }
            } else {
                let skipped = if token.kind == SyntaxKind::DispatchPrefix
                    && mode == ReadMode::Normal
                    && token.dispatch() == Some(b'.')
                {
                    token.evaluated = self.read_eval(&token, depth)?.map(Box::new);
                    token.evaluated.is_none()
                } else {
                    mode != ReadMode::Normal
                        && token.kind == SyntaxKind::DispatchPrefix
                        && token.text.ends_with('=')
                };
                if !skipped {
                    if needs_operand && token.kind == SyntaxKind::CloseParen {
                        return Err(token.error("unexpected )"));
                    }
                    if needs_operand && mode == ReadMode::Normal && token.is_dot() {
                        return Err(token.error("unexpected dot"));
                    }
                    return Ok(Some(token));
                }
            }
            if !needs_operand {
                return Ok(None);
            }
            token = self
                .next_token_with_pending(mode, nesting, true)?
                .ok_or_else(|| self.unexpected_eof("expected a form after reader conditional"))?;
        }
    }

    fn read_eval(&mut self, token: &Token<'_>, depth: usize) -> Result<Option<Form>, EvalError> {
        if depth >= MAX_NESTING {
            return Err(token.error("form nesting exceeds the reader limit"));
        }
        if features::special_value(self.runtime, "*READ-EVAL*", token.at)? == Word::NIL {
            return Err(token.error("#. is disabled by *READ-EVAL*"));
        }
        let operand = self.required_form(depth + 1, 0)?;
        let runtime = self.runtime;
        let package = self.package;
        let calls = self.calls;
        let values = self.call_at_cursor(self.at, token.at, || {
            if calls.is_some() {
                let eval = runtime.intern(&Name::common_lisp("EVAL"), token.at)?;
                let eval = runtime
                    .root(vm::function_cell(runtime, eval, token.at).map_err(|fault| fault.host)?);
                let form = form_value::to_value(runtime, &operand)?;
                let binding = package.map(|package| (runtime.package_variable(), package.word()));
                return reader_call(runtime, calls, &eval, &[&form], binding, token.at);
            }
            let evaluate = || vm::eval_host(runtime, operand, None);
            if let Some(package) = package {
                vm::with_special_bindings(
                    runtime,
                    [(runtime.package_variable(), package.word())],
                    evaluate,
                )
            } else {
                evaluate()
            }
        })?;
        Ok(values
            .into_iter()
            .next()
            .map(|value| evaluated_form(self.runtime, value, token.at, false)))
    }

    fn feature_test(&mut self, depth: usize, backquotes: usize) -> Result<bool, EvalError> {
        // A nested conditional still decides how many objects to consume even
        // when its surrounding READ is suppressed.
        let previous = self.package.replace(Package::KEYWORD);
        let mut guard = DropGuard::new((self, previous), |(reader, previous)| {
            reader.package = previous;
        });
        let test = guard.0.required_syntax(depth, backquotes)?;
        let runtime = guard.0.runtime;
        drop(guard);
        features::enabled(runtime, &test)
    }

    fn unexpected_eof(&self, message: &str) -> EvalError {
        let at = self.input.location(self.at);
        EvalError::unexpected_read_eof(at..at, message)
    }

    fn required_token(
        &mut self,
        depth: usize,
        backquotes: usize,
        mode: ReadMode,
    ) -> Result<Token<'a>, EvalError> {
        self.next_object_token(depth, backquotes, mode)?
            .ok_or_else(|| self.unexpected_eof("expected a form"))
    }

    fn required_form(&mut self, depth: usize, backquotes: usize) -> Result<Form, EvalError> {
        let token = self.required_token(depth, backquotes, ReadMode::Normal)?;
        self.form(token, depth, backquotes)
    }

    fn required_syntax(&mut self, depth: usize, backquotes: usize) -> Result<Form, EvalError> {
        let form = self.required_form(depth, backquotes)?;
        if let FormKind::Literal(root) = &form.kind
            && self.runtime.heap.cons_fields(root.get()).is_some()
        {
            return form_value::from_value(self.runtime, &self.runtime.root(root.get()), form.at)
                .map(|mut decoded| {
                    let mut source = form.source.clone();
                    source.origin = FormOrigin::Reader;
                    decoded.generated_from(source);
                    decoded
                });
        }
        Ok(form)
    }

    fn skip_required_form(&mut self, depth: usize, backquotes: usize) -> Result<(), EvalError> {
        let token = self.required_token(depth, backquotes, ReadMode::Suppressed)?;
        self.skip_from_token(token, depth, backquotes)
    }

    fn skip_from_token(
        &mut self,
        token: Token<'a>,
        depth: usize,
        backquotes: usize,
    ) -> Result<(), EvalError> {
        use SyntaxKind::*;
        if depth >= MAX_NESTING {
            return Err(token.error("form nesting exceeds the reader limit"));
        }
        match token.kind {
            OpenParen | VectorStart => loop {
                match self.next_object_token(depth + 1, backquotes, ReadMode::Suppressed)? {
                    Some(Token {
                        kind: CloseParen, ..
                    }) => return Ok(()),
                    Some(token) => self.skip_from_token(token, depth + 1, backquotes)?,
                    None => return Err(self.unexpected_eof("unterminated list or vector")),
                }
            },
            CloseParen => Err(token.error("unexpected )")),
            Quote | FunctionQuote => self.skip_required_form(depth + 1, backquotes),
            Backquote => self.skip_required_form(depth + 1, backquotes + 1),
            Unquote | UnquoteSplicing | UnquoteDestructive if backquotes != 0 => {
                self.skip_required_form(depth + 1, backquotes - 1)
            }
            DispatchPrefix => {
                let backquotes = if token.text.ends_with('.') {
                    0
                } else {
                    backquotes
                };
                self.skip_required_form(depth + 1, backquotes)
            }
            _ => Ok(()),
        }
    }

    fn form(
        &mut self,
        token: Token<'a>,
        depth: usize,
        backquotes: usize,
    ) -> Result<Form, EvalError> {
        let at = token.at;
        let generated = token.evaluated.is_some() || token.kind == SyntaxKind::Backquote;
        let result = self.form_inner(token, depth, backquotes);
        let end = self.input.location(self.at);
        result.map(|form| form.with_reader_range(at..end, generated))
    }

    fn form_inner(
        &mut self,
        mut token: Token<'a>,
        depth: usize,
        backquotes: usize,
    ) -> Result<Form, EvalError> {
        if let Some(form) = token.evaluated.take() {
            return Ok(*form);
        }
        let at = token.at;
        // Reading and compiling nested forms still use the native stack.
        if depth >= MAX_NESTING {
            return Err(token.error("form nesting exceeds the reader limit"));
        }
        let kind = match token.kind {
            SyntaxKind::OpenParen => {
                let kind = self.list(depth + 1, backquotes)?;
                return finish_list(self.runtime, at, kind, backquotes);
            }
            SyntaxKind::VectorStart => return self.vector(&token, depth + 1, backquotes),
            SyntaxKind::DispatchPrefix if token.dispatch() == Some(b'A') => {
                self.ranked_array(&token, depth + 1, backquotes)?
            }
            SyntaxKind::Atom if token.dispatch() == Some(b'*') => self.bit_vector(&token)?,
            SyntaxKind::String => FormKind::String(self.decode_string(&token.text)?),
            SyntaxKind::Character => {
                if token.has_dispatch_argument() {
                    return Err(token.error("character syntax does not accept a numeric argument"));
                }
                let character = crate::character::from_token(&token.text[2..])
                    .ok_or_else(|| token.error("unknown character name"))?;
                FormKind::Character(character)
            }
            SyntaxKind::CloseParen => return Err(token.error("unexpected )")),
            SyntaxKind::Quote => self.abbreviation("QUOTE", at, depth, backquotes)?,
            SyntaxKind::Backquote => {
                let template = self.required_form(depth + 1, backquotes + 1)?;
                return quasiquote::expand(template);
            }
            SyntaxKind::Unquote | SyntaxKind::UnquoteSplicing | SyntaxKind::UnquoteDestructive => {
                if backquotes == 0 {
                    return Err(token.error("comma outside backquote"));
                }
                let splice = token.kind != SyntaxKind::Unquote;
                let operand = self.required_form(depth + 1, backquotes - 1)?;
                quasiquote::comma(at, splice, operand)
            }
            SyntaxKind::FunctionQuote => self.abbreviation("FUNCTION", at, depth, backquotes)?,
            SyntaxKind::DispatchPrefix if token.dispatch() == Some(b'P') => {
                if token.has_dispatch_argument() {
                    return Err(token.error("#P does not accept a numeric argument"));
                }
                let operand = self.required_form(depth + 1, backquotes)?;
                let text = match operand.kind {
                    FormKind::String(text) => Some(text),
                    FormKind::Literal(value) => self.runtime.heap.string_contents(value.get()),
                    _ => None,
                }
                .ok_or_else(|| EvalError::read(operand.at, "#P requires a namestring"))?;
                let defaults = crate::pathname::default_pathname(self.runtime, operand.at)
                    .map_err(|error| error.host)?;
                let path = crate::pathname::parse_namestring(
                    self.runtime,
                    &text,
                    None,
                    defaults.logical_host.as_deref(),
                )
                .map_err(|error| {
                    EvalError::with_source(crate::EvalErrorKind::Read, operand.at, Box::new(error))
                })?;
                FormKind::Literal(std::rc::Rc::new(path.to_lisp_value(self.runtime, at)?.slot))
            }
            SyntaxKind::DispatchPrefix if token.dispatch() == Some(b'=') => {
                let Some(label) = label_number(&token) else {
                    return Err(token.error("#= requires a non-negative integer label"));
                };
                let labels = Rc::clone(self.labels.get_or_insert_with(Default::default));
                let index = labels.borrow_mut().begin(label, &token)?;
                let definition = DropGuard::new(labels, |labels| {
                    labels.borrow_mut().abandon(index);
                });
                let mut operand = self.required_form(depth + 1, backquotes)?;
                let value = form_value::to_value(self.runtime, &operand)?;
                definition
                    .borrow_mut()
                    .complete(self.runtime, index, &value, at)?;
                operand.original_object = Some(FormObject::NeedsSyntax(Rc::new(value.slot)));
                DropGuard::dismiss(definition);
                return Ok(operand);
            }
            SyntaxKind::Atom if token.dispatch() == Some(b'#') => {
                let label = label_number(&token)
                    .ok_or_else(|| token.error("## requires a non-negative integer label"))?;
                let labels = self
                    .labels
                    .as_ref()
                    .ok_or_else(|| token.error("#n# requires an earlier #n= label"))?;
                let value = labels.borrow_mut().reference(self.runtime, label, &token)?;
                // A #n# in a literal is a single edge, even when the target is
                // a large shared DAG. Decode executable uses on demand.
                if backquotes == 0 && self.runtime.heap.cons_fields(value).is_some() {
                    let root = Rc::new(self.runtime.root_slot(value));
                    let mut form = Form::new(at, FormKind::Literal(Rc::clone(&root)));
                    form.original_object = Some(FormObject::NeedsSyntax(root));
                    return Ok(form);
                }
                return Ok(evaluated_form(
                    self.runtime,
                    self.runtime.root(value),
                    at,
                    backquotes != 0,
                ));
            }
            SyntaxKind::DispatchPrefix if token.dispatch() == Some(b'S') => {
                if token.has_dispatch_argument() {
                    return Err(token.error("#S does not accept a numeric argument"));
                }
                let operand = self.required_syntax(depth + 1, backquotes)?;
                let runtime = self.runtime;
                let calls = self.calls;
                self.call_at_cursor(self.at, at, || {
                    structure::read_structure(runtime, &operand, calls)
                })?
            }
            SyntaxKind::DispatchPrefix if token.dispatch() == Some(b'C') => {
                if token.has_dispatch_argument() {
                    return Err(token.error("#C does not accept a numeric argument"));
                }
                let operand = self.required_syntax(depth + 1, backquotes)?;
                numbers::complex(self.runtime, &token, &operand)?
            }
            SyntaxKind::DispatchPrefix if token.dispatch() == Some(b'Y') => {
                if token.has_dispatch_argument() {
                    return Err(token.error("#Y does not accept a numeric argument"));
                }
                let operand = self.required_form(depth + 1, backquotes)?;
                random_state(self.runtime, operand, at)?
            }
            SyntaxKind::Atom if token.text.starts_with("#:") => {
                let mut payload = token.text[2..].to_owned();
                if numeric_or_reserved(&payload, self.read_base(&token)?) {
                    return Err(token.error("an uninterned symbol requires a symbol token"));
                }
                // The payload after #: is a symbol name even when it is a token
                // of dots; escaping the dots bypasses the ordinary token rule.
                if payload.bytes().all(|byte| byte == b'.') {
                    payload.insert(0, '\\');
                }
                FormKind::Name(Name::read_uninterned(
                    self.runtime,
                    &payload,
                    at,
                    self.readtable_case(&token)?,
                    &mut self.symbol_token,
                )?)
            }
            SyntaxKind::Atom if matches!(token.dispatch(), Some(b'B' | b'O' | b'R' | b'X')) => {
                numbers::explicit_radix(self.runtime, &token)?
            }
            SyntaxKind::Atom if token.is_dot() => return Err(token.error("unexpected dot")),
            SyntaxKind::Atom if token.dispatch().is_none() => {
                if let Some(number) =
                    numbers::atom(self.runtime, &token, || self.read_base(&token))?
                {
                    number
                } else {
                    let package = self.symbol_package(at)?;
                    FormKind::Name(Name::read_in_package(
                        self.runtime,
                        &token.text,
                        at,
                        package,
                        self.readtable_case(&token)?,
                        &mut self.symbol_token,
                    )?)
                }
            }
            _ => return Err(token.error("unsupported reader dispatch")),
        };
        Ok(Form::new(at, kind))
    }

    fn readtable_case(&self, token: &Token<'_>) -> Result<ReadtableCase, EvalError> {
        self.standard_case()
            .or_else(|| self.runtime.heap.readtable_case(self.current_table()))
            .ok_or_else(|| token.error("*READTABLE* must hold a readtable"))
    }

    fn abbreviation(
        &mut self,
        operator: &str,
        at: usize,
        depth: usize,
        backquotes: usize,
    ) -> Result<FormKind, EvalError> {
        Ok(FormKind::List(
            vec![
                Form::new(at, FormKind::Name(Name::common_lisp(operator))),
                self.required_form(depth + 1, backquotes)?,
            ],
            None,
        ))
    }

    fn list(&mut self, depth: usize, backquotes: usize) -> Result<FormKind, EvalError> {
        let mut items = Vec::new();
        while let Some(token) = self.next_object_token(depth, backquotes, ReadMode::Normal)? {
            match token.kind {
                SyntaxKind::CloseParen => {
                    return Ok(if items.is_empty() {
                        nil()
                    } else {
                        FormKind::List(items, None)
                    });
                }
                SyntaxKind::Atom if token.is_dot() => {
                    if items.is_empty() {
                        return Err(token.error("a dotted list needs a preceding element"));
                    }
                    let tail = self.required_form(depth, backquotes)?;
                    match self.next_object_token(depth, backquotes, ReadMode::Normal)? {
                        Some(Token {
                            kind: SyntaxKind::CloseParen,
                            ..
                        }) => {}
                        Some(extra) => return Err(extra.error("expected ) after dotted tail")),
                        None => {
                            return Err(self.unexpected_eof("expected ) after dotted tail"));
                        }
                    }
                    if backquotes == 0 && tail.original_object().is_none() {
                        match tail.kind {
                            FormKind::Name(ref name) if name.operator() == "NIL" => {
                                return Ok(FormKind::List(items, None));
                            }
                            FormKind::List(mut rest, tail) => {
                                items.append(&mut rest);
                                return Ok(FormKind::List(items, tail));
                            }
                            _ => {}
                        }
                    }
                    return Ok(FormKind::List(items, Some(Box::new(tail))));
                }
                _ => items.push(self.form(token, depth, backquotes)?),
            }
        }
        Err(self.unexpected_eof("unterminated list"))
    }
}

impl Reader<'_> {
    fn invoked_macro_form(
        &mut self,
        kind: BuiltinMacro,
        ch: char,
    ) -> Result<Option<Form>, EvalError> {
        let nesting = self.begin_read(true, false);
        self.sync_readtable()?;
        let mode = self.mode_from_read_suppress()?;
        let dispatch = matches!(kind, BuiltinMacro::Dispatch);
        // A saved macro function can be called with a character that has since
        // been redefined. Only a dispatch function consults its subcharacter table.
        if dispatch
            && let Some(MacroDefinition::Dispatch {
                entries, standard, ..
            }) = self.macro_definition_at(ch)?
        {
            let (digits, sub, end) = self.dispatch_prefix(ch.len_utf8())?;
            if let Some(function) = dispatch_function(self.runtime, entries, sub) {
                return Ok(self
                    .call_macro(function, end, sub, Some(&digits), mode, nesting)?
                    .map(|mut token| {
                        token
                            .evaluated
                            .take()
                            .map_or_else(|| Form::new(token.at, nil()), |form| *form)
                    }));
            }
            if !standard {
                return Err(EvalError::read(
                    0,
                    format!("no dispatch macro function for {sub:?}"),
                ));
            }
        }
        let Some(token) = self.scan_buffered_token(
            self.current_table(),
            CharSyntax::Macro,
            false,
            Some(kind),
            mode,
        )?
        else {
            return Ok(None);
        };
        let Some(token) = self.object_token(token, nesting, mode)? else {
            return Ok(None);
        };
        self.form_in_mode(token, nesting, mode).map(Some)
    }
}

impl Reader<'_> {
    /// Read a delimiter only between complete objects at this nesting depth.
    fn delimited_list(&mut self, delimiter: char, recursive: bool) -> Result<Form, EvalError> {
        let start = self.input.location(self.at);
        let ReadNesting { depth, backquotes } = self.begin_read(recursive, false);
        self.delimiter = Some((depth, delimiter));
        let mode = self.mode_from_read_suppress()?;
        let mut items = Vec::new();
        while let Some(token) = self.next_object_token(depth, backquotes, mode)? {
            if mode == ReadMode::Normal {
                items.push(self.form(token, depth, backquotes)?);
            } else {
                self.skip_from_token(token, depth, backquotes)?;
            }
        }
        if self.at == self.input.text().len() {
            return Err(self.unexpected_eof("end of input before the delimiter"));
        }
        self.at += delimiter.len_utf8();
        Ok(Form::new(start, FormKind::List(items, None))
            .with_reader_range(start..self.input.location(self.at), false))
    }
}

fn random_state(runtime: &Runtime, operand: Form, at: usize) -> Result<FormKind, EvalError> {
    use crate::heap::RandomState;

    let text = match operand.kind {
        FormKind::String(text)
            if text.len() == 64 && text.bytes().all(|byte| byte.is_ascii_hexdigit()) =>
        {
            text
        }
        _ => {
            return Err(EvalError::read(
                operand.at,
                "#Y requires a quoted 64-digit hexadecimal state",
            ));
        }
    };
    let words = std::array::from_fn(|index| {
        let start = index * 16;
        u64::from_str_radix(&text[start..start + 16], 16).expect("validated hexadecimal digits")
    });
    let state = RandomState::from_words(words)
        .ok_or_else(|| EvalError::read(operand.at, "#Y random state cannot be zero"))?;
    let word = runtime
        .heap
        .allocate_random_state(state)
        .map_err(|error| EvalError::out_of_memory(at, error))?;
    Ok(FormKind::Literal(Rc::new(runtime.root_slot(word))))
}

fn evaluated_form(runtime: &Runtime, value: crate::Root<'_>, at: usize, template: bool) -> Form {
    let word = value.slot.get();
    let decoded = if template {
        form_value::from_quasiquote_value(runtime, &value, at)
    } else {
        form_value::from_value(runtime, &value, at)
    };
    match decoded {
        Ok(mut form) => {
            if runtime.heap.cons_fields(word).is_some() {
                form.original_object = Some(FormObject::NeedsSyntax(Rc::new(value.slot)));
            }
            form
        }
        Err(_) => {
            // Cycles and deep objects can be read as data even where they
            // cannot be decoded as executable forms.
            let value = Rc::new(value.slot);
            Form {
                at,
                kind: FormKind::Literal(Rc::clone(&value)),
                original_object: Some(FormObject::NeedsSyntax(value)),
                source: FormSource::default(),
            }
        }
    }
}

fn finish_list(
    runtime: &Runtime,
    at: usize,
    kind: FormKind,
    backquotes: usize,
) -> Result<Form, EvalError> {
    if backquotes != 0 {
        return Ok(Form::new(at, kind));
    }
    let FormKind::List(mut items, Some(tail)) = kind else {
        return Ok(Form::new(at, kind));
    };
    let Some(root) = tail.original_object() else {
        return Ok(Form::new(at, FormKind::List(items, Some(tail))));
    };
    // The cdr supplied by #. may already be visible in the runtime. Keep
    // that exact cdr, while still flattening a proper list for the compiler.
    let mut value = runtime.root(root.get());
    for item in items.iter().rev() {
        let car = form_value::to_value(runtime, item)?;
        value = runtime
            .cons(&car, &value)
            .map_err(|error| EvalError::out_of_memory(at, error))?;
    }
    let tail = match *tail {
        Form {
            kind: FormKind::List(mut rest, tail),
            ..
        } => {
            items.append(&mut rest);
            tail
        }
        tail => Some(Box::new(tail)),
    };
    Ok(Form {
        at,
        kind: FormKind::List(items, tail),
        original_object: Some(FormObject::NeedsSyntax(Rc::new(value.slot))),
        source: FormSource::default(),
    })
}

fn nil() -> FormKind {
    FormKind::Name(Name::common_lisp("NIL"))
}

#[cfg(test)]
mod source_tests {
    use super::*;

    #[test]
    fn reader_retains_whole_utf8_forms_and_each_occurrence() {
        let runtime = Runtime::start().unwrap();
        runtime.initialize_packages().unwrap();
        let source = "  (list \"中文\" x x)";
        let form = read(&runtime, source).unwrap();
        assert_eq!(form.source.range, Some(2..source.len()));
        assert_eq!(form.source.origin, FormOrigin::Original);
        let items = form.list().unwrap();
        assert_eq!(items[0].source.range, Some(3..7));
        assert_eq!(items[1].source.range, Some(8..16));
        assert_eq!(items[2].source.range, Some(17..18));
        assert_eq!(items[3].source.range, Some(19..20));
    }

    #[test]
    fn reader_generated_objects_use_generating_range() {
        let runtime = Runtime::start().unwrap();
        runtime.initialize_packages().unwrap();
        let source = "#.(list 'car '(1 2))";
        let form = read(&runtime, source).unwrap();
        assert_eq!(form.source.range, Some(0..source.len()));
        assert_eq!(form.source.origin, FormOrigin::Reader);
        for item in form.list().unwrap() {
            assert_eq!(item.source.range, Some(0..source.len()));
            assert_eq!(item.source.origin, FormOrigin::Reader);
        }
    }
}
