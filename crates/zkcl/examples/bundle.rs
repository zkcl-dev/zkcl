//! Load prepared project code and resources without the project resolver.
use std::error::Error;
use std::path::PathBuf;
use zkcl::Runtime;
use zkcl::project::ProjectBundle;

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args_os().skip(1);
    let directory = PathBuf::from(
        args.next()
            .ok_or("usage: bundle BUNDLE_DIRECTORY CACHE_DIRECTORY")?,
    );
    let cache = PathBuf::from(
        args.next()
            .ok_or("usage: bundle BUNDLE_DIRECTORY CACHE_DIRECTORY")?,
    );
    let runtime = Runtime::start()?;
    let bundle = ProjectBundle::open(&directory)?;
    bundle.load(&runtime, &cache)?;
    for value in bundle.call_entry(&runtime)? {
        println!("{}", runtime.repr(&value)?);
    }
    Ok(())
}
