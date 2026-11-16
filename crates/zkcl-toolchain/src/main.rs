mod cli;
mod dist;
mod selector;
mod store;

use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail, ensure};
use zkcl_diagnostics::{Diagnostic, Note, NoteKind, Severity};

use dist::Distribution;
use selector::{Channel, ReleaseId, Selector};
use store::{State, Store, Transaction};

const PIN_FILE: &str = "zkcl-toolchain";

#[derive(Debug)]
enum SelectionError {
    NotInstalled(Selector),
    MissingProjectPin,
    UnmanagedLauncher,
}

impl SelectionError {
    fn help(&self) -> String {
        match self {
            Self::NotInstalled(selector) => {
                format!("install it with `zkcl toolchain install {selector}`")
            }
            Self::MissingProjectPin => {
                "pass a toolchain, for example `zkcl toolchain install stable`".to_owned()
            }
            Self::UnmanagedLauncher => {
                "install the launcher with install-toolchain.sh first".to_owned()
            }
        }
    }
}

impl fmt::Display for SelectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotInstalled(selector) => {
                write!(formatter, "toolchain {selector} is not installed")
            }
            Self::MissingProjectPin => formatter.write_str("no project pin found"),
            Self::UnmanagedLauncher => {
                formatter.write_str("this launcher is not in a managed installation")
            }
        }
    }
}

impl std::error::Error for SelectionError {}

fn main() -> ExitCode {
    let invocation = match cli::parse(env::args_os()) {
        Ok(invocation) => invocation,
        Err(error) => {
            zkcl_terminal::configure(error.color);
            let code = error.exit_code();
            let _ = error.print();
            return ExitCode::from(code as u8);
        }
    };
    zkcl_terminal::configure(invocation.color);
    match run(invocation) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let mut diagnostic = Diagnostic::new(Severity::Error, error.to_string());
            diagnostic.code = Some(if error.downcast_ref::<SelectionError>().is_some() {
                "toolchain/selection".into()
            } else {
                "toolchain/operation".into()
            });
            diagnostic
                .notes
                .extend(error.chain().skip(1).map(|cause| Note {
                    kind: NoteKind::Note,
                    message: format!("caused by: {cause}"),
                }));
            if let Some(selection) = error.downcast_ref::<SelectionError>() {
                diagnostic.notes.push(Note {
                    kind: NoteKind::Help,
                    message: selection.help(),
                });
            }
            zkcl_terminal::emit(&diagnostic);
            ExitCode::FAILURE
        }
    }
}

fn run(invocation: cli::Invocation) -> Result<()> {
    for directory in invocation.directories {
        env::set_current_dir(&directory)
            .with_context(|| format!("cannot enter {}", zkcl_terminal::display_path(&directory)))?;
    }
    match invocation.operation {
        cli::Operation::Bootstrap(request) => bootstrap(request),
        cli::Operation::Manage(request) => manage(request.command, invocation.toolchain.as_ref()),
        cli::Operation::Launch(args) => launch(
            &installed_store()?,
            &args,
            invocation.toolchain.as_ref(),
            invocation.as_package_runner,
        ),
    }
}

fn bootstrap(request: cli::Bootstrap) -> Result<()> {
    let cli::Bootstrap {
        prefix,
        source,
        selector,
        id,
    } = request;
    let store = Store::new(prefix, host()?);
    let distribution = Distribution::new(&source)?;
    let mut transaction = store.lock()?;
    transaction.check_launcher()?;
    ensure!(
        transaction.state.default.is_none() || transaction.state.source == distribution.source(),
        "this installation uses {}; use a different --prefix for another source",
        transaction.state.source
    );
    transaction.state.source = distribution.source().to_owned();
    let artifact = distribution.resolve(&Selector::Exact(id.clone()), store.host)?;
    remember_channel(&mut transaction.state, &selector, &id)?;
    transaction.install(&distribution, &artifact)?;
    transaction.state.default = Some(selector);
    transaction.promote_launcher(&env::current_exe()?)?;
    transaction.commit()?;
    println!("Installed zkcl and zkclx with {id}.");
    Ok(())
}

fn manage(command: cli::ManagementCommand, override_arg: Option<&Selector>) -> Result<()> {
    use cli::ManagementCommand as Command;
    let store = installed_store()?;
    match command {
        Command::Target { command } => manage_component(&store, command.into(), override_arg)?,
        Command::Component { command } => manage_component(&store, command.into(), override_arg)?,
        Command::List => {
            let _lock = store.read_lock()?;
            let state = store.read()?;
            let active = active_selector(&state, override_arg)?;
            let installed = store.installed_ids()?;
            for id in &installed {
                println!("{id}");
            }
            println!("Default: {}", describe(&state, &default_selector(&state)));
            let selection = describe(&state, &active);
            if state
                .resolve(&active)
                .is_some_and(|id| installed.contains(&id))
            {
                println!("Active: {selection}");
            } else {
                println!("Active: {selection} (not installed)");
            }
        }
        Command::Which { toolchain } => {
            let _lock = store.read_lock()?;
            let state = store.read()?;
            let selector = match toolchain {
                Some(selector) => selector,
                None => active_selector(&state, override_arg)?,
            };
            let (id, _) = selected_release(&store, &state, &selector)?;
            println!("{}", store.binary(&id).display());
        }
        Command::Default { toolchain: None } => {
            let _lock = store.read_lock()?;
            let state = store.read()?;
            println!("{}", describe(&state, &default_selector(&state)));
        }
        Command::Default {
            toolchain: Some(selector),
        } => {
            let mut transaction = store.lock()?;
            let id = install(&store, &mut transaction, &selector, false)?;
            transaction.state.default = Some(selector);
            transaction.commit()?;
            println!("Default toolchain: {id}");
        }
        Command::Install { toolchain } => {
            let selector = match toolchain {
                Some(selector) => selector,
                None => Selector::Exact(project_pin()?.ok_or(SelectionError::MissingProjectPin)?),
            };
            let mut transaction = store.lock()?;
            let id = install(&store, &mut transaction, &selector, true)?;
            transaction.commit()?;
            println!("Installed {id}.");
        }
        Command::Pin {
            toolchain: selector,
        } => {
            let path = project_pin_path()?;
            let mut transaction = store.lock()?;
            let id = install(&store, &mut transaction, &selector, false)?;
            transaction.commit()?;
            write_pin(&path, &id)?;
            println!("Pinned {id} in {}.", zkcl_terminal::display_path(&path));
        }
        Command::Unpin => {
            let path = project_pin_path()?;
            fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
            println!("Removed {}.", zkcl_terminal::display_path(&path));
        }
        Command::Upgrade { channel } => {
            let mut transaction = store.lock()?;
            let channel = channel.unwrap_or_else(|| default_channel(&transaction.state));
            let selector = Selector::Channel(channel);
            let id = install(&store, &mut transaction, &selector, true)?;
            transaction.state.default = Some(selector);
            transaction.commit()?;
            println!("Default toolchain: {channel} ({id})");
        }
        Command::Uninstall { version: id } => {
            store.lock()?.uninstall(&id)?;
            println!("Uninstalled {id}.");
        }
        Command::SelfUpdate { channel } => {
            let transaction = store.lock()?;
            let distribution = Distribution::new(&transaction.state.source)?;
            let artifact = distribution.resolve(&Selector::Channel(channel), store.host)?;
            let staging = tempfile::tempdir_in(&store.root)?;
            let launcher = distribution.install_launcher(&artifact, store.host, staging.path())?;
            transaction.promote_launcher(&launcher)?;
            println!("Updated the launcher from {}.", artifact.id);
        }
    }
    Ok(())
}

fn manage_component(
    store: &Store,
    request: cli::ComponentRequest,
    override_arg: Option<&Selector>,
) -> Result<()> {
    let transaction = store.lock()?;
    let selector = active_selector(&transaction.state, override_arg)?;
    let (id, installed) = selected_release(store, &transaction.state, &selector)?;
    let distribution = Distribution::new(&transaction.state.source)?;
    let artifact = distribution.resolve(&Selector::Exact(id.clone()), store.host)?;
    ensure!(
        installed.commit == artifact.commit,
        "release commit changed since installation"
    );
    match request {
        cli::ComponentRequest::ListTargets => {
            for target in artifact.sdk.keys() {
                let installed = store.release_dir(&id).join("sdk").join(target).is_dir();
                println!("{target}{}", if installed { " (installed)" } else { "" });
            }
        }
        cli::ComponentRequest::ListComponents => {
            if artifact.sdk.contains_key(store.host) {
                let path = format!("sdk/{}", store.host);
                println!(
                    "sdk{}",
                    if store.release_dir(&id).join(path).is_dir() {
                        " (installed)"
                    } else {
                        ""
                    }
                );
            }
        }
        cli::ComponentRequest::ModifyTarget { target, remove } => {
            ensure!(
                artifact.sdk.contains_key(&target),
                "release does not provide sdk for {target}"
            );
            transaction.component(&distribution, &artifact, "sdk", &target, remove)?;
            let operation = if remove { "remove" } else { "add" };
            println!("{operation} {target} for {id}");
        }
        cli::ComponentRequest::ModifyComponent { component, remove } => {
            let component = component.as_str();
            ensure!(
                component == "sdk" && artifact.sdk.contains_key(store.host),
                "release does not provide {component} for {}",
                store.host
            );
            transaction.component(&distribution, &artifact, component, store.host, remove)?;
            let operation = if remove { "remove" } else { "add" };
            println!("{operation} {component} for {id}");
        }
    }
    Ok(())
}

fn install(
    store: &Store,
    transaction: &mut Transaction<'_>,
    selector: &Selector,
    refresh: bool,
) -> Result<ReleaseId> {
    if (!refresh || matches!(selector, Selector::Exact(_)))
        && let Some(id) = transaction.state.resolve(selector)
        && store.installed(&id)?
    {
        return Ok(id);
    }
    let distribution = Distribution::new(&transaction.state.source)?;
    let artifact = distribution.resolve(selector, store.host)?;
    remember_channel(&mut transaction.state, selector, &artifact.id)?;
    transaction.install(&distribution, &artifact)?;
    Ok(artifact.id)
}

fn remember_channel(state: &mut State, selector: &Selector, id: &ReleaseId) -> Result<()> {
    if let Selector::Channel(channel) = selector {
        if let Some(previous) = state.channels.get(channel) {
            let precedence = match (id, previous) {
                (ReleaseId::Version(id), ReleaseId::Version(previous)) => {
                    id.cmp_precedence(previous)
                }
                _ => id.cmp(previous),
            };
            ensure!(
                !precedence.is_lt(),
                "{channel} points to {id}, older than the installed {previous}; select {id} explicitly to downgrade"
            );
        }
        state.channels.insert(*channel, id.clone());
    }
    Ok(())
}

fn default_selector(state: &State) -> Selector {
    state
        .default
        .clone()
        .unwrap_or(Selector::Channel(Channel::Stable))
}

fn default_channel(state: &State) -> Channel {
    match default_selector(state) {
        Selector::Channel(channel) => channel,
        Selector::Exact(ReleaseId::Nightly(_)) => Channel::Nightly,
        Selector::Exact(id) if Channel::Beta.accepts(&id) => Channel::Beta,
        Selector::Exact(_) => Channel::Stable,
    }
}

fn active_selector(state: &State, override_arg: Option<&Selector>) -> Result<Selector> {
    if let Some(arg) = override_arg {
        return Ok(arg.clone());
    }
    if let Some(value) = env::var_os("ZKCL_TOOLCHAIN") {
        return text(&value)
            .and_then(str::parse)
            .context("invalid ZKCL_TOOLCHAIN selection");
    }
    if let Some(id) = project_pin()? {
        return Ok(Selector::Exact(id));
    }
    Ok(default_selector(state))
}

fn selected_release(
    store: &Store,
    state: &State,
    selector: &Selector,
) -> Result<(ReleaseId, dist::Artifact)> {
    if let Some(id) = state.resolve(selector)
        && let Some(artifact) = store.artifact(&id)?
    {
        return Ok((id, artifact));
    }
    Err(SelectionError::NotInstalled(selector.clone()).into())
}

fn describe(state: &State, selector: &Selector) -> String {
    match (selector, state.resolve(selector)) {
        (Selector::Channel(_), Some(id)) => format!("{selector} ({id})"),
        _ => selector.to_string(),
    }
}

fn installed_store() -> Result<Store> {
    let executable = env::current_exe()?.canonicalize()?;
    let root = executable
        .parent()
        .filter(|path| path.ends_with("bin"))
        .and_then(Path::parent)
        .ok_or(SelectionError::UnmanagedLauncher)?;
    Ok(Store::new(root.to_owned(), host()?))
}

fn host() -> Result<&'static str> {
    match (env::consts::OS, env::consts::ARCH) {
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-gnu"),
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-gnu"),
        (os, arch) => bail!("no published zkcl toolchain for {os}/{arch}"),
    }
}

fn text(value: &OsStr) -> Result<&str> {
    value.to_str().context("toolchain arguments must be UTF-8")
}

fn project_pin() -> Result<Option<ReleaseId>> {
    let directory = env::current_dir()?;
    for directory in directory.ancestors() {
        let path = directory.join(PIN_FILE);
        let metadata = match path.symlink_metadata() {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
        };
        ensure!(
            metadata.is_file(),
            "{} must be a regular file",
            path.display()
        );
        let mut contents = String::new();
        File::open(&path)?
            .take(257)
            .read_to_string(&mut contents)
            .with_context(|| format!("read {}", path.display()))?;
        ensure!(contents.len() <= 256, "{} is too large", path.display());
        let id = contents
            .trim()
            .parse()
            .with_context(|| format!("invalid pin in {}", path.display()))?;
        return Ok(Some(id));
    }
    Ok(None)
}

fn project_pin_path() -> Result<PathBuf> {
    let directory = env::current_dir()?;
    for parent in directory.ancestors() {
        let path = parent.join(PIN_FILE);
        match path.symlink_metadata() {
            Ok(metadata) => {
                ensure!(
                    metadata.is_file(),
                    "{} must be a regular file",
                    path.display()
                );
                return Ok(path);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
        }
        if parent.join("zkcl.sexp").is_file() {
            return Ok(path);
        }
    }
    Ok(directory.join(PIN_FILE))
}

fn write_pin(path: &Path, id: &ReleaseId) -> Result<()> {
    let directory = path.parent().context("project pin has no parent")?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    writeln!(temporary, "{id}")?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn launch(
    store: &Store,
    args: &[OsString],
    override_arg: Option<&Selector>,
    as_x: bool,
) -> Result<()> {
    let _lock = store.read_lock()?;
    let state = store.read()?;
    let selector = active_selector(&state, override_arg)?;
    let (id, artifact) = selected_release(store, &state, &selector)?;
    if matches!(id, ReleaseId::Nightly(_))
        && let [arg] = args
        && (arg == "--version" || arg == "-V")
    {
        println!("zkcl {} ({id})", artifact.version);
        return Ok(());
    }
    let binary = store.binary(&id);
    let mut command = Command::new(&binary);
    command.args(args);
    let release_root = store.release_dir(&id);
    command.env("ZKCL_TOOLCHAIN_ROOT", &release_root);
    exec(command, as_x).with_context(|| format!("run {}", binary.display()))
}

#[cfg(unix)]
fn exec(mut command: Command, as_x: bool) -> Result<()> {
    use std::os::unix::process::CommandExt;

    if as_x {
        command.arg0("zkclx");
    }
    Err(command.exec().into())
}

#[cfg(not(unix))]
fn exec(_command: Command, _as_x: bool) -> Result<()> {
    bail!("this host cannot run managed zkcl toolchains")
}
