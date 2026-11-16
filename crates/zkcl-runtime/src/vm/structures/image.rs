//! Structure metadata refers to the same object graph as Lisp bindings.

use super::*;
use crate::image::{ImageReader, ImageWriter};

impl Registry {
    pub(crate) fn write_image(&self, image: &mut ImageWriter<'_>) -> Result<(), EvalError> {
        image.usize(self.definitions.len());
        for (&id, definition) in &self.definitions {
            image.usize(id);
            match &definition.variant {
                None => image.usize(0),
                Some(VariantRole::Constructor) => image.usize(1),
                Some(VariantRole::Family(cases)) => {
                    image.usize(2);
                    image.usize(cases.len());
                    for &case in cases {
                        image.usize(case);
                    }
                }
            }
            image.word(definition.name.get())?;
            image.bool(definition.parent.is_some());
            if let Some(parent) = &definition.parent {
                image.word(parent.get())?;
            }
            image.usize(definition.slots.len());
            for slot in &definition.slots {
                image.word(slot.name.get())?;
                image.word(slot.initform.get())?;
                image.word(slot.type_specifier.get())?;
                image.word(slot.reader.get())?;
                image.bool(slot.read_only);
                image.bool(slot.direct);
                write_cache(image, &slot.direct_metaobject)?;
                write_cache(image, &slot.effective_metaobject)?;
            }
            image.word(definition.keyword_constructor.get())?;
            image.word(definition.documentation.get())?;
        }
        image.usize(self.typed.len());
        for (&id, definition) in &self.typed {
            image.usize(id);
            image.bool(definition.is_list);
            image.usize(definition.total_len);
            image.usize(definition.prefix_names.len());
            for (offset, name) in &definition.prefix_names {
                image.usize(*offset);
                image.word(name.get())?;
            }
            image.usize(definition.slots.len());
            for slot in &definition.slots {
                image.word(slot.name.get())?;
                image.word(slot.initform.get())?;
                image.word(slot.type_specifier.get())?;
                image.word(slot.reader.get())?;
                image.bool(slot.read_only);
                image.usize(slot.offset);
            }
        }
        image.usize(self.readers.len());
        for (&reader, &(owner, slot)) in &self.readers {
            image.usize(reader);
            image.usize(owner);
            image.usize(slot);
        }
        Ok(())
    }

    pub(crate) fn read_image(image: &mut ImageReader<'_>) -> Result<Self, EvalError> {
        let mut registry = Self::default();
        for _ in 0..image.usize()? {
            let id = image.usize()?;
            let variant = match image.usize()? {
                0 => None,
                1 => Some(VariantRole::Constructor),
                2 => {
                    let mut cases = Vec::new();
                    for _ in 0..image.usize()? {
                        cases.push(image.usize()?);
                    }
                    Some(VariantRole::Family(cases))
                }
                _ => return Err(EvalError::new(0, "invalid image structure variant")),
            };
            let name = image.runtime.root_slot(image.word()?);
            let parent = image
                .bool()?
                .then(|| image.word().map(|w| image.runtime.root_slot(w)))
                .transpose()?;
            let mut slots = Vec::new();
            for _ in 0..image.usize()? {
                slots.push(SlotDefinition {
                    name: image.runtime.root_slot(image.word()?),
                    initform: image.runtime.root_slot(image.word()?),
                    type_specifier: image.runtime.root_slot(image.word()?),
                    reader: image.runtime.root_slot(image.word()?),
                    read_only: image.bool()?,
                    direct: image.bool()?,
                    direct_metaobject: read_cache(image)?,
                    effective_metaobject: read_cache(image)?,
                });
            }
            let definition = StructureDefinition {
                variant,
                name,
                parent,
                slots: slots.into_boxed_slice(),
                keyword_constructor: image.runtime.root_slot(image.word()?),
                documentation: image.runtime.root_slot(image.word()?),
            };
            if registry
                .definitions
                .insert(id, Rc::new(definition))
                .is_some()
            {
                return Err(EvalError::new(0, "duplicate image structure definition"));
            }
        }
        for _ in 0..image.usize()? {
            let id = image.usize()?;
            let is_list = image.bool()?;
            let total_len = image.usize()?;
            let mut prefix_names = Vec::new();
            for _ in 0..image.usize()? {
                prefix_names.push((image.usize()?, image.runtime.root_slot(image.word()?)));
            }
            let mut slots = Vec::new();
            for _ in 0..image.usize()? {
                slots.push(TypedSlotDefinition {
                    name: image.runtime.root_slot(image.word()?),
                    initform: image.runtime.root_slot(image.word()?),
                    type_specifier: image.runtime.root_slot(image.word()?),
                    reader: image.runtime.root_slot(image.word()?),
                    read_only: image.bool()?,
                    offset: image.usize()?,
                });
            }
            let definition = TypedStructureDefinition {
                is_list,
                total_len,
                prefix_names: prefix_names.into_boxed_slice(),
                slots: slots.into_boxed_slice(),
            };
            if registry.definitions.contains_key(&id)
                || registry.typed.insert(id, Rc::new(definition)).is_some()
            {
                return Err(EvalError::new(0, "duplicate image structure definition"));
            }
        }
        for _ in 0..image.usize()? {
            let reader = image.usize()?;
            let owner = image.usize()?;
            let index = image.usize()?;
            if !registry
                .definitions
                .get(&owner)
                .is_some_and(|d| index < d.slots.len())
                || registry.readers.insert(reader, (owner, index)).is_some()
            {
                return Err(EvalError::new(0, "invalid image structure reader"));
            }
        }
        Ok(registry)
    }
}

fn write_cache(image: &mut ImageWriter<'_>, value: &OnceCell<RootSlot>) -> Result<(), EvalError> {
    image.bool(value.get().is_some());
    if let Some(value) = value.get() {
        image.word(value.get())?;
    }
    Ok(())
}

fn read_cache(image: &mut ImageReader<'_>) -> Result<OnceCell<RootSlot>, EvalError> {
    Ok(if image.bool()? {
        OnceCell::from(image.runtime.root_slot(image.word()?))
    } else {
        OnceCell::new()
    })
}
