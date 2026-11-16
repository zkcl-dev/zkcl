use super::*;
use crate::image::{ImageReader, ImageWriter};

impl MethodCombinations {
    pub(crate) fn write_image(&self, image: &mut ImageWriter<'_>) -> Result<(), EvalError> {
        for (name, function) in &self.effective_method_macros {
            image.string(name.spelling());
            image.word(*function)?;
        }
        image.word(self.method_group_trampoline.get())?;
        write_array(image, &self.primary_trampolines)?;
        image.word(self.check_next_arguments)?;
        image.word(self.combine.get())?;
        image.word(self.standard.get())?;
        write_array(image, &self.auxiliary)?;
        write_array(image, &self.operators)?;
        write_array(image, &self.order)?;
        let definitions = self.definitions.borrow();
        image.usize(definitions.len());
        for definition in definitions.iter() {
            definition.write_image(image)?;
        }
        image.word(self.effective_method_template.get())?;
        image.word(self.make_method_template.get())
    }

    pub(crate) fn read_image(image: &mut ImageReader<'_>) -> Result<Self, EvalError> {
        let mut read_macro = || -> Result<_, EvalError> {
            let name = Name::common_lisp(&image.string()?);
            let function = image.word()?;
            Ok((name, function))
        };
        let effective_method_macros = [read_macro()?, read_macro()?];
        let method_group_trampoline = image.runtime.root_slot(image.word()?);
        let primary_trampolines = read_array(image)?;
        let check_next_arguments = image.word()?;
        let combine = image.runtime.root_slot(image.word()?);
        let standard = image.runtime.root_slot(image.word()?);
        let auxiliary = read_array(image)?;
        let operators = read_array(image)?;
        let order = read_array(image)?;
        let mut definitions = Vec::new();
        for _ in 0..image.usize()? {
            definitions.push(Definition::read_image(image)?);
        }
        Ok(Self {
            effective_method_macros,
            method_group_trampoline,
            primary_trampolines,
            check_next_arguments,
            combine,
            standard,
            auxiliary,
            operators,
            order,
            definitions: definitions.into(),
            effective_method_template: image.runtime.root_slot(image.word()?),
            make_method_template: image.runtime.root_slot(image.word()?),
        })
    }
}

fn write_array(image: &mut ImageWriter<'_>, roots: &[RootSlot]) -> Result<(), EvalError> {
    for root in roots {
        image.word(root.get())?;
    }
    Ok(())
}

fn read_array<const N: usize>(image: &mut ImageReader<'_>) -> Result<[RootSlot; N], EvalError> {
    let mut roots = Vec::new();
    for _ in 0..N {
        roots.push(image.runtime.root_slot(image.word()?));
    }
    Ok(roots.try_into().ok().expect("fixed number of image roots"))
}
