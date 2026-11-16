//! Runtime class metadata. Lisp values use the image's one shared object graph;
//! Rust metadata aliases also survive, because class publication compares them.

use super::*;
use crate::image::{ImageReader, ImageWriter};

type Accessors = std::cell::RefCell<Vec<(RootSlot, RootSlot)>>;
type SlotCache = OnceCell<RootSlot>;

struct Writer<'a, 'b> {
    image: &'a mut ImageWriter<'b>,
    aliases: HashMap<(u8, usize), usize>,
}

#[derive(Default)]
struct Aliases {
    roots: HashMap<usize, Rc<RootSlot>>,
    direct: HashMap<usize, Rc<[DirectSlot]>>,
    effective: HashMap<usize, Rc<[EffectiveSlot]>>,
    declarations: HashMap<usize, Rc<ClassDeclaration>>,
    registered: HashMap<usize, Rc<RegisteredClass>>,
    definitions: HashMap<usize, Rc<ClassDefinition>>,
    shared: HashMap<usize, Rc<SharedSlot>>,
    accessors: HashMap<usize, Rc<Accessors>>,
    precedence: HashMap<usize, Rc<[Rc<RootSlot>]>>,
    caches: HashMap<usize, Rc<SlotCache>>,
}

struct Reader<'a, 'b> {
    image: &'a mut ImageReader<'b>,
    aliases: Aliases,
}

fn invalid(message: &str) -> EvalError {
    EvalError::new(0, message)
}

macro_rules! read_alias {
    ($self:ident, $field:ident, $body:block) => {{
        let id = $self.image.usize()?;
        if !$self.image.bool()? {
            return $self
                .aliases
                .$field
                .get(&id)
                .cloned()
                .ok_or_else(|| invalid("invalid image class metadata reference"));
        }
        if $self.aliases.$field.contains_key(&id) {
            return Err(invalid("duplicate image class metadata reference"));
        }
        let value = $body;
        $self.aliases.$field.insert(id, Rc::clone(&value));
        Ok(value)
    }};
}

impl Registry {
    pub(crate) fn write_image(&self, image: &mut ImageWriter<'_>) -> Result<(), EvalError> {
        Writer {
            image,
            aliases: HashMap::new(),
        }
        .registry(self)
    }

    pub(crate) fn read_image(image: &mut ImageReader<'_>) -> Result<Self, EvalError> {
        Reader {
            image,
            aliases: Aliases::default(),
        }
        .registry()
    }
}

impl Writer<'_, '_> {
    fn reference<T: ?Sized>(&mut self, kind: u8, value: &Rc<T>) -> bool {
        let key = (kind, Rc::as_ptr(value) as *const () as usize);
        if let Some(&id) = self.aliases.get(&key) {
            self.image.usize(id);
            self.image.bool(false);
            true
        } else {
            let id = self.aliases.len();
            self.aliases.insert(key, id);
            self.image.usize(id);
            self.image.bool(true);
            false
        }
    }

    fn map<T>(
        &mut self,
        values: &HashMap<usize, T>,
        mut write: impl FnMut(&mut Self, &T) -> Result<(), EvalError>,
    ) -> Result<(), EvalError> {
        self.image.usize(values.len());
        let mut entries: Vec<_> = values.iter().collect();
        entries.sort_by_key(|(id, _)| **id);
        for (&id, value) in entries {
            self.image.usize(id);
            write(self, value)?;
        }
        Ok(())
    }

    fn ids(&mut self, values: impl ExactSizeIterator<Item = usize>) {
        self.image.usize(values.len());
        for value in values {
            self.image.usize(value);
        }
    }

    fn registry(&mut self, value: &Registry) -> Result<(), EvalError> {
        self.map(&value.objects, |s, v| s.image.word(v.get()))?;
        self.map(&value.associations, |s, v| {
            s.image.bool(v.is_some());
            if let Some(v) = v {
                s.image.word(v.get())?;
            }
            Ok(())
        })?;
        self.ids(value.claimed_names.iter().copied());
        self.map(&value.object_definitions, Self::definition)?;
        self.map(&value.system_precedence, Self::precedence)?;
        self.map(&value.subclasses, |s, v| {
            s.ids(v.iter().copied());
            Ok(())
        })?;
        self.map(&value.subclass_edits, |s, edits| {
            s.map(edits, |s, (class, add)| {
                s.root(class)?;
                s.image.bool(*add);
                Ok(())
            })
        })?;
        self.map(&value.prototypes, |s, v| s.image.word(v.get()))?;
        self.image.usize(value.pending.len());
        for pending in &value.pending {
            self.update(pending)?;
        }
        self.image.usize(value.next_serial);
        self.image.usize(value.builtin_condition_direct_slots.len());
        for (class, slots) in &value.builtin_condition_direct_slots {
            self.image.usize(*class as usize);
            self.direct(slots)?;
        }
        self.image
            .usize(value.builtin_condition_effective_slots.len());
        for (class, slots) in &value.builtin_condition_effective_slots {
            self.image.usize(*class as usize);
            self.effective(slots)?;
        }
        self.map(&value.object_declarations, Self::registered)
    }

    fn root(&mut self, value: &Rc<RootSlot>) -> Result<(), EvalError> {
        if self.reference(0, value) {
            return Ok(());
        }
        self.image.word(value.get())
    }

    fn roots(&mut self, values: &[Rc<RootSlot>]) -> Result<(), EvalError> {
        self.image.usize(values.len());
        for value in values {
            self.root(value)?;
        }
        Ok(())
    }

    fn optional_root(&mut self, value: &Option<Rc<RootSlot>>) -> Result<(), EvalError> {
        self.image.bool(value.is_some());
        if let Some(value) = value {
            self.root(value)?;
        }
        Ok(())
    }

    fn cache(&mut self, value: &OnceCell<RootSlot>) -> Result<(), EvalError> {
        self.image.bool(value.get().is_some());
        if let Some(value) = value.get() {
            self.image.word(value.get())?;
        }
        Ok(())
    }

    fn slot_cache(&mut self, value: &Rc<SlotCache>) -> Result<(), EvalError> {
        if self.reference(9, value) {
            return Ok(());
        }
        self.cache(value)
    }

    fn direct(&mut self, value: &Rc<[DirectSlot]>) -> Result<(), EvalError> {
        if self.reference(1, value) {
            return Ok(());
        }
        self.image.usize(value.len());
        for slot in value.iter() {
            self.root(&slot.name)?;
            self.roots(&slot.initargs)?;
            self.optional_root(&slot.initform)?;
            self.optional_root(&slot.initform_source)?;
            self.optional_root(&slot.readers)?;
            self.optional_root(&slot.writers)?;
            self.cache(&slot.metaobject)?;
            match &slot.allocation {
                SlotKind::Instance => self.image.usize(0),
                SlotKind::Class => self.image.usize(1),
                SlotKind::Custom(value) => {
                    self.image.usize(2);
                    self.root(value)?;
                }
            }
            self.optional_root(&slot.type_specifier)?;
            self.optional_root(&slot.documentation)?;
        }
        Ok(())
    }

    fn effective(&mut self, value: &Rc<[EffectiveSlot]>) -> Result<(), EvalError> {
        if self.reference(2, value) {
            return Ok(());
        }
        self.image.usize(value.len());
        for slot in value.iter() {
            self.root(&slot.name)?;
            match &slot.allocation {
                SlotAllocation::Instance(index) => {
                    self.image.usize(0);
                    self.image.usize(*index);
                }
                SlotAllocation::Class(shared) => {
                    self.image.usize(1);
                    self.shared(shared)?;
                }
                SlotAllocation::Custom(value) => {
                    self.image.usize(2);
                    self.root(value)?;
                }
            }
            self.roots(&slot.initargs)?;
            self.optional_root(&slot.initform)?;
            self.optional_root(&slot.initform_source)?;
            self.slot_cache(&slot.metaobject)?;
            self.roots(&slot.type_constraints)?;
            self.optional_root(&slot.documentation)?;
        }
        Ok(())
    }

    fn defaults(&mut self, values: &[DefaultInitarg]) -> Result<(), EvalError> {
        self.image.usize(values.len());
        for value in values {
            self.root(&value.name)?;
            self.root(&value.initform)?;
            self.root(&value.initform_source)?;
        }
        Ok(())
    }

    fn declaration(&mut self, value: &Rc<ClassDeclaration>) -> Result<(), EvalError> {
        if self.reference(3, value) {
            return Ok(());
        }
        self.image.bool(value.kind == RuntimeClassKind::Condition);
        self.root(&value.name)?;
        self.direct(&value.slots)?;
        self.defaults(&value.default_initargs)?;
        self.optional_root(&value.documentation)?;
        self.optional_root(&value.report)
    }

    fn registered(&mut self, value: &Rc<RegisteredClass>) -> Result<(), EvalError> {
        if self.reference(4, value) {
            return Ok(());
        }
        self.root(&value.class)?;
        self.roots(&value.direct_superclasses)?;
        self.declaration(&value.declaration)?;
        self.image.bool(value.active);
        self.map(&value.shared_slots.borrow(), Self::shared)?;
        self.accessors(&value.accessors)
    }

    fn definition(&mut self, value: &Rc<ClassDefinition>) -> Result<(), EvalError> {
        if self.reference(5, value) {
            return Ok(());
        }
        self.image.bool(value.kind == RuntimeClassKind::Condition);
        self.effective(&value.slots)?;
        self.map(&value.slot_indices, |s, v| {
            s.image.usize(*v);
            Ok(())
        })?;
        self.image.usize(match value.representation {
            Representation::Instance => 0,
            Representation::FuncallableInstance => 1,
            Representation::GenericFunction => 2,
            Representation::Method => 3,
            Representation::Specializer => 4,
            Representation::DirectSlot => 5,
            Representation::EffectiveSlot => 6,
            Representation::StandardClass => 7,
            Representation::FuncallableStandardClass => 8,
            Representation::ForwardReferencedClass => 9,
        });
        self.image.bool(value.prototype.get().is_some());
        if let Some(prototype) = value.prototype.get() {
            self.root(prototype)?;
        }
        self.defaults(&value.default_initargs)?;
        self.image.usize(value.instance_slots);
        self.root(&value.instance_layout)?;
        self.optional_root(&value.documentation)?;
        self.optional_root(&value.report)?;
        self.precedence(&value.precedence)
    }

    fn shared(&mut self, value: &Rc<SharedSlot>) -> Result<(), EvalError> {
        if self.reference(6, value) {
            return Ok(());
        }
        self.root(&value.owner)?;
        self.root(&value.cell)?;
        self.image.bool(value.initialized.get());
        Ok(())
    }

    fn accessors(&mut self, value: &Rc<Accessors>) -> Result<(), EvalError> {
        if self.reference(7, value) {
            return Ok(());
        }
        let values = value.borrow();
        self.image.usize(values.len());
        for (generic, method) in values.iter() {
            self.image.word(generic.get())?;
            self.image.word(method.get())?;
        }
        Ok(())
    }

    fn precedence(&mut self, value: &Rc<[Rc<RootSlot>]>) -> Result<(), EvalError> {
        if self.reference(8, value) {
            return Ok(());
        }
        self.roots(value)
    }

    fn optional_registered(
        &mut self,
        value: &Option<Rc<RegisteredClass>>,
    ) -> Result<(), EvalError> {
        self.image.bool(value.is_some());
        if let Some(value) = value {
            self.registered(value)?;
        }
        Ok(())
    }

    fn update(&mut self, value: &ClassUpdate) -> Result<(), EvalError> {
        self.registered(&value.registered)?;
        self.optional_registered(&value.previous)?;
        self.image.usize(value.forward_classes.len());
        for (id, class) in &value.forward_classes {
            self.image.usize(*id);
            self.root(class)?;
        }
        self.ids(value.affected.iter().copied());
        self.map(&value.definitions.borrow(), |s, value| {
            s.image.bool(value.is_some());
            if let Some(value) = value {
                s.definition(value)?;
            }
            Ok(())
        })?;
        self.map(&value.inputs, Self::optional_registered)?;
        self.ids(value.finalized.borrow().iter().copied());
        let shared = value.computed_shared.borrow();
        self.image.usize(shared.len());
        for (&(owner, name), cell) in shared.iter() {
            self.image.usize(owner);
            self.image.usize(name);
            self.shared(cell)?;
        }
        Ok(())
    }
}

impl Reader<'_, '_> {
    fn map<T>(
        &mut self,
        mut read: impl FnMut(&mut Self) -> Result<T, EvalError>,
    ) -> Result<HashMap<usize, T>, EvalError> {
        let mut values = HashMap::new();
        for _ in 0..self.image.usize()? {
            let id = self.image.usize()?;
            let value = read(self)?;
            if values.insert(id, value).is_some() {
                return Err(invalid("duplicate image class registry key"));
            }
        }
        Ok(values)
    }

    fn ids(&mut self) -> Result<Vec<usize>, EvalError> {
        let mut ids = Vec::new();
        for _ in 0..self.image.usize()? {
            ids.push(self.image.usize()?);
        }
        Ok(ids)
    }

    fn registry(&mut self) -> Result<Registry, EvalError> {
        let objects = self.map(|s| Ok(self.image.runtime.root_slot(s.image.word()?)))?;
        let associations = self.map(|s| {
            s.image
                .bool()?
                .then(|| s.image.word().map(|w| s.image.runtime.root_slot(w)))
                .transpose()
        })?;
        let claimed_names = self.ids()?.into_iter().collect();
        let object_definitions = self.map(Self::definition)?;
        let system_precedence = self.map(Self::precedence)?;
        let subclasses = self.map(Self::ids)?;
        let subclass_edits = self.map(|s| s.map(|s| Ok((s.root()?, s.image.bool()?))))?;
        let prototypes = self.map(|s| Ok(self.image.runtime.root_slot(s.image.word()?)))?;
        let mut pending = Vec::new();
        for _ in 0..self.image.usize()? {
            pending.push(Rc::new(self.update()?));
        }
        let next_serial = self.image.usize()?;
        let mut builtin_condition_direct_slots = HashMap::new();
        for _ in 0..self.image.usize()? {
            let class = self.condition_class()?;
            let slots = self.direct()?;
            if builtin_condition_direct_slots
                .insert(class, slots)
                .is_some()
            {
                return Err(invalid("duplicate image condition slots"));
            }
        }
        let mut builtin_condition_effective_slots = HashMap::new();
        for _ in 0..self.image.usize()? {
            let class = self.condition_class()?;
            let slots = self.effective()?;
            if builtin_condition_effective_slots
                .insert(class, slots)
                .is_some()
            {
                return Err(invalid("duplicate image condition slots"));
            }
        }
        let object_declarations = self.map(Self::registered)?;
        Ok(Registry {
            objects,
            associations,
            claimed_names,
            object_definitions,
            system_precedence,
            subclasses,
            subclass_edits,
            prototypes,
            pending,
            next_serial,
            builtin_condition_direct_slots,
            builtin_condition_effective_slots,
            object_declarations,
        })
    }

    fn condition_class(&mut self) -> Result<Class, EvalError> {
        let id = self.image.usize()?;
        Class::all()
            .find(|&class| class as usize == id)
            .ok_or_else(|| invalid("invalid image condition class"))
    }

    fn root(&mut self) -> Result<Rc<RootSlot>, EvalError> {
        read_alias!(self, roots, { Rc::new(self.image.runtime.root_slot(self.image.word()?)) })
    }

    fn roots(&mut self) -> Result<Vec<Rc<RootSlot>>, EvalError> {
        let mut values = Vec::new();
        for _ in 0..self.image.usize()? {
            values.push(self.root()?);
        }
        Ok(values)
    }

    fn optional_root(&mut self) -> Result<Option<Rc<RootSlot>>, EvalError> {
        self.image.bool()?.then(|| self.root()).transpose()
    }

    fn cache(&mut self) -> Result<SlotCache, EvalError> {
        Ok(if self.image.bool()? {
            OnceCell::from(self.image.runtime.root_slot(self.image.word()?))
        } else {
            OnceCell::new()
        })
    }

    fn slot_cache(&mut self) -> Result<Rc<SlotCache>, EvalError> {
        read_alias!(self, caches, { Rc::new(self.cache()?) })
    }

    fn direct(&mut self) -> Result<Rc<[DirectSlot]>, EvalError> {
        read_alias!(self, direct, {
            let mut slots = Vec::new();
            for _ in 0..self.image.usize()? {
                let name = self.root()?;
                let initargs = self.roots()?;
                let initform = self.optional_root()?;
                let initform_source = self.optional_root()?;
                let readers = self.optional_root()?;
                let writers = self.optional_root()?;
                let metaobject = self.cache()?;
                let allocation = match self.image.usize()? {
                    0 => SlotKind::Instance,
                    1 => SlotKind::Class,
                    2 => SlotKind::Custom(self.root()?),
                    _ => return Err(invalid("invalid image direct slot allocation")),
                };
                slots.push(DirectSlot {
                    name,
                    initargs,
                    initform,
                    initform_source,
                    readers,
                    writers,
                    metaobject,
                    allocation,
                    type_specifier: self.optional_root()?,
                    documentation: self.optional_root()?,
                });
            }
            Rc::from(slots)
        })
    }

    fn effective(&mut self) -> Result<Rc<[EffectiveSlot]>, EvalError> {
        read_alias!(self, effective, {
            let mut slots = Vec::new();
            for _ in 0..self.image.usize()? {
                let name = self.root()?;
                let allocation = match self.image.usize()? {
                    0 => SlotAllocation::Instance(self.image.usize()?),
                    1 => SlotAllocation::Class(self.shared()?),
                    2 => SlotAllocation::Custom(self.root()?),
                    _ => return Err(invalid("invalid image effective slot allocation")),
                };
                slots.push(EffectiveSlot {
                    name,
                    allocation,
                    initargs: self.roots()?,
                    initform: self.optional_root()?,
                    initform_source: self.optional_root()?,
                    metaobject: self.slot_cache()?,
                    type_constraints: self.roots()?,
                    documentation: self.optional_root()?,
                });
            }
            Rc::from(slots)
        })
    }

    fn defaults(&mut self) -> Result<Vec<DefaultInitarg>, EvalError> {
        let mut values = Vec::new();
        for _ in 0..self.image.usize()? {
            values.push(DefaultInitarg {
                name: self.root()?,
                initform: self.root()?,
                initform_source: self.root()?,
            });
        }
        Ok(values)
    }

    fn kind(&mut self) -> Result<RuntimeClassKind, EvalError> {
        Ok(if self.image.bool()? {
            RuntimeClassKind::Condition
        } else {
            RuntimeClassKind::Standard
        })
    }

    fn declaration(&mut self) -> Result<Rc<ClassDeclaration>, EvalError> {
        read_alias!(self, declarations, {
            Rc::new(ClassDeclaration {
                kind: self.kind()?,
                name: self.root()?,
                slots: self.direct()?,
                default_initargs: self.defaults()?,
                documentation: self.optional_root()?,
                report: self.optional_root()?,
            })
        })
    }

    fn registered(&mut self) -> Result<Rc<RegisteredClass>, EvalError> {
        read_alias!(self, registered, {
            Rc::new(RegisteredClass {
                class: self.root()?,
                direct_superclasses: self.roots()?,
                declaration: self.declaration()?,
                active: self.image.bool()?,
                shared_slots: self.map(Self::shared)?.into(),
                accessors: self.accessors()?,
            })
        })
    }

    fn definition(&mut self) -> Result<Rc<ClassDefinition>, EvalError> {
        read_alias!(self, definitions, {
            let kind = self.kind()?;
            let slots = self.effective()?;
            let slot_indices = self.map(|s| s.image.usize())?;
            if slot_indices.values().any(|&index| index >= slots.len()) {
                return Err(invalid("invalid image class slot index"));
            }
            let representation = match self.image.usize()? {
                0 => Representation::Instance,
                1 => Representation::FuncallableInstance,
                2 => Representation::GenericFunction,
                3 => Representation::Method,
                4 => Representation::Specializer,
                5 => Representation::DirectSlot,
                6 => Representation::EffectiveSlot,
                7 => Representation::StandardClass,
                8 => Representation::FuncallableStandardClass,
                9 => Representation::ForwardReferencedClass,
                _ => return Err(invalid("invalid image class representation")),
            };
            let prototype = self
                .optional_root()?
                .map_or_else(OnceCell::new, OnceCell::from);
            Rc::new(ClassDefinition {
                kind,
                slots,
                slot_indices,
                representation,
                prototype,
                default_initargs: self.defaults()?,
                instance_slots: self.image.usize()?,
                instance_layout: self.root()?,
                documentation: self.optional_root()?,
                report: self.optional_root()?,
                precedence: self.precedence()?,
            })
        })
    }

    fn shared(&mut self) -> Result<Rc<SharedSlot>, EvalError> {
        read_alias!(self, shared, {
            Rc::new(SharedSlot {
                owner: self.root()?,
                cell: self.root()?,
                initialized: self.image.bool()?.into(),
            })
        })
    }

    fn accessors(&mut self) -> Result<Rc<Accessors>, EvalError> {
        read_alias!(self, accessors, {
            let mut values = Vec::new();
            for _ in 0..self.image.usize()? {
                values.push((
                    self.image.runtime.root_slot(self.image.word()?),
                    self.image.runtime.root_slot(self.image.word()?),
                ));
            }
            Rc::new(values.into())
        })
    }

    fn precedence(&mut self) -> Result<Rc<[Rc<RootSlot>]>, EvalError> {
        read_alias!(self, precedence, { Rc::from(self.roots()?) })
    }

    fn optional_registered(&mut self) -> Result<Option<Rc<RegisteredClass>>, EvalError> {
        self.image.bool()?.then(|| self.registered()).transpose()
    }

    fn update(&mut self) -> Result<ClassUpdate, EvalError> {
        let registered = self.registered()?;
        let previous = self.optional_registered()?;
        let mut forward_classes = Vec::new();
        for _ in 0..self.image.usize()? {
            forward_classes.push((self.image.usize()?, self.root()?));
        }
        let affected = self.ids()?;
        let definitions = self
            .map(|s| s.image.bool()?.then(|| s.definition()).transpose())?
            .into();
        let inputs = self.map(Self::optional_registered)?;
        let finalized = self.ids()?.into_iter().collect::<HashSet<_>>().into();
        let mut computed_shared = HashMap::new();
        for _ in 0..self.image.usize()? {
            let key = (self.image.usize()?, self.image.usize()?);
            if computed_shared.insert(key, self.shared()?).is_some() {
                return Err(invalid("duplicate image shared slot"));
            }
        }
        Ok(ClassUpdate {
            registered,
            previous,
            forward_classes,
            affected,
            definitions,
            inputs,
            finalized,
            computed_shared: computed_shared.into(),
        })
    }
}
