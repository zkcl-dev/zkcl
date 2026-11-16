use std::collections::{BTreeMap, HashMap, HashSet};

use crate::heap::{HeapRootRegistry, RootSlot, Word};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct Package(usize);

impl Package {
    pub(crate) const COMMON_LISP: Self = Self(0);
    pub(crate) const USER: Self = Self(1);
    pub(crate) const KEYWORD: Self = Self(2);
    pub(crate) const MOP: Self = Self(3);
    pub(crate) const ZK: Self = Self(4);
    pub(crate) const SIMD: Self = Self(5);
    pub(crate) const TREELIST: Self = Self(6);
    pub(crate) const SYS: Self = Self(7);
    pub(crate) const ATOMIC: Self = Self(8);

    pub(crate) fn word(self) -> Word {
        Word::package(self.0)
    }

    pub(crate) fn from_word(word: Word) -> Option<Self> {
        word.package_index().map(Self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SymbolStatus {
    Internal,
    External,
    Inherited,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub(crate) struct PackageError {
    pub(crate) package: Package,
    message: String,
    /// Whether the operation has a CONTINUE restart, as for the correctable
    /// errors of MAKE-PACKAGE, IMPORT, EXPORT and DELETE-PACKAGE.
    pub(crate) correctable: bool,
}

impl PackageError {
    fn new(package: Package, message: impl Into<String>) -> Self {
        Self {
            package,
            message: message.into(),
            correctable: false,
        }
    }

    fn conflict(package: Package, name: &str) -> Self {
        Self::new(package, format!("conflicting symbols named {name:?}"))
    }

    fn deleted(package: Package) -> Self {
        Self::new(package, "the package has been deleted")
    }

    fn with_continue_restart(mut self) -> Self {
        self.correctable = true;
        self
    }
}

struct PresentSymbol {
    symbol: RootSlot,
    external: bool,
    shadowing: bool,
}

impl PresentSymbol {
    fn new(symbol: RootSlot, external: bool, shadowing: bool) -> Self {
        Self {
            symbol,
            external,
            shadowing,
        }
    }

    fn get(&self) -> (Word, SymbolStatus) {
        let status = if self.external {
            SymbolStatus::External
        } else {
            SymbolStatus::Internal
        };
        (self.symbol.get(), status)
    }
}

struct PackageRecord {
    name: String,
    nicknames: Vec<String>,
    local_nicknames: BTreeMap<String, Package>,
    use_list: Vec<Package>,
    present_symbols: HashMap<String, PresentSymbol>,
    documentation: RootSlot,
}

impl PackageRecord {
    fn new(name: String, nicknames: Vec<String>) -> Self {
        Self {
            name,
            nicknames,
            local_nicknames: BTreeMap::new(),
            use_list: Vec::new(),
            present_symbols: HashMap::new(),
            documentation: RootSlot::nil(),
        }
    }

    fn names(&self) -> impl Iterator<Item = &String> {
        std::iter::once(&self.name).chain(&self.nicknames)
    }

    fn shadows(&self, name: &str) -> bool {
        self.present_symbols
            .get(name)
            .is_some_and(|entry| entry.shadowing)
    }
}

/// The registry owns the roots for present symbols. Inheritance follows direct
/// use edges to external entries, so it neither copies entries nor adds roots.
pub(crate) struct PackageRegistry {
    roots: HeapRootRegistry,
    packages_by_id: BTreeMap<Package, PackageRecord>,
    names: HashMap<String, Package>,
    next_id: usize,
    revision: u64,
}

impl PackageRegistry {
    pub(crate) fn new(roots: HeapRootRegistry) -> Self {
        let mut registry = Self {
            roots,
            packages_by_id: BTreeMap::new(),
            names: HashMap::new(),
            next_id: 9,
            revision: 0,
        };
        for (package, name, nickname) in [
            (Package::COMMON_LISP, "COMMON-LISP", Some("CL")),
            (Package::USER, "COMMON-LISP-USER", Some("CL-USER")),
            (Package::KEYWORD, "KEYWORD", None),
            (Package::MOP, "ZK-MOP", None),
            (Package::ZK, "ZK", None),
            (Package::SIMD, zkcl_syntax::simd_builtins::PACKAGE, None),
            (Package::TREELIST, "ZK-TREELIST", None),
            (Package::SYS, "ZK-SYS", None),
            (Package::ATOMIC, "ZK-ATOMIC", None),
        ] {
            let nicknames = nickname.into_iter().map(str::to_owned).collect();
            let mut record = PackageRecord::new(name.into(), nicknames);
            if package == Package::USER {
                record.use_list.push(Package::COMMON_LISP);
            }
            registry.register_package(package, record);
        }
        registry
    }

    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    fn changed(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    pub(crate) fn named(&self, name: &str) -> Option<Package> {
        self.names.get(name).copied()
    }

    pub(crate) fn named_from(&self, current: Option<Package>, name: &str) -> Option<Package> {
        current
            .and_then(|id| self.packages_by_id.get(&id))
            .and_then(|owner| owner.local_nicknames.get(name).copied())
            .or_else(|| self.named(name))
    }

    pub(crate) fn prefix_for(&self, current: Package, home: Package) -> Option<&str> {
        if let Some(owner) = self.packages_by_id.get(&current)
            && let Some((nick, _)) = owner
                .local_nicknames
                .iter()
                .rev()
                .find(|(_, target)| **target == home)
        {
            return Some(nick);
        }
        self.name(home)
    }

    pub(crate) fn local_nicknames(&self, owner: Package) -> &BTreeMap<String, Package> {
        &self.packages_by_id[&owner].local_nicknames
    }

    pub(crate) fn replace_local_nicknames(
        &mut self,
        owner: Package,
        nicknames: BTreeMap<String, Package>,
    ) {
        self.packages_by_id
            .get_mut(&owner)
            .expect("live package")
            .local_nicknames = nicknames;
        self.changed();
    }

    pub(crate) fn remove_local_nickname(&mut self, owner: Package, name: &str) -> bool {
        let removed = self
            .packages_by_id
            .get_mut(&owner)
            .expect("live package")
            .local_nicknames
            .remove(name)
            .is_some();
        if removed {
            self.changed();
        }
        removed
    }

    pub(crate) fn locally_nicknamed_by(&self, target: Package) -> Vec<Package> {
        self.packages_by_id
            .iter()
            .filter_map(|(id, owner)| {
                owner
                    .local_nicknames
                    .values()
                    .any(|pkg| *pkg == target)
                    .then_some(*id)
            })
            .collect()
    }

    pub(crate) fn name(&self, package: Package) -> Option<&str> {
        Some(&self.packages_by_id.get(&package)?.name)
    }

    pub(crate) fn nicknames(&self, package: Package) -> Option<&[String]> {
        Some(&self.packages_by_id.get(&package)?.nicknames)
    }

    pub(crate) fn documentation(&self, package: Package) -> Word {
        self.packages_by_id
            .get(&package)
            .map_or(Word::NIL, |record| record.documentation.get())
    }

    pub(crate) fn set_documentation(
        &mut self,
        package: Package,
        documentation: Word,
    ) -> Result<(), PackageError> {
        let record = self
            .packages_by_id
            .get_mut(&package)
            .ok_or_else(|| PackageError::deleted(package))?;
        record.documentation = self.roots.root(documentation);
        Ok(())
    }

    pub(crate) fn use_list(&self, package: Package) -> Option<&[Package]> {
        Some(&self.packages_by_id.get(&package)?.use_list)
    }

    pub(crate) fn used_by(&self, package: Package) -> Vec<Package> {
        self.users(package).map(|(id, _)| id).collect()
    }

    pub(crate) fn list(&self) -> impl Iterator<Item = Package> + '_ {
        self.packages_by_id.keys().copied()
    }

    pub(crate) fn present(&self, package: Package, name: &str) -> Option<(Word, SymbolStatus)> {
        self.packages_by_id
            .get(&package)?
            .present_symbols
            .get(name)
            .map(PresentSymbol::get)
    }

    pub(crate) fn find(&self, package: Package, name: &str) -> Option<(Word, SymbolStatus)> {
        self.accessible(self.packages_by_id.get(&package)?, name)
    }

    pub(crate) fn present_symbols(&self, name: &str) -> Vec<Word> {
        let mut seen = HashSet::new();
        self.packages_by_id
            .values()
            .filter_map(|record| {
                record
                    .present_symbols
                    .get(name)
                    .map(|entry| entry.symbol.get())
            })
            .filter(|word| seen.insert(*word))
            .collect()
    }

    pub(crate) fn shadowing_symbols(&self, package: Package) -> Vec<Word> {
        self.packages_by_id
            .get(&package)
            .into_iter()
            .flat_map(|record| record.present_symbols.values())
            .filter(|entry| entry.shadowing)
            .map(|entry| entry.symbol.get())
            .collect()
    }

    pub(crate) fn entries_snapshot(
        &self,
        package: Package,
        internal: bool,
        external: bool,
        inherited: bool,
    ) -> Result<Vec<(Word, SymbolStatus, Package)>, PackageError> {
        let record = self.record(package)?;
        let mut out = Vec::new();
        for entry in record.present_symbols.values() {
            let include = if entry.external { external } else { internal };
            if include {
                let (word, status) = entry.get();
                out.push((word, status, package));
            }
        }
        if inherited {
            let mut seen = HashSet::new();
            for used in &record.use_list {
                for (name, entry) in &self.packages_by_id[used].present_symbols {
                    if entry.external
                        && !record.present_symbols.contains_key(name)
                        && seen.insert(name.as_str())
                    {
                        out.push((entry.symbol.get(), SymbolStatus::Inherited, package));
                    }
                }
            }
        }
        Ok(out)
    }

    pub(crate) fn all_present_symbols(&self) -> Vec<Word> {
        self.packages_by_id
            .values()
            .flat_map(|record| record.present_symbols.values())
            .map(|entry| entry.symbol.get())
            .collect()
    }

    pub(crate) fn entries(
        &self,
        package: Package,
    ) -> impl Iterator<Item = (&str, Word, SymbolStatus)> {
        self.packages_by_id[&package]
            .present_symbols
            .iter()
            .map(|(name, entry)| {
                let (word, status) = entry.get();
                (name.as_str(), word, status)
            })
    }

    pub(crate) fn reserve_symbols(&mut self, package: Package, additional: usize) {
        self.packages_by_id
            .get_mut(&package)
            .expect("symbol reservation has a live package")
            .present_symbols
            .reserve(additional);
    }

    /// Install a newly allocated symbol in a known package, after lookup has
    /// established its absence. The bootstrap uses this for canonical symbols.
    pub(crate) fn insert(&mut self, package: Package, name: String, word: Word, external: bool) {
        self.packages_by_id
            .get_mut(&package)
            .expect("symbol insertion has a live package")
            .present_symbols
            .insert(
                name,
                PresentSymbol::new(self.roots.root(word), external, false),
            );
        self.changed();
    }

    pub(crate) fn create(
        &mut self,
        name: String,
        nicknames: Vec<String>,
        use_list: Vec<Package>,
    ) -> Result<Package, PackageError> {
        let mut record = PackageRecord::new(name, nicknames);
        self.check_names(&mut record, None)
            .map_err(PackageError::with_continue_restart)?;
        for used in use_list {
            if !record.use_list.contains(&used) {
                self.check_use(used, &record, used)?;
                record.use_list.push(used);
            }
        }
        if self.next_id > usize::MAX >> 3 {
            return Err(PackageError::new(
                Package::USER,
                "package identities are exhausted",
            ));
        }
        let id = Package(self.next_id);
        self.next_id += 1;
        self.register_package(id, record);
        Ok(id)
    }

    pub(crate) fn rename(
        &mut self,
        package: Package,
        name: String,
        nicknames: Vec<String>,
    ) -> Result<(), PackageError> {
        self.record(package)?;
        Self::allow_renaming(package)?;
        let mut renamed = PackageRecord::new(name, nicknames);
        self.check_names(&mut renamed, Some(package))?;
        self.names.retain(|_, id| *id != package);
        self.names
            .extend(renamed.names().cloned().map(|name| (name, package)));
        let record = self.packages_by_id.get_mut(&package).unwrap();
        record.name = renamed.name;
        record.nicknames = renamed.nicknames;
        self.changed();
        Ok(())
    }

    pub(crate) fn define(
        &mut self,
        current: Option<Package>,
        name: String,
        mut nicknames: Vec<String>,
    ) -> Result<Package, PackageError> {
        let global = self.named(&name);
        let Some(package) = self.named_from(current, &name) else {
            return self.create(name, nicknames, Vec::new());
        };
        if Some(package) != global {
            return Err(PackageError::new(
                package,
                format!("{name:?} is a package-local nickname"),
            ));
        }
        let record = self.record(package)?;
        let canonical_name = record.name.clone();
        // A definition through an existing nickname must preserve that mapping.
        if name != canonical_name && !nicknames.contains(&name) {
            nicknames.push(name);
        }
        if record.nicknames != nicknames {
            self.rename(package, canonical_name, nicknames)?;
        }
        Ok(package)
    }

    /// Return the removed membership roots so the runtime can clear symbols'
    /// home fields while the symbols are still live.
    pub(crate) fn delete(
        &mut self,
        package: Package,
    ) -> Result<Option<Vec<RootSlot>>, PackageError> {
        let Some(record) = self.packages_by_id.get(&package) else {
            return Ok(None);
        };
        Self::allow_renaming(package)?;
        if let Some((_, user)) = self.users(package).find(|(id, _)| *id != package) {
            return Err(PackageError::new(
                package,
                format!("package {:?} still uses {:?}", user.name, record.name),
            )
            .with_continue_restart());
        }
        Ok(Some(self.remove_record(package)))
    }

    /// Delete a package after removing it from the use lists of the packages
    /// that use it. This is the continuation of DELETE-PACKAGE's correctable
    /// error for a package still in use.
    pub(crate) fn force_delete(
        &mut self,
        package: Package,
    ) -> Result<Option<Vec<RootSlot>>, PackageError> {
        if !self.packages_by_id.contains_key(&package) {
            return Ok(None);
        }
        Self::allow_renaming(package)?;
        for record in self.packages_by_id.values_mut() {
            record.use_list.retain(|id| *id != package);
        }
        Ok(Some(self.remove_record(package)))
    }

    fn remove_record(&mut self, package: Package) -> Vec<RootSlot> {
        let record = self.packages_by_id.remove(&package).unwrap();
        self.changed();
        for owner in self.packages_by_id.values_mut() {
            owner.local_nicknames.retain(|_, target| *target != package);
        }
        for name in record.names() {
            self.names.remove(name);
        }
        record
            .present_symbols
            .into_values()
            .map(|entry| entry.symbol)
            .collect()
    }

    pub(crate) fn use_package(
        &mut self,
        package: Package,
        used: Package,
    ) -> Result<(), PackageError> {
        let record = self.record(package)?;
        if package == Package::KEYWORD {
            return Err(PackageError::new(
                package,
                "the KEYWORD package cannot use another package",
            ));
        }
        if package == used || record.use_list.contains(&used) {
            return Ok(());
        }
        self.check_use(package, record, used)?;
        self.packages_by_id
            .get_mut(&package)
            .unwrap()
            .use_list
            .push(used);
        self.changed();
        Ok(())
    }

    pub(crate) fn unuse_package(
        &mut self,
        package: Package,
        used: Package,
    ) -> Result<(), PackageError> {
        self.record(used)?;
        let uses = &mut self
            .packages_by_id
            .get_mut(&package)
            .ok_or_else(|| PackageError::deleted(package))?
            .use_list;
        let before = uses.len();
        uses.retain(|id| *id != used);
        if uses.len() != before {
            self.changed();
        }
        Ok(())
    }

    pub(crate) fn export(
        &mut self,
        package: Package,
        name: &str,
        word: Word,
    ) -> Result<bool, PackageError> {
        let record = self.record(package)?;
        let status = self
            .accessible_status(package, record, name, word)
            .map_err(PackageError::with_continue_restart)?;
        self.publish_export(package, name, word, Some(status))
    }

    /// CONTINUE granted permission to import an inaccessible symbol. Check both
    /// the target and its users before changing membership or symbol ownership.
    pub(crate) fn import_and_export(
        &mut self,
        package: Package,
        name: &str,
        word: Word,
    ) -> Result<bool, PackageError> {
        let record = self.record(package)?;
        let status = match self.accessible(record, name) {
            Some((existing, _)) if existing != word => {
                return Err(PackageError::conflict(package, name).with_continue_restart());
            }
            accessible => accessible.map(|(_, status)| status),
        };
        self.publish_export(package, name, word, status)
    }

    fn publish_export(
        &mut self,
        package: Package,
        name: &str,
        word: Word,
        status: Option<SymbolStatus>,
    ) -> Result<bool, PackageError> {
        if status == Some(SymbolStatus::External) {
            return Ok(false);
        }
        if package == Package::COMMON_LISP {
            return Err(PackageError::new(
                package,
                "the COMMON-LISP export set is fixed",
            ));
        }
        for (user, record) in self.users(package) {
            self.check_incoming_symbol(user, record, name, word)?;
        }
        if status == Some(SymbolStatus::Internal) {
            self.packages_by_id
                .get_mut(&package)
                .unwrap()
                .present_symbols
                .get_mut(name)
                .unwrap()
                .external = true;
            self.changed();
            Ok(false)
        } else {
            self.insert(package, name.into(), word, true);
            Ok(true)
        }
    }

    pub(crate) fn unexport(
        &mut self,
        package: Package,
        name: &str,
        word: Word,
    ) -> Result<(), PackageError> {
        let record = self.record(package)?;
        if self.accessible_status(package, record, name, word)? == SymbolStatus::External {
            Self::allow_removing_external(package)?;
            self.packages_by_id
                .get_mut(&package)
                .unwrap()
                .present_symbols
                .get_mut(name)
                .unwrap()
                .external = false;
            self.changed();
        }
        Ok(())
    }

    /// Return whether a local entry was added, so the runtime can assign a
    /// home to a previously homeless imported symbol.
    pub(crate) fn import(
        &mut self,
        package: Package,
        name: String,
        word: Word,
    ) -> Result<bool, PackageError> {
        let record = self.record(package)?;
        if let Some((existing, status)) = self.accessible(record, &name) {
            if existing != word {
                return Err(PackageError::conflict(package, &name).with_continue_restart());
            }
            if status != SymbolStatus::Inherited {
                return Ok(false);
            }
        }
        self.insert(package, name, word, false);
        Ok(true)
    }

    /// The supplied symbol is the existing present symbol or a fresh local one.
    pub(crate) fn shadow(
        &mut self,
        package: Package,
        name: String,
        word: Word,
    ) -> Result<(), PackageError> {
        let record = self
            .packages_by_id
            .get_mut(&package)
            .ok_or_else(|| PackageError::deleted(package))?;
        record
            .present_symbols
            .entry(name)
            .and_modify(|entry| entry.shadowing = true)
            .or_insert_with(|| PresentSymbol::new(self.roots.root(word), false, true));
        self.changed();
        Ok(())
    }

    pub(crate) fn shadowing_import(
        &mut self,
        package: Package,
        name: String,
        word: Word,
    ) -> Result<Option<RootSlot>, PackageError> {
        let record = self
            .packages_by_id
            .get_mut(&package)
            .ok_or_else(|| PackageError::deleted(package))?;
        if let Some(entry) = record.present_symbols.get_mut(&name) {
            if entry.symbol.get() == word {
                entry.shadowing = true;
                return Ok(None);
            }
            if matches!(package, Package::COMMON_LISP | Package::MOP) && entry.external {
                Self::allow_removing_external(package)?;
            }
        }
        let displaced = record
            .present_symbols
            .insert(name, PresentSymbol::new(self.roots.root(word), false, true))
            .map(|entry| entry.symbol);
        self.changed();
        Ok(displaced)
    }

    pub(crate) fn unintern(
        &mut self,
        package: Package,
        name: &str,
        word: Word,
    ) -> Result<Option<RootSlot>, PackageError> {
        let record = self.record(package)?;
        let Some(entry) = record
            .present_symbols
            .get(name)
            .filter(|entry| entry.symbol.get() == word)
        else {
            return Ok(None);
        };
        if matches!(package, Package::COMMON_LISP | Package::MOP) && entry.external {
            Self::allow_removing_external(package)?;
        }
        if entry.shadowing {
            let mut candidates = self.inherited(record, name);
            if let Some(first) = candidates.next()
                && candidates.any(|next| next != first)
            {
                return Err(PackageError::conflict(package, name));
            }
        }
        let removed = self
            .packages_by_id
            .get_mut(&package)
            .unwrap()
            .present_symbols
            .remove(name)
            .map(|entry| entry.symbol);
        self.changed();
        Ok(removed)
    }

    fn record(&self, package: Package) -> Result<&PackageRecord, PackageError> {
        self.packages_by_id
            .get(&package)
            .ok_or_else(|| PackageError::deleted(package))
    }

    fn users(&self, package: Package) -> impl Iterator<Item = (Package, &PackageRecord)> {
        self.packages_by_id.iter().filter_map(move |(&id, record)| {
            record.use_list.contains(&package).then_some((id, record))
        })
    }

    fn inherited(&self, record: &PackageRecord, name: &str) -> impl Iterator<Item = Word> {
        record.use_list.iter().filter_map(move |used| {
            self.packages_by_id[used]
                .present_symbols
                .get(name)
                .filter(|entry| entry.external)
                .map(|entry| entry.symbol.get())
        })
    }

    fn accessible(&self, record: &PackageRecord, name: &str) -> Option<(Word, SymbolStatus)> {
        record
            .present_symbols
            .get(name)
            .map(PresentSymbol::get)
            .or_else(|| {
                self.inherited(record, name)
                    .next()
                    .map(|word| (word, SymbolStatus::Inherited))
            })
    }

    fn accessible_status(
        &self,
        package: Package,
        record: &PackageRecord,
        name: &str,
        word: Word,
    ) -> Result<SymbolStatus, PackageError> {
        self.accessible(record, name)
            .filter(|(existing, _)| *existing == word)
            .map(|(_, status)| status)
            .ok_or_else(|| PackageError::new(package, format!("symbol {name:?} is not accessible")))
    }

    fn check_incoming_symbol(
        &self,
        package: Package,
        record: &PackageRecord,
        name: &str,
        word: Word,
    ) -> Result<(), PackageError> {
        if !record.shadows(name)
            && self
                .accessible(record, name)
                .is_some_and(|(existing, _)| existing != word)
        {
            return Err(PackageError::conflict(package, name));
        }
        Ok(())
    }

    fn check_use(
        &self,
        package: Package,
        record: &PackageRecord,
        used: Package,
    ) -> Result<(), PackageError> {
        if used == Package::KEYWORD {
            return Err(PackageError::new(
                package,
                "the KEYWORD package cannot be used by another package",
            ));
        }
        for (name, entry) in &self.record(used)?.present_symbols {
            if entry.external {
                self.check_incoming_symbol(package, record, name, entry.symbol.get())?;
            }
        }
        Ok(())
    }

    fn check_names(
        &self,
        record: &mut PackageRecord,
        rename: Option<Package>,
    ) -> Result<(), PackageError> {
        for name in record.names() {
            if let Some(id) = self.named(name)
                && Some(id) != rename
            {
                return Err(PackageError::new(
                    id,
                    format!("package name {name:?} is already in use"),
                ));
            }
        }
        let mut seen = HashSet::new();
        record
            .nicknames
            .retain(|name| name != &record.name && seen.insert(name.clone()));
        Ok(())
    }

    fn register_package(&mut self, package: Package, record: PackageRecord) {
        self.names
            .extend(record.names().cloned().map(|name| (name, package)));
        self.packages_by_id.insert(package, record);
        self.changed();
    }

    fn allow_renaming(package: Package) -> Result<(), PackageError> {
        if matches!(
            package,
            Package::COMMON_LISP | Package::KEYWORD | Package::MOP
        ) {
            return Err(PackageError::new(
                package,
                "a built-in package cannot be renamed or deleted",
            ));
        }
        Ok(())
    }

    fn allow_removing_external(package: Package) -> Result<(), PackageError> {
        if matches!(
            package,
            Package::COMMON_LISP | Package::KEYWORD | Package::MOP
        ) {
            return Err(PackageError::new(
                package,
                "a built-in package's external symbols cannot be replaced or unexported",
            ));
        }
        Ok(())
    }
}

impl PackageRegistry {
    pub(crate) fn write_image(
        &self,
        image: &mut crate::image::ImageWriter<'_>,
    ) -> Result<(), crate::EvalError> {
        image.usize(self.next_id);
        image.usize(self.revision as usize);
        image.usize(self.packages_by_id.len());
        for (id, record) in &self.packages_by_id {
            image.usize(id.0);
            image.string(&record.name);
            image.usize(record.nicknames.len());
            for name in &record.nicknames {
                image.string(name);
            }
            image.usize(record.local_nicknames.len());
            for (name, target) in &record.local_nicknames {
                image.string(name);
                image.usize(target.0);
            }
            image.usize(record.use_list.len());
            for id in &record.use_list {
                image.usize(id.0);
            }
            image.word(record.documentation.get())?;
            image.usize(record.present_symbols.len());
            for (name, entry) in &record.present_symbols {
                image.string(name);
                image.word(entry.symbol.get())?;
                image.bool(entry.external);
                image.bool(entry.shadowing);
            }
        }
        Ok(())
    }
    pub(crate) fn read_image(
        image: &mut crate::image::ImageReader<'_>,
    ) -> Result<Self, crate::EvalError> {
        let roots = image.runtime.heap.root_registry();
        let next_id = image.usize()?;
        let revision = image.usize()? as u64;
        let mut registry = Self {
            roots,
            packages_by_id: BTreeMap::new(),
            names: HashMap::new(),
            next_id,
            revision,
        };
        for _ in 0..image.usize()? {
            let id = Package(image.usize()?);
            let name = image.string()?;
            let mut nicknames = Vec::new();
            for _ in 0..image.usize()? {
                nicknames.push(image.string()?);
            }
            let mut record = PackageRecord::new(name, nicknames);
            for _ in 0..image.usize()? {
                record
                    .local_nicknames
                    .insert(image.string()?, Package(image.usize()?));
            }
            for _ in 0..image.usize()? {
                record.use_list.push(Package(image.usize()?));
            }
            record.documentation = registry.roots.root(image.word()?);
            for _ in 0..image.usize()? {
                let name = image.string()?;
                let symbol = image.word()?;
                if image.runtime.heap.symbol_attributes(symbol).is_none() {
                    return Err(crate::image::invalid());
                }
                let external = image.bool()?;
                let shadowing = image.bool()?;
                record.present_symbols.insert(
                    name,
                    PresentSymbol::new(registry.roots.root(symbol), external, shadowing),
                );
            }
            if registry.packages_by_id.contains_key(&id) || id.0 >= next_id {
                return Err(crate::image::invalid());
            }
            for name in record.names() {
                if registry.names.insert(name.clone(), id).is_some() {
                    return Err(crate::image::invalid());
                }
            }
            registry.packages_by_id.insert(id, record);
        }
        for record in registry.packages_by_id.values() {
            if record
                .use_list
                .iter()
                .chain(record.local_nicknames.values())
                .any(|id| !registry.packages_by_id.contains_key(id))
            {
                return Err(crate::image::invalid());
            }
        }
        for id in 0..7 {
            if !registry.packages_by_id.contains_key(&Package(id)) {
                return Err(crate::image::invalid());
            }
        }
        Ok(registry)
    }
}
