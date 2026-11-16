//! Definitions installed by DEFINE-METHOD-COMBINATION and the options accepted by a generic function.
use crate::condition::Fault;
use crate::heap::{RootSlot, Word};
use crate::{Root, Runtime};

use super::{MethodCombinations, method_error};

impl Definition {
    pub(super) fn write_image(
        &self,
        image: &mut crate::image::ImageWriter<'_>,
    ) -> Result<(), crate::EvalError> {
        image.word(self.name.get())?;
        image.word(self.documentation.get())?;
        match &self.body {
            CombinationBody::Short { operator, identity } => {
                image.bool(false);
                image.word(operator.get())?;
                image.bool(*identity);
            }
            CombinationBody::Long {
                bind_options,
                argument_lambda_list,
            } => {
                image.bool(true);
                image.word(bind_options.get())?;
                image.word(argument_lambda_list.get())?;
            }
        }
        Ok(())
    }

    pub(super) fn read_image(
        image: &mut crate::image::ImageReader<'_>,
    ) -> Result<Self, crate::EvalError> {
        let name = image.runtime.root_slot(image.word()?);
        let documentation = image.runtime.root_slot(image.word()?);
        let body = if image.bool()? {
            CombinationBody::Long {
                bind_options: image.runtime.root_slot(image.word()?),
                argument_lambda_list: image.runtime.root_slot(image.word()?),
            }
        } else {
            CombinationBody::Short {
                operator: image.runtime.root_slot(image.word()?),
                identity: image.bool()?,
            }
        };
        Ok(Self {
            name,
            documentation,
            body,
        })
    }
}

pub(super) struct Definition {
    name: RootSlot,
    documentation: RootSlot,
    body: CombinationBody,
}

enum CombinationBody {
    Short {
        operator: RootSlot,
        identity: bool,
    },
    Long {
        bind_options: RootSlot,
        argument_lambda_list: RootSlot,
    },
}

pub(in crate::vm) enum MethodCombinationKind<'a> {
    Standard,
    SimpleBuiltIn {
        operator: &'a RootSlot,
        most_specific_last: bool,
    },
    Short {
        name: RootSlot,
        operator: RootSlot,
        identity: bool,
        most_specific_last: bool,
    },
    Long {
        bind_options: RootSlot,
        argument_lambda_list: RootSlot,
        options: RootSlot,
    },
}

impl MethodCombinations {
    pub(super) fn most_specific_last(
        &self,
        order: Word,
        at: usize,
        error: &'static str,
    ) -> Result<bool, Fault> {
        if order == self.order[0].get() {
            Ok(false)
        } else if order == self.order[1].get() {
            Ok(true)
        } else {
            Err(method_error(at, error))
        }
    }

    pub(super) fn set_definition_documentation(
        &self,
        runtime: &Runtime,
        name: Word,
        documentation: Word,
    ) {
        if let Some(definition) = self
            .definitions
            .borrow_mut()
            .iter_mut()
            .find(|definition| definition.name.get() == name)
        {
            definition.documentation = runtime.root_slot(documentation);
        }
    }

    /// Resolve the DEFGENERIC :method-combination value of a generic function.
    pub(in crate::vm) fn parse(
        &self,
        runtime: &Runtime,
        combination: Word,
        at: usize,
    ) -> Result<MethodCombinationKind<'_>, Fault> {
        let Some((name, options)) = runtime.heap.cons_fields(combination) else {
            return Err(method_error(at, "the method combination requires a name"));
        };
        if name == self.standard.get() {
            if options != Word::NIL {
                return Err(method_error(
                    at,
                    "the STANDARD method combination takes no options",
                ));
            }
            return Ok(MethodCombinationKind::Standard);
        }
        if let Some(definition) = self
            .definitions
            .borrow()
            .iter()
            .find(|def| def.name.get() == name)
        {
            return Ok(match &definition.body {
                CombinationBody::Short { operator, identity } => {
                    let most_specific_last = if options == Word::NIL {
                        false
                    } else {
                        let (order, tail) = runtime
                            .heap
                            .cons_fields(options)
                            .ok_or_else(|| method_error(at, "invalid method combination option"))?;
                        let last =
                            self.most_specific_last(order, at, "invalid method combination order")?;
                        if tail != Word::NIL {
                            return Err(method_error(
                                at,
                                "the method combination takes at most one argument",
                            ));
                        }
                        last
                    };
                    MethodCombinationKind::Short {
                        name: runtime.root_slot(name),
                        operator: runtime.root_slot(operator.get()),
                        identity: *identity,
                        most_specific_last,
                    }
                }
                CombinationBody::Long {
                    bind_options,
                    argument_lambda_list,
                } => MethodCombinationKind::Long {
                    argument_lambda_list: runtime.root_slot(argument_lambda_list.get()),
                    bind_options: runtime.root_slot(bind_options.get()),
                    options: runtime.root_slot(options),
                },
            });
        }
        let Some(operator) = self.operators.iter().find(|symbol| symbol.get() == name) else {
            return Err(method_error(at, "unknown method combination type"));
        };
        let mut most_specific_last = false;
        let mut option = options;
        while option != Word::NIL {
            let Some((name, more)) = runtime.heap.cons_fields(option) else {
                return Err(method_error(at, "invalid method combination option"));
            };
            most_specific_last =
                self.most_specific_last(name, at, "invalid method combination option")?;
            option = more;
        }
        Ok(MethodCombinationKind::SimpleBuiltIn {
            operator,
            most_specific_last,
        })
    }
}

pub(super) fn define_short_method_combination<'rt>(
    runtime: &'rt Runtime,
    arguments: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let [name, documentation, operator, identity] = arguments else {
        return Err(method_error(at, "invalid method combination definition"));
    };
    if !runtime.heap.is_symbol(name.slot.get()) {
        return Err(method_error(
            at,
            "the method combination name must be a symbol",
        ));
    }
    if !runtime.heap.is_symbol(operator.slot.get()) {
        return Err(method_error(
            at,
            "the method combination operator must be a symbol",
        ));
    }
    if documentation.slot.get() != Word::NIL && !runtime.heap.is_string(documentation.slot.get()) {
        return Err(method_error(
            at,
            "the method combination documentation must be a string",
        ));
    }
    let body = CombinationBody::Short {
        operator: runtime.root_slot(operator.slot.get()),
        identity: !identity.is_nil(),
    };
    register_definition(runtime, name, documentation, body);
    Ok(vec![name.clone()])
}

pub(super) fn define_long_method_combination<'rt>(
    runtime: &'rt Runtime,
    arguments: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let [name, bind_options, documentation, argument_lambda_list] = arguments else {
        return Err(method_error(at, "invalid method combination definition"));
    };
    if !runtime.heap.is_symbol(name.slot.get()) {
        return Err(method_error(
            at,
            "the method combination name must be a symbol",
        ));
    }
    if !crate::vm::is_function(runtime, bind_options.slot.get()) {
        return Err(method_error(
            at,
            "the method combination option parser must be a function",
        ));
    }
    if documentation.slot.get() != Word::NIL && !runtime.heap.is_string(documentation.slot.get()) {
        return Err(method_error(
            at,
            "the method combination documentation must be a string",
        ));
    }
    let body = CombinationBody::Long {
        argument_lambda_list: runtime.root_slot(argument_lambda_list.slot.get()),
        bind_options: runtime.root_slot(bind_options.slot.get()),
    };
    register_definition(runtime, name, documentation, body);
    Ok(vec![name.clone()])
}

fn register_definition(
    runtime: &Runtime,
    name: &Root<'_>,
    documentation: &Root<'_>,
    body: CombinationBody,
) {
    let combinations = runtime
        .method_combinations
        .get()
        .expect("initialized runtime");
    let definition = Definition {
        name: runtime.root_slot(name.slot.get()),
        documentation: runtime.root_slot(documentation.slot.get()),
        body,
    };
    let mut definitions = combinations.definitions.borrow_mut();
    if let Some(current) = definitions
        .iter_mut()
        .find(|def| def.name.get() == name.slot.get())
    {
        *current = definition;
    } else {
        definitions.push(definition);
    }
}
