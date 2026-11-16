use std::fs;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use zkcl::native::{
    AbiFingerprint, AotCodeDescriptor, AotModuleDescriptor, Arity, Fault, JitBackend,
    JitCallContext, JitCompilationRequest, JitFunction, JitReturn, NativeContext, NativeEntry,
    NativeReturn, NativeRootFrame, RootSlots, Word, WordLayout, jit_runtime_functions,
    native_begin_cleanup, native_bind_special, native_end_cleanup, native_enter_catch,
    native_intern, native_invoke, native_leave_target, native_protect, native_unbind,
};
use zkcl::{ConditionClass, EvalErrorKind, Runtime};

mod common;

use common::files::TempDir;

static CLEANUP_LOG: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
static AOT_CALLBACK_CALLS: AtomicUsize = AtomicUsize::new(0);
static AOT_CATCH_CALLS: AtomicUsize = AtomicUsize::new(0);
static JIT_ENTRY_CALLED: AtomicBool = AtomicBool::new(false);

fn fixnum_from_tagged_word(word: usize) -> Option<isize> {
    (word & WordLayout::TAG_MASK == WordLayout::FIXNUM_TAG)
        .then_some((word as isize) >> WordLayout::FIXNUM_SHIFT)
}

fn aot_driver_generic<'rt, 'exec>(
    rt: &'rt Runtime,
    ctx: &mut NativeContext<'rt, 'exec>,
    target: Word,
    args: &RootSlots,
    range: Range<usize>,
) -> Result<NativeReturn, Fault> {
    AOT_CALLBACK_CALLS.fetch_add(1, Ordering::SeqCst);

    // Slots: [0]=code/fn, [1]=closure, [2]=arg0, [3]=callback_value_symbol, [4]=worker_symbol, [5]=obj_tmp
    let frame = NativeRootFrame::new(ctx, 6);
    frame.set(0, target);
    frame.set(1, target);
    if !range.is_empty() {
        frame.set(2, args.get(range.start));
    }

    let callback_value_symbol = native_intern(rt, None, "*CALLBACK-VALUE*", 0)?;
    frame.set(3, callback_value_symbol);
    let worker_symbol = native_intern(rt, None, "VM-WORKER", 0)?;
    frame.set(4, worker_symbol);

    let callback_value = Word::fixnum(500).unwrap();
    native_bind_special(ctx, &frame, frame.get(3), callback_value);

    // Protect body (cleanup state = 1, clear from slot 5).
    native_protect(ctx, &frame, 1, 5);

    let worker_result = native_invoke(ctx, frame.get(4), frame.slots(), 2..3, 0);
    let pending_unwind = match worker_result {
        Ok(NativeReturn::Unwind {
            target_frame,
            state,
        }) if target_frame == frame.frame_index() => Some(state),
        Ok(other @ NativeReturn::Unwind { .. }) => {
            native_unbind(ctx, 1);
            return Ok(other);
        }
        Ok(returned_values) => {
            ctx.store_return(returned_values);
            native_begin_cleanup(ctx);
            Some(1)
        }
        Err(fault) => {
            native_unbind(ctx, 1);
            return Err(fault);
        }
    };

    if let Some(1) = pending_unwind {
        // Trigger a moving GC *inside* cleanup to verify `Entry::Cleanup.saved`
        // keeps the pending `(values obj 14)` alive and relocated!
        rt.collect();
        CLEANUP_LOG.lock().unwrap().push("aot-cleanup");
        let after_cleanup = native_end_cleanup(ctx)?;
        native_unbind(ctx, 1);
        return Ok(after_cleanup);
    }

    native_unbind(ctx, 1);
    Ok(NativeReturn::One(Word::NIL))
}

fn aot_catch_throw_generic<'rt, 'exec>(
    rt: &'rt Runtime,
    ctx: &mut NativeContext<'rt, 'exec>,
    target: Word,
    args: &RootSlots,
    range: Range<usize>,
) -> Result<NativeReturn, Fault> {
    AOT_CATCH_CALLS.fetch_add(1, Ordering::SeqCst);

    // Slots: [0]=code, [1]=closure, [2]=catch_tag, [3]=thrower_symbol, [4]=arg0
    let frame = NativeRootFrame::new(ctx, 5);
    frame.set(0, target);
    frame.set(1, target);
    let escape_tag_symbol = native_intern(rt, None, "NATIVE-ESCAPE-TAG", 0)?;
    frame.set(2, escape_tag_symbol);
    let thrower_symbol = native_intern(rt, None, "VM-THROWER", 0)?;
    frame.set(3, thrower_symbol);
    if !range.is_empty() {
        frame.set(4, args.get(range.start));
    }

    // Outer CATCH on frame slot 2 -> resume state 2.
    native_enter_catch(ctx, &frame, 2, 2, 4);
    // Inner UNWIND-PROTECT -> cleanup state 1.
    native_protect(ctx, &frame, 1, 4);

    let thrower_return = native_invoke(ctx, frame.get(3), frame.slots(), 4..5, 0)?;
    if let Some(state) = frame.catch_unwind(ctx, thrower_return) {
        if state == 1 {
            CLEANUP_LOG.lock().unwrap().push("aot-inner-cleanup");
            let next = native_end_cleanup(ctx)?;
            if let Some(2) = frame.catch_unwind(ctx, next) {
                // Caught at state 2! Preserve `(values ...)` in ctx.
                return Ok(NativeReturn::Values);
            }
            return Ok(next);
        }
        if state == 2 {
            return Ok(NativeReturn::Values);
        }
    }

    native_begin_cleanup(ctx);
    CLEANUP_LOG.lock().unwrap().push("aot-inner-cleanup");
    let _ = native_end_cleanup(ctx)?;
    native_leave_target(ctx);
    Ok(NativeReturn::Values)
}

unsafe extern "C" fn add_hundred_jit_entry(
    ctx: *mut JitCallContext,
    _target: usize,
    args_ptr: *const usize,
    argc: usize,
    out_ret: *mut JitReturn,
) {
    JIT_ENTRY_CALLED.store(true, Ordering::SeqCst);
    let table = jit_runtime_functions();
    let _ = ctx;
    let x = if argc >= 1 && !args_ptr.is_null() {
        fixnum_from_tagged_word(unsafe { *args_ptr }).unwrap_or(0)
    } else {
        0
    };
    let result = Word::fixnum(x + 100).unwrap();
    let _ = table.abi_version;
    unsafe {
        *out_ret = JitReturn {
            tag: JitReturn::TAG_ONE,
            single_value: result.raw(),
            target_frame: 0,
            state: 0,
        };
    }
}

struct AddHundredJitBackend {
    submitted: Mutex<Vec<JitCompilationRequest>>,
    ready: Mutex<Vec<JitFunction>>,
}

impl AddHundredJitBackend {
    fn new() -> Self {
        Self {
            submitted: Mutex::new(Vec::new()),
            ready: Mutex::new(Vec::new()),
        }
    }
}

// SAFETY: The test entry returns a tagged immediate, uses no GC or VM state,
// and preserves the fixture's request identity and C ABI.
unsafe impl JitBackend for AddHundredJitBackend {
    unsafe fn submit(&self, request: JitCompilationRequest) {
        let unit = JitFunction {
            native_debug: false,
            code_id: request.code_id,
            entry: NativeEntry::from_jit_c(0, request.revision),
            c_entry: Some(add_hundred_jit_entry),
            resumable: false,
            leaf_entry: None,
            leaf_guards: Box::default(),
            executable_owner: None,
        };
        self.submitted.lock().unwrap().push(request);
        self.ready.lock().unwrap().push(unit);
    }

    fn poll_completions(&self) -> Vec<JitFunction> {
        std::mem::take(&mut *self.ready.lock().unwrap())
    }
}

// SAFETY: The descriptor contains no native entries.
static INCOMPATIBLE_AOT_MODULE: AotModuleDescriptor = unsafe {
    AotModuleDescriptor::new(
        "incompatible-abi",
        AbiFingerprint {
            pointer_width: 0,
            word_layout_version: 999,
            bytecode_contract_version: 0,
        },
        None,
        &[],
    )
};

static AOT_CODES: [AotCodeDescriptor; 2] = [
    AotCodeDescriptor {
        native_debug: false,
        entry_index: 1,
        arity: Arity {
            required: 1,
            maximum: Some(1),
        },
        root_slots: 6,
        fast_entry: None,
        generic_entry: aot_driver_generic,
        c_entry: None,
        continuation_abi: None,
        leaf_entry: None,
        leaf_guards: &[],
    },
    AotCodeDescriptor {
        native_debug: false,
        entry_index: 3,
        arity: Arity {
            required: 1,
            maximum: Some(1),
        },
        root_slots: 5,
        fast_entry: None,
        generic_entry: aot_catch_throw_generic,
        c_entry: None,
        continuation_abi: None,
        leaf_entry: None,
        leaf_guards: &[],
    },
];

#[test]
fn aot_module_registration_rejects_an_incompatible_fingerprint() {
    let rt = Runtime::start().unwrap();
    let incompatible_abi = rt
        .register_aot_module(&INCOMPATIBLE_AOT_MODULE)
        .unwrap_err();
    assert_eq!(
        incompatible_abi.kind(),
        EvalErrorKind::LispCondition(ConditionClass::ProgramError)
    );
}

#[test]
fn aot_module_registration_checks_the_compiled_continuation_abi() {
    const CURRENT: u32 = zkcl_bytecode::native_abi::NATIVE_CALL_ABI_REVISION;
    static CODES: [AotCodeDescriptor; 1] = [AotCodeDescriptor {
        continuation_abi: Some(CURRENT - 1),
        ..AOT_CODES[0]
    }];
    // SAFETY: The deliberately obsolete entry is rejected during registration.
    static MODULE: AotModuleDescriptor = unsafe {
        AotModuleDescriptor::new(
            "obsolete-continuation",
            AbiFingerprint::CURRENT,
            None,
            &CODES,
        )
    };
    let runtime = Runtime::start().unwrap();
    let error = runtime.register_aot_module(&MODULE).unwrap_err();
    assert_eq!(
        error.kind(),
        EvalErrorKind::LispCondition(ConditionClass::ProgramError)
    );
    assert!(error.message().contains("continuation ABI"));
}

#[test]
fn rust_aot_entry_cannot_suspend_its_native_caller() {
    fn call_thunk<'rt, 'exec>(
        _rt: &'rt Runtime,
        ctx: &mut NativeContext<'rt, 'exec>,
        _target: Word,
        args: &RootSlots,
        range: Range<usize>,
    ) -> Result<NativeReturn, Fault> {
        let _frame = NativeRootFrame::new(ctx, 0);
        native_invoke(ctx, args.get(range.start), RootSlots::empty(), 0..0, 0)
    }

    let rt = Runtime::start().unwrap();
    let dir = TempDir::new("aot-sync-boundary");
    let source = dir.path().join("call-thunk.lisp");
    fs::write(&source, "(defun native-call-thunk (fn) (funcall fn))").unwrap();
    let compiled_path = rt.compile_file(&source, None).unwrap();
    let fasl: &'static [u8] = Box::leak(fs::read(compiled_path).unwrap().into_boxed_slice());
    let compiled = zkcl_bytecode::CompiledFile::decode(fasl).unwrap();
    let codes = Box::leak(Box::new([AotCodeDescriptor {
        native_debug: false,
        entry_index: compiled.entries.len() as u32,
        arity: Arity {
            required: 1,
            maximum: Some(1),
        },
        root_slots: 0,
        fast_entry: None,
        generic_entry: call_thunk,
        c_entry: None,
        continuation_abi: None,
        leaf_entry: None,
        leaf_guards: &[],
    }]));
    // SAFETY: The static entry calls the one argument of the corresponding FASL function
    // synchronously. The argument stays rooted in the caller's slots.
    let module = Box::leak(Box::new(unsafe {
        AotModuleDescriptor::new(
            "aot-sync-boundary",
            AbiFingerprint::CURRENT,
            Some(fasl),
            codes,
        )
    }));
    rt.load_aot_module(module).unwrap();
    let values = rt
        .eval(
            "(let ((entered 0) (cleaned 0))
               (and (eq (handler-case
                            (native-call-thunk
                             (lambda ()
                               (unwind-protect
                                    (progn (incf entered)
                                           (zk-async:with-task-group (group)
                                             (zk-async:spawn-task (group) :child)))
                                 (incf cleaned))))
                          (control-error () :sync-boundary))
                        :sync-boundary)
                    (= entered cleaned 1)
                    (equal (multiple-value-list (native-call-thunk (lambda () (values 1 2))))
                           '(1 2))
                    (zk-async:with-task-group (group)
                      (zk-async:await (zk-async:spawn-task (group) t)))))",
        )
        .unwrap();
    assert!(values[0].is_t());
    let err = rt
        .eval("(native-call-thunk (lambda () (error \"native parent\")))")
        .err()
        .expect("the Lisp callback signals an unhandled error");
    let frame = err
        .stack_frames()
        .iter()
        .find(|frame| frame.native && frame.function.as_deref() == Some("NATIVE-CALL-THUNK"))
        .expect("the registered native activation is in the backtrace");
    assert_eq!(frame.source_path, Some(source.canonicalize().unwrap()));
    assert!(frame.bytecode_pc.is_none());
}

#[test]
fn native_vm_round_trips_preserve_the_callers_dynamic_context() {
    let rt = Runtime::start().unwrap();
    let dir = TempDir::new("native-vm-interop");
    // Compile two VM functions alongside the functions to be replaced by native entries.
    let lisp_path = dir.path().join("native-entries.lisp");
    let fasl_path = dir.path().join("native-entries.zkclfasl");
    let source = r#"
(defvar *callback-value* 10)
(defvar *vm-cleanup-count* 0)

;; Native entry for the host callback round trip.
(defun aot-driver (n)
  (declare (ignore n))
  :vm-fallback-should-not-run)

;; VM callee that rebinds the callback value.
(defun vm-worker (n)
  (let ((*callback-value* (+ *callback-value* 1)))
    (host-collect-and-return-values (list n))))

;; Native entry for a throw from the VM.
(defun aot-catch-throw (x)
  (declare (ignore x))
  :vm-fallback-should-not-run)

;; VM callee that throws through a cleanup.
(defun vm-thrower (x)
  (unwind-protect
      (throw 'native-escape-tag (values x 99))
    (setq *vm-cleanup-count* (+ *vm-cleanup-count* 1))))
"#;
    fs::write(&lisp_path, source).unwrap();

    // Pre-register the host callback & compile to FASL.
    rt.register_function("host-collect-and-return-values", |rt, args| {
        let [arg] = args else {
            return Err("host-collect-and-return-values expects 1 arg".into());
        };
        // Re-enter Lisp, verify dynamic binding from AOT (500) + VM (+ 1) = 501.
        let callback_value = rt.eval("*callback-value*")?.pop().unwrap();
        assert_eq!(callback_value.as_fixnum(), Some(501));

        // Allocate garbage and force moving GC while `arg` is rooted across callbacks.
        rt.load_str("(dotimes (i 200) (list i (+ i 1)))")?;
        rt.collect();

        let fourteen = rt.fixnum(14).unwrap();
        Ok(vec![arg.clone(), fourteen])
    })
    .unwrap();

    rt.load_str(
        r#"
(defvar *expansion-count* 0)
(defmacro count-expansion (x)
  (setq *expansion-count* (+ *expansion-count* 1))
  x)
"#,
    )
    .unwrap();

    // The native-entry file does not invoke the macro.
    let compiled_path = rt.compile_file(&lisp_path, Some(&fasl_path)).unwrap();
    assert_eq!(
        rt.eval("*expansion-count*").unwrap()[0].as_fixnum(),
        Some(0),
        "macro was not invoked in source yet"
    );

    // Also compile a second file that actually invokes `count-expansion` so we can
    // prove loading FASL does not replay compile-time macro expansions.
    let macro_lisp = dir.path().join("compile-time-expansions.lisp");
    let macro_fasl = dir.path().join("compile-time-expansions.zkclfasl");
    fs::write(&macro_lisp, "(defun macro-result () (count-expansion 77))").unwrap();
    rt.compile_file(&macro_lisp, Some(&macro_fasl)).unwrap();
    assert_eq!(
        rt.eval("*expansion-count*").unwrap()[0].as_fixnum(),
        Some(1)
    );
    fs::remove_file(&macro_lisp).unwrap();
    rt.load_file(&macro_fasl).unwrap();
    assert_eq!(rt.eval("(macro-result)").unwrap()[0].as_fixnum(), Some(77));
    assert_eq!(
        rt.eval("*expansion-count*").unwrap()[0].as_fixnum(),
        Some(1),
        "loading FASL must not replay compile-time macro"
    );

    // Delete original source and register static AOT module matching exact FASL bytes.
    fs::remove_file(&lisp_path).unwrap();
    let fasl_bytes: &'static [u8] = Box::leak(fs::read(&compiled_path).unwrap().into_boxed_slice());

    // These DEFUNs put a function constant in each top-level entry. The nested
    // function entries are numbered after the top-level entries, in source order.
    let compiled_file = zkcl_bytecode::CompiledFile::decode(fasl_bytes).unwrap();
    let mut function_entry_indices = Vec::new();
    let mut next_entry_index = compiled_file.entries.len() as u32;
    for entry in &compiled_file.entries {
        for &constant in &entry.constants {
            if matches!(
                compiled_file.nodes[constant as usize],
                zkcl_bytecode::LiteralNode::Function(_)
            ) {
                function_entry_indices.push(next_entry_index);
                next_entry_index += 1;
            }
        }
    }
    assert!(
        function_entry_indices.len() >= 4,
        "expected at least four function entries, got {function_entry_indices:?}"
    );
    let code_descriptors: &'static [AotCodeDescriptor] = Box::leak(Box::new([
        AotCodeDescriptor {
            native_debug: false,
            entry_index: function_entry_indices[0],
            arity: Arity {
                required: 1,
                maximum: Some(1),
            },
            root_slots: 6,
            fast_entry: None,
            generic_entry: aot_driver_generic,
            c_entry: None,
            continuation_abi: None,
            leaf_entry: None,
            leaf_guards: &[],
        },
        AotCodeDescriptor {
            native_debug: false,
            entry_index: function_entry_indices[2],
            arity: Arity {
                required: 1,
                maximum: Some(1),
            },
            root_slots: 5,
            fast_entry: None,
            generic_entry: aot_catch_throw_generic,
            c_entry: None,
            continuation_abi: None,
            leaf_entry: None,
            leaf_guards: &[],
        },
    ]));
    // SAFETY: The two statically linked Rust callbacks implement the functions at the
    // selected FASL indices with the listed root slots and arity.
    let aot_module: &'static AotModuleDescriptor = Box::leak(Box::new(unsafe {
        AotModuleDescriptor::new(
            "native-vm-interop",
            AbiFingerprint::CURRENT,
            Some(fasl_bytes),
            code_descriptors,
        )
    }));

    rt.load_aot_module(aot_module).unwrap();
    rt.register_aot_module(aot_module).unwrap();
    // SAFETY: The duplicate descriptor contains no native entries.
    let duplicate = Box::leak(Box::new(unsafe {
        AotModuleDescriptor::new(
            aot_module.module_id(),
            AbiFingerprint::CURRENT,
            Some(fasl_bytes),
            &[],
        )
    }));
    let error = rt.load_aot_module(duplicate).unwrap_err();
    assert!(error.to_string().contains("already registered"));

    // 3. Test AOT -> VM -> Rust callback -> Lisp -> Moving GC -> Multi-value -> AOT UNWIND-PROTECT!
    CLEANUP_LOG.lock().unwrap().clear();
    let callback_values = rt.eval("(aot-driver 93)").unwrap();
    assert_eq!(AOT_CALLBACK_CALLS.load(Ordering::SeqCst), 1);
    assert_eq!(
        callback_values.len(),
        2,
        "expected 2 return values (obj, 14)"
    );
    assert_eq!(rt.repr(&callback_values[0]).unwrap(), "(93)");
    assert_eq!(callback_values[1].as_fixnum(), Some(14));
    assert_eq!(
        rt.eval("*callback-value*").unwrap()[0].as_fixnum(),
        Some(10),
        "dynamic binding must be restored on exit"
    );
    assert_eq!(*CLEANUP_LOG.lock().unwrap(), vec!["aot-cleanup"]);

    // 4. Test non-local THROW across VM UNWIND-PROTECT -> AOT UNWIND-PROTECT -> AOT CATCH!
    CLEANUP_LOG.lock().unwrap().clear();
    let thrown = rt.eval("(aot-catch-throw 'escaped)").unwrap();
    assert_eq!(AOT_CATCH_CALLS.load(Ordering::SeqCst), 1);
    assert_eq!(thrown.len(), 2);
    assert_eq!(rt.repr(&thrown[0]).unwrap(), "ESCAPED");
    assert_eq!(thrown[1].as_fixnum(), Some(99));
    assert_eq!(
        rt.eval("*vm-cleanup-count*").unwrap()[0].as_fixnum(),
        Some(1),
        "VM UNWIND-PROTECT cleanup must run before AOT cleanup"
    );
    assert_eq!(*CLEANUP_LOG.lock().unwrap(), vec!["aot-inner-cleanup"]);

    // Resolve an existing COMMON-LISP symbol through the native lookup API.
    assert!(rt.lookup_symbol("COMMON-LISP", "CAR").unwrap().is_some());
    let _ = &AOT_CODES;
}

#[test]
fn redefining_a_function_supersedes_its_installed_jit_entry() {
    let rt = Runtime::start().unwrap();
    // Bootstrap the runtime before attaching a backend that substitutes x + 100.
    rt.load_str("(defun jit-target (x) (+ x 1))").unwrap();
    let jit = Arc::new(AddHundredJitBackend::new());
    rt.set_jit_backend(jit.clone());
    // First call runs VM and submits `jit-target` to `AddHundredJitBackend`.
    assert_eq!(rt.eval("(jit-target 7)").unwrap()[0].as_fixnum(), Some(8));
    assert!(!jit.submitted.lock().unwrap().is_empty());
    // Poll completions and invoke again -> hits `add_hundred_jit_entry` (+ 100)!
    assert!(rt.poll_jit_completions() >= 1);
    assert_eq!(rt.eval("(jit-target 7)").unwrap()[0].as_fixnum(), Some(107));
    assert!(JIT_ENTRY_CALLED.load(Ordering::SeqCst));

    // Redefine `jit-target` -> new CodeId supersedes old NativeEntry.
    rt.load_str("(defun jit-target (x) (+ x 200))").unwrap();
    assert_eq!(rt.eval("(jit-target 7)").unwrap()[0].as_fixnum(), Some(207));
}

/// Give the VM, array ABI and scalar ABI the same register callee.
/// Counts distinguish an accepted call from a correct-looking bytecode result.
#[test]
fn register_calls_check_live_guards_from_vm_and_native_callers() {
    use std::sync::atomic::AtomicBool;

    use zkcl::native::{
        FixnumPrimitive, JitCEntryFn, LeafGuard, RegisterLeafFn, zkcl_register_leaf_dispatch,
    };
    use zkcl_bytecode::{
        Callee, Instruction,
        native_abi::{JIT_CALL_PRIMARY_SLOW, LEAF_MISS},
    };

    static LEAF_CALLS: AtomicUsize = AtomicUsize::new(0);
    static CHILD_SYMBOL: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "C" fn sum(a: usize, b: usize, c: usize, d: usize) -> usize {
        LEAF_CALLS.fetch_add(1, Ordering::Relaxed);
        if c != Word::NIL.raw() || d != Word::NIL.raw() {
            return Word::NIL.raw();
        }
        let in_range = |word| fixnum_from_tagged_word(word).is_some_and(|n| (0..=20).contains(&n));
        if in_range(a) && in_range(b) {
            a + b - 1
        } else {
            LEAF_MISS as usize
        }
    }

    unsafe extern "C" fn parent(
        ctx: *mut JitCallContext,
        target: usize,
        args: *const usize,
        argc: usize,
        out: *mut JitReturn,
    ) {
        unsafe {
            (jit_runtime_functions().call_named)(
                ctx,
                target,
                CHILD_SYMBOL.load(Ordering::Relaxed),
                args,
                argc,
                0,
                out,
            );
        }
    }

    unsafe extern "C" fn scalar_parent(
        ctx: *mut JitCallContext,
        target: usize,
        args: *const usize,
        argc: usize,
        out: *mut JitReturn,
    ) {
        assert_eq!(argc, 2);
        // A successful register call must leave the structured result alone.
        let poison = JitReturn {
            tag: JitReturn::TAG_FAULT,
            ..JitReturn::zeroed()
        };
        unsafe { out.write(poison) };
        let leaf_calls = LEAF_CALLS.load(Ordering::Relaxed);
        let word = unsafe {
            (jit_runtime_functions().call_primary)(
                ctx,
                target,
                CHILD_SYMBOL.load(Ordering::Relaxed),
                0,
                argc,
                args.read(),
                args.add(1).read(),
                out,
            )
        };
        if word != JIT_CALL_PRIMARY_SLOW as usize {
            // Some fallback paths write a single value before extracting it.
            // The scalar leaf itself returns without touching the record.
            let preserved = unsafe { out.read() };
            if LEAF_CALLS.load(Ordering::Relaxed) != leaf_calls
                && fixnum_from_tagged_word(word) == Some(20)
            {
                assert_eq!(preserved, poison);
            }
            unsafe {
                out.write(JitReturn {
                    single_value: word,
                    ..JitReturn::zeroed()
                })
            };
        }
    }

    #[derive(Default)]
    struct Backend {
        completed: Mutex<Vec<JitFunction>>,
        scalar_parent: AtomicBool,
    }
    // SAFETY: Both fixture entries are static, preserve their request identities
    // and use the constant index and leaf guard taken from the matching snapshot.
    unsafe impl JitBackend for Backend {
        unsafe fn submit(&self, request: JitCompilationRequest) {
            let c_entry: JitCEntryFn;
            let mut leaf_entry: Option<RegisterLeafFn> = None;
            let mut guards = Vec::new();
            if let Some(symbol) = request.cl_const_names.iter().position(|s| *s == Some("+")) {
                c_entry = zkcl_register_leaf_dispatch;
                leaf_entry = Some(sum);
                guards.push(LeafGuard {
                    symbol,
                    primitive: FixnumPrimitive::Arithmetic(zkcl_bytecode::FixnumArithmetic::Add)
                        .into(),
                });
            } else {
                let symbol = request
                    .canonical
                    .iter()
                    .find_map(|inst| match inst {
                        Instruction::Call {
                            callee: Callee::Named(sym),
                            ..
                        } => Some(*sym),
                        _ => None,
                    })
                    .expect("parent calls child by name");
                CHILD_SYMBOL.store(symbol, Ordering::Relaxed);
                c_entry = if self.scalar_parent.load(Ordering::Relaxed) {
                    scalar_parent
                } else {
                    parent
                };
            }
            self.completed.lock().unwrap().push(JitFunction {
                native_debug: false,
                code_id: request.code_id,
                entry: NativeEntry::from_jit_c(0, request.revision),
                c_entry: Some(c_entry),
                resumable: false,
                leaf_entry,
                leaf_guards: guards.into(),
                executable_owner: None,
            });
        }

        fn poll_completions(&self) -> Vec<JitFunction> {
            std::mem::take(&mut *self.completed.lock().unwrap())
        }
    }

    let rt = Runtime::builder().jit_threshold(u32::MAX).start().unwrap();
    rt.load_str(
        "(defun reg-child (x y) (+ x y))
         (defun reg-parent (x y) (reg-child x y))
         (defun reg-scalar-parent (x y) (reg-child x y))",
    )
    .unwrap();
    let backend = Arc::new(Backend::default());
    rt.set_jit_backend(backend.clone());
    rt.eval("(compile nil #'reg-child)").unwrap();
    assert_eq!(rt.poll_jit_completions(), 1);
    let eleven = rt.fixnum(11).unwrap();
    let nine = rt.fixnum(9).unwrap();
    for (name, native_parent) in [
        ("reg-parent", false),
        ("reg-parent", true),
        ("reg-scalar-parent", true),
    ] {
        if native_parent {
            backend
                .scalar_parent
                .store(name == "reg-scalar-parent", Ordering::Relaxed);
            rt.eval(&format!("(compile nil #'{name})")).unwrap();
            assert_eq!(rt.poll_jit_completions(), 1);
        }
        let parent = rt.eval(&format!("#'{name}")).unwrap().pop().unwrap();
        let before = LEAF_CALLS.load(Ordering::Relaxed);
        for _ in 0..3 {
            assert_eq!(
                rt.call(&parent, &[&eleven, &nine]).unwrap()[0].as_fixnum(),
                Some(20)
            );
        }
        assert_eq!(LEAF_CALLS.load(Ordering::Relaxed), before + 3);
        assert_eq!(
            rt.eval(&format!("({name} 21 9)")).unwrap()[0].as_fixnum(),
            Some(30)
        );
        let handled = rt
            .eval(&format!(
                "(handler-case ({name} 11 :bad) (type-error (c) (type-error-datum c)))"
            ))
            .unwrap();
        assert_eq!(rt.repr(&handled[0]).unwrap(), ":BAD");
        let traced = rt.eval(&format!(r#"(let ((*trace-output* (make-string-output-stream)))
              (trace +)
              (unwind-protect
                  (values ({name} 11 9) (search "TRACE +: enter" (get-output-stream-string *trace-output*)))
                (untrace +)))"#)).unwrap();
        assert_eq!(traced[0].as_fixnum(), Some(20));
        assert!(traced[1].as_fixnum().is_some());
        assert_eq!(LEAF_CALLS.load(Ordering::Relaxed), before + 5);
    }
    rt.load_str("(defun reg-child (x y) (values y x))").unwrap();
    for name in ["reg-parent", "reg-scalar-parent"] {
        let values = rt.eval(&format!("({name} 11 9)")).unwrap();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0].as_fixnum(), Some(9));
        assert_eq!(values[1].as_fixnum(), Some(11));
    }
}
