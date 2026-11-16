#[path = "src/bundle.rs"]
mod bundle;

use std::{env, fs, path::PathBuf};

fn main() -> std::io::Result<()> {
    println!("cargo:rerun-if-changed=runtime");
    println!("cargo:rerun-if-changed=src/bundle.rs");

    let mut modules = Vec::new();
    for file in fs::read_dir("runtime")? {
        let path = file?.path();
        if path.extension().is_some_and(|extension| extension == "mjs") {
            let name = path.file_stem().unwrap().to_str().unwrap().to_owned();
            modules.push((name, fs::read_to_string(&path)?));
        }
    }
    let sources: Vec<_> = modules
        .iter()
        .map(|(name, source)| (name.as_str(), source.as_str()))
        .collect();
    let runtime = bundle::bundle_modules("runtime", &sources);
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("zkcl-runtime.mjs");
    fs::write(output, runtime)
}
