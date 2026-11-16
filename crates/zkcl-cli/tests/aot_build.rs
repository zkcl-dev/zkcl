use std::fs;
use std::path::PathBuf;
use std::process::Command;

struct TempDir(PathBuf);

impl TempDir {
    fn new(prefix: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("{prefix}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn llvm_backend_emits_inspectable_ir_when_enabled() {
    let tmp = TempDir::new("zkcl-cli-build-llvm");
    let source = tmp.0.join("answer.lisp");
    fs::write(&source, "(defun answer () 42)").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_zkcl"))
        .arg("build")
        .arg(&source)
        .args(["--backend", "llvm", "--emit-dir"])
        .arg(&tmp.0)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    if zkcl_aot::AotBackendKind::Llvm.is_available() {
        assert!(output.status.success(), "{stderr}");
        let ir = fs::read_to_string(tmp.0.join("answer.ll")).unwrap();
        assert!(ir.contains("define void @zkcl_aot_answer_fn_"));
        assert!(tmp.0.join("answer.o").is_file());
    } else {
        assert!(!output.status.success());
        assert!(stderr.contains("`llvm` Cargo feature"), "{stderr}");
        assert!(!tmp.0.join("answer.o").exists());
    }
}

#[cfg(feature = "llvm-aot")]
const POPCOUNT_VOP: &str = r#"
        ;; Return a sentinel on the ordinary path so the test detects a VM handoff.
        (defun word-popcount (x) (declare (ignore x)) 99)
        (defun before-registration (x) (word-popcount x))
        (zk-sys:define-vop count-bits
          (:translate word-popcount)
          (:args ((x :type (unsigned-byte 64) :representation :u64)))
          (:results ((r :type (integer 0 64) :representation :u64)))
          (:effects :pure :no-gc :no-unwind)
          (:lower :portable (:popcnt x))
          (:asm :aarch64
            (:requires :asimd)
            (:in (x :gpr64)) (:out (r :gpr64)) (:temp (v :simd128))
            "fmov {v:d}, {x}"
            "cnt {v:v}.8b, {v:v}.8b"
            "addv {v:b}, {v:v}.8b"
            "fmov {r}, {v:d}")
          (:fallback word-popcount))
        (defun inline-bits (x) (word-popcount x))
        (defun ordinary-bits (x) (declare (notinline word-popcount)) (word-popcount x))
        (defun debug-bits (x) (declare (optimize (debug 3))) (word-popcount x))
    "#;

#[cfg(feature = "llvm-aot")]
#[test]
fn lisp_vop_selects_the_declared_assembly_and_respects_notinline() {
    let tmp = TempDir::new("zkcl-lisp-vop");
    let source = tmp.0.join("bitmap.lisp");
    fs::write(&source, POPCOUNT_VOP).unwrap();
    for (triple, expected) in [
        ("aarch64-apple-darwin", "addv ${1:b}, $1.8b"),
        ("x86_64-unknown-linux-gnu", "call i64 @llvm.ctpop.i64"),
    ] {
        let out = tmp.0.join(triple);
        let output = Command::new(env!("CARGO_BIN_EXE_zkcl"))
            .arg("build")
            .arg(&source)
            .args(["--backend", "llvm", "--target", triple, "--emit-dir"])
            .arg(&out)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let fasl =
            zkcl_bytecode::CompiledFile::decode(&fs::read(out.join("bitmap.zkclfasl")).unwrap())
                .unwrap();
        let ir = fs::read_to_string(out.join("bitmap.ll")).unwrap();
        for (name, optimized) in [
            ("BEFORE-REGISTRATION", false),
            ("INLINE-BITS", true),
            ("ORDINARY-BITS", false),
            ("DEBUG-BITS", false),
        ] {
            let (index, code) = fasl
                .code_entries()
                .enumerate()
                .find(|(_, entry)| entry.name.as_deref() == Some(name))
                .unwrap();
            assert_eq!(
                code.vop_calls.len(),
                usize::from(optimized || name == "DEBUG-BITS"),
                "{name}"
            );
            let symbol = format!("define void @zkcl_aot_bitmap_fn_{index}(");
            let (_, rest) = ir.split_once(&symbol).unwrap();
            let (body, _) = rest.split_once("\n}").unwrap();
            assert_eq!(body.contains(expected), optimized, "{triple} {name}");
            assert_eq!(
                body.contains("@zkcl_rt_prepare_vop_u64"),
                optimized,
                "{triple} {name}"
            );
        }
    }

    fs::write(
        &source,
        POPCOUNT_VOP.replace("addv {v:b}, {v:v}.8b", "ldr x0, [x0]"),
    )
    .unwrap();
    let invalid = Command::new(env!("CARGO_BIN_EXE_zkcl"))
        .arg("compile")
        .arg(&source)
        .arg(tmp.0.join("invalid.zkclfasl"))
        .output()
        .unwrap();
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("expected UADDLV"));
}

#[cfg(all(
    feature = "llvm-aot",
    any(target_arch = "aarch64", target_arch = "x86_64")
))]
#[test]
fn lisp_vop_runs_inside_a_linked_array_loop() {
    let tmp = TempDir::new("zkcl-lisp-vop-run");
    let source = tmp.0.join("bitmap_loop.lisp");
    let executable = tmp
        .0
        .join(format!("bitmap{}", std::env::consts::EXE_SUFFIX));
    let caller = r#"
        (defun bitmap-count (words)
          (declare (type (simple-array (unsigned-byte 64) (*)) words))
          (let ((total 0))
            (dotimes (i (length words) total)
              (incf total (word-popcount (aref words i))))))

        (defvar *argument-evaluations* 0)
        (defun evaluate-argument (value)
          (setq *argument-evaluations* (+ *argument-evaluations* 1))
          value)
        (defun popcount-after-argument (value)
          (word-popcount (evaluate-argument value)))

        (defvar *type-expansions* 0)
        (deftype custom-word ()
          (incf *type-expansions*)
          '(unsigned-byte 64))
        (defun matches-custom-array (value)
          (if (typep value '(simple-array custom-word (*))) :yes :no))

        (defun first-element (array)
          (list (aref array 0)))

        (let ((words (make-array 4 :element-type '(unsigned-byte 64)
                                 :initial-contents '(0 1 #x8000000000000000 #xffffffffffffffff)))
              (baseline (symbol-function 'word-popcount)))
          (assert (= 66 (bitmap-count words)))
          (assert (= 64 (popcount-after-argument #xffffffffffffffff)))
          (assert (= 99 (popcount-after-argument -1)))
          (assert (= 99 (popcount-after-argument #x10000000000000000)))
          (assert (= 3 *argument-evaluations*))
          (assert (= 99 (before-registration 3) (ordinary-bits 3) (debug-bits 3)))
          (assert (eq :yes (matches-custom-array words)))
          (assert (= 1 *type-expansions*))
          (assert (eq :caught (handler-case (first-element :not-an-array)
                                (type-error () :caught))))
          (unwind-protect
              (progn
                (setf (symbol-function 'word-popcount) (lambda (x) (declare (ignore x)) 7))
                (assert (= 28 (bitmap-count words))))
            (setf (symbol-function 'word-popcount) baseline))
          (assert (= 66 (bitmap-count words))))
    "#;
    fs::write(&source, format!("{POPCOUNT_VOP}{caller}")).unwrap();
    let build = Command::new(env!("CARGO_BIN_EXE_zkcl"))
        .arg("build")
        .arg(&source)
        .args(["--backend", "llvm", "--release", "--emit-dir"])
        .arg(&tmp.0)
        .arg("-o")
        .arg(&executable)
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let run = Command::new(executable).output().unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
}

#[test]
fn build_help_lists_aot_options() {
    let bin = env!("CARGO_BIN_EXE_zkcl");
    let help = Command::new(bin)
        .args(["build", "--help"])
        .output()
        .expect("run zkcl build --help");
    assert!(help.status.success(), "zkcl build --help must succeed");
    let help_stdout = String::from_utf8_lossy(&help.stdout);
    assert!(help_stdout.contains("--backend"));
    assert!(help_stdout.contains("--emit-dir"));
    let available = help_stdout
        .lines()
        .find_map(|line| line.strip_prefix("Available AOT backends: "))
        .unwrap();
    let names: Vec<_> = available.split(", ").collect();
    for (name, kind) in [
        ("cranelift", zkcl_aot::AotBackendKind::CraneliftObject),
        ("llvm", zkcl_aot::AotBackendKind::Llvm),
        ("direct", zkcl_aot::AotBackendKind::Direct),
    ] {
        assert_eq!(names.contains(&name), kind.is_available(), "{help_stdout}");
    }
}

#[test]
fn source_compilation_uses_target_reader_features_in_fasl_and_aot_builds() {
    let tmp = TempDir::new("zkcl-cli-cross-source");
    let source = tmp.0.join("platform.lisp");
    fs::write(
        &source,
        "(defmacro platform-literal ()
           (list 'quote '(#+macos :macos #+ios :ios #+simulator :simulator
                          #+android :android #+linux :linux #+windows :windows
                          #+unix :unix #+msvc :msvc #+gnu :gnu
                          #+arm64 :arm64 #+x86-64 :x86-64)))
         (defun compiled-platform () (platform-literal))",
    )
    .unwrap();
    for (triple, expected) in [
        ("aarch64-apple-darwin", "(:macos :unix :arm64)"),
        ("aarch64-apple-ios", "(:ios :unix :arm64)"),
        ("aarch64-apple-ios-sim", "(:ios :simulator :unix :arm64)"),
        ("aarch64-linux-android", "(:android :linux :unix :arm64)"),
        ("x86_64-unknown-linux-gnu", "(:linux :unix :gnu :x86-64)"),
        ("x86_64-pc-windows-msvc", "(:windows :msvc :x86-64)"),
    ] {
        let dir = tmp.0.join(triple);
        fs::create_dir(&dir).unwrap();
        let fasl = dir.join("compile.zkclfasl");
        let cli = env!("CARGO_BIN_EXE_zkcl");
        let compile = Command::new(cli)
            .arg("compile")
            .arg(&source)
            .args(["--target", triple])
            .arg(&fasl)
            .output()
            .unwrap();
        let build = Command::new(cli)
            .arg("build")
            .arg(&source)
            .args(["--backend", "direct", "--target", triple, "--emit-dir"])
            .arg(&dir)
            .output()
            .unwrap();
        for output in [compile, build] {
            assert!(
                output.status.success(),
                "{triple}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        assert!(dir.join("platform.o").is_file());
        let target: zkcl::CompilationTarget = triple.parse().unwrap();
        for path in [fasl, dir.join("platform.zkclfasl")] {
            let runtime = zkcl::Runtime::builder()
                .compilation_target(target.clone())
                .start()
                .unwrap();
            runtime.load_file(&path).unwrap();
            runtime
                .eval(&format!("(assert (equal (compiled-platform) '{expected}))"))
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        }
    }
}

#[test]
fn lists_native_and_bytecode_entries_without_changing_the_output_path() {
    let tmp = TempDir::new("zkcl-aot-entry-list");
    let source = tmp.0.join("mixed.lisp");
    let output_dir = tmp.0.join("out");
    fs::write(
        &source,
        "(defun direct-id (x) x) (defun keyword-value (&key x) (list x))",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_zkcl"))
        .arg("build")
        .arg(&source)
        .args(["--backend", "direct", "--list-entries", "--emit-dir"])
        .arg(&output_dir)
        .output()
        .unwrap();
    let listing = String::from_utf8(output.stderr).unwrap();
    assert!(output.status.success(), "{listing}");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        output_dir.to_str().unwrap()
    );
    assert!(
        listing.contains("  native  zkcl_aot_mixed_fn_"),
        "{listing}"
    );
    assert!(
        listing.lines().any(|line| line.ends_with("  bytecode")),
        "{listing}"
    );
    assert!(
        listing.contains("leaf functions require fixed arity"),
        "{listing}"
    );
}

#[test]
fn aot_projects_link_for_requested_output_kind() {
    let bin = env!("CARGO_BIN_EXE_zkcl");
    let tmp = TempDir::new("zkcl-cli-build-test");
    let lisp_path = tmp.0.join("sample_app.lisp");
    fs::write(
        &lisp_path,
        "(defmacro aot-inc (x) `(+ ,x 1))\n\
         (defun app-add (a &optional (b 22))\n\
           (let ((sum 0)\n\
                 (cleaned nil))\n\
             (dotimes (i 3)\n\
               (setq sum (+ sum i)))\n\
             (let ((mv (multiple-value-call (lambda (x y z) (+ x y z))\n\
                         (unwind-protect (values a b (aot-inc sum))\n\
                           (setq cleaned t)))))\n\
               (unless (and cleaned (= (handler-case mv (error () -1)) 46))\n\
                 (error \"bad aot result\"))\n\
               mv)))\n\
         (unless (= (app-add 20) 46) (error \"bad\"))\n",
    )
    .expect("write sample_app.lisp");

    let emit_dir = tmp.0.join("build {app} 雪");
    let emit = Command::new(bin)
        .arg("build")
        .arg(&lisp_path)
        .arg("--emit-dir")
        .arg(&emit_dir)
        .output()
        .expect("run zkcl build --emit-dir");
    assert!(
        emit.status.success(),
        "zkcl build --emit-dir failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&emit.stdout),
        String::from_utf8_lossy(&emit.stderr),
    );
    assert!(emit_dir.join("sample_app.o").exists());
    assert!(emit_dir.join("sample_app.zkclfasl").exists());
    assert!(emit_dir.join("src/aot_module.rs").exists());
    assert!(emit_dir.join("src/main.rs").exists());
    assert!(emit_dir.join("build.rs").exists());
    assert!(emit_dir.join("Cargo.toml").exists());

    let fasl = emit_dir.join("sample_app.zkclfasl");
    let from_fasl = tmp.0.join("from_fasl");
    let build_from_fasl = Command::new(bin)
        .arg("build")
        .arg(&fasl)
        .arg("--emit-dir")
        .arg(&from_fasl)
        .output()
        .expect("build from FASL");
    assert!(
        build_from_fasl.status.success(),
        "{}",
        String::from_utf8_lossy(&build_from_fasl.stderr)
    );
    assert!(from_fasl.join("sample_app.o").exists());
    assert_eq!(
        fs::read(&fasl).unwrap(),
        fs::read(from_fasl.join("sample_app.zkclfasl")).unwrap()
    );

    // Re-emitting identical bytes must not invalidate Cargo's generated sources.
    let build_script = emit_dir.join("build.rs");
    let old_mtime = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
    fs::File::options()
        .write(true)
        .open(&build_script)
        .unwrap()
        .set_modified(old_mtime)
        .unwrap();

    let out_bin = tmp.0.join(if cfg!(windows) {
        "sample_app.exe"
    } else {
        "sample_app_bin"
    });
    let build = Command::new(bin)
        .arg("build")
        .arg(&lisp_path)
        .arg("--emit-dir")
        .arg(&emit_dir)
        .arg("-o")
        .arg(&out_bin)
        .output()
        .expect("run zkcl build -o");
    assert!(
        build.status.success(),
        "zkcl build -o failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&build.stdout),
        String::from_utf8_lossy(&build.stderr),
    );
    assert!(out_bin.exists(), "output executable must exist");
    assert_eq!(
        fs::metadata(&build_script).unwrap().modified().unwrap(),
        old_mtime
    );

    let run = Command::new(&out_bin)
        .output()
        .expect("execute built standalone binary");
    assert!(
        run.status.success(),
        "standalone binary failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr),
    );

    // Emit an iOS object and a static library project
    let ios_static_dir = tmp.0.join("ios_static");
    let ios_emit = Command::new(bin)
        .arg("build")
        .arg(&lisp_path)
        .args(["--target", "aarch64-apple-ios", "--staticlib", "--emit-dir"])
        .arg(&ios_static_dir)
        .output()
        .expect("run zkcl build --target --staticlib --emit-dir");
    assert!(
        ios_emit.status.success(),
        "zkcl build --target aarch64-apple-ios --staticlib --emit-dir failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&ios_emit.stdout),
        String::from_utf8_lossy(&ios_emit.stderr),
    );
    assert!(ios_static_dir.join("sample_app.o").exists());
    assert!(ios_static_dir.join("sample_app.zkclfasl").exists());
    assert!(ios_static_dir.join("src/aot_module.rs").exists());
    assert!(ios_static_dir.join("src/lib.rs").exists());
    assert!(!ios_static_dir.join("src/main.rs").exists());
    let cargo_toml = fs::read_to_string(ios_static_dir.join("Cargo.toml")).unwrap();
    assert!(
        cargo_toml.contains("crate-type = [\"staticlib\"]"),
        "{cargo_toml}"
    );

    // Host staticlib build
    let host_static_lib = tmp
        .0
        .join(if cfg!(all(target_os = "windows", target_env = "msvc")) {
            "sample_app.lib"
        } else {
            "libsample_app.a"
        });
    let static_build = Command::new(bin)
        .arg("build")
        .arg(&lisp_path)
        .arg("--staticlib")
        .arg("-o")
        .arg(&host_static_lib)
        .output()
        .expect("run zkcl build --staticlib -o");
    assert!(
        static_build.status.success(),
        "zkcl build --staticlib -o failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&static_build.stdout),
        String::from_utf8_lossy(&static_build.stderr),
    );
    assert!(host_static_lib.exists(), "output static library must exist");
}

#[test]
fn concurrent_builds_sharing_an_emit_directory_link_their_own_programs() {
    let tmp = TempDir::new("zkcl-cli-shared-emit");
    let emit_dir = tmp.0.join("emit");
    let cli = env!("CARGO_BIN_EXE_zkcl");
    let source = tmp.0.join("same.lisp");
    let fasl = tmp.0.join("same.zkclfasl");
    let program = |value| {
        format!(
            "(with-open-file (s \"result\" :direction :output :if-exists :supersede)
           (princ {value} s))"
        )
    };
    fs::write(&source, program(42)).unwrap();
    assert!(
        Command::new(cli)
            .arg("compile")
            .arg(&source)
            .arg(&fasl)
            .status()
            .unwrap()
            .success()
    );
    fs::write(&source, program(21)).unwrap();

    let mut builds = Vec::new();
    for (input, value) in [(source, 21), (fasl, 42)] {
        let exe = tmp
            .0
            .join(format!("app-{value}{}", std::env::consts::EXE_SUFFIX));
        let child = Command::new(cli)
            .arg("build")
            .arg(input)
            .arg("--emit-dir")
            .arg(&emit_dir)
            .arg("-o")
            .arg(&exe)
            .spawn()
            .unwrap();
        builds.push((value, exe, child));
    }
    for (value, exe, mut child) in builds {
        assert!(child.wait().unwrap().success());
        assert!(
            Command::new(exe)
                .current_dir(&tmp.0)
                .status()
                .unwrap()
                .success()
        );
        assert_eq!(
            fs::read_to_string(tmp.0.join("result")).unwrap(),
            value.to_string()
        );
    }
}
