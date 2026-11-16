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

#[cfg(all(
    feature = "llvm-aot",
    any(target_arch = "aarch64", target_arch = "x86_64")
))]
#[test]
fn llvm_aot_preserves_float_precision_and_resumes_at_the_failing_operation() {
    let tmp = TempDir::new("zkcl-llvm-float");
    let source = tmp.0.join("floats.lisp");
    let executable = tmp
        .0
        .join(format!("floats{}", std::env::consts::EXE_SUFFIX));
    fs::write(
        &source,
        r#"
        (defun single-step (x y z)
          (declare (type single-float x y z) (optimize (speed 3) (safety 1) (debug 0)))
          (+ (* x y) z))

        (defun double-step (x y z)
          (declare (type double-float x y z) (optimize (speed 3) (safety 1) (debug 0)))
          (- (+ x y) z))

        (defun single-quotient (x y z)
          (declare (type single-float x y z) (optimize (speed 3) (safety 1) (debug 0)))
          (/ (* x y) z))

        (defun collect-and-add (a b marker)
          (declare (type single-float a b) (optimize (speed 3) (safety 1) (debug 0)))
          (zk:gc)
          (values marker (+ a b)))

        (defun double-at (array index)
          (declare (type (simple-array double-float (*)) array)
                   (optimize (speed 3) (safety 1) (debug 0)))
          (+ (aref array index) 1d0))

        (defun double-write (array index value)
          (declare (type (simple-array double-float (*)) array)
                   (type double-float value)
                   (optimize (speed 3) (safety 1) (debug 0)))
          (setf (aref array index) value))

        (defun double-recur (n x scale offset)
          (declare (type (integer 0 1000000) n) (type double-float x scale offset)
                   (optimize (speed 3) (safety 1) (debug 0)))
          (dotimes (i n x)
            (setf x (+ (* scale x) offset))))

        (assert (eql 0f0 (single-step 1.0001220703125f0 0.9998779296875f0 -1f0)))
        (assert (eql -1f0 (float-sign (single-step -0f0 1f0 -0f0))))
        (assert (eql 0d0 (double-step 9007199254740992d0 1d0 9007199254740992d0)))
        (assert (eql 251d0 (double-recur 1000 1d0 1d0 0.25d0)))
        (assert (handler-case (single-quotient 2f0 3f0 -0f0)
                  (division-by-zero (c)
                    (let ((operands (arithmetic-error-operands c)))
                      (and (eq '/ (arithmetic-error-operation c))
                           (eql 6f0 (first operands))
                           (eql -1f0 (float-sign (second operands))))))))
        (assert (handler-case (single-quotient most-positive-single-float 2f0 -0f0)
                  (floating-point-overflow (c) (eq '* (arithmetic-error-operation c)))))
        (let ((marker (list :live)))
          (multiple-value-bind (kept result) (collect-and-add 1f0 2f0 marker)
            (assert (and (eq kept marker) (eql 3f0 result)))))
        (let ((*trace-output* (make-string-output-stream)))
          (trace cl:*)
          (assert (eql 7f0 (single-step 2f0 3f0 1f0)))
          (untrace cl:*)
          (assert (search "TRACE *: enter" (get-output-stream-string *trace-output*))))
        (let ((array (make-array 2 :element-type 'double-float :initial-contents '(1d0 2.5d0)))
              (*trace-output* (make-string-output-stream)))
          (assert (eql 3.5d0 (double-at array 1)))
          (assert (handler-case (progn (double-at array 2) nil) (error () t)))
          (trace cl:aref)
          (assert (eql 2d0 (double-at array 0)))
          (untrace cl:aref)
          (assert (search "TRACE AREF: enter" (get-output-stream-string *trace-output*)))
          (assert (eql 4.5d0 (double-write array 1 4.5d0)))
          (assert (handler-case (progn (double-write array 2 7.5d0) nil) (error () t)))
          (assert (eql 4.5d0 (aref array 1)))
          (trace cl:funcall)
          (assert (eql 5.5d0 (double-write array 1 5.5d0)))
          (untrace cl:funcall)
          (assert (eql 5.5d0 (aref array 1)))
          (assert (search "TRACE FUNCALL: enter" (get-output-stream-string *trace-output*))))
        "#,
    )
    .unwrap();
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

    let fasl =
        zkcl_bytecode::CompiledFile::decode(&fs::read(tmp.0.join("floats.zkclfasl")).unwrap())
            .unwrap();
    let ir = fs::read_to_string(tmp.0.join("floats.ll")).unwrap();
    let function_body = |name: &str| {
        let (index, _) = fasl
            .code_entries()
            .enumerate()
            .find(|(_, entry)| entry.name.as_deref() == Some(name))
            .unwrap();
        let (_, definition) = ir
            .split_once(&format!("define void @zkcl_aot_floats_fn_{index}("))
            .unwrap();
        definition.split_once("\n}").unwrap().0
    };
    let single = function_body("SINGLE-STEP");
    assert!(
        single.contains("fmul float") && single.contains("fadd float"),
        "{single}"
    );
    let double = function_body("DOUBLE-STEP");
    assert!(
        double.contains("fadd double") && double.contains("fsub double"),
        "{double}"
    );
    let quotient = function_body("SINGLE-QUOTIENT");
    assert!(quotient.contains("fdiv float"), "{quotient}");
    assert!(
        quotient.contains("call void @zkcl_rt_deopt_float_to_vm("),
        "{quotient}"
    );
    let double_at = function_body("DOUBLE-AT");
    assert!(
        double_at.contains("call i64 @zkcl_rt_read_float_vector("),
        "{double_at}"
    );
    assert!(double_at.contains("fadd double"), "{double_at}");
    let double_write = function_body("DOUBLE-WRITE");
    assert!(
        double_write.contains("call zeroext i1 @zkcl_rt_write_float_vector("),
        "{double_write}"
    );
    let recur = function_body("DOUBLE-RECUR");
    assert!(recur.contains("phi double"), "{recur}");
    assert!(
        recur.contains("call void @zkcl_rt_poll_execution("),
        "{recur}"
    );

    let run = Command::new(executable).output().unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
}

#[cfg(feature = "llvm-aot")]
const POPCOUNT_VOP: &str = r#"
        ;; Return a sentinel on the ordinary path so the test detects a VM handoff.
        (defun word-popcount (x) (declare (ignore x)) 99)
        (defun second-word-popcount (x) (declare (ignore x)) 88)
        (defun before-registration (x) (word-popcount x))
        (defun word-leading-zeroes (x) (declare (ignore x)) 97)
        (defun word-trailing-zeroes (x) (declare (ignore x)) 96)
        (zk-sys:defknown word-popcount (integer) (integer 0 64))
        (zk-sys:defknown second-word-popcount (integer) (integer 0 64))
        (zk-sys:defknown word-leading-zeroes (integer) (integer 0 64))
        (zk-sys:defknown word-trailing-zeroes (integer) (integer 0 64))

        ;; Both templates implement the same operation. The cheaper applicable
        ;; template must win, regardless of the ordinary function being called.
        (zk-sys:define-vop wide-word-popcount
          :popcnt
          (:cost 32)
          (:asm :aarch64 :uaddlv))
        (zk-sys:define-vop compact-word-popcount
          :popcnt
          (:asm :aarch64 :addv))
        (zk-sys:deftransform word-popcount (x) (:popcnt x))
        (zk-sys:deftransform second-word-popcount (x) (:popcnt x))
        (zk-sys:deftransform word-leading-zeroes (x) (:clz x))
        (zk-sys:deftransform word-trailing-zeroes (x) (:ctz x))

        (defun inline-bits (x) (word-popcount x))
        (defun second-inline-bits (x) (second-word-popcount x))
        (defun ordinary-bits (x) (declare (notinline word-popcount)) (word-popcount x))
        (defun lexical-notinline-bits (x)
          (flet ((word-popcount (value) (+ value 5)))
            (declare (notinline word-popcount))
            (word-popcount x)))
        (defun debug-bits (x) (declare (optimize (debug 3))) (word-popcount x))
        (defun leading-zeroes (x) (word-leading-zeroes x))
        (defun trailing-zeroes (x) (word-trailing-zeroes x))
    "#;

#[cfg(feature = "llvm-aot")]
#[test]
fn lisp_transforms_share_the_cheapest_applicable_target_template() {
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
        for (name, lowering) in [
            ("BEFORE-REGISTRATION", None),
            ("INLINE-BITS", Some(expected)),
            ("SECOND-INLINE-BITS", Some(expected)),
            ("ORDINARY-BITS", None),
            ("LEXICAL-NOTINLINE-BITS", None),
            ("DEBUG-BITS", None),
            ("LEADING-ZEROES", Some("call i64 @llvm.ctlz.i64")),
            ("TRAILING-ZEROES", Some("call i64 @llvm.cttz.i64")),
        ] {
            let (index, code) = fasl
                .code_entries()
                .enumerate()
                .find(|(_, entry)| entry.name.as_deref() == Some(name))
                .unwrap();
            assert_eq!(
                code.vop_calls.len(),
                usize::from(lowering.is_some() || name == "DEBUG-BITS"),
                "{name}"
            );
            let symbol = format!("define void @zkcl_aot_bitmap_fn_{index}(");
            let (_, rest) = ir.split_once(&symbol).unwrap();
            let (body, _) = rest.split_once("\n}").unwrap();
            if let Some(lowering) = lowering {
                assert!(body.contains(lowering), "{triple} {name}");
                assert!(
                    body.contains("@zkcl_rt_check_transform_binding"),
                    "{triple} {name}"
                );
                assert!(body.contains("@zkcl_rt_unbox_u64"), "{triple} {name}");
                if triple == "aarch64-apple-darwin" && expected.contains("addv") {
                    assert!(body.contains("addv ${1:b}, $1.8b"), "{triple} {name}");
                    assert!(!body.contains("uaddlv"), "{triple} {name}");
                }
            }
        }
    }

    fs::write(
        &source,
        POPCOUNT_VOP.replace("(:asm :aarch64 :addv)", "(:asm :aarch64 :ldr)"),
    )
    .unwrap();
    let invalid = Command::new(env!("CARGO_BIN_EXE_zkcl"))
        .arg("compile")
        .arg(&source)
        .arg(tmp.0.join("invalid.zkclfasl"))
        .output()
        .unwrap();
    assert!(!invalid.status.success());
    let stderr = String::from_utf8_lossy(&invalid.stderr);
    assert!(stderr.contains(":ADDV"), "{stderr}");
    assert!(stderr.contains(":UADDLV"), "{stderr}");
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

        (defun packed-bitmap-count (words)
          (declare (type (simple-array (unsigned-byte 64) (*)) words))
          (let ((total 0))
            (dotimes (i (length words) total)
              (incf total (second-word-popcount (aref words i))))))

        (defvar *first-argument-evaluations* 0)
        (defun evaluate-first-argument (value)
          (setq *first-argument-evaluations* (+ *first-argument-evaluations* 1))
          value)
        (defun popcount-after-argument (value)
          (word-popcount (evaluate-first-argument value)))

        (defvar *second-argument-evaluations* 0)
        (defun evaluate-second-argument (value)
          (setq *second-argument-evaluations* (+ *second-argument-evaluations* 1))
          value)
        (defun second-popcount-after-argument (value)
          (second-word-popcount (evaluate-second-argument value)))

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
              (first-baseline (symbol-function 'word-popcount))
              (second-baseline (symbol-function 'second-word-popcount)))
          (assert (= 66 (bitmap-count words)))
          (assert (= 66 (packed-bitmap-count words)))
          (assert (= 64 (popcount-after-argument #xffffffffffffffff)))
          (assert (= 64 (second-popcount-after-argument #xffffffffffffffff)))
          (assert (= 99 (popcount-after-argument -1)))
          (assert (= 99 (popcount-after-argument #x10000000000000000)))
          (assert (= 88 (second-popcount-after-argument -1)))
          (assert (= 3 *first-argument-evaluations*))
          (assert (= 2 *second-argument-evaluations*))
          (assert (= 99 (before-registration 3) (ordinary-bits 3) (debug-bits 3)))
          (assert (= 8 (lexical-notinline-bits 3)))
          (assert (= 64 (leading-zeroes 0)))
          (assert (= 0 (leading-zeroes #x8000000000000000)))
          (assert (= 97 (leading-zeroes -1)))
          (assert (= 64 (trailing-zeroes 0)))
          (assert (= 63 (trailing-zeroes #x8000000000000000)))
          (assert (= 96 (trailing-zeroes -1)))
          (assert (eq :yes (matches-custom-array words)))
          (assert (= 1 *type-expansions*))
          (assert (eq :caught (handler-case (first-element :not-an-array)
                                (type-error () :caught))))
          (unwind-protect
              (progn
                (setf (symbol-function 'word-popcount) (lambda (x) (declare (ignore x)) 7))
                (setf (symbol-function 'second-word-popcount)
                      (lambda (x) (declare (ignore x)) 11))
                (assert (= 28 (bitmap-count words)))
                (assert (= 44 (packed-bitmap-count words))))
            (setf (symbol-function 'word-popcount) first-baseline)
            (setf (symbol-function 'second-word-popcount) second-baseline))
          (assert (= 66 (bitmap-count words)))
          (assert (= 66 (packed-bitmap-count words))))
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
