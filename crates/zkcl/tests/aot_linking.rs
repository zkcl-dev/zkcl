use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use zkcl::Runtime;
use zkcl_aot::{AotBackendKind, AotError, AotOptLevel, AotTargetConfig, compile_fasl_to_object};
use zkcl_bytecode::{Callee, CompiledFile, FixnumArithmetic, Immediate, Instruction, LiteralNode};

mod common;

use common::files::TempDir;

fn compile_lisp_to_fasl(runtime: &Runtime, dir: &Path, module_name: &str, source: &str) -> Vec<u8> {
    let lisp_path = dir.join(format!("{module_name}.lisp"));
    let fasl_path = dir.join(format!("{module_name}.zkclfasl"));
    fs::write(&lisp_path, source).expect("write lisp source");
    runtime
        .compile_file(&lisp_path, Some(&fasl_path))
        .expect("compile_file succeeds");
    fs::read(&fasl_path).expect("read compiled fasl")
}

fn read_optional_after_collection(file: &mut CompiledFile, name: &str) {
    let code = file
        .nodes
        .iter_mut()
        .find_map(|node| match node {
            LiteralNode::Function(code) if code.name.as_deref() == Some(name) => Some(code),
            _ => None,
        })
        .expect("optional argument fixture");
    let collect = code
        .instructions
        .iter()
        .find_map(|instruction| match instruction {
            Instruction::Call {
                callee: Callee::Named(index),
                ..
            } => Some(*index),
            _ => None,
        })
        .expect("named collection hook");
    // External bytecode can read optional arguments anywhere in the function.
    // Check both a read across collection and a read after the tail is released.
    code.registers = code.registers.max(2);
    code.instructions = vec![
        Instruction::Call {
            callee: Callee::Named(collect),
            args: 0..0,
            out: Some(0),
            at: 0,
            associate: false,
            notinline: true,
        },
        Instruction::OptionalArgument(0, 0),
        Instruction::IfPresent(0, 4),
        Instruction::Load(0, Immediate::Nil),
        Instruction::ReleaseArguments,
        Instruction::OptionalArgument(0, 1),
        Instruction::IfPresent(1, 9),
        Instruction::Load(1, Immediate::Nil),
        Instruction::Jump(10),
        Instruction::Load(1, Immediate::True),
        Instruction::SetValues(0..2),
    ];
}

#[test]
fn linked_aot_modules_run_in_a_rust_host() {
    let temp_dir = TempDir::new("aot-linking");
    let runtime = Runtime::start().expect("runtime starts");

    let base_module_source = r#"
(defun aot-native-debug3 (x)
  (declare (optimize (debug 3)))
  (values x (1+ x)))
(defun aot-double (x) (+ x x))
(defun aot-sum-to (n)
  (if (<= n 0)
      0
      (+ n (aot-sum-to (- n 1)))))
(defun aot-catch (x)
  (catch 'k
    (if (= x 0)
        7
        (throw 'k 41))))
(defun aot-handler (x)
  (handler-case (if (= x 0) 7 (error "boom"))
    (error (c) (if c (+ 40 x) 0))))
(defun aot-cleanup-single (x)
  (unwind-protect (+ x 1) (+ x 2)))
(defun aot-checked-range (x)
  (declare (type (unsigned-byte 8) x))
  x)
"#;
    let dependent_module_source = r#"
(defun llvm-native-debug3 (x)
  (declare (optimize (debug 3)))
  (values x (1+ x)))
(defun aot-quad (x) (aot-double (aot-double x)))
(defun aot-host-call (x) (rust-host-add x))
(defun aot-add-big (a b) (+ a b))
(defun aot-save-values (x)
  (multiple-value-prog1 (values x (+ x 1)) (values 90 91)))
(defun aot-append-values (x)
  (multiple-value-call #'list (values x (+ x 1)) (values 99)))
(defun llvm-cleanup-single (x)
  (unwind-protect (+ x 1) (+ x 2)))
(defun llvm-checked-range (x)
  (declare (type (signed-byte 8) x))
  x)
(defun aot-joined-values (flag &optional (x 40))
  (multiple-value-bind (a b) (if flag (values x (+ x 2)) (values))
    (values a b)))
"#;

    let unwind_registers = r#"
(defun unwind-registers (n)
  (block result
    (let ((cancel (or (eq n :cancel-go) (eq n :cancel-branch))) pair)
      (tagbody
        (if n
            (progn (let ((value (if cancel 41 n)))
                     (setq pair (list value (1+ value))))
                   (unwind-protect
                       (if cancel
                           (progn (request-cancel)
                                  (if (eq n :cancel-go)
                                      (let ((count 0))
                                        (tagbody again
                                           (incf count)
                                           (when (< count 50) (go again))))
                                      (let ((total 0))
                                        (dotimes (index 100000 total)
                                          (incf total index)))))
                           (go selected))
                     (collect-in-cleanup)
                     (when cancel (record-cancel-cleanup pair))))
            (go other))
        other (setq pair (list 3 4))
        selected (return-from result (values (car pair) (cadr pair)))))))
"#;
    let float_step = r#"
(defun float-step (a b c)
  (values (+ (* a b) c) (/ a b)))
"#;
    let fixnum_steps = r#"
(defun fixnum-steps (a b)
  (declare (type (signed-byte 8) a b))
  (values (+ a b) (- a b) (* a b) (1+ a) (1- b)))
"#;
    let argument_binding = r#"
(defun backend-optional (&optional value)
  (collect-in-cleanup)
  value)
(defun backend-keywords (first &optional (step 2) &rest keys &key (extra (collect-in-cleanup)) value)
  (values first step value keys extra))
"#;
    let async_call = r#"
(defun backend-async-inner (thunk payload)
  (unwind-protect
       (multiple-value-bind (head tail) (funcall thunk payload)
         (values head tail payload))
    (collect-in-cleanup)))
(defun backend-async-outer (thunk payload)
  (multiple-value-prog1 (backend-async-inner thunk payload)
    (collect-in-cleanup)))
(defun backend-async-count (limit)
  (let ((total 0))
    (dotimes (index limit total) (incf total index))))
"#;
    let base_module_source = base_module_source.to_owned()
        + &unwind_registers.replace("unwind-registers", "aot-registers")
        + &float_step.replace("float-step", "aot-float-step")
        + &fixnum_steps.replace("fixnum-steps", "aot-fixnum-steps")
        + &argument_binding.replace("backend-", "aot-")
        + &async_call.replace("backend-", "aot-");
    let dependent_module_source = dependent_module_source.to_owned()
        + &unwind_registers.replace("unwind-registers", "llvm-registers")
        + &float_step.replace("float-step", "llvm-float-step")
        + &fixnum_steps.replace("fixnum-steps", "llvm-fixnum-steps")
        + &argument_binding.replace("backend-", "llvm-")
        + &async_call.replace("backend-", "llvm-");

    let base_fasl = compile_lisp_to_fasl(
        &runtime,
        temp_dir.path(),
        "base_module",
        &base_module_source,
    );
    let dependent_fasl = compile_lisp_to_fasl(
        &runtime,
        temp_dir.path(),
        "dependent_module",
        &dependent_module_source,
    );

    let mut base_compiled_file = CompiledFile::decode(&base_fasl).expect("decode base_module fasl");
    let mut dependent_compiled_file =
        CompiledFile::decode(&dependent_fasl).expect("decode dependent_module fasl");
    read_optional_after_collection(&mut base_compiled_file, "AOT-OPTIONAL");
    read_optional_after_collection(&mut dependent_compiled_file, "LLVM-OPTIONAL");
    for file in [&base_compiled_file, &dependent_compiled_file] {
        assert!(
            file.nested_function_nodes().any(|(_, code)| {
                code.instructions
                    .iter()
                    .filter_map(|inst| match inst {
                        Instruction::TryFixnumArithmetic { op, .. } => Some(*op),
                        _ => None,
                    })
                    .eq([
                        FixnumArithmetic::Add,
                        FixnumArithmetic::Sub,
                        FixnumArithmetic::Mul,
                        FixnumArithmetic::Inc,
                        FixnumArithmetic::Dec,
                    ])
            }),
            "the source fixture must exercise all five checked operations"
        );
    }

    let mut general_backends = [AotBackendKind::CraneliftObject, AotBackendKind::Llvm]
        .into_iter()
        .filter(|backend| backend.is_available());
    let base_backend = general_backends.next().unwrap_or(AotBackendKind::Direct);
    let dependent_backend = general_backends.next().unwrap_or(base_backend);
    let base_aot_object = match compile_fasl_to_object(
        &base_compiled_file,
        &AotTargetConfig::host("base-module")
            .with_backend(base_backend)
            .with_opt_level(AotOptLevel::Speed),
    ) {
        Ok(object) => object,
        Err(AotError::UnsupportedTarget { .. }) if base_backend == AotBackendKind::Direct => {
            eprintln!("No enabled AOT backend supports this host");
            return;
        }
        Err(error) => panic!("compile base module with {base_backend:?}: {error}"),
    };
    let dependent_aot_object = compile_fasl_to_object(
        &dependent_compiled_file,
        &AotTargetConfig::host("dependent-module")
            .with_backend(dependent_backend)
            .with_opt_level(AotOptLevel::Speed),
    )
    .expect("compile dependent_module with AOT");
    if base_backend != AotBackendKind::Direct {
        assert_eq!(
            base_aot_object.registration_entries.len(),
            29,
            "{base_backend:?}"
        );
        assert_eq!(
            dependent_aot_object.registration_entries.len(),
            33,
            "{dependent_backend:?}"
        );
    }

    let leaf_fasl = compile_lisp_to_fasl(
        &runtime,
        temp_dir.path(),
        "leaf_module",
        "(defun aot-fourth-word (a b c d) d)
         (defun aot-range-inc (x) (declare (type (unsigned-byte 8) x)) (1+ x))
         (defun aot-duplicate (x) (values x x))",
    );
    let leaf_fasl = CompiledFile::decode(&leaf_fasl).unwrap();
    let leaf_config = AotTargetConfig::host("leaf-module").with_backend(AotBackendKind::Direct);
    let (leaf_object, direct) = match compile_fasl_to_object(&leaf_fasl, &leaf_config) {
        Ok(object) => (object, true),
        Err(AotError::UnsupportedTarget { .. }) => (
            compile_fasl_to_object(&leaf_fasl, &leaf_config.with_backend(dependent_backend))
                .unwrap(),
            false,
        ),
        Err(error) => panic!("direct leaf module: {error}"),
    };
    if direct {
        assert!(
            leaf_object
                .registration_entries
                .iter()
                .any(|e| e.arity.required == 4 && e.leaf_symbol.is_some())
        );
        assert!(
            leaf_object
                .registration_entries
                .iter()
                .any(|e| !e.leaf_guards.is_empty())
        );
    }
    let host_project = temp_dir.path().join("linked-host");
    let host_source_directory = host_project.join("src");
    fs::create_dir_all(&host_source_directory).expect("create host project source directory");

    let leaf_object_path = host_project.join("leaf_module.o");
    fs::write(&leaf_object_path, &leaf_object.object_bytes).unwrap();
    fs::write(
        host_source_directory.join("leaf_module.rs"),
        &leaf_object.registration_source,
    )
    .unwrap();

    let base_object_path = host_project.join("base_module.o");
    let dependent_object_path = host_project.join("dependent_module.o");
    fs::write(&base_object_path, &base_aot_object.object_bytes).expect("write base_module.o");
    fs::write(&dependent_object_path, &dependent_aot_object.object_bytes)
        .expect("write dependent_module.o");
    fs::write(
        host_source_directory.join("base_module.rs"),
        &base_aot_object.registration_source,
    )
    .expect("write base_module.rs");
    fs::write(
        host_source_directory.join("dependent_module.rs"),
        &dependent_aot_object.registration_source,
    )
    .expect("write dependent_module.rs");

    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates dir")
        .parent()
        .expect("workspace root")
        .to_path_buf();
    let zkcl_crate_path = workspace_root.join("crates/zkcl");

    let workspace_lockfile = workspace_root.join("Cargo.lock");
    if workspace_lockfile.exists() {
        fs::copy(&workspace_lockfile, host_project.join("Cargo.lock")).expect("copy Cargo.lock");
    }

    let cargo_toml = format!(
        "[package]\nname = \"zkcl-aot-linked-host\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n\n[dependencies]\nzkcl = {{ path = \"{}\" }}\n",
        zkcl_crate_path.display().to_string().escape_default()
    );
    fs::write(host_project.join("Cargo.toml"), cargo_toml).expect("write Cargo.toml");

    let host_build_source = format!(
        "fn main() {{\n    println!(\"cargo:rustc-link-arg={}\");\n    println!(\"cargo:rustc-link-arg={}\");\n    println!(\"cargo:rustc-link-arg={}\");\n}}\n",
        leaf_object_path.display().to_string().escape_default(),
        base_object_path.display().to_string().escape_default(),
        dependent_object_path.display().to_string().escape_default(),
    );
    fs::write(host_project.join("build.rs"), host_build_source).expect("write build.rs");

    let host_main_source = r#"mod base_module;
mod dependent_module;
mod leaf_module;

fn main() {
    let rt = zkcl::Runtime::start().expect("runtime starts");
    assert!(base_module::AOT_CODES.iter().all(|c| c.c_entry.is_some()));
    assert!(dependent_module::AOT_CODES.iter().all(|c| c.c_entry.is_some()));

    rt.register_function("RUST-HOST-ADD", |rt, args| {
        let x = args[0].as_fixnum().expect("fixnum arg");
        Ok(vec![rt.fixnum(10 + x).expect("fixnum result")])
    })
    .expect("register host callback");

    rt.register_function("COLLECT-IN-CLEANUP", |rt, _| {
        rt.collect();
        Ok(Vec::new())
    }).unwrap();
    rt.register_function("REQUEST-CANCEL", |rt, _| {
        rt.cancellation_handle().cancel_with_cleanup();
        Ok(Vec::new())
    }).unwrap();
    let cancelled_cleanups = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    rt.register_function("RECORD-CANCEL-CLEANUP", {
        let cancelled_cleanups = cancelled_cleanups.clone();
        move |rt, args| {
            assert_eq!(rt.repr(&args[0])?, "(41 42)");
            cancelled_cleanups.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(Vec::new())
        }
    }).unwrap();

    base_module::load(&rt).expect("load base_module");
    dependent_module::load(&rt).expect("load dependent_module");
    leaf_module::load(&rt).expect("load leaf_module");
    assert!(base_module::AOT_CODES.iter().any(|code| code.native_debug));
    assert!(dependent_module::AOT_CODES.iter().any(|code| code.native_debug));
    rt.load_str(r"(defvar *aot-debug-mode* :edit)
      (defvar *aot-debug-flags* nil)
      (defun aot-native-debug-frontend (condition)
        (declare (ignore condition))
        (push (seventh (first (zk::%debug-frames))) *aot-debug-flags*)
        (zk:gc)
        (case *aot-debug-mode*
          (:return (zk::%debug-return 0 '(8 9)))
          (:restart (setq *aot-debug-mode* :edit) (zk::%debug-restart-frame 0))
          (otherwise (zk::%set-debug-local 40 0 'x) (zk::%debug-motion :continue))))").unwrap();
    let frontend = rt.eval("(function aot-native-debug-frontend)").unwrap().remove(0);
    rt.set_debugger(Some(&frontend)).unwrap();
    for name in ["aot-native-debug3", "llvm-native-debug3"] {
        let function = rt.eval(&format!("(function {name})")).unwrap().remove(0);
        let breakpoint = rt.add_breakpoint(&function, None).unwrap();
        for (mode, expected, flags) in [
            (":edit", vec![Some(40), Some(41)], "(T)"),
            (":return", vec![Some(8), Some(9)], "(T)"),
            (":restart", vec![Some(40), Some(41)], "(T T)"),
        ] {
            rt.eval(&format!("(setq *aot-debug-mode* {mode} *aot-debug-flags* nil)")).unwrap();
            let values = rt.call(&function, &[&rt.fixnum(7).unwrap()]).unwrap();
            assert_eq!(values.iter().map(|value| value.as_fixnum()).collect::<Vec<_>>(), expected, "{name} {mode}");
            assert_eq!(rt.repr(&rt.eval("*aot-debug-flags*").unwrap()[0]).unwrap(), flags);
        }
        rt.remove_breakpoint(breakpoint.id);
    }
    rt.set_debugger(None).unwrap();
    for outer in ["aot-async-outer", "llvm-async-outer"] {
        let result = rt.eval(&format!("({outer}
            (lambda (payload)
              (zk-async:with-task-group (group)
                (zk-async:await (zk-async:spawn-task (group)
                                 (zk:gc)
                                 (values 42 payload)))))
            (list :live))")).unwrap();
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].as_fixnum(), Some(42));
        assert_eq!(rt.repr(&result[1]).unwrap(), "(:LIVE)");
        assert!(result[1].same_identity(&result[2]));
    }
    for count in ["aot-async-count", "llvm-async-count"] {
        let target = rt.eval(&format!("(function {count})")).unwrap().remove(0);
        let limit = rt.fixnum(2_000).unwrap();
        let mut evaluation = rt.start_call(&target, &[&limit]).unwrap();
        assert!(matches!(evaluation.resume(None).unwrap(), zkcl::EvaluationPoll::Runnable));
        rt.collect();
        let result = (0..10_000).find_map(|_| match evaluation.resume(None).unwrap() {
            zkcl::EvaluationPoll::Runnable => None,
            zkcl::EvaluationPoll::Ready(values) => Some(values),
            zkcl::EvaluationPoll::Waiting(_) | zkcl::EvaluationPoll::Paused(_) => {
                panic!("counting should stay runnable")
            }
        }).unwrap_or_else(|| panic!("{count} should finish within its slices"));
        assert_eq!(result[0].as_fixnum(), Some(1_999_000));
    }
    assert!(rt.eval("(let ((x (list 1))) (eq (aot-fourth-word nil 2 3 x) x))").unwrap()[0].is_t());
    let duplicated = rt.eval("(aot-duplicate 17)").unwrap();
    assert_eq!(duplicated.len(), 2);
    assert!(duplicated.iter().all(|value| value.as_fixnum() == Some(17)));
    assert_eq!(rt.eval("(aot-range-inc 255)").unwrap()[0].as_fixnum(), Some(256));
    assert!(rt.eval("(handler-case (aot-range-inc -1) (type-error () t))").unwrap()[0].is_t());

    if let Some(fourth) = leaf_module::AOT_CODES.iter().find(|code| code.arity.required == 4 && code.leaf_entry.is_some()) {
        use zkcl::native::{JitCallContext, JitReturn, Word};
        let args = [2, 2, 2, Word::fixnum(-7).unwrap().raw()];
        let mut out = JitReturn::zeroed();
        let mut context = JitCallContext::detached();
        unsafe {
            assert_eq!(fourth.leaf_entry.unwrap()(args[0], args[1], args[2], args[3]), args[3]);
            fourth.c_entry.unwrap()(&mut context, 2, args.as_ptr(), 4, &mut out);
            assert_eq!((out.tag, out.single_value), (JitReturn::TAG_ONE, args[3]));
            fourth.c_entry.unwrap()(&mut context, 2, std::ptr::null(), 0, &mut out);
            assert_eq!((out.tag, out.single_value), (JitReturn::TAG_FAULT, 0));
        }
    }

    let dbl = rt.eval("(aot-double 21)").unwrap()[0]
        .as_fixnum()
        .expect("fixnum double");
    assert_eq!(dbl, 42);
    let s0 = rt.eval("(aot-sum-to 0)").unwrap()[0]
        .as_fixnum()
        .expect("fixnum sum 0");
    assert_eq!(s0, 0);
    let s1 = rt.eval("(aot-sum-to 1)").unwrap()[0]
        .as_fixnum()
        .expect("fixnum sum 1");
    assert_eq!(s1, 1);

    let sum = rt.eval("(aot-sum-to 10)").unwrap()[0]
        .as_fixnum()
        .expect("fixnum sum");
    let quad = rt.eval("(aot-quad 7)").unwrap()[0]
        .as_fixnum()
        .expect("fixnum quad");
    let host = rt.eval("(aot-host-call 5)").unwrap()[0]
        .as_fixnum()
        .expect("fixnum host");
    assert_eq!(
        rt.eval("(aot-catch 0)").unwrap()[0].as_fixnum(),
        Some(7),
        "normal CATCH exit"
    );
    assert_eq!(
        rt.eval("(aot-catch 1)").unwrap()[0].as_fixnum(),
        Some(41),
        "THROW crosses the AOT CATCH frame"
    );
    assert_eq!(
        rt.eval("(aot-handler 0)").unwrap()[0].as_fixnum(),
        Some(7),
        "HANDLER-CASE normal exit"
    );
    assert_eq!(
        rt.eval("(aot-handler 1)").unwrap()[0].as_fixnum(),
        Some(41),
        "the AOT handler receives the condition and preserves the live argument"
    );
    for form in ["(aot-cleanup-single 41)", "(llvm-cleanup-single 41)"] {
        let values = rt.eval(form).unwrap();
        assert_eq!(values.len(), 1, "{form} must return one value");
        assert_eq!(values[0].as_fixnum(), Some(42));
    }
    for (function, low, high, spec) in [
        ("aot-checked-range", 0, 255, "(unsigned-byte 8)"),
        ("llvm-checked-range", -128, 127, "(signed-byte 8)"),
    ] {
        for arg in [low, high] {
            let values = rt.eval(&format!("({function} {arg})")).unwrap();
            assert_eq!(values[0].as_fixnum(), Some(arg));
        }
        for arg in [low - 1, high + 1] {
            let form = format!(
                "(handler-case ({function} {arg})
                   (type-error (c)
                     (and (eql {arg} (type-error-datum c))
                          (equal '{spec} (type-error-expected-type c)))))"
            );
            assert!(rt.eval(&form).unwrap()[0].is_t(), "{form}");
        }
    }
    let values = rt.eval("(aot-save-values 41)").unwrap();
    assert_eq!(values.len(), 2);
    assert_eq!(values[0].as_fixnum(), Some(41));
    assert_eq!(values[1].as_fixnum(), Some(42));
    let list = rt.eval("(aot-append-values 41)").unwrap();
    assert_eq!(rt.repr(&list[0]).unwrap(), "(41 42 99)");

    for (form, expected) in [
        ("(aot-joined-values t)", "(40 42)"),
        ("(aot-joined-values t 5)", "(5 7)"),
        ("(aot-joined-values nil)", "(NIL NIL)"),
    ] {
        let values = rt.eval(&format!("(multiple-value-list {form})")).unwrap();
        assert_eq!(rt.repr(&values[0]).unwrap(), expected);
    }

    for backend in ["aot", "llvm"] {
        for (call, expected) in [
            (format!("(let ((original (list 42)))
                         (multiple-value-bind (value released) ({backend}-optional original)
                           (values (eq original value) value released)))"), "(T (42) NIL)"),
            (format!("({backend}-optional)"), "(NIL NIL)"),
            (format!("({backend}-keywords 11)"), "(11 2 NIL NIL NIL)"),
            (format!("({backend}-keywords 11 nil :value (list 31) :value 77)"),
             "(11 NIL (31) (:VALUE (31) :VALUE 77) NIL)"),
        ] {
            let values = rt.eval(&format!("(multiple-value-list {call})")).unwrap();
            assert_eq!(rt.repr(&values[0]).unwrap(), expected, "{call}");
        }
        let bad_keyword = format!("(handler-case ({backend}-keywords 11 nil :unknown 42) (program-error () t))");
        assert!(rt.eval(&bad_keyword).unwrap()[0].is_t(), "{bad_keyword}");
    }

    for function in ["aot-registers", "llvm-registers"] {
        for (arg, expected) in [("41", [41, 42]), ("nil", [3, 4])] {
            let values = rt.eval(&format!("({function} {arg})")).unwrap();
            assert_eq!(values.iter().map(|v| v.as_fixnum().unwrap()).collect::<Vec<_>>(), expected);
        }
        for cancellation_case in [":cancel-go", ":cancel-branch"] {
            rt.set_step_budget(Some(10_000));
            let error = rt.eval(&format!("({function} {cancellation_case})")).err().unwrap();
            assert_eq!(error.execution_stop(), Some(zkcl::ExecutionStop::Cancelled));
            rt.cancellation_handle().reset();
            rt.set_step_budget(None);
        }
    }
    assert_eq!(
        cancelled_cleanups.load(std::sync::atomic::Ordering::Relaxed),
        4
    );

    for function in ["aot-fixnum-steps", "llvm-fixnum-steps"] {
        let call = format!("({function} -128 127)");
        let values = rt.eval(&call).unwrap();
        assert_eq!(values.iter().map(|v| v.as_fixnum().unwrap()).collect::<Vec<_>>(),
                   [-1, -255, -16256, -127, 126]);
        let traced = format!("(let ((*trace-output* (make-string-output-stream)))
            (unwind-protect
                (progn (trace cl:1+)
                       (and (equal (multiple-value-list {call}) '(-1 -255 -16256 -127 126))
                            (search \"TRACE 1+: enter\" (get-output-stream-string *trace-output*))))
              (untrace cl:1+)))");
        assert!(rt.eval(&traced).unwrap()[0].as_fixnum().is_some());
    }

    for function in ["aot-float-step", "llvm-float-step"] {
        let rounded = rt.eval(&format!(
            "(let ((eps (scale-float 1f0 -13))) ({function} (+ 1f0 eps) (- 1f0 eps) -1f0))"
        )).unwrap();
        assert_eq!(rounded[0].as_single_float().unwrap().to_bits(), 0);
        let mixed = rt.eval(&format!("({function} 1f0 2d0 3f0)")).unwrap();
        assert_eq!(mixed[0].as_double_float(), Some(5.0));
        assert_eq!(mixed[1].as_double_float(), Some(0.5));
        let condition = rt.eval(&format!(
            "(handler-case ({function} 1d0 -0d0 0d0)
              (division-by-zero (c)
                (and (eq '/ (arithmetic-error-operation c))
                     (equal '(1d0 -0d0) (arithmetic-error-operands c)))))"
        )).unwrap();
        assert!(condition[0].is_t());
    }

    let big_vals = rt.eval("(aot-add-big 4611686018427387903 10)").unwrap();
    let big_str = rt.repr(&big_vals[0]).expect("repr bignum");

    rt.eval("(defun aot-double (x)
               (case x (:zero (values)) (:one 7) (:many (values 8 9)) (otherwise (list x))))").unwrap();
    for (arg, expected) in [(":many", "(8)"), (":zero", "(NIL)"), (":one", "(7)")] {
        let result = rt.eval(&format!("(aot-quad {arg})")).unwrap();
        assert_eq!(rt.repr(&result[0]).unwrap(), expected);
    }

    println!("AOT_OK:{sum}:{quad}:{host}:{big_str}");
}
"#;
    fs::write(host_source_directory.join("main.rs"), host_main_source).expect("write main.rs");

    let host_build_directory = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root.join("target"))
        .join("aot-link-cache");
    let build_output = Command::new("cargo")
        .arg("build")
        .arg("--offline")
        .arg("--manifest-path")
        .arg(host_project.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(&host_build_directory)
        .output()
        .expect("invoke cargo build for AOT test executable");

    assert!(
        build_output.status.success(),
        "AOT link build failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&build_output.stdout),
        String::from_utf8_lossy(&build_output.stderr),
    );

    let host_executable = host_build_directory.join("debug").join(if cfg!(windows) {
        "zkcl-aot-linked-host.exe"
    } else {
        "zkcl-aot-linked-host"
    });
    let host_output = Command::new(&host_executable)
        .output()
        .expect("execute linked AOT binary");

    assert!(
        host_output.status.success(),
        "AOT binary exited with failure:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&host_output.stdout),
        String::from_utf8_lossy(&host_output.stderr),
    );

    let stdout = String::from_utf8_lossy(&host_output.stdout);
    assert!(
        stdout
            .lines()
            .any(|l| l == "AOT_OK:55:28:15:4611686018427387913"),
        "unexpected AOT binary output:\n{stdout}"
    );
}
