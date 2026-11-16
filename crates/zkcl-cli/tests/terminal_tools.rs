mod common;

#[cfg(unix)]
#[test]
fn lisp_ed_passes_pathnames_and_running_definition_files_to_editor() {
    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Stdio;

    let scratch = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../.agents/tmp/zkcl/repl-completion/terminal");
    fs::create_dir_all(&scratch).unwrap();
    let temp = tempfile::tempdir_in(scratch).unwrap();
    let script = temp.path().join("editor");
    let log = temp.path().join("editor-paths");
    fs::write(
        &script,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"$ZKCL_EDITOR_LOG\"\n",
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let source = temp.path().join("source with spaces.lisp");
    fs::write(&source, "(defun editor-origin (x) (+ x 1))").unwrap();
    let literal = |path: &std::path::Path| serde_json::to_string(path.to_str().unwrap()).unwrap();
    let mut process = common::zkcl()
        .args(["repl", "--no-init"])
        .env("VISUAL", &script)
        .env("ZKCL_EDITOR_LOG", &log)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let input = format!(
        "(load {})\n(ed {})\n(ed 'editor-origin)\n",
        literal(&source),
        literal(&source)
    );
    process
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let output = process.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected = source.canonicalize().unwrap();
    let invoked_paths = fs::read_to_string(log).unwrap();
    let invoked_paths: Vec<_> = invoked_paths
        .lines()
        .map(|path| std::path::Path::new(path).canonicalize().unwrap())
        .collect();
    assert_eq!(invoked_paths, [expected.clone(), expected]);
}

#[test]
fn dribble_records_lisp_input_and_output_in_a_real_process() {
    use std::fs;
    use std::io::Write;
    use std::process::Stdio;

    let scratch = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../.agents/tmp/zkcl/repl-completion/terminal");
    fs::create_dir_all(&scratch).unwrap();
    let temp = tempfile::tempdir_in(scratch).unwrap();
    let transcript = temp.path().join("session.log");
    let path = serde_json::to_string(transcript.to_str().unwrap()).unwrap();
    let script = temp.path().join("record.lisp");
    fs::write(
        &script,
        format!("(dribble {path})\n(write-line (read-line))\n(dribble)\n"),
    )
    .unwrap();
    let mut process = common::zkcl()
        .arg("run")
        .arg(&script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let input = "dribble process output\n";
    process
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let output = process.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = fs::read_to_string(transcript).unwrap();
    assert_eq!(
        text.matches("dribble process output\n").count(),
        2,
        "{text}"
    );
}
