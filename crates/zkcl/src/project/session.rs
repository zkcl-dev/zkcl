use std::collections::BTreeSet;
use std::error::Error;
use std::path::{Path, PathBuf};

use zkcl_project::manifest::BuildTarget;
use zkcl_project::workspace::TargetSelection;
use zkcl_runtime::{EvalError, Root, Runtime};

use super::build_script::BuildOptions;
use super::delivery::{Delivery, LoadEvent};
use super::{Project, ProjectBundle, ResolvedProject, Update};

/// Dependency acquisition is explicit. Offline mode also requires an existing lock.
#[derive(Clone, Debug, Default)]
pub struct ResolveOptions {
    pub locked: bool,
    pub offline: bool,
    pub registries: Vec<PathBuf>,
    pub features: BTreeSet<String>,
    pub target: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct LoadOptions {
    pub resolve: ResolveOptions,
    pub target: TargetSelection,
    pub build: BuildOptions,
    /// Parent for per-session compilation files. None uses the project's target directory.
    pub output_directory: Option<PathBuf>,
}

/// One project initialized in a caller-owned runtime. Owns its compilation and
/// ASDF output directory. Keep this session alive while Lisp can use those paths.
/// Dropping it does not undo Lisp definitions or external initialization effects.
pub struct LoadedProject<'rt> {
    runtime: &'rt Runtime,
    resolved: ResolvedProject,
    delivery: Delivery,
    output: tempfile::TempDir,
    entry: Option<String>,
    test_suite: Option<String>,
}

/// Project resolution and runtime loading operations hosted on [`Project`].
pub trait ProjectSession {
    /// Resolve and prepare sources without creating a runtime or executing Lisp.
    fn resolve(&self, options: &ResolveOptions) -> Result<ResolvedProject, Box<dyn Error>>;

    /// Prepare dependencies and initialize the selected target on `runtime`.
    /// Register host functions and configure streams before calling this method.
    fn load<'rt>(
        &self,
        runtime: &'rt Runtime,
        options: &LoadOptions,
    ) -> Result<LoadedProject<'rt>, Box<dyn Error>>;
}

impl ProjectSession for Project {
    fn resolve(&self, options: &ResolveOptions) -> Result<ResolvedProject, Box<dyn Error>> {
        ResolvedProject::resolve_for_target(
            self.clone(),
            options.locked || options.offline,
            options.offline,
            &options.registries,
            Some(options.features.clone()),
            Update::Keep,
            options.target.as_deref(),
        )
    }

    fn load<'rt>(
        &self,
        runtime: &'rt Runtime,
        options: &LoadOptions,
    ) -> Result<LoadedProject<'rt>, Box<dyn Error>> {
        let resolved = self
            .resolve(&options.resolve)?
            .with_build_options(options.build.clone());
        resolved.fetch()?;
        resolved.load(
            runtime,
            &options.target,
            options.output_directory.as_deref(),
        )
    }
}

impl ResolvedProject {
    /// Compile and initialize already-fetched dependencies and a selected target.
    /// Dependency acquisition and lockfile writes remain separate operations.
    pub fn load<'rt>(
        self,
        runtime: &'rt Runtime,
        selection: &TargetSelection,
        output_directory: Option<&Path>,
    ) -> Result<LoadedProject<'rt>, Box<dyn Error>> {
        if self.target() != runtime.compilation_target().to_string() {
            return Err(format!(
                "project target {} differs from runtime target {}",
                self.target(),
                runtime.compilation_target()
            )
            .into());
        }
        let target = self.project.select_target(selection)?.cloned();
        let directory =
            output_directory.map_or_else(|| self.project.root.join("target/zkcl"), Path::to_owned);
        std::fs::create_dir_all(&directory)?;
        let output = tempfile::Builder::new()
            .prefix("session-")
            .tempdir_in(directory)?;
        if matches!(selection, TargetSelection::Test) {
            runtime.require_module("zk-test")?;
        }
        let delivery = self.load_target(runtime, output.path(), &self.project, target.as_ref())?;
        Ok(LoadedProject {
            runtime,
            resolved: self,
            delivery,
            output,
            entry: target.as_ref().and_then(|target| target.entry.clone()),
            test_suite: target.and_then(|target| target.suite),
        })
    }

    /// The common target initialization path used by the CLI, REPL and hosts.
    pub fn load_target(
        &self,
        runtime: &Runtime,
        output: &Path,
        project: &Project,
        target: Option<&BuildTarget>,
    ) -> Result<Delivery, Box<dyn Error>> {
        self.load_target_observing(runtime, output, project, target, &mut |_| Ok(()))
    }

    /// Observe target loads in execution order, including the source revision
    /// actually used to compile each native file.
    pub fn load_target_observing(
        &self,
        runtime: &Runtime,
        output: &Path,
        project: &Project,
        target: Option<&BuildTarget>,
        observe: &mut dyn FnMut(LoadEvent<'_>) -> Result<(), EvalError>,
    ) -> Result<Delivery, Box<dyn Error>> {
        let mut delivery = self.load_target_libraries_observing(runtime, output, project, observe)?;
        if let Some(target) = target {
            let generated = super::build_script::output_dir(output);
            let files = target
                .files
                .iter()
                .map(|file| project.compilation_source(file, &generated))
                .collect::<Result<Vec<_>, _>>()?;
            delivery.append_files_observing(runtime, &files, &output.join("target"), observe)?;
        }
        Ok(delivery)
    }
}

/// Resolve an entry without involving the Lisp reader or changing its package.
pub fn lookup_entry<'rt>(
    runtime: &'rt Runtime,
    qualified: &str,
) -> Result<Root<'rt>, Box<dyn Error>> {
    let (package, name) = qualified
        .split_once(':')
        .ok_or_else(|| format!("expected a package-qualified entry, found {qualified:?}"))?;
    let name = name.strip_prefix(':').unwrap_or(name);
    runtime
        .lookup_symbol(package, name)?
        .ok_or_else(|| format!("cannot find {qualified}").into())
}

impl LoadedProject<'_> {
    pub fn resolved(&self) -> &ResolvedProject {
        &self.resolved
    }
    pub fn delivery(&self) -> &Delivery {
        &self.delivery
    }
    pub fn output_directory(&self) -> &Path {
        self.output.path()
    }
    pub fn entry(&self) -> Option<&str> {
        self.entry.as_deref()
    }
    pub fn call_entry(&self) -> Result<Vec<Root<'_>>, Box<dyn Error>> {
        let name = self
            .entry
            .as_deref()
            .ok_or("loaded target has no binary entry")?;
        Ok(self.runtime.call(&lookup_entry(self.runtime, name)?, &[])?)
    }
    pub fn run_tests(&self) -> Result<(), Box<dyn Error>> {
        let suite = self
            .test_suite
            .as_deref()
            .ok_or("loaded target has no test suite")?;
        self.runtime.call(
            &lookup_entry(self.runtime, "ZK-TEST:CHECK")?,
            &[&lookup_entry(self.runtime, suite)?],
        )?;
        Ok(())
    }
    /// Export prepared code and declared ASDF resources for a source-free host.
    pub fn write_bundle(&self, destination: &Path) -> Result<ProjectBundle, super::BundleError> {
        ProjectBundle::write(
            &self.delivery,
            destination,
            self.resolved.target(),
            self.entry.clone(),
        )
    }
}
