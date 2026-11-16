//! Load a source project into a caller-owned runtime, optionally exporting a bundle.
use std::error::Error;
use std::path::PathBuf;
use zkcl::Runtime;
use zkcl::project::{LoadOptions, Project, ProjectSession, TargetSelection};

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args_os().skip(1);
    let path = PathBuf::from(
        args.next()
            .ok_or("usage: project PROJECT_PATH [BUNDLE_DIRECTORY]")?,
    );
    let runtime = Runtime::start()?;
    // Configure streams, environment and host callbacks here, before initialization.
    let project = Project::open(&path)?;
    let options = LoadOptions {
        target: TargetSelection::Binary(None),
        ..LoadOptions::default()
    };
    let loaded = project.load(&runtime, &options)?;
    for value in loaded.call_entry()? {
        println!("{}", runtime.repr(&value)?);
    }
    if let Some(destination) = args.next() {
        loaded.write_bundle(&PathBuf::from(destination))?;
    }
    Ok(())
}
