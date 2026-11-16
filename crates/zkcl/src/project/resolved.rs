use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use zkcl_project::lockfile::{
    BuildScope, LOCKFILE_NAME, LockedGraph, LockedModule, Lockfile, parse_lockfile,
};
use zkcl_project::manifest::{BuildTargetKind, ModuleName};
use zkcl_project::solver::{
    self, Dependency, ModuleId, Resolution, SelectedModule, SolveRequest, SourceId,
};
use zkcl_runtime::Runtime;

use super::asdf::{self, AsdfAlias, AsdfDelivery, AsdfSourceTree, AsdfSystem};
use super::build_script;
use super::delivery::{self, Delivery, StartupStep};
use super::registries::{CombinedIndex, RegistryCache};
use super::workspace::{LocalProvider, Project, ProjectMetadataFileError};

pub struct ResolvedProject {
    pub project: Project,
    pub resolution: Resolution,
    execution_root: ModuleId,
    local: LocalProvider,
    registries: RegistryCache,
    lock: LockedGraph,
    lock_root: PathBuf,
    registry_dirs: Vec<PathBuf>,
    offline: bool,
    locked: bool,
    pending_graphs: Vec<LockedGraph>,
    build_options: build_script::BuildOptions,
}

#[derive(Clone, Copy)]
pub enum Update<'a> {
    Keep,
    All,
    One(&'a ModuleName),
}

impl ResolvedProject {
    /// Configure build-script execution before loading this graph.
    pub fn with_build_options(mut self, options: build_script::BuildOptions) -> Self {
        self.build_options = options;
        self
    }

    /// Exact selected graph, including source identities, features and systems.
    pub fn locked_graph(&self) -> &LockedGraph {
        &self.lock
    }
    pub fn resolve(
        project: Project,
        locked: bool,
        offline: bool,
        registry_dirs: &[PathBuf],
        features: Option<BTreeSet<String>>,
        update: Update<'_>,
    ) -> Result<Self, Box<dyn Error>> {
        Self::resolve_for_target(
            project,
            locked,
            offline,
            registry_dirs,
            features,
            update,
            None,
        )
    }

    pub fn resolve_for_target(
        project: Project,
        locked: bool,
        offline: bool,
        registry_dirs: &[PathBuf],
        features: Option<BTreeSet<String>>,
        update: Update<'_>,
        target: Option<&str>,
    ) -> Result<Self, Box<dyn Error>> {
        Self::resolve_request(
            project,
            locked,
            offline,
            registry_dirs,
            features,
            update,
            None,
            target,
            BuildScope::Target,
            None,
        )
    }

    pub fn target(&self) -> &str {
        &self.lock.target_triple
    }

    pub fn resolve_tool(
        project: Project,
        offline: bool,
        registry_dirs: &[PathBuf],
        tool: Dependency,
    ) -> Result<Self, Box<dyn Error>> {
        Self::resolve_request(
            project,
            offline,
            offline,
            registry_dirs,
            None,
            Update::Keep,
            Some(tool),
            None,
            BuildScope::Target,
            None,
        )
    }

    fn resolve_request(
        project: Project,
        locked: bool,
        offline: bool,
        registry_dirs: &[PathBuf],
        features: Option<BTreeSet<String>>,
        update: Update<'_>,
        tool: Option<Dependency>,
        target: Option<&str>,
        scope: BuildScope,
        lock_root_override: Option<&Path>,
    ) -> Result<Self, Box<dyn Error>> {
        let target =
            target.map_or_else(|| target_lexicon::Triple::host().to_string(), str::to_owned);
        let target = target
            .parse::<zkcl_runtime::CompilationTarget>()?
            .to_string();
        let lock_root = lock_root_override
            .map_or_else(|| project.lock_root(), |root| Ok(root.to_owned()))?
            .canonicalize()?;
        let lockfile = read_lock(&lock_root.join(LOCKFILE_NAME))?.unwrap_or_default();
        let features = features.unwrap_or_default();
        let previous =
            lockfile.selected_for_scope(&project.manifest.name, &target, &features, scope);
        let locked = locked || (tool.is_some() && previous.is_some());
        if locked && previous.is_none() {
            if let Some(tool) = &tool {
                return Err(format!(
                    "no cached dependency graph for {} {}; run `zkcl x` online with the same registry identity",
                    tool.module, tool.requirement
                )
                .into());
            }
            return Err(format!(
                "no lockfile for project {} on target {target} with features {features:?}, run `zkcl lock` with the same --features",
                project.manifest.name
            )
            .into());
        }
        let mut dependency_project = project.clone();
        if scope == BuildScope::Build {
            let aliases = dependency_project
                .manifest
                .build
                .as_ref()
                .map(|build| build.dependencies.clone())
                .unwrap_or_default();
            dependency_project
                .manifest
                .dependencies
                .retain(|dependency| aliases.contains(&dependency.alias));
            dependency_project.manifest.build = None;
            dependency_project.manifest.targets.clear();
            dependency_project.manifest.packages.clear();
            dependency_project.manifest.systems.clear();
            dependency_project.manifest.asdf_aliases.clear();
        }
        let mut local = LocalProvider::for_project(&dependency_project)?;
        local.roots[0].features.clone_from(&features);
        let registries = RegistryCache::new(registry_dirs, previous.cloned(), locked, offline)?;
        let roots = tool
            .as_ref()
            .map_or_else(|| local.roots.clone(), |tool| vec![tool.clone()]);
        let mut preferred = BTreeMap::new();
        if let Some(previous) = previous {
            for (id, module) in &previous.modules {
                if matches!(update, Update::All)
                    || matches!(update, Update::One(name) if &id.name == name)
                {
                    continue;
                }
                let mut id = id.clone();
                if let SourceId::Path(path) = &id.source {
                    let Ok(path) = lock_root.join(path).canonicalize() else {
                        continue;
                    };
                    id.source = SourceId::Path(path);
                }
                preferred.insert(id, module.release.clone());
            }
        }
        let resolution = solver::solve(
            &CombinedIndex {
                local: &local,
                registries: &registries,
            },
            &SolveRequest {
                roots,
                locked: preferred,
                platform: target.clone(),
                toolchain: Some(
                    env!("CARGO_PKG_VERSION")
                        .parse()
                        .expect("Cargo package version is SemVer"),
                ),
                ..SolveRequest::default()
            },
        )?;
        let tool_id = tool.map(|tool| tool.module);
        let lock = lock_for(
            &project,
            &lock_root,
            &local,
            &registries,
            &resolution,
            target,
            features,
            scope,
            tool_id.as_ref(),
        )?;
        if locked && previous != Some(&lock) {
            if let Some(id) = &tool_id {
                return Err(format!(
                    "the cached dependency graph for {id} differs from its lockfile"
                )
                .into());
            }
            return Err("zkcl.lock differs from the dependency graph, run `zkcl lock`".into());
        }
        let mut resolved = Self {
            execution_root: tool_id.unwrap_or_else(|| project.id()),
            project,
            resolution,
            local,
            registries,
            lock,
            lock_root,
            registry_dirs: registry_dirs.to_vec(),
            offline,
            locked,
            pending_graphs: Vec::new(),
            build_options: build_script::BuildOptions::default(),
        };
        if scope == BuildScope::Target {
            resolved.prepare_build_graphs(update)?;
            if let Update::One(name) = update {
                if !resolved
                    .lock
                    .modules
                    .keys()
                    .chain(
                        resolved
                            .pending_graphs
                            .iter()
                            .flat_map(|graph| graph.modules.keys()),
                    )
                    .any(|id| &id.name == name)
                {
                    return Err(
                        format!("{name} is absent from this project's dependency graphs").into(),
                    );
                }
            }
        }
        Ok(resolved)
    }

    fn prepare_build_graphs(&mut self, update: Update<'_>) -> Result<(), Box<dyn Error>> {
        let mut pending = self.resolved_projects()?;
        let mut seen = BTreeSet::new();
        while let Some((project, features)) = pending.pop() {
            if !seen.insert((project.id(), features.clone())) {
                continue;
            }
            if project
                .manifest
                .build
                .as_ref()
                .is_none_or(|build| build.dependencies.is_empty())
            {
                continue;
            }
            let host = Self::resolve_request(
                project,
                self.locked,
                self.offline,
                &self.registry_dirs,
                Some(features),
                update,
                None,
                None,
                BuildScope::Build,
                Some(&self.lock_root),
            )?;
            if self
                .pending_graphs
                .iter()
                .any(|graph| graph.key() == host.lock.key() && graph != &host.lock)
            {
                return Err(format!(
                    "different sources for {} require incompatible host build locks",
                    host.project.manifest.name
                )
                .into());
            }
            pending.extend(
                host.resolved_projects()?
                    .into_iter()
                    .filter(|(project, _)| project.id() != host.execution_root),
            );
            self.pending_graphs.push(host.lock);
        }
        Ok(())
    }

    fn resolved_projects(&self) -> Result<Vec<(Project, BTreeSet<String>)>, Box<dyn Error>> {
        let local = self.local.snapshot();
        let mut projects = Vec::new();
        for (id, selected) in &self.resolution.modules {
            let project = if *id == self.project.id() && self.lock.scope == BuildScope::Target {
                Some(self.project.clone())
            } else if let Some(project) = local.native.get(id) {
                Some(project.clone())
            } else if let Some(release) = self.registries.selection(id, &selected.release.id) {
                if release.has_build_dependencies {
                    self.registries.fetch(id, &release)?;
                    Some(self.registries.materialize(id, &release)?)
                } else {
                    None
                }
            } else {
                None
            };
            if let Some(project) = project {
                projects.push((project, selected.features.clone()));
            }
        }
        Ok(projects)
    }

    pub fn write_lock(&self) -> Result<(), Box<dyn Error>> {
        let _guard = lockfile_write_guard(&self.lock_root)?;
        let path = self.lock_root.join(LOCKFILE_NAME);
        let mut lockfile = read_lock(&path)?.unwrap_or_default();
        lockfile.upsert(self.lock.clone());
        for graph in &self.pending_graphs {
            lockfile.upsert(graph.clone());
        }
        write_metadata(&path, &lockfile.render())
    }

    pub fn fetch(&self) -> Result<(), Box<dyn Error>> {
        for (id, module) in &self.lock.modules {
            if let Some(release) = self.registries.selection(id, &module.release) {
                self.registries.fetch(id, &release)?;
            }
        }
        if !self.pending_graphs.is_empty() {
            let mut configurations = Lockfile::default();
            for graph in &self.pending_graphs {
                configurations.upsert(graph.clone());
            }
            self.registries.fetch_all_configurations(&configurations)?;
        }
        Ok(())
    }

    pub fn materialize_tool(&self) -> Result<Project, Box<dyn Error>> {
        let id = &self.execution_root;
        let module = &self.lock.modules[id];
        let release = self
            .registries
            .selection(id, &module.release)
            .expect("the resolved tool has immutable source");
        self.registries.materialize(id, &release)
    }

    pub fn fetch_lockfile_configurations(
        project: &Project,
        registry_dirs: &[PathBuf],
        offline: bool,
    ) -> Result<(), Box<dyn Error>> {
        let lockfile = read_lock(&project.lock_root()?.join(LOCKFILE_NAME))?
            .ok_or("no zkcl.lock file, run `zkcl lock` first")?;
        let registries = RegistryCache::new(registry_dirs, None, true, offline)?;
        registries.fetch_all_configurations(&lockfile)
    }

    pub fn publish(&self, target: &Path) -> Result<zkcl_registry::Digest, Box<dyn Error>> {
        super::registries::publish(&self.project, target)
    }

    pub fn load_libraries(
        &self,
        runtime: &Runtime,
        output: &Path,
    ) -> Result<Delivery, Box<dyn Error>> {
        self.load_target_libraries(runtime, output, &self.project)
    }

    pub fn load_target_libraries(
        &self,
        runtime: &Runtime,
        output: &Path,
        target_project: &Project,
    ) -> Result<Delivery, Box<dyn Error>> {
        self.load_target_libraries_inner(runtime, output, target_project, &mut BTreeSet::new())
    }

    fn load_target_libraries_inner(
        &self,
        runtime: &Runtime,
        output: &Path,
        target_project: &Project,
        building: &mut BTreeSet<PathBuf>,
    ) -> Result<Delivery, Box<dyn Error>> {
        let root = &self.execution_root;
        let mut native_modules = BTreeSet::from([root]);
        let mut asdf_systems: BTreeMap<_, BTreeSet<_>> = BTreeMap::new();
        for module in self.resolution.modules.values() {
            for dependency in &module.dependencies {
                let id = &dependency.module;
                match &dependency.required_system {
                    Some(system)
                        if self.lock.modules[&locked_id(&self.lock_root, id)?]
                            .systems
                            .contains_key(system) =>
                    {
                        asdf_systems.entry(id).or_default().insert(system.as_str());
                    }
                    _ => {
                        native_modules.insert(id);
                    }
                }
            }
        }
        if let Some(id) = asdf_systems.keys().find(|id| native_modules.contains(*id)) {
            return Err(format!(
                "{id} is selected as a native library and through a separate .asd file; use a native ASDF alias to share the library"
            ).into());
        }
        let local = self.local.snapshot();
        let mut projects = local.native.clone();
        for (id, module) in &self.lock.modules {
            if id == root {
                projects.insert(id.clone(), target_project.clone());
            } else if let Some(release) = self.registries.selection(id, &module.release) {
                let project = self.registries.materialize(id, &release)?;
                projects.insert(id.clone(), project);
            }
        }
        let mut definitions = Vec::new();
        let mut aliases = Vec::new();
        let empty_aliases = BTreeMap::new();
        let root_aliases = if self.lock.scope == BuildScope::Build {
            &empty_aliases
        } else {
            &self.project.manifest.asdf_aliases
        };
        aliases.extend(
            root_aliases
                .iter()
                .map(|(name, version)| AsdfAlias { name, version }),
        );
        for (id, module) in &self.lock.modules {
            for (name, relative) in &module.systems {
                let definition = match &id.source {
                    SourceId::Path(_) => {
                        local.systems[&absolute_id(&self.lock_root, id)?][name].clone()
                    }
                    _ => projects[id].source_file(relative)?,
                };
                definitions.push((name, definition));
            }
            aliases.extend(
                module
                    .asdf_aliases
                    .iter()
                    .map(|(name, version)| AsdfAlias { name, version }),
            );
        }
        let systems: Vec<_> = definitions
            .iter()
            .map(|(name, definition)| AsdfSystem { name, definition })
            .collect();
        if !systems.is_empty() || !aliases.is_empty() {
            asdf::configure(runtime, &systems, &aliases, &output.join("asdf"))?;
        }
        let mut delivery = Delivery::default();
        if !systems.is_empty() || !aliases.is_empty() {
            delivery.asdf = Some(AsdfDelivery {
                systems: definitions
                    .iter()
                    .map(|(name, path)| ((*name).clone(), path.clone()))
                    .collect(),
                aliases: aliases
                    .iter()
                    .map(|alias| (alias.name.to_owned(), alias.version.to_owned()))
                    .collect(),
                sources: Vec::new(),
                compiled: Vec::new(),
            });
        }
        for (index, id) in self.resolution.order.iter().enumerate() {
            if let Some(systems) = asdf_systems.get(id) {
                let source = if let Some(project) = projects.get(id) {
                    let mut files: BTreeSet<_> = project.manifest.include.iter().cloned().collect();
                    files.extend(project.manifest.systems.values().cloned());
                    files.extend(
                        project
                            .manifest
                            .targets
                            .iter()
                            .filter(|target| target.kind == BuildTargetKind::Library)
                            .flat_map(|target| &target.files)
                            .filter(|file| !project.manifest.is_build_output(file))
                            .cloned(),
                    );
                    files.extend(
                        self.lock.modules[&locked_id(&self.lock_root, id)?]
                            .systems
                            .values()
                            .cloned(),
                    );
                    AsdfSourceTree {
                        root: project.root.clone(),
                        files: files.into_iter().collect(),
                    }
                } else {
                    let SourceId::Path(path) = &id.source else {
                        unreachable!("immutable modules have a project manifest");
                    };
                    let lock = &self.lock.modules[&locked_id(&self.lock_root, id)?];
                    AsdfSourceTree {
                        root: path.clone(),
                        files: lock.systems.values().cloned().collect(),
                    }
                };
                delivery
                    .asdf
                    .as_mut()
                    .expect("selected ASDF systems have a delivery mapping")
                    .sources
                    .push(source);
                for name in systems {
                    asdf::load(runtime, name)?;
                    let compiled = asdf::compiled_components(runtime, name)?;
                    let collected = &mut delivery.asdf.as_mut().expect("ASDF delivery").compiled;
                    for component in compiled {
                        if !collected
                            .iter()
                            .any(|existing| existing.path == component.path)
                        {
                            collected.push(component);
                        }
                    }
                    delivery
                        .steps
                        .push(StartupStep::LoadAsdf((*name).to_owned()));
                }
            } else if let Some(project) = projects.get(id) {
                let library = project
                    .manifest
                    .targets
                    .iter()
                    .find(|target| target.kind == BuildTargetKind::Library);
                if library.is_none() && id != root {
                    return Err(format!("{id} needs a library target").into());
                }
                let library_dir = output.join(format!("lib-{index}"));
                let generated =
                    build_script::output_dir(if id == root { output } else { &library_dir });
                self.run_build_task(
                    project,
                    &self.resolution.modules[id].features,
                    &generated,
                    &library_dir,
                    building,
                )?;
                if let Some(build) = &project.manifest.build {
                    delivery
                        .link_libraries
                        .extend(build.link_libraries.iter().cloned());
                    delivery
                        .link_search
                        .extend(build.link_search.iter().map(|path| generated.join(path)));
                }
                if let Some(target) = library {
                    let files = target
                        .files
                        .iter()
                        .map(|file| project.compilation_source(file, &generated))
                        .collect::<Result<Vec<_>, _>>()?;
                    let components = delivery::compile_files(runtime, &files, &library_dir)?;
                    delivery.steps.extend(
                        (delivery.units.len()..delivery.units.len() + components.len())
                            .map(StartupStep::LoadFasl),
                    );
                    delivery.units.extend(components);
                    let names: Vec<_> = project
                        .manifest
                        .asdf_aliases
                        .keys()
                        .map(String::as_str)
                        .collect();
                    asdf::mark_native_ready(runtime, &names)?;
                    if !names.is_empty() {
                        delivery.steps.push(StartupStep::ReadyAsdfAlias(
                            names.into_iter().map(str::to_owned).collect(),
                        ));
                    }
                }
            }
        }
        Ok(delivery)
    }
    fn run_build_task(
        &self,
        project: &Project,
        features: &BTreeSet<String>,
        generated: &Path,
        output: &Path,
        building: &mut BTreeSet<PathBuf>,
    ) -> Result<(), Box<dyn Error>> {
        let Some(build) = &project.manifest.build else {
            return Ok(());
        };
        if !building.insert(project.root.clone()) {
            return Err(format!(
                "recursive build dependency task at {}",
                project.root.display()
            )
            .into());
        }
        let result = (|| {
            if build.dependencies.is_empty() {
                return build_script::run(
                    project,
                    generated,
                    &self.lock.target_triple,
                    &self.build_options,
                );
            }
            let host = Self::resolve_request(
                project.clone(),
                self.locked,
                self.offline,
                &self.registry_dirs,
                Some(features.clone()),
                Update::Keep,
                None,
                None,
                BuildScope::Build,
                Some(&self.lock_root),
            )?
            .with_build_options(self.build_options.clone());
            host.fetch()?;
            let directory = output.join("build-dependencies");
            fs::create_dir_all(&directory)?;
            let runtime = self.build_options.runtime.clone().start()?;
            if let Some(initialize) = &self.build_options.initialize_runtime {
                initialize(&runtime)?;
            }
            let dependencies =
                host.load_target_libraries_inner(&runtime, &directory, &host.project, building)?;
            let mut fingerprint = host.lock.render().into_bytes();
            for unit in &dependencies.units {
                // Compile-only effects can change without changing the saved FASL.
                fingerprint.extend_from_slice(
                    zkcl_registry::Digest::of_bytes(&fs::read(&unit.source)?).as_bytes(),
                );
                fingerprint.extend_from_slice(
                    zkcl_registry::Digest::of_bytes(&fs::read(&unit.fasl)?).as_bytes(),
                );
            }
            if let Some(asdf) = &dependencies.asdf {
                for tree in &asdf.sources {
                    for file in &tree.files {
                        fingerprint.extend_from_slice(
                            zkcl_registry::Digest::of_bytes(&fs::read(tree.root.join(file))?)
                                .as_bytes(),
                        );
                    }
                }
            }
            let bootstrap = directory.join("build-bootstrap.lisp");
            let literal = |path: &Path| -> Result<String, Box<dyn Error>> {
                Ok(lisp_string(path.to_str().ok_or("build path is not UTF-8")?))
            };
            let mut text = String::new();
            if dependencies.asdf.is_some() {
                delivery::stage_delivery_assets(&dependencies, &directory)?;
                let asdf_output = directory.join("asdf-script");
                fs::create_dir_all(&asdf_output)?;
                text.push_str(&format!(
                    "(load {})\n(zkcl-project-asdf::initialize-delivery {} {})\n",
                    literal(&directory.join("asdf-bootstrap.lisp"))?,
                    literal(&directory)?,
                    literal(&asdf_output)?
                ));
            }
            let worker_output = directory.join("worker-compiled");
            fs::create_dir_all(&worker_output)?;
            for step in &dependencies.steps {
                match step {
                    StartupStep::LoadFasl(index) => text.push_str(&format!(
                        "(load (compile-file {} :output-file {}))\n",
                        literal(&dependencies.units[*index].source)?,
                        literal(&worker_output.join(format!("{index:04}.zkclfasl")))?
                    )),
                    StartupStep::LoadAsdf(name) => {
                        text.push_str(&format!("(asdf:load-system {})\n", lisp_string(name)))
                    }
                    StartupStep::ReadyAsdfAlias(names) => {
                        for name in names {
                            text.push_str(&format!(
                                "(zkcl-project-asdf::mark-native-ready {})\n",
                                lisp_string(name)
                            ));
                        }
                    }
                }
            }
            fs::write(&bootstrap, text)?;
            build_script::run_with_dependencies(
                project,
                generated,
                &self.lock.target_triple,
                Some(&bootstrap),
                &fingerprint,
                &self.build_options,
            )
        })();
        building.remove(&project.root);
        result
    }
}

fn lockfile_write_guard(root: &Path) -> io::Result<fs::File> {
    let directory = root.join("target/zkcl");
    fs::create_dir_all(&directory)?;
    let guard = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(directory.join("resolve.lock"))?;
    guard.lock()?;
    Ok(guard)
}

pub fn remove_locked_projects<'a>(
    root: &Path,
    projects: impl IntoIterator<Item = &'a ModuleName>,
) -> Result<(), Box<dyn Error>> {
    let _guard = lockfile_write_guard(root)?;
    let path = root.join(LOCKFILE_NAME);
    if let Some(mut lockfile) = read_lock(&path)? {
        for project in projects {
            lockfile.remove_project(project);
        }
        write_metadata(&path, &lockfile.render())?;
    }
    Ok(())
}

pub fn write_metadata(path: &Path, text: &str) -> Result<(), Box<dyn Error>> {
    if fs::read_to_string(path).ok().as_deref() == Some(text) {
        return Ok(());
    }
    let mut output =
        tempfile::NamedTempFile::new_in(path.parent().expect("project file has a parent"))?;
    if let Ok(metadata) = fs::metadata(path) {
        output.as_file().set_permissions(metadata.permissions())?;
    }
    output.write_all(text.as_bytes())?;
    output.as_file().sync_all()?;
    output.persist(path)?;
    Ok(())
}

fn read_lock(path: &Path) -> Result<Option<Lockfile>, ProjectMetadataFileError> {
    match fs::read_to_string(path) {
        Ok(text) => {
            parse_lockfile(&text)
                .map(Some)
                .map_err(|source| ProjectMetadataFileError::Parse {
                    document: zkcl_runtime::SourceDocument::file(path.to_owned(), Some(text)),
                    source,
                })
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ProjectMetadataFileError::Read {
            path: path.to_owned(),
            source,
        }),
    }
}

fn lock_for(
    project: &Project,
    lock_root: &Path,
    local: &LocalProvider,
    registries: &RegistryCache,
    resolution: &Resolution,
    target_triple: String,
    features: BTreeSet<String>,
    scope: BuildScope,
    tool: Option<&ModuleId>,
) -> Result<LockedGraph, Box<dyn Error>> {
    let local = local.snapshot();
    let root = project.id();
    let edges = |module: &SelectedModule| {
        module
            .dependencies
            .iter()
            .map(|dep| locked_id(lock_root, &dep.module))
            .collect::<Result<_, _>>()
    };
    let roots = match tool {
        Some(id) => BTreeSet::from([id.clone()]),
        None => edges(&resolution.modules[&root])?,
    };
    let mut selected_systems: BTreeMap<_, BTreeSet<_>> = BTreeMap::new();
    for module in resolution.modules.values() {
        for dependency in &module.dependencies {
            if let Some(system) = &dependency.required_system {
                selected_systems
                    .entry(&dependency.module)
                    .or_default()
                    .insert(system.as_str());
            }
        }
    }
    let mut modules = BTreeMap::new();
    for (id, module) in &resolution.modules {
        if *id == root {
            continue;
        }
        let indexed = registries.selection(id, &module.release.id);
        let mut systems = if let Some(indexed) = &indexed {
            indexed.systems.clone()
        } else if let Some(definitions) = local.systems.get(id) {
            let SourceId::Path(root) = &id.source else {
                return Err(format!("missing source directory for {id}").into());
            };
            definitions
                .iter()
                .map(|(name, path)| Ok((name.clone(), path.strip_prefix(root)?.to_owned())))
                .collect::<Result<_, std::path::StripPrefixError>>()?
        } else {
            BTreeMap::new()
        };
        let aliases = if let Some(indexed) = &indexed {
            indexed.asdf_aliases.clone()
        } else if let Some(native) = local.native.get(id) {
            native.manifest.asdf_aliases.clone()
        } else {
            BTreeMap::new()
        };
        systems.retain(|name, _| {
            selected_systems
                .get(id)
                .is_some_and(|selected| selected.contains(name.as_str()))
        });
        let asdf_aliases = if systems.is_empty() {
            aliases
        } else {
            BTreeMap::new()
        };
        modules.insert(
            locked_id(lock_root, id)?,
            LockedModule {
                release: module.release.id.clone(),
                checksum: indexed.map(|release| release.checksum.to_string()),
                features: module.features.clone(),
                dependencies: edges(module)?,
                systems,
                asdf_aliases,
            },
        );
    }
    Ok(LockedGraph {
        scope,
        project: project.manifest.name.clone(),
        target_triple,
        features,
        roots,
        modules,
    })
}

fn lisp_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

fn absolute_id(root: &Path, id: &ModuleId) -> Result<ModuleId, Box<dyn Error>> {
    let SourceId::Path(path) = &id.source else {
        return Ok(id.clone());
    };
    Ok(ModuleId {
        name: id.name.clone(),
        source: SourceId::Path(root.join(path).canonicalize()?),
    })
}

fn locked_id(root: &Path, id: &ModuleId) -> Result<ModuleId, Box<dyn Error>> {
    let SourceId::Path(path) = &id.source else {
        return Ok(id.clone());
    };
    let path = relative_path(root, path)?;
    if path.to_str().is_none() {
        return Err(format!("the lockfile needs a UTF-8 path for {id}").into());
    }
    Ok(ModuleId {
        source: SourceId::Path(path),
        name: id.name.clone(),
    })
}

fn relative_path(root: &Path, path: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let mut root_components = root.components().peekable();
    let mut path_components = path.components().peekable();
    if root_components.peek() != path_components.peek() {
        return Err(format!(
            "{} cannot be expressed relative to {}",
            path.display(),
            root.display()
        )
        .into());
    }
    while let (Some(root), Some(path)) = (root_components.peek(), path_components.peek()) {
        if root != path {
            break;
        }
        root_components.next();
        path_components.next();
    }
    let mut relative: PathBuf = root_components.map(|_| "..").collect();
    relative.extend(path_components.map(|component| component.as_os_str()));
    if relative.as_os_str().is_empty() {
        relative.push(".");
    }
    Ok(relative)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_writers_merge_member_and_target_configurations() {
        let scratch =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.agents/tmp/zkcl/build-tasks");
        fs::create_dir_all(&scratch).unwrap();
        let temporary = tempfile::tempdir_in(scratch).unwrap();
        let root = temporary.path();
        fs::write(
            root.join("zkcl.sexp"),
            "(workspace (members \"one\" \"two\"))",
        )
        .unwrap();
        for name in ["one", "two"] {
            fs::create_dir(root.join(name)).unwrap();
            fs::write(
                root.join(name).join("zkcl.sexp"),
                format!("(project \"test/{name}\" (version \"1.0.0\"))"),
            )
            .unwrap();
        }
        let first = Project::open(&root.join("one")).unwrap();
        let second = Project::open(&root.join("two")).unwrap();
        // Resolve both before either writer publishes, reproducing a stale-reader race.
        let one =
            ResolvedProject::resolve(first.clone(), false, false, &[], None, Update::Keep).unwrap();
        let two = ResolvedProject::resolve(second, false, false, &[], None, Update::Keep).unwrap();
        one.write_lock().unwrap();
        two.write_lock().unwrap();
        let other_target = ResolvedProject::resolve_for_target(
            first,
            false,
            false,
            &[],
            None,
            Update::Keep,
            Some("wasm32-unknown-unknown"),
        )
        .unwrap();
        other_target.write_lock().unwrap();
        let lock = read_lock(&root.join(LOCKFILE_NAME)).unwrap().unwrap();
        assert_eq!(lock.graphs().count(), 3);
        assert!(!root.join("one").join(LOCKFILE_NAME).exists());
        assert!(!root.join("two").join(LOCKFILE_NAME).exists());
        assert_eq!(other_target.target(), "wasm32-unknown-unknown");
    }
}
