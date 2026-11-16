use crate::{Root, Runtime};

use super::{Expansion, defclass};

pub(super) fn expand<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    if let Some(name) = forms.first()
        && runtime.heap.is_symbol(name.slot.get())
    {
        crate::compile::declarations::reject_declaration(
            runtime,
            name.slot.get(),
            "DEFINE-CONDITION",
            0,
        )?;
    }
    defclass::expand_condition(runtime, forms)
}
