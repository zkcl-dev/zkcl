use std::fs;
use std::path::Path;
use std::process::Command;

fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &destination.join(entry.file_name()));
        } else {
            fs::copy(entry.path(), destination.join(entry.file_name())).unwrap();
        }
    }
}

#[test]
fn cli_initializes_the_same_mixed_project_as_embedded_hosts() {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let scratch = crate_dir.join("../../../.agents/tmp/zkcl/cli-project-embedding");
    fs::create_dir_all(&scratch).unwrap();
    let temporary = tempfile::tempdir_in(scratch).unwrap();
    let source = temporary.path().join("source");
    copy_tree(
        &crate_dir.join("../../tests/fixtures/embedding-project"),
        &source,
    );
    let output = Command::new(env!("CARGO_BIN_EXE_zkcl"))
        .args(["run", "--manifest-path"])
        .arg(source.join("app/zkcl.sexp"))
        .args(["--bin", "app"])
        .current_dir(temporary.path())
        .env("ZKCL_CACHE_DIR", temporary.path().join("cache"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("42|embedded resources|1"));
}
