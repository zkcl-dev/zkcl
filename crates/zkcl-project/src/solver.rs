//! Deterministic dependency resolution over a fixed provider snapshot.
//!
//! Providers supply metadata, independently of how source archives are fetched.
//! Providers canonicalize local source identities when an edge becomes active.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error as StdError;
use std::fmt;
use std::path::PathBuf;

use semver::{Version, VersionReq};
use thiserror::Error;

use crate::manifest::ModuleName;

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum SourceId {
    Path(PathBuf),
    /// An unactivated manifest declaration, never a resolved or lockfile identity.
    /// Providers replace it with a canonical Path only after its condition holds.
    PathRequest {
        consumer: PathBuf,
        alias: String,
    },
    Registry(String),
    Git {
        url: String,
        rev: String,
    },
}

impl fmt::Display for SourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Path(path) => write!(f, "path:{}", path.display()),
            Self::PathRequest { consumer, alias } => {
                write!(f, "dependency {alias:?} in {}", consumer.display())
            }
            Self::Registry(identity) => write!(f, "registry:{identity}"),
            Self::Git { url, rev } => write!(f, "git:{url}#{rev}"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ModuleId {
    pub source: SourceId,
    pub name: ModuleName,
}

impl fmt::Display for ModuleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.name, self.source)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum ReleaseId {
    Version(Version),
    External(String),
}

impl fmt::Display for ReleaseId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Version(version) => version.fmt(f),
            Self::External(identity) => identity.fmt(f),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VersionSpec {
    Any,
    Version(VersionReq),
    Exact(ReleaseId),
}

impl fmt::Display for VersionSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Any => f.write_str("*"),
            Self::Version(requirement) => requirement.fmt(f),
            Self::Exact(release) => write!(f, "={release}"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Condition {
    Feature(String),
    Platform(String),
    All(Vec<Condition>),
    Any(Vec<Condition>),
    Not(Box<Condition>),
}

impl Condition {
    fn active(&self, features: &BTreeSet<String>, platform: &str) -> bool {
        match self {
            Self::Feature(feature) => features.contains(feature),
            Self::Platform(expected) => expected == platform,
            Self::All(conditions) => conditions.iter().all(|c| c.active(features, platform)),
            Self::Any(conditions) => conditions.iter().any(|c| c.active(features, platform)),
            Self::Not(condition) => !condition.active(features, platform),
        }
    }

    // Negative feature conditions make activation non-monotonic under feature
    // union. Reject those expressions rather than retaining an activated edge.
    fn monotonic_features(&self, negated: bool) -> bool {
        match self {
            Self::Feature(_) => !negated,
            Self::Platform(_) => true,
            Self::All(conditions) | Self::Any(conditions) => {
                conditions.iter().all(|c| c.monotonic_features(negated))
            }
            Self::Not(condition) => condition.monotonic_features(!negated),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Dependency {
    pub module: ModuleId,
    pub requirement: VersionSpec,
    pub required_system: Option<String>,
    pub features: BTreeSet<String>,
    pub condition: Option<Condition>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Release {
    pub module: ModuleId,
    pub id: ReleaseId,
    pub toolchain: Option<VersionReq>,
    pub dependencies: Vec<Dependency>,
    pub packages: BTreeSet<String>,
    pub nicknames: BTreeSet<String>,
    pub systems: BTreeSet<String>,
    pub features: BTreeSet<String>,
    pub exclusive_features: Vec<BTreeSet<String>>,
    pub yanked: bool,
    pub available: bool,
}

pub trait PackageIndex {
    type Error: StdError + Send + Sync + 'static;

    /// Start a graph reconstruction, discarding branch-local source declarations.
    fn begin_graph(&self) -> Result<(), Self::Error> {
        Ok(())
    }

    /// Prepare an active edge, including canonicalizing and validating its source.
    fn activate(&self, dependency: &Dependency) -> Result<Dependency, Self::Error> {
        Ok(dependency.clone())
    }

    /// Refresh metadata augmented by newly activated source declarations.
    fn refresh(&self, release: &Release) -> Result<Release, Self::Error> {
        Ok(release.clone())
    }

    /// Return all known releases from one immutable metadata snapshot.
    fn releases(&self, module: &ModuleId) -> Result<Vec<Release>, Self::Error>;
}

#[derive(Clone, Debug, Default)]
pub struct SolveRequest {
    pub roots: Vec<Dependency>,
    pub locked: BTreeMap<ModuleId, ReleaseId>,
    pub features: BTreeSet<String>,
    pub platform: String,
    pub toolchain: Option<Version>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectedModule {
    pub release: Release,
    pub features: BTreeSet<String>,
    /// Edges whose conditions are active with the final feature union.
    pub dependencies: Vec<Dependency>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Resolution {
    pub modules: BTreeMap<ModuleId, SelectedModule>,
    /// Dependencies precede their consumers. Ties use module identity order.
    pub order: Vec<ModuleId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DependencyConstraint {
    pub requirement: VersionSpec,
    pub required_system: Option<String>,
    pub features: BTreeSet<String>,
    pub path: Vec<ModuleId>,
}

impl fmt::Display for DependencyConstraint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.requirement)?;
        if let Some(system) = &self.required_system {
            write!(f, " providing ASDF system {system:?}")?;
        }
        if !self.features.is_empty() {
            write!(f, " with features {:?}", self.features)?;
        }
        write!(f, " via {}", display_path(&self.path))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RejectedRelease {
    pub release: ReleaseId,
    pub reason: String,
}

#[derive(Debug, Error)]
pub enum SolveError {
    #[error("cannot read releases for {module}: {source}")]
    PackageIndex {
        module: ModuleId,
        #[source]
        source: Box<dyn StdError + Send + Sync>,
    },
    #[error("provider returned duplicate release {release} for {module}")]
    DuplicateRelease {
        module: ModuleId,
        release: ReleaseId,
    },
    #[error("provider returned a release for {actual} when queried for {module}")]
    ReleaseModuleMismatch {
        module: ModuleId,
        actual: Box<ModuleId>,
    },
    #[error("no release of {module} satisfies {}. Candidates: {}", display_constraints(.constraints), display_rejections(.candidates))]
    NoMatchingRelease {
        module: ModuleId,
        constraints: Vec<DependencyConstraint>,
        candidates: Vec<RejectedRelease>,
    },
    #[error("{kind} {name:?} is owned by both {} and {}", display_path(.first), display_path(.second))]
    NameConflict {
        kind: &'static str,
        name: String,
        first: Vec<ModuleId>,
        second: Vec<ModuleId>,
    },
    #[error("dependency cycle: {}", display_path(.path))]
    Cycle { path: Vec<ModuleId> },
    #[error("negative feature condition in dependency on {module} via {}", display_path(.path))]
    NonMonotonicCondition {
        module: ModuleId,
        path: Vec<ModuleId>,
    },
}

impl SolveError {
    fn is_conflict(&self) -> bool {
        match self {
            Self::NoMatchingRelease { .. }
            | Self::NameConflict { .. }
            | Self::Cycle { .. }
            | Self::NonMonotonicCondition { .. } => true,
            Self::PackageIndex { .. }
            | Self::DuplicateRelease { .. }
            | Self::ReleaseModuleMismatch { .. } => false,
        }
    }
}

fn display_path(path: &[ModuleId]) -> String {
    path.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" -> ")
}

fn display_constraints(constraints: &[DependencyConstraint]) -> String {
    constraints
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

fn display_rejections(candidates: &[RejectedRelease]) -> String {
    if candidates.is_empty() {
        return "none in the provider snapshot".to_owned();
    }
    candidates
        .iter()
        .map(|c| format!("{} ({})", c.release, c.reason))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Resolve with all reachable locked releases if possible. If the lock conflicts
/// with the requested graph, prefer locked releases per module, then newer
/// stable SemVer releases. Opaque release IDs have deterministic lexical priority.
pub fn solve(
    provider: &impl PackageIndex,
    request: &SolveRequest,
) -> Result<Resolution, SolveError> {
    let mut solver = Solver {
        provider,
        request,
        require_locked: !request.locked.is_empty(),
    };
    let resolution = solver.search(BTreeMap::new());
    if solver.require_locked && resolution.as_ref().is_err_and(SolveError::is_conflict) {
        solver.require_locked = false;
        return solver.search(BTreeMap::new());
    }
    resolution
}

struct Solver<'a, P> {
    provider: &'a P,
    request: &'a SolveRequest,
    require_locked: bool,
}

#[derive(Default)]
struct ActiveGraph {
    constraints: BTreeMap<ModuleId, Vec<DependencyConstraint>>,
    features: BTreeMap<ModuleId, BTreeSet<String>>,
    dependencies: BTreeMap<ModuleId, Vec<Dependency>>,
    paths: BTreeMap<ModuleId, Vec<ModuleId>>,
}

impl ActiveGraph {
    fn add(&mut self, dependency: &Dependency, path: Vec<ModuleId>) {
        self.features
            .entry(dependency.module.clone())
            .or_default()
            .extend(dependency.features.iter().cloned());
        self.paths
            .entry(dependency.module.clone())
            .or_insert_with(|| path.clone());
        self.constraints
            .entry(dependency.module.clone())
            .or_default()
            .push(DependencyConstraint {
                requirement: dependency.requirement.clone(),
                required_system: dependency.required_system.clone(),
                features: dependency.features.clone(),
                path,
            });
    }
}

impl<P: PackageIndex> Solver<'_, P> {
    fn graph(&self, selected: &BTreeMap<ModuleId, Release>) -> Result<ActiveGraph, SolveError> {
        if let Some(root) = self.request.roots.first() {
            self.provider
                .begin_graph()
                .map_err(|source| SolveError::PackageIndex {
                    module: root.module.clone(),
                    source: Box::new(source),
                })?;
        }
        let mut graph = ActiveGraph::default();
        for dependency in &self.request.roots {
            if dependency
                .condition
                .as_ref()
                .is_none_or(|c| c.active(&self.request.features, &self.request.platform))
            {
                let dependency = self.activate(dependency)?;
                graph.add(&dependency, vec![dependency.module.clone()]);
            }
        }
        let mut activated = BTreeSet::new();
        loop {
            let mut added = false;
            let reachable: Vec<_> = graph.constraints.keys().cloned().collect();
            for module in reachable {
                let Some(release) = selected.get(&module) else {
                    continue;
                };
                for (index, dependency) in release.dependencies.iter().enumerate() {
                    self.check_condition(dependency, &graph.paths[&module])?;
                    if activated.contains(&(module.clone(), index)) {
                        continue;
                    }
                    if dependency.condition.as_ref().is_some_and(|c| {
                        !c.active(&graph.features[&module], &self.request.platform)
                    }) {
                        continue;
                    }
                    let dependency = self.activate(dependency)?;
                    let mut path = graph.paths[&module].clone();
                    path.push(dependency.module.clone());
                    graph.add(&dependency, path);
                    graph
                        .dependencies
                        .entry(module.clone())
                        .or_default()
                        .push(dependency.clone());
                    activated.insert((module.clone(), index));
                    added = true;
                }
            }
            if !added {
                return Ok(graph);
            }
        }
    }

    fn activate(&self, dependency: &Dependency) -> Result<Dependency, SolveError> {
        self.provider
            .activate(dependency)
            .map_err(|source| SolveError::PackageIndex {
                module: dependency.module.clone(),
                source: Box::new(source),
            })
    }

    fn check_condition(
        &self,
        dependency: &Dependency,
        parent: &[ModuleId],
    ) -> Result<(), SolveError> {
        if dependency
            .condition
            .as_ref()
            .is_some_and(|c| !c.monotonic_features(false))
        {
            let mut path = parent.to_vec();
            path.push(dependency.module.clone());
            return Err(SolveError::NonMonotonicCondition {
                module: dependency.module.clone(),
                path,
            });
        }
        Ok(())
    }

    fn search(
        &mut self,
        mut selected: BTreeMap<ModuleId, Release>,
    ) -> Result<Resolution, SolveError> {
        let graph = self.graph(&selected)?;
        for release in selected.values_mut() {
            *release =
                self.provider
                    .refresh(release)
                    .map_err(|source| SolveError::PackageIndex {
                        module: release.module.clone(),
                        source: Box::new(source),
                    })?;
        }
        for (module, constraints) in &graph.constraints {
            if let Some(release) = selected.get(module)
                && let Some(reason) = self.rejection(release, constraints, &graph.features[module])
            {
                return Err(SolveError::NoMatchingRelease {
                    module: module.clone(),
                    constraints: constraints.clone(),
                    candidates: vec![RejectedRelease {
                        release: release.id.clone(),
                        reason,
                    }],
                });
            }
        }
        self.check_names(&selected, &graph)?;
        let Some(module) = graph
            .constraints
            .keys()
            .find(|module| !selected.contains_key(*module))
            .cloned()
        else {
            let order = dependency_order(&graph)?;
            let modules = graph
                .constraints
                .keys()
                .map(|module| {
                    (
                        module.clone(),
                        SelectedModule {
                            release: selected[module].clone(),
                            features: graph.features[module].clone(),
                            dependencies: graph
                                .dependencies
                                .get(module)
                                .cloned()
                                .unwrap_or_default(),
                        },
                    )
                })
                .collect();
            return Ok(Resolution { modules, order });
        };
        let candidates = self.candidates(&module)?;
        let mut rejected = Vec::new();
        let mut branch_error = None;
        for release in candidates {
            let release =
                self.provider
                    .refresh(&release)
                    .map_err(|source| SolveError::PackageIndex {
                        module: module.clone(),
                        source: Box::new(source),
                    })?;
            if let Some(reason) = self.rejection(
                &release,
                &graph.constraints[&module],
                &graph.features[&module],
            ) {
                rejected.push(RejectedRelease {
                    release: release.id,
                    reason,
                });
                continue;
            }
            let mut branch = selected.clone();
            branch.insert(module.clone(), release);
            match self.search(branch) {
                Ok(resolution) => return Ok(resolution),
                Err(error) if error.is_conflict() => branch_error = Some(error),
                Err(error) => return Err(error),
            }
        }
        Err(
            branch_error.unwrap_or_else(|| SolveError::NoMatchingRelease {
                module: module.clone(),
                constraints: graph.constraints[&module].clone(),
                candidates: rejected,
            }),
        )
    }

    fn candidates(&mut self, module: &ModuleId) -> Result<Vec<Release>, SolveError> {
        let mut releases =
            self.provider
                .releases(module)
                .map_err(|source| SolveError::PackageIndex {
                    module: module.clone(),
                    source: Box::new(source),
                })?;
        let mut ids = BTreeSet::new();
        for release in &releases {
            if &release.module != module {
                return Err(SolveError::ReleaseModuleMismatch {
                    module: module.clone(),
                    actual: Box::new(release.module.clone()),
                });
            }
            if !ids.insert(&release.id) {
                return Err(SolveError::DuplicateRelease {
                    module: module.clone(),
                    release: release.id.clone(),
                });
            }
        }
        let locked = self.request.locked.get(module);
        releases.sort_by(|a, b| {
            let priority = |release: &Release| {
                (
                    locked == Some(&release.id),
                    matches!(&release.id, ReleaseId::Version(version) if version.pre.is_empty()),
                )
            };
            priority(b).cmp(&priority(a)).then_with(|| b.id.cmp(&a.id))
        });
        Ok(releases)
    }

    fn rejection(
        &self,
        release: &Release,
        constraints: &[DependencyConstraint],
        features: &BTreeSet<String>,
    ) -> Option<String> {
        let locked = self.request.locked.get(&release.module);
        if self.require_locked
            && let Some(id) = locked
            && id != &release.id
        {
            return Some(format!("lock selects {id}"));
        }
        let locked = locked == Some(&release.id);
        if !release.available {
            return Some("source content is unavailable".to_owned());
        }
        if release.yanked && !locked {
            return Some("release is yanked and is not already locked".to_owned());
        }
        if let (Some(requirement), Some(version)) = (&release.toolchain, &self.request.toolchain)
            && !requirement.matches(version)
        {
            return Some(format!("requires zkcl {requirement}, found {version}"));
        }
        for constraint in constraints {
            let matches = match (&constraint.requirement, &release.id) {
                (VersionSpec::Any, ReleaseId::Version(version)) => version.pre.is_empty() || locked,
                (VersionSpec::Any, ReleaseId::External(_)) => true,
                (VersionSpec::Version(requirement), ReleaseId::Version(version)) => {
                    requirement.matches(version)
                }
                (VersionSpec::Exact(expected), actual) => expected == actual,
                _ => false,
            };
            if !matches {
                return Some(format!("does not satisfy {constraint}"));
            }
            if let Some(system) = &constraint.required_system
                && !release.systems.contains(system)
            {
                return Some(format!("does not provide ASDF system {system:?}"));
            }
        }
        if let Some(unknown) = features.difference(&release.features).next() {
            return Some(format!("does not provide feature {unknown:?}"));
        }
        for exclusive in &release.exclusive_features {
            let enabled: Vec<_> = features.intersection(exclusive).collect();
            if enabled.len() > 1 {
                return Some(format!(
                    "mutually exclusive features {enabled:?} are enabled"
                ));
            }
        }
        None
    }

    fn check_names(
        &self,
        selected: &BTreeMap<ModuleId, Release>,
        graph: &ActiveGraph,
    ) -> Result<(), SolveError> {
        let mut packages = BTreeMap::new();
        let mut systems = BTreeMap::new();
        for module in graph.constraints.keys() {
            let Some(release) = selected.get(module) else {
                continue;
            };
            for (kind, names, owners) in [
                (
                    "Lisp package or global nickname",
                    release
                        .packages
                        .union(&release.nicknames)
                        .cloned()
                        .collect::<BTreeSet<_>>(),
                    &mut packages,
                ),
                ("ASDF system", release.systems.clone(), &mut systems),
            ] {
                for name in names {
                    if let Some(previous) = owners.insert(name.clone(), module.clone()) {
                        return Err(SolveError::NameConflict {
                            kind,
                            name,
                            first: graph.paths[&previous].clone(),
                            second: graph.paths[module].clone(),
                        });
                    }
                }
            }
        }
        Ok(())
    }
}

fn dependency_order(graph: &ActiveGraph) -> Result<Vec<ModuleId>, SolveError> {
    fn visit(
        module: &ModuleId,
        graph: &ActiveGraph,
        visiting: &mut Vec<ModuleId>,
        finished: &mut BTreeSet<ModuleId>,
        order: &mut Vec<ModuleId>,
    ) -> Result<(), SolveError> {
        if finished.contains(module) {
            return Ok(());
        }
        if let Some(start) = visiting.iter().position(|ancestor| ancestor == module) {
            let mut path = visiting[start..].to_vec();
            path.push(module.clone());
            return Err(SolveError::Cycle { path });
        }
        visiting.push(module.clone());
        let dependencies: BTreeSet<_> = graph
            .dependencies
            .get(module)
            .into_iter()
            .flatten()
            .map(|dependency| &dependency.module)
            .collect();
        for dependency in dependencies {
            visit(dependency, graph, visiting, finished, order)?;
        }
        visiting.pop();
        finished.insert(module.clone());
        order.push(module.clone());
        Ok(())
    }
    let mut order = Vec::new();
    let mut finished = BTreeSet::new();
    for module in graph.constraints.keys() {
        visit(module, graph, &mut Vec::new(), &mut finished, &mut order)?;
    }
    Ok(order)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;
    use std::io;

    #[derive(Default)]
    struct Index(BTreeMap<ModuleId, Vec<Release>>);

    impl PackageIndex for Index {
        type Error = Infallible;

        fn releases(&self, module: &ModuleId) -> Result<Vec<Release>, Self::Error> {
            Ok(self.0.get(module).cloned().unwrap_or_default())
        }
    }

    impl Index {
        fn add(&mut self, release: Release) {
            self.0
                .entry(release.module.clone())
                .or_default()
                .push(release);
        }
    }

    fn id(name: &str) -> ModuleId {
        ModuleId {
            source: SourceId::Registry("test-registry".to_owned()),
            name: format!("test/{name}").parse().unwrap(),
        }
    }

    fn dependency(name: &str, version: &str) -> Dependency {
        Dependency {
            module: id(name),
            requirement: VersionSpec::Version(version.parse().unwrap()),
            required_system: None,
            features: BTreeSet::new(),
            condition: None,
        }
    }

    fn release(name: &str, version: &str, dependencies: Vec<Dependency>) -> Release {
        Release {
            module: id(name),
            id: ReleaseId::Version(version.parse().unwrap()),
            toolchain: None,
            dependencies,
            packages: BTreeSet::new(),
            nicknames: BTreeSet::new(),
            systems: BTreeSet::new(),
            features: BTreeSet::new(),
            exclusive_features: Vec::new(),
            yanked: false,
            available: true,
        }
    }

    fn request(roots: Vec<Dependency>) -> SolveRequest {
        SolveRequest {
            roots,
            ..SolveRequest::default()
        }
    }

    #[test]
    fn activates_source_requests_only_after_transitive_feature_union() {
        struct LazyIndex {
            index: Index,
            activations: std::cell::Cell<usize>,
        }
        impl PackageIndex for LazyIndex {
            type Error = std::convert::Infallible;

            fn activate(&self, dependency: &Dependency) -> Result<Dependency, Self::Error> {
                let mut dependency = dependency.clone();
                if matches!(dependency.module.source, SourceId::PathRequest { .. }) {
                    self.activations.set(self.activations.get() + 1);
                    dependency.module = id("optional");
                }
                Ok(dependency)
            }

            fn releases(&self, module: &ModuleId) -> Result<Vec<Release>, Self::Error> {
                assert!(!matches!(module.source, SourceId::PathRequest { .. }));
                self.index.releases(module)
            }
        }
        let mut optional = dependency("optional", "^1");
        optional.module.source = SourceId::PathRequest {
            consumer: PathBuf::from("/consumer"),
            alias: "optional".to_owned(),
        };
        optional.condition = Some(Condition::Feature("enabled".to_owned()));
        let mut a = release("a", "1.0.0", vec![optional]);
        a.features.insert("enabled".to_owned());
        let mut enable_a = dependency("a", "^1");
        enable_a.features.insert("enabled".to_owned());
        let mut index = Index::default();
        index.add(a);
        index.add(release("b", "1.0.0", vec![enable_a]));
        index.add(release("optional", "1.0.0", vec![]));
        let provider = LazyIndex {
            index,
            activations: std::cell::Cell::new(0),
        };
        let inactive = solve(&provider, &request(vec![dependency("a", "^1")])).unwrap();
        assert_eq!(inactive.modules.len(), 1);
        assert_eq!(provider.activations.get(), 0);
        let active = solve(
            &provider,
            &request(vec![dependency("a", "^1"), dependency("b", "^1")]),
        )
        .unwrap();
        assert_eq!(active.modules.len(), 3);
        assert!(provider.activations.get() > 0);
        assert_eq!(
            active.modules[&id("a")].dependencies[0].module,
            id("optional")
        );
    }

    #[test]
    fn picks_a_release_for_the_selected_toolchain() {
        let mut index = Index::default();
        let mut new = release("a", "1.1.0", vec![]);
        new.toolchain = Some("^0.2".parse().unwrap());
        index.add(new);
        index.add(release("a", "1.0.0", vec![]));
        let mut request = request(vec![dependency("a", "^1")]);
        request.toolchain = Some("0.1.0".parse().unwrap());
        request
            .locked
            .insert(id("a"), ReleaseId::Version("1.1.0".parse().unwrap()));
        let result = solve(&index, &request).unwrap();
        assert_eq!(result.modules[&id("a")].release.id.to_string(), "1.0.0");
    }

    #[test]
    fn backtracks_across_intersecting_transitive_ranges() {
        let mut index = Index::default();
        index.add(release("a", "2.0.0", vec![dependency("shared", "^2")]));
        index.add(release("a", "1.0.0", vec![dependency("shared", "^1")]));
        index.add(release("b", "1.0.0", vec![dependency("shared", ">=1, <2")]));
        index.add(release("shared", "2.0.0", vec![]));
        index.add(release("shared", "1.5.0", vec![]));
        let result = solve(
            &index,
            &request(vec![dependency("a", "*"), dependency("b", "*")]),
        )
        .unwrap();
        assert_eq!(result.modules[&id("a")].release.id.to_string(), "1.0.0");
        assert_eq!(
            result.modules[&id("shared")].release.id.to_string(),
            "1.5.0"
        );
        let position = |name| {
            result
                .order
                .iter()
                .position(|module| module == &id(name))
                .unwrap()
        };
        assert!(position("shared") < position("a"));
        assert!(position("shared") < position("b"));
    }

    #[test]
    fn retains_a_transitive_lock_when_updating_its_consumer() {
        let mut index = Index::default();
        index.add(release("a", "2.0.0", vec![dependency("shared", "^2")]));
        index.add(release("a", "1.1.0", vec![dependency("shared", "^1")]));
        index.add(release("a", "1.0.0", vec![dependency("shared", "^1")]));
        index.add(release("shared", "2.0.0", vec![]));
        index.add(release("shared", "1.0.0", vec![]));
        let mut request = request(vec![dependency("a", "*")]);
        request
            .locked
            .insert(id("shared"), ReleaseId::Version(Version::new(1, 0, 0)));

        let result = solve(&index, &request).unwrap();
        assert_eq!(result.modules[&id("a")].release.id.to_string(), "1.1.0");
        assert_eq!(
            result.modules[&id("shared")].release.id,
            request.locked[&id("shared")]
        );

        request.roots[0] = dependency("a", "^2");
        let result = solve(&index, &request).unwrap();
        assert_eq!(result.modules[&id("a")].release.id.to_string(), "2.0.0");
        assert_eq!(
            result.modules[&id("shared")].release.id.to_string(),
            "2.0.0"
        );
    }

    #[test]
    fn transitive_provider_failure_is_not_a_version_conflict() {
        struct FailingIndex(Index);

        impl PackageIndex for FailingIndex {
            type Error = io::Error;

            fn releases(&self, module: &ModuleId) -> Result<Vec<Release>, Self::Error> {
                if module == &id("shared") {
                    return Err(io::Error::new(io::ErrorKind::PermissionDenied, "snapshot"));
                }
                Ok(self.0.releases(module).unwrap())
            }
        }

        let mut index = FailingIndex(Index::default());
        index
            .0
            .add(release("a", "2.0.0", vec![dependency("shared", "*")]));
        index.0.add(release("a", "1.0.0", vec![]));
        let error = solve(&index, &request(vec![dependency("a", "*")])).unwrap_err();
        assert!(
            matches!(&error, SolveError::PackageIndex { module, .. } if module == &id("shared"))
        );
        let source = error.source().unwrap().downcast_ref::<io::Error>().unwrap();
        assert_eq!(source.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn preserves_locked_yanked_release_but_requires_available_content() {
        let mut index = Index::default();
        let mut old = release("a", "1.0.0", vec![]);
        old.yanked = true;
        index.add(old);
        index.add(release("a", "1.1.0", vec![]));
        let mut request = request(vec![dependency("a", "^1")]);
        assert_eq!(
            solve(&index, &request).unwrap().modules[&id("a")]
                .release
                .id
                .to_string(),
            "1.1.0"
        );
        request
            .locked
            .insert(id("a"), ReleaseId::Version("1.0.0".parse().unwrap()));
        assert_eq!(
            solve(&index, &request).unwrap().modules[&id("a")]
                .release
                .id
                .to_string(),
            "1.0.0"
        );
        index.0.get_mut(&id("a")).unwrap()[0].available = false;
        assert_eq!(
            solve(&index, &request).unwrap().modules[&id("a")]
                .release
                .id
                .to_string(),
            "1.1.0"
        );
    }

    #[test]
    fn incompatible_versions_report_both_dependency_paths() {
        let mut index = Index::default();
        index.add(release("a", "1.0.0", vec![dependency("shared", "^1")]));
        index.add(release("b", "1.0.0", vec![dependency("shared", "^2")]));
        index.add(release("shared", "1.0.0", vec![]));
        index.add(release("shared", "2.0.0", vec![]));
        let error = solve(
            &index,
            &request(vec![dependency("a", "*"), dependency("b", "*")]),
        )
        .unwrap_err();
        let SolveError::NoMatchingRelease {
            module,
            constraints,
            ..
        } = error
        else {
            panic!("expected range conflict")
        };
        assert_eq!(module, id("shared"));
        assert_eq!(constraints.len(), 2);
        assert_eq!(constraints[0].path, vec![id("a"), id("shared")]);
        assert_eq!(constraints[1].path, vec![id("b"), id("shared")]);
    }

    #[test]
    fn distinct_sources_cannot_own_the_same_global_package() {
        let mut index = Index::default();
        let mut first = release("a", "1.0.0", vec![]);
        first.packages.insert("SHARED".to_owned());
        index.add(first);
        let mut second = release("a", "1.0.0", vec![]);
        second.module.source = SourceId::Registry("another-registry".to_owned());
        second.nicknames.insert("SHARED".to_owned());
        let mut other = dependency("a", "*");
        other.module = second.module.clone();
        index.add(second);
        let error = solve(&index, &request(vec![dependency("a", "*"), other])).unwrap_err();
        assert!(matches!(error, SolveError::NameConflict { name, .. } if name == "SHARED"));
    }

    #[test]
    fn backtracks_when_system_binding_or_package_ownership_requires_it() {
        let mut index = Index::default();
        let mut a2 = release("a", "2.0.0", vec![]);
        a2.packages.insert("B".to_owned());
        index.add(a2);
        let mut a1 = release("a", "1.0.0", vec![]);
        a1.systems.insert("a".to_owned());
        index.add(a1);
        let mut b = release("b", "1.0.0", vec![]);
        b.packages.insert("B".to_owned());
        index.add(b);
        let result = solve(
            &index,
            &request(vec![dependency("a", "*"), dependency("b", "*")]),
        )
        .unwrap();
        assert_eq!(result.modules[&id("a")].release.id.to_string(), "1.0.0");
        let mut required = dependency("a", "*");
        required.required_system = Some("a".to_owned());
        let result = solve(&index, &request(vec![required])).unwrap();
        assert_eq!(result.modules[&id("a")].release.id.to_string(), "1.0.0");
    }

    #[test]
    fn cycle_has_the_complete_closed_chain_and_can_backtrack() {
        let mut index = Index::default();
        index.add(release("a", "2.0.0", vec![dependency("b", "*")]));
        index.add(release("b", "1.0.0", vec![dependency("a", "*")]));
        let error = solve(&index, &request(vec![dependency("a", "*")])).unwrap_err();
        assert!(
            matches!(error, SolveError::Cycle { path } if path == vec![id("a"), id("b"), id("a")])
        );
        index.add(release("a", "1.0.0", vec![]));
        let result = solve(&index, &request(vec![dependency("a", "*")])).unwrap();
        assert_eq!(result.order, vec![id("a")]);
    }

    #[test]
    fn feature_union_activates_edges_only_when_explicitly_enabled() {
        let mut index = Index::default();
        let mut optional = dependency("optional", "*");
        optional.condition = Some(Condition::Feature("extra".to_owned()));
        let mut shared = release("shared", "1.0.0", vec![optional]);
        shared.features.insert("extra".to_owned());
        index.add(shared);
        index.add(release("optional", "1.0.0", vec![]));
        index.add(release("a", "1.0.0", vec![dependency("shared", "*")]));
        let mut enabled = dependency("shared", "*");
        enabled.features.insert("extra".to_owned());
        index.add(release("b", "1.0.0", vec![enabled]));
        let result = solve(&index, &request(vec![dependency("a", "*")])).unwrap();
        assert!(!result.modules.contains_key(&id("optional")));
        let result = solve(
            &index,
            &request(vec![dependency("a", "*"), dependency("b", "*")]),
        )
        .unwrap();
        assert!(result.modules.contains_key(&id("optional")));
        assert_eq!(result.modules[&id("shared")].dependencies.len(), 1);
    }

    #[test]
    fn prereleases_require_an_explicit_selection_and_opaque_ids_are_exact() {
        let mut index = Index::default();
        index.add(release("a", "2.0.0-rc.1", vec![]));
        index.add(release("a", "1.0.0", vec![]));
        let result = solve(&index, &request(vec![dependency("a", "*")])).unwrap();
        assert_eq!(result.modules[&id("a")].release.id.to_string(), "1.0.0");
        let result = solve(&index, &request(vec![dependency("a", "=2.0.0-rc.1")])).unwrap();
        assert_eq!(
            result.modules[&id("a")].release.id.to_string(),
            "2.0.0-rc.1"
        );
        let mut external = release("external", "1.0.0", vec![]);
        external.id = ReleaseId::External("quicklisp-2026-01-01".to_owned());
        index.add(external);
        let mut external = dependency("external", "*");
        external.requirement =
            VersionSpec::Exact(ReleaseId::External("quicklisp-2026-01-01".to_owned()));
        assert!(solve(&index, &request(vec![external])).is_ok());
    }

    #[test]
    fn duplicate_asdf_systems_report_their_transitive_owners() {
        let mut index = Index::default();
        index.add(release(
            "root",
            "1.0.0",
            vec![dependency("a", "*"), dependency("b", "*")],
        ));
        for name in ["a", "b"] {
            let mut owner = release(name, "1.0.0", vec![]);
            owner.systems.insert("shared/core".to_owned());
            index.add(owner);
        }
        let error = solve(&index, &request(vec![dependency("root", "*")])).unwrap_err();
        let SolveError::NameConflict {
            kind,
            name,
            first,
            second,
        } = error
        else {
            panic!("expected duplicate system owners");
        };
        assert_eq!(kind, "ASDF system");
        assert_eq!(name, "shared/core");
        assert_eq!(first, vec![id("root"), id("a")]);
        assert_eq!(second, vec![id("root"), id("b")]);
    }

    #[test]
    fn conflicting_feature_requests_keep_both_paths() {
        let mut index = Index::default();
        let mut shared = release("shared", "1.0.0", vec![]);
        shared.features = BTreeSet::from(["fast".to_owned(), "small".to_owned()]);
        shared.exclusive_features = vec![shared.features.clone()];
        index.add(shared);
        for (name, feature) in [("a", "fast"), ("b", "small")] {
            let mut edge = dependency("shared", "*");
            edge.features.insert(feature.to_owned());
            index.add(release(name, "1.0.0", vec![edge]));
        }
        let error = solve(
            &index,
            &request(vec![dependency("a", "*"), dependency("b", "*")]),
        )
        .unwrap_err();
        let SolveError::NoMatchingRelease {
            constraints,
            candidates,
            ..
        } = error
        else {
            panic!("expected incompatible features");
        };
        assert_eq!(constraints.len(), 2);
        assert_eq!(constraints[0].path, vec![id("a"), id("shared")]);
        assert_eq!(constraints[1].path, vec![id("b"), id("shared")]);
        assert!(candidates[0].reason.contains("mutually exclusive"));
    }
}
