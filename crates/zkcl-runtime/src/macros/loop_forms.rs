use std::iter::once;

use crate::heap::Word;
use crate::{EvalError, Root, Runtime};

use super::{Expansion, binding, call, fresh, invalid, list, symbol};

pub(super) fn expand<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    expand_loop(runtime, forms)
}

fn expand_loop<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    if forms
        .first()
        .is_none_or(|first| runtime.cons_fields(first).is_some())
    {
        if forms.iter().any(|form| runtime.cons_fields(form).is_none()) {
            return Err(invalid("LOOP", "simple LOOP requires compound forms"));
        }
        let start = fresh(runtime, "LOOP START")?;
        let repeat = call(runtime, "GO", once(&start))?;
        let body = call(
            runtime,
            "TAGBODY",
            once(&start).chain(forms).chain(once(&repeat)),
        )?;
        return call(runtime, "BLOCK", [&runtime.nil(), &body].into_iter());
    }
    Loop::new(runtime, forms)?.expand()
}

/// One group of induction clauses. Control expressions use the enclosing
/// bindings, then the iteration variables get their own lexical bindings.
/// Steps run first, exhaustion checks can stop the iteration before anything
/// is assigned, then all assignments are committed, then post-step bounds are
/// checked.
#[derive(Default)]
struct Induction<'rt> {
    inputs: Vec<Root<'rt>>,
    variables: Vec<Root<'rt>>,
    steps: Vec<Root<'rt>>,
    /// Checks that stop the iteration before any variable is assigned
    /// (list/vector/hash exhaustion).
    exhaustion: Vec<Root<'rt>>,
    assignments: Vec<Root<'rt>>,
    destructures: Vec<Root<'rt>>,
    /// Checks that run after assignments, as for ON with a simple variable.
    bounds: Vec<Root<'rt>>,
}

enum ListIteration {
    Element,
    Tail,
    DestructuredTail,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AccumulationKind {
    List,
    Sum,
    Extremum,
}

struct Accumulator<'rt> {
    kind: AccumulationKind,
    value: Root<'rt>,
    tail: Option<Root<'rt>>,
}

struct Loop<'a, 'rt> {
    runtime: &'rt Runtime,
    forms: &'a [Root<'rt>],
    block: Root<'rt>,
    finish: Root<'rt>,
    first: Root<'rt>,
    /// Binding batches, in source order. Each batch is a parallel LET.
    prologue: Vec<Vec<Root<'rt>>>,
    temporaries: Vec<Root<'rt>>,
    variables: Vec<Root<'rt>>,
    initially: Vec<Root<'rt>>,
    finally: Vec<Root<'rt>>,
    induction: Vec<Root<'rt>>,
    body: Vec<Root<'rt>>,
    accumulators: Vec<Accumulator<'rt>>,
    result: Option<usize>,
    boolean_result: Option<bool>,
    main_clauses: bool,
    /// Local function that copies a list spine while keeping its final cdr,
    /// used by APPEND so an improper final value is still appended.
    append_copy: Option<Root<'rt>>,
}

impl<'a, 'rt> Loop<'a, 'rt> {
    fn new(runtime: &'rt Runtime, forms: &'a [Root<'rt>]) -> Result<Self, EvalError> {
        let first = fresh(runtime, "LOOP FIRST ITERATION")?;
        let temporaries = vec![binding(runtime, &first, &runtime.root(Word::T))?];
        Ok(Self {
            runtime,
            forms,
            block: runtime.nil(),
            finish: fresh(runtime, "LOOP FINISH")?,
            first,
            prologue: Vec::new(),
            temporaries,
            variables: Vec::new(),
            initially: Vec::new(),
            finally: Vec::new(),
            induction: Vec::new(),
            body: Vec::new(),
            accumulators: Vec::new(),
            result: None,
            boolean_result: None,
            main_clauses: false,
            append_copy: None,
        })
    }

    fn expand(mut self) -> Expansion<'rt> {
        if self.consume("NAMED") {
            self.block = self.take("expects a block name")?;
            if !self.runtime.heap.is_symbol(self.block.slot.get()) {
                return Err(invalid("LOOP", "expects a symbol as its block name"));
            }
        }
        while !self.forms.is_empty() {
            let keyword = self.keyword()?;
            match keyword.as_str() {
                "WITH" => self.with_clause()?,
                "FOR" | "AS" => self.for_clause()?,
                "REPEAT" => self.repeat_clause()?,
                // SBCL permits termination tests between iteration clauses.
                // Keep the test at its source position among the induction steps.
                "WHILE" | "UNTIL" if !self.main_clauses => {
                    let test = self.clause(&keyword, None, false)?;
                    self.induction.push(test);
                }
                "INITIALLY" => self.initially.extend(
                    complete_forms(&mut self.forms, self.runtime, "INITIALLY")?
                        .iter()
                        .cloned(),
                ),
                "FINALLY" => {
                    self.finally.extend(
                        complete_forms(&mut self.forms, self.runtime, "FINALLY")?
                            .iter()
                            .cloned(),
                    );
                }
                _ => {
                    self.main_clauses = true;
                    let form = self.clause(&keyword, None, false)?;
                    self.body.push(form);
                }
            }
        }
        self.emit()
    }

    fn emit(self) -> Expansion<'rt> {
        let runtime = self.runtime;
        let start = fresh(runtime, "LOOP START")?;
        let started = call(runtime, "SETQ", [&self.first, &runtime.nil()].into_iter())?;
        let repeat = call(runtime, "GO", once(&start))?;
        let result = if let Some(index) = self.result {
            self.accumulators[index].value.clone()
        } else if self.boolean_result == Some(true) {
            runtime.root(Word::T)
        } else {
            runtime.nil()
        };
        let result = call(runtime, "RETURN-FROM", [&self.block, &result].into_iter())?;
        let tagbody = call(
            runtime,
            "TAGBODY",
            self.initially
                .iter()
                .chain(once(&start))
                .chain(&self.induction)
                .chain(once(&started))
                .chain(self.body.iter())
                .chain([&repeat, &self.finish])
                .chain(&self.finally)
                .chain(once(&result)),
        )?;

        // A lexical macro gives user code the normal exit, including FINALLY.
        let leave = call(runtime, "GO", once(&self.finish))?;
        let quote = call(runtime, "QUOTE", once(&leave))?;
        let finish = symbol(runtime, "LOOP-FINISH")?;
        let definition = list(runtime, [&finish, &runtime.nil(), &quote].into_iter())?;
        let macros = list(runtime, once(&definition))?;
        let mut body = call(runtime, "MACROLET", [&macros, &tagbody].into_iter())?;
        for batch in self.prologue.iter().rev().chain(once(&self.temporaries)) {
            if !batch.is_empty() {
                let bindings = list(runtime, batch.iter())?;
                body = call(runtime, "LET", [&bindings, &body].into_iter())?;
            }
        }
        let body = call(runtime, "BLOCK", [&self.block, &body].into_iter())?;
        let Some(copy) = self.append_copy.as_ref() else {
            return Ok(body);
        };
        let source = fresh(runtime, "LOOP APPEND SOURCE")?;
        let rest = call(runtime, "CDR", once(&source))?;
        let function = call(runtime, "FUNCTION", once(copy))?;
        let recursive = call(runtime, "FUNCALL", [&function, &rest].into_iter())?;
        let item = call(runtime, "CAR", once(&source))?;
        let copied = call(runtime, "CONS", [&item, &recursive].into_iter())?;
        let compound = call(runtime, "CONSP", once(&source))?;
        let spine = call(runtime, "IF", [&compound, &copied, &source].into_iter())?;
        let parameters = list(runtime, once(&source))?;
        let definition = list(runtime, [copy, &parameters, &spine].into_iter())?;
        let definitions = list(runtime, once(&definition))?;
        call(runtime, "LABELS", [&definitions, &body].into_iter())
    }

    fn with_clause(&mut self) -> Result<(), EvalError> {
        self.in_prologue("WITH")?;
        let runtime = self.runtime;
        let mut bindings = Vec::new();
        let mut destructured = Vec::new();
        loop {
            let designator = self.take("expects a loop variable")?;
            let declaration = self.type_specifier()?;
            let value = if self.consume("=") {
                self.take("expects a WITH initial value")?
            } else {
                self.default_pattern_value(&designator, declaration.as_ref())?
            };
            if designator.is_nil() {
                // The initializer still runs, and RETURN in it must see the
                // loop's NIL block, so bind it to an ignored variable.
                let ignored = fresh(runtime, "LOOP IGNORED")?;
                bindings.push(binding(runtime, &ignored, &value)?);
            } else if runtime.heap.is_variable(designator.slot.get()) {
                self.record_binding(&designator)?;
                bindings.push(binding(runtime, &designator, &value)?);
            } else if runtime.cons_fields(&designator).is_some() {
                let temporary = fresh(runtime, "LOOP WITH VALUE")?;
                bindings.push(binding(runtime, &temporary, &value)?);
                let mut parts = Vec::new();
                collect_destructure_bindings(runtime, &designator, &temporary, &mut parts)?;
                for (name, _) in &parts {
                    self.record_binding(name)?;
                }
                destructured.extend(
                    parts
                        .iter()
                        .map(|(name, value)| binding(runtime, name, value))
                        .collect::<Result<Vec<_>, _>>()?,
                );
            } else {
                return Err(invalid(
                    "LOOP",
                    "expects a variable name or a destructuring pattern",
                ));
            }
            if !self.consume("AND") {
                break;
            }
        }
        self.prologue.push(bindings);
        if !destructured.is_empty() {
            self.prologue.push(destructured);
        }
        Ok(())
    }

    /// The initial value of a WITH variable that has no explicit initializer.
    /// A destructuring pattern gets one default per component from its OF-TYPE
    /// specifier, for example (FIXNUM FLOAT T) becomes (0 0.0 NIL).
    fn default_pattern_value(
        &self,
        pattern: &Root<'rt>,
        declaration: Option<&Root<'rt>>,
    ) -> Expansion<'rt> {
        let runtime = self.runtime;
        if pattern.is_nil() {
            return Ok(runtime.nil());
        }
        if runtime.heap.is_variable(pattern.slot.get()) {
            return self.default_initial_value(declaration);
        }
        let Some((head, tail)) = runtime.cons_fields(pattern) else {
            return Err(invalid("LOOP", "invalid destructuring pattern"));
        };
        // A compound specifier pairs with the pattern; a simple one applies
        // to every component (the shorthand for a uniform element type).
        let (head_type, tail_type) = declaration
            .and_then(|declaration| runtime.cons_fields(declaration))
            .map_or(
                (declaration.cloned(), declaration.cloned()),
                |(head, tail)| (Some(head), Some(tail)),
            );
        let head = self.default_pattern_value(&head, head_type.as_ref())?;
        let tail = self.default_pattern_value(&tail, tail_type.as_ref())?;
        call(runtime, "CONS", [&head, &tail].into_iter())
    }

    fn for_clause(&mut self) -> Result<(), EvalError> {
        self.in_prologue("FOR")?;
        let mut group = Induction::default();
        loop {
            let (name, pattern) = self.iteration_variable()?;
            self.record_binding(&name)?;
            self.type_specifier()?;
            let operator = self.keyword()?;
            match operator.as_str() {
                "IN" | "ON" => {
                    let mode = match (operator.as_str(), pattern.is_some()) {
                        ("IN", _) => ListIteration::Element,
                        (_, false) => ListIteration::Tail,
                        (_, true) => ListIteration::DestructuredTail,
                    };
                    let form = self.take("expects a list expression")?;
                    self.iterate_list_expr(&mut group, &name, &form, mode)?;
                }
                "=" => {
                    let first = self.take("expects an initial value after =")?;
                    let then = if self.consume("THEN") {
                        self.take("expects a step form after THEN")?
                    } else {
                        first.clone()
                    };
                    group
                        .variables
                        .push(binding(self.runtime, &name, &self.runtime.nil())?);
                    let value = call(self.runtime, "IF", [&self.first, &first, &then].into_iter())?;
                    let next = self.stage(&mut group, "LOOP VALUE", &value)?;
                    group.assignments.extend([name.clone(), next]);
                }
                "FROM" | "UPFROM" | "DOWNFROM" | "TO" | "UPTO" | "BELOW" | "DOWNTO" | "ABOVE"
                | "BY" => {
                    if pattern.is_some() {
                        return Err(invalid(
                            "LOOP",
                            "arithmetic iteration requires a simple variable",
                        ));
                    }
                    self.arithmetic_iteration(&mut group, &name, operator)?;
                }
                "ACROSS" => {
                    self.across_iteration(&mut group, &name)?;
                }
                "BEING" => {
                    self.being_iteration(&mut group, &name, pattern.as_ref())?;
                }
                _ => {
                    return Err(invalid(
                        "LOOP",
                        &format!("unsupported FOR subclause {operator}"),
                    ));
                }
            }
            if let Some(pattern) = pattern {
                let mut vars = Vec::new();
                collect_destructure_assignments(
                    self.runtime,
                    &pattern,
                    &name,
                    &mut vars,
                    &mut group.destructures,
                )?;
                for (var, user) in vars {
                    if user {
                        self.record_binding(&var)?;
                    }
                    group
                        .variables
                        .push(binding(self.runtime, &var, &self.runtime.nil())?);
                }
            }
            if !self.consume("AND") {
                break;
            }
        }
        self.add_induction(group)
    }

    /// `FOR var ACROSS vector` walks a vector's elements by index. The
    /// element is read with the current index, then the index advances and the
    /// bound is checked, so the last element runs before the clause leaves.
    fn across_iteration(
        &mut self,
        group: &mut Induction<'rt>,
        name: &Root<'rt>,
    ) -> Result<(), EvalError> {
        let runtime = self.runtime;
        let form = self.take("expects a sequence expression")?;
        let vector = input(runtime, &mut group.inputs, "LOOP ACROSS VECTOR", &form)?;
        // The length is bound with the iteration variables: the inputs above
        // form one parallel LET, so a length input could not see the vector.
        let length = fresh(runtime, "LOOP ACROSS LENGTH")?;
        let computed = call(runtime, "LENGTH", once(&vector))?;
        group.variables.push(binding(runtime, &length, &computed)?);
        let index = fresh(runtime, "LOOP ACROSS INDEX")?;
        group
            .variables
            .push(binding(runtime, &index, &runtime.fixnum(0).unwrap())?);
        group
            .variables
            .push(binding(runtime, name, &runtime.nil())?);
        let element = call(runtime, "AREF", [&vector, &index].into_iter())?;
        let step = call(runtime, "1+", [&index].into_iter())?;
        group
            .assignments
            .extend([name.clone(), element, index.clone(), step]);
        // A vector with no current element leaves before the assignments, so
        // the element just stored still reaches the loop body.
        let bound = call(runtime, ">=", [&index, &length].into_iter())?;
        group.exhaustion.push(self.leave_when(&bound, false)?);
        Ok(())
    }

    fn being_iteration(
        &mut self,
        group: &mut Induction<'rt>,
        name: &Root<'rt>,
        pattern: Option<&Root<'rt>>,
    ) -> Result<(), EvalError> {
        if !self.consume("EACH") {
            let _ = self.consume("THE");
        }
        let kind = self.keyword()?;
        match kind.as_str() {
            "SYMBOL" | "SYMBOLS" | "PRESENT-SYMBOL" | "PRESENT-SYMBOLS" | "EXTERNAL-SYMBOL"
            | "EXTERNAL-SYMBOLS" => {
                if pattern.is_some() {
                    return Err(invalid(
                        "LOOP",
                        "symbol iteration does not support destructuring",
                    ));
                }
                let pkg_form = if self.consume("IN") || self.consume("OF") {
                    self.take("expects a package expression")?
                } else {
                    symbol(self.runtime, "*PACKAGE*")?
                };
                let mode = match kind.as_str() {
                    "SYMBOL" | "SYMBOLS" => 1 | 2 | 4 | 16,
                    "PRESENT-SYMBOL" | "PRESENT-SYMBOLS" => 1 | 2 | 16,
                    "EXTERNAL-SYMBOL" | "EXTERNAL-SYMBOLS" => 2 | 16,
                    _ => unreachable!(),
                };
                let mode_word = self.runtime.fixnum(mode).unwrap();
                let list_expr = super::native_call(
                    self.runtime,
                    super::PACKAGE_ITERATOR_ENTRIES,
                    [&pkg_form, &mode_word],
                )?;
                self.iterate_list_expr(group, name, &list_expr, ListIteration::Element)
            }
            "HASH-KEY" | "HASH-KEYS" | "HASH-VALUE" | "HASH-VALUES" => {
                let iterate_keys = matches!(kind.as_str(), "HASH-KEY" | "HASH-KEYS");
                if !self.consume("IN") && !self.consume("OF") {
                    return Err(invalid("LOOP", "expects IN or OF before a hash table"));
                }
                let form = self.take("expects a hash table expression")?;
                let mut using_var = None;
                let mut using_binding = None;
                if self.consume("USING") {
                    let pair = self.take("expects a (HASH-VALUE/HASH-KEY var) form after USING")?;
                    let items = super::proper_list(self.runtime, pair, "LOOP USING")?;
                    let [other_kw, designator] = items.as_slice() else {
                        return Err(invalid("LOOP", "USING expects (key var)"));
                    };
                    let other_name = self.runtime.symbol_name(other_kw).unwrap_or_default();
                    let expected = if iterate_keys {
                        ["HASH-VALUE", "HASH-VALUES"]
                    } else {
                        ["HASH-KEY", "HASH-KEYS"]
                    };
                    if !expected.contains(&other_name.as_str()) {
                        return Err(invalid(
                            "LOOP",
                            "mismatched USING clause for hash table iteration",
                        ));
                    }
                    if designator.is_nil() {
                        // NIL consumes the other value without binding it.
                    } else if self.runtime.heap.is_variable(designator.slot.get()) {
                        self.record_binding(designator)?;
                        using_var = Some(designator.clone());
                        using_binding = Some(designator.clone());
                    } else if self.runtime.cons_fields(designator).is_some() {
                        let temporary = fresh(self.runtime, "LOOP USING VALUE")?;
                        group.variables.push(binding(
                            self.runtime,
                            &temporary,
                            &self.runtime.nil(),
                        )?);
                        let mut parts = Vec::new();
                        collect_destructure_assignments(
                            self.runtime,
                            designator,
                            &temporary,
                            &mut parts,
                            &mut group.destructures,
                        )?;
                        for (var, user) in parts {
                            if user {
                                self.record_binding(&var)?;
                            }
                            group
                                .variables
                                .push(binding(self.runtime, &var, &self.runtime.nil())?);
                        }
                        using_var = Some(temporary);
                    } else {
                        return Err(invalid(
                            "LOOP",
                            "expected a variable or destructuring pattern in USING",
                        ));
                    }
                }
                let runtime = self.runtime;
                let table = input(runtime, &mut group.inputs, "LOOP HASH TABLE", &form)?;
                let cursor = fresh(runtime, "LOOP HASH CURSOR")?;
                let next = fresh(runtime, "LOOP HASH NEXT")?;
                let key = fresh(runtime, "LOOP HASH KEY")?;
                let val = fresh(runtime, "LOOP HASH VALUE")?;
                group
                    .variables
                    .push(binding(runtime, &cursor, &runtime.fixnum(0).unwrap())?);
                group
                    .variables
                    .push(binding(runtime, &next, &runtime.nil())?);
                group
                    .variables
                    .push(binding(runtime, &key, &runtime.nil())?);
                group
                    .variables
                    .push(binding(runtime, &val, &runtime.nil())?);
                group
                    .variables
                    .push(binding(runtime, name, &runtime.nil())?);
                if let Some(other) = using_binding.as_ref() {
                    group
                        .variables
                        .push(binding(runtime, other, &runtime.nil())?);
                }
                let step_call =
                    super::native_call(runtime, super::HASH_TABLE_STEP, [&table, &cursor])?;
                let mv_vars = list(runtime, [&next, &key, &val].into_iter())?;
                let step = call(
                    runtime,
                    "MULTIPLE-VALUE-SETQ",
                    [&mv_vars, &step_call].into_iter(),
                )?;
                group.steps.push(step);
                let done = call(runtime, "NULL", once(&next))?;
                group.exhaustion.push(self.leave_when(&done, false)?);
                let (primary_val, secondary_val) =
                    if iterate_keys { (key, val) } else { (val, key) };
                group
                    .assignments
                    .extend([cursor, next, name.clone(), primary_val]);
                if let Some(other) = using_var {
                    group.assignments.extend([other, secondary_val]);
                }
                Ok(())
            }
            _ => Err(invalid(
                "LOOP",
                &format!("unsupported BEING subclause {kind}"),
            )),
        }
    }

    fn iterate_list_expr(
        &mut self,
        group: &mut Induction<'rt>,
        name: &Root<'rt>,
        form: &Root<'rt>,
        mode: ListIteration,
    ) -> Result<(), EvalError> {
        let runtime = self.runtime;
        let initial = input(runtime, &mut group.inputs, "LOOP LIST", form)?;
        let cursor = fresh(runtime, "LOOP LIST CURSOR")?;
        group.variables.push(binding(runtime, &cursor, &initial)?);
        group
            .variables
            .push(binding(runtime, name, &runtime.nil())?);
        let step = if self.consume("BY") {
            let form = self.take("expects a step function")?;
            let function = input(runtime, &mut group.inputs, "LOOP STEP FUNCTION", &form)?;
            call(runtime, "FUNCALL", [&function, &cursor].into_iter())?
        } else {
            call(runtime, "CDR", once(&cursor))?
        };
        let advancement = call(runtime, "IF", [&self.first, &cursor, &step].into_iter())?;
        let next = self.stage(group, "LOOP NEXT LIST", &advancement)?;
        let done = call(
            runtime,
            match mode {
                ListIteration::Element => "ENDP",
                ListIteration::Tail | ListIteration::DestructuredTail => "ATOM",
            },
            once(&next),
        )?;
        // A simple ON variable holds the terminal atom in FINALLY.
        // A destructuring pattern only receives cons tails.
        let check = self.leave_when(&done, false)?;
        let value = match mode {
            ListIteration::Element => {
                group.exhaustion.push(check);
                let item = call(runtime, "CAR", once(&next))?;
                self.stage(group, "LOOP NEXT ITEM", &item)?
            }
            ListIteration::Tail => {
                group.bounds.push(check);
                next.clone()
            }
            ListIteration::DestructuredTail => {
                group.exhaustion.push(check);
                next.clone()
            }
        };
        group
            .assignments
            .extend([cursor, next, name.clone(), value]);
        Ok(())
    }

    fn arithmetic_iteration(
        &mut self,
        group: &mut Induction<'rt>,
        name: &Root<'rt>,
        mut operator: String,
    ) -> Result<(), EvalError> {
        let runtime = self.runtime;
        let mut start = None;
        let mut limit = None;
        let mut increment = None;
        let mut direction = None;
        loop {
            let form = self.take("expects an arithmetic control expression")?;
            let value = input(
                runtime,
                &mut group.inputs,
                "LOOP ARITHMETIC PARAMETER",
                &form,
            )?;
            match operator.as_str() {
                "FROM" | "UPFROM" | "DOWNFROM" if start.is_none() => {
                    start = Some(value);
                    let requested = match operator.as_str() {
                        "UPFROM" => Some(false),
                        "DOWNFROM" => Some(true),
                        _ => None,
                    };
                    set_direction(&mut direction, requested)?;
                }
                "TO" | "UPTO" | "BELOW" | "DOWNTO" | "ABOVE" if limit.is_none() => {
                    let inclusive = !matches!(operator.as_str(), "BELOW" | "ABOVE");
                    let requested = match operator.as_str() {
                        "UPTO" | "BELOW" => Some(false),
                        "DOWNTO" | "ABOVE" => Some(true),
                        _ => None,
                    };
                    set_direction(&mut direction, requested)?;
                    limit = Some((value, inclusive));
                }
                "BY" if increment.is_none() => increment = Some(value),
                _ => return Err(invalid("LOOP", "duplicate arithmetic parameter")),
            }
            let Some(next) = self.peek_keyword() else {
                break;
            };
            if !matches!(
                next.as_str(),
                "FROM"
                    | "UPFROM"
                    | "DOWNFROM"
                    | "TO"
                    | "UPTO"
                    | "BELOW"
                    | "DOWNTO"
                    | "ABOVE"
                    | "BY"
            ) {
                break;
            }
            operator = self.keyword()?;
        }
        let decreasing = direction == Some(true);
        if decreasing && start.is_none() {
            return Err(invalid(
                "LOOP",
                "descending arithmetic iteration requires a starting value",
            ));
        }
        let start = start.unwrap_or_else(|| runtime.fixnum(0).unwrap());
        let increment = increment.unwrap_or_else(|| runtime.fixnum(1).unwrap());
        group.variables.push(binding(runtime, name, &start)?);
        let step = call(
            runtime,
            if decreasing { "-" } else { "+" },
            [name, &increment].into_iter(),
        )?;
        let value = call(runtime, "IF", [&self.first, name, &step].into_iter())?;
        let next = self.stage(group, "LOOP NEXT INDEX", &value)?;
        if let Some((limit, inclusive)) = limit {
            let (left, right) = if decreasing == inclusive {
                (&next, &limit)
            } else {
                (&limit, &next)
            };
            let bound = call(runtime, "<", [left, right].into_iter())?;
            // The bound is tested before the awaited value replaces the
            // variable, so FINALLY sees the last value inside the range.
            group.exhaustion.push(self.leave_when(&bound, !inclusive)?);
        }
        group.assignments.extend([name.clone(), next]);
        Ok(())
    }

    fn repeat_clause(&mut self) -> Result<(), EvalError> {
        self.in_prologue("REPEAT")?;
        let runtime = self.runtime;
        let form = self.take("expects a REPEAT count")?;
        let mut group = Induction::default();
        let count = input(runtime, &mut group.inputs, "LOOP REPEAT COUNT", &form)?;
        let step = call(
            runtime,
            "-",
            [&count, &runtime.fixnum(1).unwrap()].into_iter(),
        )?;
        let value = call(runtime, "IF", [&self.first, &count, &step].into_iter())?;
        let next = self.stage(&mut group, "LOOP REMAINING", &value)?;
        let positive = call(
            runtime,
            "<",
            [&runtime.fixnum(0).unwrap(), &next].into_iter(),
        )?;
        group.assignments.extend([count, next]);
        group.bounds.push(self.leave_when(&positive, true)?);
        self.add_induction(group)
    }

    fn stage(
        &mut self,
        group: &mut Induction<'rt>,
        name: &str,
        value: &Root<'rt>,
    ) -> Expansion<'rt> {
        let temp = fresh(self.runtime, name)?;
        self.temporaries
            .push(binding(self.runtime, &temp, &self.runtime.nil())?);
        group
            .steps
            .push(call(self.runtime, "SETQ", [&temp, value].into_iter())?);
        Ok(temp)
    }

    fn add_induction(&mut self, group: Induction<'rt>) -> Result<(), EvalError> {
        self.prologue.extend([group.inputs, group.variables]);
        self.induction.extend(group.steps);
        self.induction.extend(group.exhaustion);
        self.induction
            .push(call(self.runtime, "SETQ", group.assignments.iter())?);
        self.induction.extend(group.destructures);
        self.induction.extend(group.bounds);
        Ok(())
    }

    fn clause(
        &mut self,
        keyword: &str,
        it: Option<&Root<'rt>>,
        selectable: bool,
    ) -> Expansion<'rt> {
        let runtime = self.runtime;
        match keyword {
            "DO" | "DOING" => {
                let forms = complete_forms(&mut self.forms, runtime, "DO")?;
                call(runtime, "PROGN", forms.iter())
            }
            "RETURN" => {
                let value = self.value(it)?;
                call(runtime, "RETURN-FROM", [&self.block, &value].into_iter())
            }
            "IF" | "WHEN" | "UNLESS" => self.conditional(keyword == "UNLESS"),
            "WHILE" | "UNTIL" if !selectable => {
                let condition = self.value(None)?;
                self.leave_when(&condition, keyword == "WHILE")
            }
            "ALWAYS" | "NEVER" | "THEREIS" if !selectable => self.boolean_clause(keyword),
            "COLLECT" | "COLLECTING" | "APPEND" | "APPENDING" | "NCONC" | "NCONCING" => {
                let value = self.value(it)?;
                self.accumulate_list(keyword, value)
            }
            "SUM" | "SUMMING" | "COUNT" | "COUNTING" | "MAXIMIZE" | "MAXIMIZING" | "MINIMIZE"
            | "MINIMIZING" => {
                let value = self.value(it)?;
                self.accumulate_number(keyword, value)
            }
            _ => Err(invalid("LOOP", &format!("unsupported clause {keyword}"))),
        }
    }

    fn conditional(&mut self, inverted: bool) -> Expansion<'rt> {
        let runtime = self.runtime;
        let test = self.take("expects a conditional test form")?;
        let it = fresh(runtime, "LOOP TEST")?;
        let yes = self.selection(&it)?;
        let no = if self.consume("ELSE") {
            self.selection(&it)?
        } else {
            runtime.nil()
        };
        self.consume("END");
        let (yes, no) = if inverted { (no, yes) } else { (yes, no) };
        let action = call(runtime, "IF", [&it, &yes, &no].into_iter())?;
        let bindings = list(runtime, once(&binding(runtime, &it, &test)?))?;
        call(runtime, "LET", [&bindings, &action].into_iter())
    }

    fn selection(&mut self, it: &Root<'rt>) -> Expansion<'rt> {
        let keyword = self.keyword()?;
        let mut forms = vec![self.clause(&keyword, Some(it), true)?];
        while self.consume("AND") {
            let keyword = self.keyword()?;
            forms.push(self.clause(&keyword, None, true)?);
        }
        call(self.runtime, "PROGN", forms.iter())
    }

    fn boolean_clause(&mut self, keyword: &str) -> Expansion<'rt> {
        if self.result.is_some() {
            return Err(invalid(
                "LOOP",
                "boolean termination cannot share an implicit accumulation result",
            ));
        }
        let runtime = self.runtime;
        let condition = self.value(None)?;
        if keyword == "THEREIS" {
            self.boolean_result.get_or_insert(false);
            let value = fresh(runtime, "LOOP FOUND VALUE")?;
            let leave = call(runtime, "RETURN-FROM", [&self.block, &value].into_iter())?;
            let test = call(runtime, "IF", [&value, &leave].into_iter())?;
            let bindings = list(runtime, once(&binding(runtime, &value, &condition)?))?;
            return call(runtime, "LET", [&bindings, &test].into_iter());
        }
        self.boolean_result = Some(true);
        let leave = call(runtime, "RETURN-FROM", once(&self.block))?;
        let test = if keyword == "ALWAYS" {
            call(runtime, "NOT", once(&condition))?
        } else {
            condition
        };
        call(runtime, "IF", [&test, &leave].into_iter())
    }

    fn accumulate_list(&mut self, keyword: &str, value: Root<'rt>) -> Expansion<'rt> {
        let runtime = self.runtime;
        let target = self.consume("INTO").then(|| self.variable()).transpose()?;
        let index = self.accumulator(AccumulationKind::List, target, None)?;
        let tail = self.accumulators[index]
            .tail
            .clone()
            .expect("list accumulator cursor");
        let accumulator = self.accumulators[index].value.clone();
        let chunk = fresh(runtime, "LOOP LIST CHUNK")?;
        let value = match keyword {
            "COLLECT" | "COLLECTING" => call(runtime, "LIST", once(&value))?,
            "APPEND" | "APPENDING" => {
                let copy = match &self.append_copy {
                    Some(copy) => copy.clone(),
                    None => {
                        let copy = fresh(runtime, "LOOP APPEND COPY")?;
                        self.append_copy = Some(copy.clone());
                        copy
                    }
                };
                let function = call(runtime, "FUNCTION", once(&copy))?;
                call(runtime, "FUNCALL", [&function, &value].into_iter())?
            }
            _ => value,
        };
        let link = call(runtime, "RPLACD", [&tail, &chunk].into_iter())?;
        let head = call(runtime, "SETQ", [&accumulator, &chunk].into_iter())?;
        let append = call(runtime, "IF", [&tail, &link, &head].into_iter())?;
        let last = call(runtime, "LAST", once(&chunk))?;
        let advance = call(runtime, "SETQ", [&tail, &last].into_iter())?;
        let append = call(runtime, "WHEN", [&chunk, &append, &advance].into_iter())?;
        let bindings = list(runtime, once(&binding(runtime, &chunk, &value)?))?;
        call(runtime, "LET", [&bindings, &append].into_iter())
    }

    fn accumulate_number(&mut self, keyword: &str, value: Root<'rt>) -> Expansion<'rt> {
        let runtime = self.runtime;
        let kind = if matches!(keyword, "SUM" | "SUMMING" | "COUNT" | "COUNTING") {
            AccumulationKind::Sum
        } else {
            AccumulationKind::Extremum
        };
        let target = self.consume("INTO").then(|| self.variable()).transpose()?;
        let declaration = self.type_specifier()?;
        let index = self.accumulator(kind, target, declaration.as_ref())?;
        let current = &self.accumulators[index].value;
        if kind == AccumulationKind::Sum {
            let counted = matches!(keyword, "COUNT" | "COUNTING");
            let amount = if counted {
                runtime.fixnum(1).unwrap()
            } else {
                value.clone()
            };
            let sum = call(runtime, "+", [current, &amount].into_iter())?;
            let add = call(runtime, "SETQ", [current, &sum].into_iter())?;
            return if counted {
                call(runtime, "IF", [&value, &add].into_iter())
            } else {
                Ok(add)
            };
        }
        let candidate = fresh(runtime, "LOOP EXTREMUM CANDIDATE")?;
        let operands = if matches!(keyword, "MINIMIZE" | "MINIMIZING") {
            [&candidate, current]
        } else {
            [current, &candidate]
        };
        let compare = call(runtime, "<", operands.into_iter())?;
        let missing = call(runtime, "NULL", once(current))?;
        let better = call(runtime, "OR", [&missing, &compare].into_iter())?;
        let assign = call(runtime, "SETQ", [current, &candidate].into_iter())?;
        let test = call(runtime, "IF", [&better, &assign].into_iter())?;
        let bindings = list(runtime, once(&binding(runtime, &candidate, &value)?))?;
        call(runtime, "LET", [&bindings, &test].into_iter())
    }

    fn accumulator(
        &mut self,
        kind: AccumulationKind,
        target: Option<Root<'rt>>,
        declaration: Option<&Root<'rt>>,
    ) -> Result<usize, EvalError> {
        let runtime = self.runtime;
        let index = match &target {
            Some(target) => self
                .accumulators
                .iter()
                .position(|entry| entry.value.same_identity(target)),
            None => {
                if self.boolean_result.is_some() {
                    return Err(invalid(
                        "LOOP",
                        "implicit accumulation cannot share a boolean result",
                    ));
                }
                self.result
            }
        };
        if let Some(index) = index {
            if self.accumulators[index].kind != kind {
                return Err(invalid(
                    "LOOP",
                    "incompatible accumulation clauses share a result",
                ));
            }
            return Ok(index);
        }
        let implicit = target.is_none();
        let target = target.map_or_else(|| fresh(runtime, "LOOP RESULT"), Ok)?;
        if !implicit {
            self.record_binding(&target)?;
        }
        let initial = if kind == AccumulationKind::Sum {
            self.numeric_zero(declaration)?
        } else {
            runtime.nil()
        };
        self.prologue
            .push(vec![binding(runtime, &target, &initial)?]);
        let tail = if kind == AccumulationKind::List {
            let cursor = fresh(runtime, "LOOP RESULT TAIL")?;
            self.temporaries
                .push(binding(runtime, &cursor, &runtime.nil())?);
            Some(cursor)
        } else {
            None
        };
        let index = self.accumulators.len();
        self.accumulators.push(Accumulator {
            kind,
            value: target,
            tail,
        });
        if implicit {
            self.result = Some(index);
        }
        Ok(index)
    }

    fn in_prologue(&self, keyword: &str) -> Result<(), EvalError> {
        if self.main_clauses {
            return Err(invalid(
                "LOOP",
                &format!("{keyword} must precede the main clauses"),
            ));
        }
        Ok(())
    }

    fn leave_when(&self, condition: &Root<'rt>, inverted: bool) -> Expansion<'rt> {
        let leave = call(self.runtime, "GO", once(&self.finish))?;
        if inverted {
            call(
                self.runtime,
                "IF",
                [condition, &self.runtime.nil(), &leave].into_iter(),
            )
        } else {
            call(self.runtime, "IF", [condition, &leave].into_iter())
        }
    }

    fn peek_keyword(&self) -> Option<String> {
        self.forms
            .first()
            .and_then(|form| self.runtime.symbol_name(form))
    }

    fn consume(&mut self, keyword: &str) -> bool {
        if self.peek_keyword().as_deref() != Some(keyword) {
            return false;
        }
        self.forms = &self.forms[1..];
        true
    }

    fn take(&mut self, message: &str) -> Expansion<'rt> {
        let Some((first, rest)) = self.forms.split_first() else {
            return Err(invalid("LOOP", message));
        };
        self.forms = rest;
        Ok(first.clone())
    }

    fn keyword(&mut self) -> Result<String, EvalError> {
        let word = self.take("expects a clause keyword")?;
        self.runtime
            .symbol_name(&word)
            .ok_or_else(|| invalid("LOOP", "expects a clause keyword"))
    }

    /// A FOR variable is either a symbol or a destructuring pattern. A
    /// pattern iterates a fresh variable and binds its parts with
    /// DESTRUCTURING-BIND around the loop body.
    fn iteration_variable(&mut self) -> Result<(Root<'rt>, Option<Root<'rt>>), EvalError> {
        let designator = self.take("expects a loop variable")?;
        if designator.is_nil() {
            return Ok((fresh(self.runtime, "LOOP IGNORED")?, None));
        }
        if self.runtime.heap.is_variable(designator.slot.get()) {
            return Ok((designator, None));
        }
        if self.runtime.cons_fields(&designator).is_some() {
            return Ok((fresh(self.runtime, "LOOP ELEMENT")?, Some(designator)));
        }
        Err(invalid(
            "LOOP",
            "expects a loop variable or a destructuring pattern",
        ))
    }

    fn variable(&mut self) -> Expansion<'rt> {
        let name = self.take("expects a loop variable")?;
        if !self.runtime.heap.is_variable(name.slot.get()) {
            return Err(invalid("LOOP", "expects a simple variable name"));
        }
        Ok(name)
    }

    fn record_binding(&mut self, name: &Root<'rt>) -> Result<(), EvalError> {
        if self.variables.iter().any(|bound| name.same_identity(bound)) {
            return Err(invalid("LOOP", "a variable is bound more than once"));
        }
        self.variables.push(name.clone());
        Ok(())
    }

    fn type_specifier(&mut self) -> Result<Option<Root<'rt>>, EvalError> {
        if self.consume("OF-TYPE")
            || matches!(
                self.peek_keyword().as_deref(),
                Some("FIXNUM" | "FLOAT" | "T" | "NIL")
            )
        {
            self.take("expects a type specifier").map(Some)
        } else {
            Ok(None)
        }
    }

    fn default_initial_value(&self, declaration: Option<&Root<'rt>>) -> Expansion<'rt> {
        let runtime = self.runtime;
        match self.type_name(declaration).as_deref() {
            Some(
                "NUMBER" | "REAL" | "RATIONAL" | "INTEGER" | "FIXNUM" | "SIGNED-BYTE"
                | "UNSIGNED-BYTE" | "BIT",
            ) => Ok(runtime.fixnum(0).unwrap()),
            Some("FLOAT" | "SHORT-FLOAT" | "SINGLE-FLOAT" | "DOUBLE-FLOAT" | "LONG-FLOAT") => {
                self.numeric_zero(declaration)
            }
            _ => Ok(runtime.nil()),
        }
    }

    /// A SUM or COUNT accumulator starts at zero. A declared float type gets
    /// a float zero of that format, so an empty loop still returns a float.
    fn numeric_zero(&self, declaration: Option<&Root<'rt>>) -> Expansion<'rt> {
        let runtime = self.runtime;
        let float = self.type_name(declaration).is_some_and(|name| {
            matches!(
                name.as_str(),
                "FLOAT" | "SHORT-FLOAT" | "SINGLE-FLOAT" | "DOUBLE-FLOAT" | "LONG-FLOAT"
            )
        });
        let zero = runtime.fixnum(0).unwrap();
        if !float {
            return Ok(zero);
        }
        let declaration = declaration.expect("a float type has a specifier");
        let quoted = call(runtime, "QUOTE", once(declaration))?;
        call(runtime, "COERCE", [&zero, &quoted].into_iter())
    }

    fn type_name(&self, declaration: Option<&Root<'rt>>) -> Option<String> {
        let runtime = self.runtime;
        declaration.and_then(|declaration| {
            if let Some((head, _)) = runtime.cons_fields(declaration) {
                runtime.symbol_name(&head)
            } else {
                runtime.symbol_name(declaration)
            }
        })
    }

    fn value(&mut self, it: Option<&Root<'rt>>) -> Expansion<'rt> {
        if let Some(it) = it
            && self.consume("IT")
        {
            return Ok(it.clone());
        }
        self.take("expects a form after its clause keyword")
    }
}

fn complete_forms<'a, 'rt>(
    forms: &mut &'a [Root<'rt>],
    runtime: &'rt Runtime,
    keyword: &str,
) -> Result<&'a [Root<'rt>], EvalError> {
    let count = forms
        .iter()
        .take_while(|form| runtime.cons_fields(form).is_some())
        .count();
    if count == 0 {
        return Err(invalid(
            "LOOP",
            &format!("{keyword} expects a compound form"),
        ));
    }
    let (body, rest) = forms.split_at(count);
    *forms = rest;
    Ok(body)
}

fn input<'rt>(
    runtime: &'rt Runtime,
    bindings: &mut Vec<Root<'rt>>,
    name: &str,
    value: &Root<'rt>,
) -> Expansion<'rt> {
    let temporary = fresh(runtime, name)?;
    bindings.push(binding(runtime, &temporary, value)?);
    Ok(temporary)
}

fn set_direction(direction: &mut Option<bool>, requested: Option<bool>) -> Result<(), EvalError> {
    if let Some(requested) = requested {
        if direction.is_some_and(|previous| previous != requested) {
            return Err(invalid("LOOP", "conflicting arithmetic directions"));
        }
        *direction = Some(requested);
    }
    Ok(())
}

/// Binding pairs for a destructuring pattern, evaluated in parallel against
/// the already-bound temporary holding the pattern's value.
fn collect_destructure_bindings<'rt>(
    runtime: &'rt Runtime,
    pattern: &Root<'rt>,
    expr: &Root<'rt>,
    bindings: &mut Vec<(Root<'rt>, Root<'rt>)>,
) -> Result<(), EvalError> {
    if pattern.is_nil() {
        return Ok(());
    }
    if runtime.heap.is_variable(pattern.slot.get()) {
        bindings.push((pattern.clone(), expr.clone()));
        return Ok(());
    }
    if let Some((head, tail)) = runtime.cons_fields(pattern) {
        if !head.is_nil() {
            let value = call(runtime, "CAR", once(expr))?;
            collect_destructure_bindings(runtime, &head, &value, bindings)?;
        }
        if !tail.is_nil() {
            let value = call(runtime, "CDR", once(expr))?;
            collect_destructure_bindings(runtime, &tail, &value, bindings)?;
        }
        return Ok(());
    }
    Err(invalid("LOOP", "invalid destructuring pattern"))
}

fn collect_destructure_assignments<'rt>(
    runtime: &'rt Runtime,
    pattern: &Root<'rt>,
    expr: &Root<'rt>,
    vars: &mut Vec<(Root<'rt>, bool)>,
    setqs: &mut Vec<Root<'rt>>,
) -> Result<(), EvalError> {
    if pattern.is_nil() {
        return Ok(());
    }
    if runtime.heap.is_variable(pattern.slot.get()) {
        vars.push((pattern.clone(), true));
        setqs.push(call(runtime, "SETQ", [pattern, expr].into_iter())?);
        return Ok(());
    }
    if let Some((car_pat, cdr_pat)) = runtime.cons_fields(pattern) {
        let has_car = !car_pat.is_nil();
        let has_cdr = !cdr_pat.is_nil();
        if !has_car && !has_cdr {
            return Ok(());
        }
        let cursor = if has_car && has_cdr {
            let tmp = fresh(runtime, "LOOP PAIR")?;
            vars.push((tmp.clone(), false));
            setqs.push(call(runtime, "SETQ", [&tmp, expr].into_iter())?);
            tmp
        } else {
            expr.clone()
        };
        if has_car {
            let car_expr = call(runtime, "CAR", once(&cursor))?;
            collect_destructure_assignments(runtime, &car_pat, &car_expr, vars, setqs)?;
        }
        if has_cdr {
            let cdr_expr = call(runtime, "CDR", once(&cursor))?;
            collect_destructure_assignments(runtime, &cdr_pat, &cdr_expr, vars, setqs)?;
        }
        return Ok(());
    }
    Err(invalid("LOOP", "invalid destructuring pattern"))
}
