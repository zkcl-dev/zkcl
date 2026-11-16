use crate::heap::Word;
use crate::printer::{self, PrintControls, PrintDispatchMode};
use crate::vm::printer::PrintContext;
use crate::{EvalError, Root, Runtime};

/// One component of an inspected value. An unbound slot has no value.
pub struct InspectionPart<'rt> {
    pub index: usize,
    pub label: String,
    pub value: Option<Root<'rt>>,
    pub writable: bool,
}

/// A bounded page of an actual Lisp object. Components remain rooted and may
/// be inspected directly without converting them to Lisp source.
pub struct Inspection<'rt> {
    pub summary: String,
    pub total: usize,
    pub parts: Vec<InspectionPart<'rt>>,
}

impl Runtime {
    /// Render a compact, cycle-aware view for debugging tools.
    pub fn preview(&self, value: &Root<'_>) -> Result<String, EvalError> {
        self.validate_inspected_value(value)?;
        let options = PrintControls {
            length: Some(8),
            level: Some(3),
            ..PrintControls::default()
        };
        let package = printer::current_package(self, 0).unwrap_or(crate::package::Package::USER);
        printer::render(
            &PrintContext::host(self),
            package,
            value.slot.get(),
            &options,
            None,
            PrintDispatchMode::Preview,
            0,
        )
        .map_err(|fault| fault.host)
    }

    /// Return the Lisp type specifier for a debugger or inspector display.
    pub fn inspection_type(&self, value: &Root<'_>) -> Result<String, EvalError> {
        let types = self.inspection_call("TYPE-OF", &[value])?;
        self.repr(&types[0]).or_else(|_| self.preview(&types[0]))
    }

    /// Read a page of the object's components in row-major, slot, or storage order.
    /// Instance inspection updates obsolete storage and invokes the Lisp slot
    /// protocol. Hash table keys identify entries and are read-only; their values
    /// are writable. Component indices refer to the object's current contents.
    pub fn inspect_object<'rt>(
        &'rt self,
        object: &Root<'_>,
        start: usize,
        count: usize,
    ) -> Result<Inspection<'rt>, EvalError> {
        self.validate_inspected_value(object)?;
        let mut result = Inspection {
            summary: self.preview(object)?,
            total: 0,
            parts: Vec::new(),
        };
        let end = start.saturating_add(count);
        let word = object.slot.get();
        let mut part = |index, label: String, word: Option<Word>, writable| {
            if (start..end).contains(&index) {
                result.parts.push(InspectionPart {
                    index,
                    label,
                    value: word
                        .filter(|word| *word != Word::UNBOUND)
                        .map(|word| self.root(word)),
                    writable,
                });
            }
        };
        if let Some((car, cdr)) = self.heap.cons_fields(word) {
            result.total = 2;
            part(0, "Car".into(), Some(car), true);
            part(1, "Cdr".into(), Some(cdr), true);
        } else if let Some(info) = self.heap.array_info(word) {
            result.total = info.size;
            for index in start..end.min(info.size) {
                let value = self
                    .heap
                    .array_slot(object.slot.get(), index)
                    .map_err(|error| EvalError::out_of_memory(0, error))?;
                part(index, format!("[{index}]"), value, true);
            }
        } else if let Some(info) = self.heap.hash_table_info(word) {
            result.total = info.count.saturating_mul(2);
            let mut cursor = 0;
            for index in 0..end.min(result.total).div_ceil(2) {
                let Some((key, value)) = self.heap.hash_table_next_entry(word, &mut cursor) else {
                    break;
                };
                part(index * 2, format!("Key {index}"), Some(key), false);
                part(index * 2 + 1, format!("Value {index}"), Some(value), true);
            }
        } else if self.heap.instance_class(word).is_some() {
            let slots = self.inspection_slot_names(object)?;
            result.total = slots.len();
            for (index, name) in slots.into_iter().enumerate().take(end).skip(start) {
                let bound = self.inspection_call("SLOT-BOUNDP", &[object, &name])?;
                let value = if bound[0].is_nil() {
                    None
                } else {
                    self.inspection_call("SLOT-VALUE", &[object, &name])?
                        .into_iter()
                        .next()
                };
                let label = self
                    .heap
                    .symbol_name(name.slot.get())
                    .unwrap_or_else(|| format!("Slot {index}"));
                part(
                    index,
                    label,
                    value.as_ref().map(|value| value.slot.get()),
                    true,
                );
            }
        } else if self.heap.is_symbol(word) {
            result.total = 4;
            let attributes = self.heap.symbol_attributes(word).expect("symbol");
            part(
                0,
                "Value".into(),
                self.heap.symbol_cell(word, false),
                self.heap.is_variable(word),
            );
            part(
                1,
                "Function".into(),
                self.heap
                    .symbol_cell(word, true)
                    .or_else(|| self.heap.macro_expander(word)),
                !attributes.canonical,
            );
            part(
                2,
                "Property list".into(),
                Some(self.symbol_plist(word)),
                true,
            );
            part(
                3,
                "Package".into(),
                self.heap.symbol_home(word).map(|package| package.word()),
                false,
            );
        } else if let Some(code) = self.heap.function_code(word) {
            let info = self.heap.function_info(code).expect("function code");
            let code = self.heap.code_store.get(info.code_id);
            result.total = usize::from(code.lambda_list.is_some()) + code.captures.len();
            let base = usize::from(code.lambda_list.is_some());
            if let Some(lambda_list) = code.lambda_list {
                part(
                    0,
                    "Lambda list".into(),
                    Some(self.heap.function_constant(
                        self.heap.function_code(word).unwrap(),
                        lambda_list as usize,
                    )),
                    false,
                );
            }
            for index in
                start.saturating_sub(base)..end.saturating_sub(base).min(code.captures.len())
            {
                let value = self.heap.capture(word, index).and_then(|capture| {
                    if code.captures[index] == zkcl_bytecode::CaptureKind::MutableCell {
                        self.heap.read_cell(capture)
                    } else {
                        Some(capture)
                    }
                });
                part(
                    base + index,
                    format!("Capture {index}"),
                    value,
                    code.captures[index] == zkcl_bytecode::CaptureKind::MutableCell,
                );
            }
        }
        Ok(result)
    }

    /// Replace a writable inspector component with a value from this runtime.
    /// Instance slots and symbol bindings use their ordinary Lisp write protocols.
    pub fn replace_inspected_part(
        &self,
        object: &Root<'_>,
        index: usize,
        value: &Root<'_>,
    ) -> Result<(), EvalError> {
        self.validate_inspected_value(object)?;
        self.validate_inspected_value(value)?;
        let word = object.slot.get();
        if self.heap.cons_fields(word).is_some() {
            match index {
                0 => self.heap.set_cons_car(word, value.slot.get()),
                1 => self.heap.set_cons_cdr(word, value.slot.get()),
                _ => return Err(EvalError::new(0, "cons component is out of range")),
            }
            return Ok(());
        }
        if self.heap.array_info(word).is_some() {
            if self
                .heap
                .set_array_slot(word, index, value.slot.get())
                .map_err(|error| EvalError::out_of_memory(0, error))?
            {
                return Ok(());
            }
            return Err(EvalError::new(0, "array index or element type is invalid"));
        }
        if let Some(info) = self.heap.hash_table_info(word) {
            if index >= info.count.saturating_mul(2) {
                return Err(EvalError::new(0, "hash table component is out of range"));
            }
            // A key names an entry. Replacing it would also require a collision
            // policy and could reorder every later component in the inspector.
            if index.is_multiple_of(2) {
                return Err(EvalError::new(0, "hash table keys are read-only"));
            }
            let mut cursor = 0;
            let key = (0..=index / 2)
                .filter_map(|_| self.heap.hash_table_next_entry(word, &mut cursor))
                .last()
                .map(|(key, _)| self.root(key))
                .ok_or_else(|| EvalError::new(0, "hash table component is out of range"))?;
            self.inspection_call("(SETF GETHASH)", &[value, &key, object])?;
            return Ok(());
        }
        if self.heap.instance_class(word).is_some() {
            let slots = self.inspection_slot_names(object)?;
            let name = slots
                .get(index)
                .ok_or_else(|| EvalError::new(0, "instance slot is out of range"))?;
            self.inspection_call("(SETF SLOT-VALUE)", &[value, object, name])?;
            return Ok(());
        }
        if self.heap.is_symbol(word) {
            let (function, arguments) = match index {
                0 => ("SET", vec![object, value]),
                1 => ("(SETF SYMBOL-FUNCTION)", vec![value, object]),
                2 => ("(SETF SYMBOL-PLIST)", vec![value, object]),
                3 => return Err(EvalError::new(0, "the symbol package is read-only")),
                _ => return Err(EvalError::new(0, "symbol component is out of range")),
            };
            self.inspection_call(function, &arguments)?;
            return Ok(());
        }
        if let Some(code) = self.heap.function_code(word) {
            let info = self.heap.function_info(code).expect("function code");
            let code = self.heap.code_store.get(info.code_id);
            let base = usize::from(code.lambda_list.is_some());
            let capture_index = index.checked_sub(base);
            if let Some(capture_index) = capture_index
                && code.captures.get(capture_index)
                    == Some(&zkcl_bytecode::CaptureKind::MutableCell)
                && let Some(cell) = self.heap.capture(word, capture_index)
            {
                self.heap
                    .write_cell(cell, value.slot.get())
                    .expect("mutable capture cell");
                return Ok(());
            }
        }
        Err(EvalError::new(0, "this inspector component is read-only"))
    }

    fn inspection_call<'rt>(
        &'rt self,
        name: &str,
        arguments: &[&Root<'_>],
    ) -> Result<Vec<Root<'rt>>, EvalError> {
        let function = self.root(crate::vm::Primitive::named(name).expect("inspector primitive"));
        self.call(&function, arguments)
    }

    fn inspection_slot_names<'rt>(
        &'rt self,
        object: &Root<'_>,
    ) -> Result<Vec<Root<'rt>>, EvalError> {
        // Migration can invoke Lisp and collect. Keep the object and all slot
        // names rooted, and let the same slot protocol used by Lisp own storage.
        self.inspection_call("%UPDATE-OBSOLETE-INSTANCE", &[object])?;
        let class = self
            .heap
            .instance_class(object.slot.get())
            .expect("instance");
        let layout = self
            .heap
            .instance_layout_of(object.slot.get())
            .expect("instance layout");
        let definition = crate::classes::instance_definition(self, class, layout)
            .ok_or_else(|| EvalError::new(0, "instance has no slot definition"))?;
        Ok(definition
            .slots
            .iter()
            .map(|slot| self.root(slot.name.get()))
            .collect())
    }

    fn validate_inspected_value(&self, value: &Root<'_>) -> Result<(), EvalError> {
        if value.belongs_to(self) {
            Ok(())
        } else {
            Err(EvalError::new(0, "object belongs to a different runtime"))
        }
    }

    /// Install a host inspector called by the Common Lisp `INSPECT` function.
    pub fn set_inspector(&self, function: Option<&Root<'_>>) -> Result<(), EvalError> {
        let root = function
            .map(|function| self.resolve_function(function))
            .transpose()?;
        *self.inspector.borrow_mut() = root.map(|root| root.slot);
        Ok(())
    }
}
