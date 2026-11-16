use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::ProjectMetadataError;
use crate::manifest::{
    self, BuildTarget, BuildTargetKind, DependencySource, MANIFEST_NAME, Manifest, ManifestFile,
};
use crate::solver::{
    self as solver, ModuleId, PackageIndex, Release, ReleaseId, SourceId, VersionSpec,
};

/// Library-only loading, a named binary, or the project's test target.
#[derive(Clone, Debug, Default)]
pub enum TargetSelection {
    #[default]
    Library,
    Binary(Option<String>),
    Test,
}

#[derive(Clone)]
pub struct Project {
    pub root: PathBuf,
    pub manifest_path: PathBuf,
    pub manifest: Manifest,
}

#[derive(Clone)]
pub struct Workspace {
    pub root: PathBuf,
    pub manifest_path: PathBuf,
    pub members: Vec<Project>,
}

impl Workspace {
    pub fn open(path: &Path) -> Result<Self, Box<dyn Error>> {
        Self::open_manifest(path, None)
    }

    fn open_manifest(
        path: &Path,
        discovered: Option<manifest::Workspace>,
    ) -> Result<Self, Box<dyn Error>> {
        let manifest_path = if path.is_dir() {
            path.join(MANIFEST_NAME)
        } else {
            path.to_owned()
        }
        .canonicalize()?;
        let root = manifest_path
            .parent()
            .expect("canonical manifest parent")
            .to_owned();
        let workspace = if let Some(workspace) = discovered
            && manifest_path == path
        {
            workspace
        } else {
            let ManifestFile::Workspace(workspace) = read_manifest(&manifest_path)? else {
                return Err("expected a workspace declaration".into());
            };
            workspace
        };
        let mut members = Vec::<Project>::new();
        let mut names = BTreeSet::new();
        for member in workspace.members {
            let member = scoped_file_or_directory(&root, &member)?;
            let project = Project::open_metadata(&member)?;
            if project.root == root || !project.root.starts_with(&root) {
                return Err("workspace member must be inside its workspace root".into());
            }
            if members.iter().any(|previous| previous.root == project.root) {
                return Err(
                    format!("duplicate workspace member {}", project.root.display()).into(),
                );
            }
            if !names.insert(project.manifest.name.clone()) {
                return Err(format!("duplicate workspace module {}", project.manifest.name).into());
            }
            members.push(project);
        }
        Ok(Self {
            root,
            manifest_path,
            members,
        })
    }

    pub fn discover(start: &Path) -> Result<Option<Self>, Box<dyn Error>> {
        let start = start.canonicalize()?;
        let directory = if start.is_dir() {
            start.as_path()
        } else {
            start.parent().expect("canonical input parent")
        };
        for ancestor in directory.ancestors() {
            let path = ancestor.join(MANIFEST_NAME);
            if path.is_file()
                && let ManifestFile::Workspace(manifest) = read_manifest(&path)?
            {
                return Self::open_manifest(&path, Some(manifest)).map(Some);
            }
        }
        Ok(None)
    }
}

impl Project {
    pub fn open(manifest_path: &Path) -> Result<Self, Box<dyn Error>> {
        let project = Self::open_metadata(manifest_path)?;
        project.validate_authored_sources()?;
        Ok(project)
    }

    /// Read a manifest without requiring authored source files to exist.
    pub fn open_metadata(manifest_path: &Path) -> Result<Self, Box<dyn Error>> {
        Self::open_manifest(manifest_path, None)
    }

    fn open_manifest(path: &Path, discovered: Option<Manifest>) -> Result<Self, Box<dyn Error>> {
        let manifest_path = if path.is_dir() {
            path.join(MANIFEST_NAME)
        } else {
            path.to_owned()
        };
        let file_name = manifest_path.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected a project manifest path",
            )
        })?;
        let root = manifest_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .canonicalize()?;
        let manifest_path = scoped_file(&root, Path::new(file_name))?;
        let manifest = if let Some(manifest) = discovered
            && manifest_path == path
        {
            manifest
        } else {
            let ManifestFile::Project(manifest) = read_manifest(&manifest_path)? else {
                return Err(io::Error::other(format!(
                    "{} declares a workspace; select one of its project members",
                    manifest_path.display()
                ))
                .into());
            };
            manifest
        };
        Ok(Self {
            root,
            manifest_path,
            manifest,
        })
    }

    fn validate_authored_sources(&self) -> Result<(), Box<dyn Error>> {
        for file in self.authored_target_files() {
            self.source_file(file)?;
        }
        Ok(())
    }

    pub fn workspace(&self) -> Result<Option<Workspace>, Box<dyn Error>> {
        Ok(Workspace::discover(&self.root)?.filter(|workspace| {
            workspace
                .members
                .iter()
                .any(|member| member.root == self.root)
        }))
    }

    pub fn lock_root(&self) -> Result<PathBuf, Box<dyn Error>> {
        Ok(self
            .workspace()?
            .map_or_else(|| self.root.clone(), |workspace| workspace.root))
    }

    /// Discover a project and validate its authored source paths.
    pub fn discover(start: &Path) -> Result<Option<Self>, Box<dyn Error>> {
        Self::discover_with(start, true)
    }

    /// Discover manifest metadata without requiring authored source files to exist.
    /// Execution validates source paths when each input is materialized.
    pub fn discover_metadata(start: &Path) -> Result<Option<Self>, Box<dyn Error>> {
        Self::discover_with(start, false)
    }

    fn discover_with(start: &Path, validate_sources: bool) -> Result<Option<Self>, Box<dyn Error>> {
        let start = start.canonicalize()?;
        let directory = if start.is_dir() {
            start.as_path()
        } else {
            start.parent().expect("canonical input has a parent")
        };
        for ancestor in directory.ancestors() {
            let manifest_path = ancestor.join(MANIFEST_NAME);
            if !manifest_path.is_file() {
                continue;
            }
            let project = match read_manifest(&manifest_path)? {
                ManifestFile::Project(manifest) => {
                    Self::open_manifest(&manifest_path, Some(manifest))?
                }
                ManifestFile::Workspace(manifest) => {
                    let workspace = Workspace::open_manifest(&manifest_path, Some(manifest))?;
                    let member = if let Some(member) = workspace
                        .members
                        .iter()
                        .find(|member| directory.starts_with(&member.root))
                    {
                        member
                    } else if let [member] = workspace.members.as_slice() {
                        member
                    } else {
                        return Err(io::Error::other(format!(
                            "workspace {} has multiple projects; select a member directory or manifest",
                            manifest_path.display()
                        ))
                        .into());
                    };
                    Self::open_metadata(&member.manifest_path)?
                }
            };
            if validate_sources {
                project.validate_authored_sources()?;
            }
            return Ok(Some(project));
        }
        Ok(None)
    }

    pub fn id(&self) -> ModuleId {
        ModuleId {
            source: SourceId::Path(self.root.clone()),
            name: self.manifest.name.clone(),
        }
    }

    pub fn source_file(&self, file: &Path) -> Result<PathBuf, Box<dyn Error>> {
        scoped_file(&self.root, file)
    }

    pub fn authored_target_files(&self) -> impl Iterator<Item = &PathBuf> {
        self.manifest
            .targets
            .iter()
            .flat_map(|target| &target.files)
            .filter(|file| !self.manifest.is_build_output(file))
    }

    pub fn compilation_source(
        &self,
        file: &Path,
        generated: &Path,
    ) -> Result<PathBuf, Box<dyn Error>> {
        if self.manifest.is_build_output(file) {
            generated
                .canonicalize()
                .map_err(Into::into)
                .and_then(|root| scoped_file(&root, file))
                .map_err(|error| format!("build script output {}: {error}", file.display()).into())
        } else {
            self.source_file(file)
        }
    }

    pub fn select_target(
        &self,
        selection: &TargetSelection,
    ) -> Result<Option<&BuildTarget>, Box<dyn Error>> {
        let (kind, name) = match selection {
            TargetSelection::Library => return Ok(None),
            TargetSelection::Binary(name) => (BuildTargetKind::Binary, name.as_deref()),
            TargetSelection::Test => (BuildTargetKind::Test, None),
        };
        let mut candidates = self.manifest.targets.iter().filter(|target| {
            target.kind == kind && name.is_none_or(|name| target.name.as_deref() == Some(name))
        });
        let target = candidates
            .next()
            .ok_or_else(|| format!("no {selection:?} target in {}", self.manifest.name))?;
        if candidates.next().is_some() {
            return Err(format!(
                "several {kind:?} targets in {}; select a binary by name",
                self.manifest.name
            )
            .into());
        }
        Ok(Some(target))
    }
}

#[derive(Clone)]
pub struct LocalIndex {
    pub releases: BTreeMap<ModuleId, Release>,
    pub native: BTreeMap<ModuleId, Project>,
    pub systems: BTreeMap<ModuleId, BTreeMap<String, PathBuf>>,
    declared_systems: BTreeMap<ModuleId, BTreeMap<String, PathBuf>>,
    pending: BTreeMap<ModuleId, (Project, manifest::Dependency)>,
    activated: BTreeMap<ModuleId, solver::Dependency>,
    workspace_members: BTreeMap<manifest::ModuleName, Project>,
}

pub struct LocalProvider {
    data: RefCell<LocalIndex>,
    pub roots: Vec<solver::Dependency>,
}

impl LocalProvider {
    pub fn for_project(project: &Project) -> Result<Self, Box<dyn Error>> {
        let mut data = LocalIndex {
            releases: BTreeMap::new(),
            native: BTreeMap::new(),
            systems: BTreeMap::new(),
            declared_systems: BTreeMap::new(),
            pending: BTreeMap::new(),
            activated: BTreeMap::new(),
            workspace_members: project
                .workspace()?
                .into_iter()
                .flat_map(|workspace| workspace.members)
                .map(|member| (member.manifest.name.clone(), member))
                .collect(),
        };
        data.collect(project.clone())?;
        Ok(Self {
            data: RefCell::new(data),
            roots: vec![solver::Dependency {
                module: project.id(),
                requirement: VersionSpec::Exact(ReleaseId::Version(
                    project.manifest.version.clone(),
                )),
                required_system: None,
                features: BTreeSet::new(),
                condition: None,
            }],
        })
    }

    pub fn snapshot(&self) -> LocalIndex {
        self.data.borrow().clone()
    }
}

impl LocalIndex {
    fn collect(&mut self, project: Project) -> Result<(), Box<dyn Error>> {
        let module = project.id();
        if self.native.contains_key(&module) {
            return Ok(());
        }
        let definitions = system_definitions(&project.manifest)?
            .into_iter()
            .map(|(system, path)| Ok((system, project.source_file(&path)?)))
            .collect::<Result<BTreeMap<_, _>, Box<dyn Error>>>()?;
        let systems = definitions
            .keys()
            .chain(project.manifest.asdf_aliases.keys())
            .cloned()
            .collect();
        self.declared_systems
            .insert(module.clone(), definitions.clone());
        if !definitions.is_empty() {
            self.systems.insert(module.clone(), definitions);
        }
        self.releases.insert(
            module.clone(),
            Release {
                module: module.clone(),
                id: ReleaseId::Version(project.manifest.version.clone()),
                toolchain: project.manifest.toolchain.clone(),
                dependencies: Vec::new(),
                packages: project.manifest.packages.iter().cloned().collect(),
                nicknames: BTreeSet::new(),
                systems,
                features: project.manifest.features.clone(),
                exclusive_features: project.manifest.exclusive_features.clone(),
                yanked: false,
                available: true,
            },
        );
        self.native.insert(module.clone(), project.clone());
        let mut dependencies = Vec::new();
        for declared in &project.manifest.dependencies {
            if project
                .manifest
                .build
                .as_ref()
                .is_some_and(|build| build.dependencies.contains(&declared.alias))
            {
                continue;
            }
            let mut local = declared.clone();
            if matches!(
                &declared.source,
                DependencySource::Registry { registry: None }
            ) && let Some(member) = self.workspace_members.get(&declared.module)
            {
                local.source = DependencySource::Path {
                    path: member.root.clone(),
                };
            }
            let dependency = &local;
            let source = match &dependency.source {
                DependencySource::Path { .. } => SourceId::PathRequest {
                    consumer: project.root.clone(),
                    alias: dependency.alias.clone(),
                },
                DependencySource::Registry { registry } => {
                    SourceId::Registry(registry.clone().unwrap_or_else(|| "zkcl".to_owned()))
                }
                DependencySource::Git { url, rev } => SourceId::Git {
                    url: url.clone(),
                    rev: rev.clone(),
                },
            };
            let request = dependency_request(dependency, source);
            if matches!(dependency.source, DependencySource::Path { .. }) {
                self.pending.insert(
                    request.module.clone(),
                    (project.clone(), dependency.clone()),
                );
            }
            dependencies.push(request);
        }
        self.releases
            .get_mut(&module)
            .expect("registered native release")
            .dependencies = dependencies;
        Ok(())
    }

    fn dependency(
        &mut self,
        consumer: &Project,
        dependency: &manifest::Dependency,
    ) -> Result<solver::Dependency, Box<dyn Error>> {
        let path = match &dependency.source {
            DependencySource::Registry { registry } => {
                return Ok(dependency_request(
                    dependency,
                    SourceId::Registry(registry.clone().unwrap_or_else(|| "zkcl".to_owned())),
                ));
            }
            DependencySource::Path { path } => path,
            DependencySource::Git { url, rev } => {
                return Ok(dependency_request(
                    dependency,
                    SourceId::Git {
                        url: url.clone(),
                        rev: rev.clone(),
                    },
                ));
            }
        };
        let source_root = consumer.root.join(path).canonicalize()?;
        if !source_root.is_dir() {
            return Err(io::Error::other(format!(
                "dependency {:?} needs a source directory: {}",
                dependency.alias,
                source_root.display()
            ))
            .into());
        }
        let module = ModuleId {
            source: SourceId::Path(source_root.clone()),
            name: dependency.module.clone(),
        };
        let manifest_path = source_root.join(MANIFEST_NAME);
        if manifest_path.is_file() {
            let project = Project::open(&manifest_path)?;
            if project.manifest.name != dependency.module {
                return Err(io::Error::other(format!(
                    "dependency {:?} requests {}, but {} declares {}",
                    dependency.alias,
                    dependency.module,
                    manifest_path.display(),
                    project.manifest.name
                ))
                .into());
            }
            self.collect(project)?;
        } else {
            let id = ReleaseId::External(
                dependency
                    .release
                    .clone()
                    .unwrap_or_else(|| "local".to_owned()),
            );
            if let Some(previous) = self.releases.get(&module) {
                if previous.id != id {
                    return Err(io::Error::other(format!(
                        "local module {module} is requested with both release {} and {id}",
                        previous.id
                    ))
                    .into());
                }
            } else {
                self.releases.insert(
                    module.clone(),
                    Release {
                        module: module.clone(),
                        id,
                        toolchain: None,
                        dependencies: Vec::new(),
                        packages: BTreeSet::new(),
                        nicknames: BTreeSet::new(),
                        systems: BTreeSet::new(),
                        features: BTreeSet::new(),
                        exclusive_features: Vec::new(),
                        yanked: false,
                        available: true,
                    },
                );
            }
            if dependency.system.is_none() || dependency.asd.is_none() {
                return Err(io::Error::other(format!(
                    "external path dependency {:?} needs explicit system and asd fields",
                    dependency.alias
                ))
                .into());
            }
        }
        if let Some(system) = &dependency.system {
            if self
                .native
                .get(&module)
                .is_some_and(|project| project.manifest.asdf_aliases.contains_key(system))
            {
                if dependency.asd.is_some() {
                    return Err(format!("dependency {:?} selects native ASDF alias {system:?} and cannot supply an .asd file", dependency.alias).into());
                }
                return Ok(dependency_request(dependency, module.source));
            }
            let definition = match &dependency.asd {
                Some(asd) => scoped_file(&source_root, asd)?,
                None => self.declared_systems.get(&module).and_then(|definitions| definitions.get(system)).cloned()
                    .ok_or_else(|| io::Error::other(format!(
                        "dependency {:?} selects ASDF system {system:?}, which {module} does not declare; supply its asd path",
                        dependency.alias
                    )))?,
            };
            let definitions = self.systems.entry(module.clone()).or_default();
            let primary = system.split('/').next().expect("system is nonempty");
            for name in [system.as_str(), primary] {
                if let Some(previous) = definitions.get(name) {
                    if previous != &definition {
                        return Err(io::Error::other(format!(
                            "ASDF system {name:?} in {module} is declared by both {} and {}",
                            previous.display(),
                            definition.display()
                        ))
                        .into());
                    }
                } else {
                    definitions.insert(name.to_owned(), definition.clone());
                }
                self.releases
                    .get_mut(&module)
                    .expect("registered source release")
                    .systems
                    .insert(name.to_owned());
            }
        }
        Ok(dependency_request(dependency, module.source))
    }
}

pub fn system_definitions(manifest: &Manifest) -> io::Result<BTreeMap<String, PathBuf>> {
    let mut definitions = BTreeMap::<String, PathBuf>::new();
    for (system, definition) in &manifest.systems {
        let primary = system.split('/').next().expect("system is nonempty");
        for name in [system.as_str(), primary] {
            if let Some(previous) = definitions.get(name) {
                if previous != definition {
                    return Err(io::Error::other(format!(
                        "ASDF system {name:?} in {} is declared by both {} and {}",
                        manifest.name,
                        previous.display(),
                        definition.display()
                    )));
                }
            } else {
                definitions.insert(name.to_owned(), definition.clone());
            }
        }
    }
    Ok(definitions)
}

pub fn dependency_request(
    dependency: &manifest::Dependency,
    source: SourceId,
) -> solver::Dependency {
    let requirement = match (&dependency.requirement, &dependency.release) {
        (Some(requirement), _) => VersionSpec::Version(requirement.clone()),
        (_, Some(release)) => VersionSpec::Exact(ReleaseId::External(release.clone())),
        _ => VersionSpec::Any,
    };
    solver::Dependency {
        module: ModuleId {
            source,
            name: dependency.module.clone(),
        },
        requirement,
        required_system: dependency.system.clone(),
        features: dependency.features.iter().cloned().collect(),
        condition: dependency.condition.clone(),
    }
}

impl PackageIndex for LocalProvider {
    type Error = io::Error;

    fn begin_graph(&self) -> Result<(), Self::Error> {
        let mut data = self.data.borrow_mut();
        data.activated.clear();
        let native_ids: BTreeSet<_> = data.native.keys().cloned().collect();
        data.releases.retain(|id, _| native_ids.contains(id));
        data.systems.clear();
        for release in data.releases.values_mut() {
            release.systems.clear();
        }
        data.systems = data.declared_systems.clone();
        let projects: Vec<_> = data.native.values().cloned().collect();
        for project in projects {
            let id = project.id();
            let systems = data.declared_systems[&id]
                .keys()
                .chain(project.manifest.asdf_aliases.keys())
                .cloned()
                .collect();
            data.releases.get_mut(&id).expect("native release").systems = systems;
        }
        Ok(())
    }

    fn activate(&self, dependency: &solver::Dependency) -> Result<solver::Dependency, Self::Error> {
        let mut data = self.data.borrow_mut();
        if let Some(active) = data.activated.get(&dependency.module) {
            return Ok(active.clone());
        }
        let Some((consumer, declaration)) = data.pending.get(&dependency.module).cloned() else {
            return Ok(dependency.clone());
        };
        let active = data
            .dependency(&consumer, &declaration)
            .map_err(|error| io::Error::other(error.to_string()))?;
        data.activated
            .insert(dependency.module.clone(), active.clone());
        Ok(active)
    }

    fn refresh(&self, release: &Release) -> Result<Release, Self::Error> {
        Ok(self
            .data
            .borrow()
            .releases
            .get(&release.module)
            .cloned()
            .unwrap_or_else(|| release.clone()))
    }

    fn releases(&self, module: &ModuleId) -> Result<Vec<Release>, Self::Error> {
        Ok(self
            .data
            .borrow()
            .releases
            .get(module)
            .cloned()
            .into_iter()
            .collect())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProjectMetadataFileError {
    #[error("cannot read {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("{}: {source}", document.path().expect("project metadata comes from a file").display())]
    Parse {
        document: zkcl_diagnostics::SourceDocument,
        source: ProjectMetadataError,
    },
}

fn read_manifest(path: &Path) -> Result<ManifestFile, ProjectMetadataFileError> {
    let text = fs::read_to_string(path).map_err(|source| ProjectMetadataFileError::Read {
        path: path.to_owned(),
        source,
    })?;
    manifest::parse_manifest(&text).map_err(|source| ProjectMetadataFileError::Parse {
        document: zkcl_diagnostics::SourceDocument::file(path.to_owned(), Some(text)),
        source,
    })
}

fn scoped_file_or_directory(root: &Path, relative: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let path = root.join(relative).canonicalize()?;
    if !path.starts_with(root) {
        return Err(io::Error::other(format!(
            "source path {} resolves outside {}",
            relative.display(),
            root.display()
        ))
        .into());
    }
    Ok(path)
}

fn scoped_file(root: &Path, relative: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let path = scoped_file_or_directory(root, relative)?;
    if !path.is_file() {
        return Err(
            io::Error::other(format!("source path is not a file: {}", path.display())).into(),
        );
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Sources(PathBuf);

    impl Sources {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../../.agents/tmp/zkcl/conditional_local_sources")
                .join(format!(
                    "local-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
            fs::create_dir_all(&directory).unwrap();
            Self(directory)
        }

        fn write(&self, relative: &str, source: &str) {
            let path = self.0.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, source).unwrap();
        }
    }

    impl Drop for Sources {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn solve(provider: &LocalProvider) -> Result<solver::Resolution, solver::SolveError> {
        solver::solve(
            provider,
            &solver::SolveRequest {
                roots: provider.roots.clone(),
                ..Default::default()
            },
        )
    }

    #[test]
    fn workspace_discovery_and_default_registry_links_are_lazy() {
        let sources = Sources::new();
        sources.write("zkcl.sexp", r#"(workspace (members "app" "lib"))"#);
        sources.write(
            "app/zkcl.sexp",
            r#"(project "test/app" (version "1.0.0") (features "extra")
            (dependency "lib" (module "test/lib") (version "^1") (when (feature "extra"))))"#,
        );
        sources.write(
            "lib/zkcl.sexp",
            r#"(project "test/lib" (version "1.2.0") (library (files "missing.lisp")))"#,
        );
        let app = Project::open(&sources.0.join("app")).unwrap();
        let workspace = Workspace::discover(&app.root).unwrap().unwrap();
        assert_eq!(workspace.members.len(), 2);
        assert_eq!(app.lock_root().unwrap(), sources.0.canonicalize().unwrap());
        let mut provider = LocalProvider::for_project(&app).unwrap();
        assert_eq!(solve(&provider).unwrap().modules.len(), 1);
        provider.roots[0].features.insert("extra".to_owned());
        assert!(solve(&provider).is_err());
        sources.write("lib/missing.lisp", "(defun lib () 42)");
        assert_eq!(solve(&provider).unwrap().modules.len(), 2);
    }

    #[test]
    fn workspace_links_check_versions_and_preserve_other_registries() {
        let sources = Sources::new();
        sources.write("zkcl.sexp", r#"(workspace (members "app" "lib"))"#);
        sources.write("lib/zkcl.sexp", r#"(project "test/lib" (version "1.0.0"))"#);
        sources.write(
            "app/zkcl.sexp",
            r#"(project "test/app" (version "1.0.0")
            (dependency "lib" (module "test/lib") (version "^2")))"#,
        );
        let app = Project::open(&sources.0.join("app")).unwrap();
        assert!(solve(&LocalProvider::for_project(&app).unwrap()).is_err());
        sources.write(
            "app/zkcl.sexp",
            r#"(project "test/app" (version "1.0.0")
            (dependency "lib" (module "test/lib") (version "^1") (registry "private")))"#,
        );
        let app = Project::open(&sources.0.join("app")).unwrap();
        let provider = LocalProvider::for_project(&app).unwrap();
        assert_eq!(
            provider.snapshot().releases[&app.id()].dependencies[0]
                .module
                .source,
            SourceId::Registry("private".to_owned())
        );
    }

    #[test]
    fn workspace_rejects_duplicate_canonical_members_and_names() {
        let sources = Sources::new();
        sources.write("app/zkcl.sexp", r#"(project "test/app" (version "1.0.0"))"#);
        sources.write("lib/zkcl.sexp", r#"(project "test/app" (version "1.0.0"))"#);
        sources.write(
            "zkcl.sexp",
            r#"(workspace (members "app" "app/zkcl.sexp"))"#,
        );
        assert!(Workspace::open(&sources.0).is_err());
        sources.write("zkcl.sexp", r#"(workspace (members "app" "lib"))"#);
        assert!(Workspace::open(&sources.0).is_err());
    }

    #[test]
    fn inactive_local_sources_are_not_opened_or_validated() {
        let sources = Sources::new();
        sources.write(
            "app/zkcl.sexp",
            r#"
            (project "test/app" (version "1.0.0") (features "optional")
              (dependency "missing" (module "test/missing") (path "../missing")
                (when (feature "optional")))
              (dependency "bad-asd" (module "test/foreign") (path "../foreign")
                (system "foreign") (asd "missing.asd") (when (feature "optional"))))
        "#,
        );
        sources.write("foreign/exists", "");
        let project = Project::open(&sources.0.join("app")).unwrap();
        let mut provider = LocalProvider::for_project(&project).unwrap();
        assert_eq!(solve(&provider).unwrap().modules.len(), 1);
        provider.roots[0].features.insert("optional".to_owned());
        assert!(solve(&provider).is_err());
    }

    #[test]
    fn transitive_feature_union_activates_native_and_explicit_asd_sources() {
        let sources = Sources::new();
        sources.write(
            "app/zkcl.sexp",
            r#"
            (project "test/app" (version "1.0.0")
              (dependency "lib" (module "test/lib") (path "../lib"))
              (dependency "other" (module "test/other") (path "../other")))
        "#,
        );
        sources.write(
            "lib/zkcl.sexp",
            r#"
            (project "test/lib" (version "1.0.0") (features "optional")
              (dependency "native" (module "test/native") (path "../native")
                (when (feature "optional")))
              (dependency "foreign" (module "test/foreign") (path "../foreign")
                (system "foreign/core") (asd "foreign.asd")
                (when (feature "optional"))))
        "#,
        );
        sources.write(
            "other/zkcl.sexp",
            r#"
            (project "test/other" (version "1.0.0")
              (dependency "lib" (module "test/lib") (path "../lib") (features "optional")))
        "#,
        );
        sources.write(
            "native/zkcl.sexp",
            r#"(project "test/native" (version "1.0.0"))"#,
        );
        sources.write("foreign/foreign.asd", "(asdf:defsystem \"foreign/core\")");
        let project = Project::open(&sources.0.join("app")).unwrap();
        let provider = LocalProvider::for_project(&project).unwrap();
        assert_eq!(provider.snapshot().native.len(), 1);
        let result = solve(&provider).unwrap();
        assert_eq!(result.modules.len(), 5);
        let foreign = result
            .order
            .iter()
            .find(|id| id.name.as_str() == "test/foreign")
            .unwrap();
        assert!(provider.snapshot().systems[foreign].contains_key("foreign/core"));
    }

    #[test]
    fn each_active_asd_edge_validates_its_own_declaration() {
        let sources = Sources::new();
        sources.write(
            "app/zkcl.sexp",
            r#"
            (project "test/app" (version "1.0.0")
              (dependency "valid" (module "test/foreign") (path "../foreign")
                (system "foreign") (asd "foreign.asd"))
              (dependency "invalid" (module "test/foreign") (path "../foreign")
                (system "foreign") (asd "missing.asd")))
        "#,
        );
        sources.write("foreign/foreign.asd", "(asdf:defsystem \"foreign\")");
        let project = Project::open(&sources.0.join("app")).unwrap();
        let provider = LocalProvider::for_project(&project).unwrap();
        assert!(solve(&provider).is_err());
    }

    #[test]
    fn graph_reconstruction_discards_external_declarations_from_abandoned_branches() {
        let sources = Sources::new();
        sources.write(
            "app/zkcl.sexp",
            r#"
            (project "test/app" (version "1.0.0")
              (dependency "first" (module "test/foreign") (path "../foreign")
                (release "first") (system "foreign") (asd "first.asd"))
              (dependency "second" (module "test/foreign") (path "../foreign")
                (release "second") (system "foreign") (asd "second.asd")))
        "#,
        );
        sources.write("foreign/first.asd", "(asdf:defsystem \"foreign\")");
        sources.write("foreign/second.asd", "(asdf:defsystem \"foreign\")");
        let project = Project::open(&sources.0.join("app")).unwrap();
        let provider = LocalProvider::for_project(&project).unwrap();
        let edges = provider.snapshot().releases[&project.id()]
            .dependencies
            .clone();
        provider.begin_graph().unwrap();
        let first = provider.activate(&edges[0]).unwrap();
        assert_eq!(
            provider.releases(&first.module).unwrap()[0].id,
            ReleaseId::External("first".to_owned())
        );
        provider.begin_graph().unwrap();
        let second = provider.activate(&edges[1]).unwrap();
        let snapshot = provider.snapshot();
        assert_eq!(
            snapshot.releases[&second.module].id,
            ReleaseId::External("second".to_owned())
        );
        assert!(snapshot.systems[&second.module]["foreign"].ends_with("second.asd"));
    }

    #[test]
    fn resolves_sibling_paths_and_multiple_external_systems_in_one_graph() {
        let sources = Sources::new();
        sources.write(
            "app/zkcl.sexp",
            r#"
            (project "test/app" (version "1.0.0")
              (dependency "lib" (module "test/lib") (path "../lib")))
        "#,
        );
        sources.write(
            "lib/zkcl.sexp",
            r#"
            (project "test/lib" (version "1.0.0")
              (dependency "core" (module "test/foreign") (path "../foreign")
                (system "foreign/core") (asd "foreign.asd"))
              (dependency "test" (module "test/foreign") (path "../foreign")
                (system "foreign/test") (asd "foreign.asd")))
        "#,
        );
        sources.write("foreign/foreign.asd", "(asdf:defsystem \"foreign\")");
        let project = Project::open(&sources.0.join("app")).unwrap();
        let provider = LocalProvider::for_project(&project).unwrap();
        let result = solver::solve(
            &provider,
            &solver::SolveRequest {
                roots: provider.roots.clone(),
                ..Default::default()
            },
        )
        .unwrap();
        let names: Vec<_> = result
            .order
            .iter()
            .map(|module| module.name.as_str())
            .collect();
        assert_eq!(names, ["test/foreign", "test/lib", "test/app"]);
        let external = result.order.first().unwrap();
        assert_eq!(
            result.modules[external].release.id,
            ReleaseId::External("local".to_owned())
        );
        assert_eq!(provider.snapshot().systems[external].len(), 3);
    }

    #[test]
    fn includes_root_package_ownership_in_resolution() {
        let sources = Sources::new();
        sources.write(
            "app/zkcl.sexp",
            r#"
            (project "test/app" (version "1.0.0") (packages "SHARED")
              (dependency "lib" (module "test/lib") (path "../lib")))
        "#,
        );
        sources.write(
            "lib/zkcl.sexp",
            r#"
            (project "test/lib" (version "1.0.0") (packages "SHARED"))
        "#,
        );
        let project = Project::open(&sources.0.join("app")).unwrap();
        let provider = LocalProvider::for_project(&project).unwrap();
        let error = solver::solve(
            &provider,
            &solver::SolveRequest {
                roots: provider.roots.clone(),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(error, solver::SolveError::NameConflict { name, .. } if name == "SHARED"));
    }

    #[test]
    fn registry_dependencies_become_requests_without_local_source_access() {
        let sources = Sources::new();
        sources.write(
            "app/zkcl.sexp",
            r#"
            (project "test/app" (version "1.0.0")
              (dependency "remote" (module "test/remote") (version "^2")
                (registry "private")))
        "#,
        );
        let project = Project::open(&sources.0.join("app")).unwrap();
        let provider = LocalProvider::for_project(&project).unwrap();
        let snapshot = provider.snapshot();
        let root = &snapshot.releases[&project.id()];
        let dependency = &root.dependencies[0];
        assert_eq!(
            dependency.module.source,
            SourceId::Registry("private".to_owned())
        );
        assert_eq!(dependency.module.name.as_str(), "test/remote");
        assert_eq!(dependency.requirement.to_string(), "^2");
        assert!(provider.releases(&dependency.module).unwrap().is_empty());
    }

    #[test]
    fn git_dependencies_are_lazy_and_preserve_feature_metadata() {
        let sources = Sources::new();
        sources.write(
            "app/zkcl.sexp",
            r#"
            (project "test/app" (version "1.0.0") (features "remote")
              (dependency "remote" (module "test/remote")
                (git "https://example.invalid/remote.git")
                (rev "0123456789abcdef0123456789abcdef01234567")
                (when (feature "remote"))))
        "#,
        );
        let project = Project::open(&sources.0.join("app")).unwrap();
        let provider = LocalProvider::for_project(&project).unwrap();
        let snapshot = provider.snapshot();
        let release = &snapshot.releases[&project.id()];
        assert_eq!(release.features, BTreeSet::from(["remote".to_owned()]));
        let dependency = &release.dependencies[0];
        assert!(matches!(dependency.module.source, SourceId::Git { .. }));
        assert_eq!(
            dependency.condition,
            Some(solver::Condition::Feature("remote".to_owned()))
        );
        assert!(provider.releases(&dependency.module).unwrap().is_empty());
    }

    #[test]
    fn uses_native_system_declarations_without_a_consumer_asd_path() {
        let sources = Sources::new();
        sources.write(
            "app/zkcl.sexp",
            r#"
            (project "test/app" (version "1.0.0")
              (dependency "lib" (module "test/lib") (path "../lib")
                (system "LIB/CORE")))
        "#,
        );
        sources.write(
            "lib/zkcl.sexp",
            r#"
            (project "test/lib" (version "1.0.0")
              (asdf-system "lib/core" "lib.asd"))
        "#,
        );
        sources.write("lib/lib.asd", "(asdf:defsystem \"lib/core\")");
        let project = Project::open(&sources.0.join("app")).unwrap();
        let provider = LocalProvider::for_project(&project).unwrap();
        let result = solver::solve(
            &provider,
            &solver::SolveRequest {
                roots: provider.roots.clone(),
                ..Default::default()
            },
        )
        .unwrap();
        let lib = result.order.first().unwrap();
        assert_eq!(
            result.modules[lib].release.systems,
            BTreeSet::from(["lib".to_owned(), "lib/core".to_owned()])
        );
        assert_eq!(
            provider.snapshot().systems[lib]["lib"],
            provider.snapshot().systems[lib]["lib/core"]
        );
    }

    #[test]
    fn rejects_secondary_systems_with_conflicting_primary_definitions() {
        let ManifestFile::Project(manifest) = manifest::parse_manifest(
            r#"
            (project "test/lib" (version "1.0.0")
              (asdf-system "lib/core" "core.asd")
              (asdf-system "lib/tests" "tests.asd"))
        "#,
        )
        .unwrap() else {
            panic!("expected project")
        };
        let error = system_definitions(&manifest).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("ASDF system \"lib\""));
        assert!(message.contains("core.asd"));
        assert!(message.contains("tests.asd"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_source_symlink_that_escapes_its_module_root() {
        let sources = Sources::new();
        sources.write(
            "app/zkcl.sexp",
            r#"
            (project "test/app" (version "1.0.0")
              (library (files "link.lisp")))
        "#,
        );
        sources.write("outside.lisp", "(print :outside)");
        std::os::unix::fs::symlink("../outside.lisp", sources.0.join("app/link.lisp")).unwrap();
        let error = Project::open(&sources.0.join("app"))
            .err()
            .expect("escaping source must fail");
        assert!(error.to_string().contains("resolves outside"));
    }
}
