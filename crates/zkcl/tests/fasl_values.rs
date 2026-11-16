use std::fs;
use std::path::Path;

use zkcl::Runtime;
use zkcl_bytecode::{
    Arity, BytecodeFunction, CompiledFile, HashTableTest, Instruction, LiteralNode,
};

mod common;

use common::files::TempDir;
use common::lisp::eval;

#[test]
fn load_time_value_recreates_captured_functions_after_compile_time_execution() {
    let temp = TempDir::new("fasl-load-time-closure");
    let source = temp.path().join("counter.lisp");
    fs::write(
        &source,
        "(eval-when (:compile-toplevel :load-toplevel :execute)
           (defvar *initializations* 0)
           (defun counter-function ()
             (load-time-value
               (let ((n (incf *initializations*)))
                 (lambda () (incf n))))))
         (eval-when (:compile-toplevel)
           (assert (= 2 (funcall (counter-function)))))",
    )
    .unwrap();
    let compiler = Runtime::start().unwrap();
    let compiled = compiler.compile_file(&source, None).unwrap();
    assert_eq!(
        eval(&compiler, "(funcall (counter-function))")[0].as_fixnum(),
        Some(3)
    );
    drop(compiler);

    let loader = Runtime::start().unwrap();
    loader.load_file(&compiled).unwrap();
    eval(&loader, "(defvar *first-counter* (counter-function))");
    loader.collect();
    loader.load_file(&compiled).unwrap();
    let values = eval(
        &loader,
        "(values *initializations* (eq *first-counter* (counter-function))
                 (eq (counter-function) (counter-function))
                 (funcall *first-counter*) (funcall (counter-function)))",
    );
    assert_eq!(values[0].as_fixnum(), Some(2));
    assert!(values[1].is_nil());
    assert!(!values[2].is_nil());
    assert_eq!(values[3].as_fixnum(), Some(2));
    assert_eq!(values[4].as_fixnum(), Some(3));
}

#[test]
fn fasl_loading_restores_shared_objects_used_as_hash_keys() {
    let temp_dir = TempDir::new("fasl-values");
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("collect-during-fasl-load", |runtime, _| {
            runtime.collect();
            Ok(vec![])
        })
        .unwrap();
    let input = temp_dir.path().join("shared.lisp");
    fs::write(
        &input,
        r#"(defstruct fasl-box payload tag)
           (eval-when (:compile-toplevel)
             (defmethod make-load-form ((obj fasl-box) &optional environment)
               (make-load-form-saving-slots obj :environment environment))
             (defvar *fasl-shared* (list 42))
             (defvar *fasl-instance* (make-fasl-box :payload *fasl-shared* :tag :saved))
             (defvar *fasl-table* (make-hash-table :test 'eq :weakness :key :rehash-size 1.5d0 :rehash-threshold 0.5d0))
             (setf (cdr *fasl-shared*) (list *fasl-instance* *fasl-table*))
             (setf (gethash *fasl-instance* *fasl-table*) *fasl-shared*)
             (let ((outer (make-hash-table :test 'equalp :rehash-size 7 :rehash-threshold 0))
                   (left (make-hash-table :test 'eql))
                   (right (make-hash-table :test 'eql)))
               (setf (gethash :label left) "red" (gethash :label right) "blue")
               (setf (gethash left outer) 101 (gethash right outer) 202)
               (setf (gethash :outer *fasl-table*) (list outer left right)))
             (defvar *fasl-box-keys*
               (let* ((table (make-hash-table :test 'equalp))
                      (a (make-fasl-box :payload table :tag :a))
                      (b (make-fasl-box :payload table :tag :b)))
                 (setf (gethash a table) 1 (gethash b table) 2)
                 (list table a b)))
             (defmacro saved-box-keys () (list 'quote *fasl-box-keys*))
             (defmacro saved-instance () (list 'quote *fasl-instance*))
             (defmacro saved-table () (list 'quote *fasl-table*))
             (defmacro saved-payload () (list 'quote *fasl-shared*)))
           (defun loaded-instance () (saved-instance))
           (collect-during-fasl-load)
           (defun loaded-table () (saved-table))
           (defun loaded-payload () (saved-payload))
           (defun loaded-box-keys () (saved-box-keys))"#,
    )
    .unwrap();
    let compiled = runtime.compile_file(&input, None).unwrap();
    fs::remove_file(&input).unwrap();
    runtime.collect();
    runtime.load_file(&compiled).unwrap();
    runtime.collect();

    let results = eval(
        &runtime,
        "(let* ((table (loaded-table)) (instance (loaded-instance))
                (payload (loaded-payload)) (nested (gethash :outer table)))
            (multiple-value-call #'values
                (values (fasl-box-p instance)
                        (eq (fasl-box-tag instance) :saved)
                        (eq instance (cadr payload))
                        (eq table (caddr payload))
                        (eq (gethash instance table) payload)
                        (eq (fasl-box-payload instance) payload)
                        (not (eq payload *fasl-shared*))
                        (and (eq (hash-table-test table) 'eq)
                             (eq (zk:hash-table-weakness table) :key)
                             (= (hash-table-rehash-size table) 1.5d0)
                             (= (hash-table-rehash-threshold table) 0.5d0))
                        (hash-table-count table)
                        (hash-table-rehash-size (first nested))
                        (hash-table-rehash-threshold (first nested))
                        (hash-table-count (first nested)))
                (gethash (second nested) (first nested))
                (gethash (third nested) (first nested))))",
    );
    assert!(results[..8].iter().all(|value| !value.is_nil()));
    assert_eq!(results[8].as_fixnum(), Some(2));
    assert_eq!(results[9].as_fixnum(), Some(7));
    assert_eq!(results[10].as_fixnum(), Some(0));
    assert_eq!(results[11].as_fixnum(), Some(2));
    assert_eq!(results[12].as_fixnum(), Some(101));
    assert!(!results[13].is_nil());
    assert_eq!(results[14].as_fixnum(), Some(202));
    assert!(!results[15].is_nil());

    assert!(
        !eval(
            &runtime,
            "(destructuring-bind (table a b) (loaded-box-keys)
                             (and (= (gethash a table) 1) (= (gethash b table) 2)
                                  (eq table (fasl-box-payload a))))"
        )[0]
        .is_nil()
    );
    reject_invalid_literals(&runtime, &temp_dir);
}

fn reject_invalid_literals(runtime: &Runtime, temp_dir: &TempDir) {
    let keys = temp_dir.path().join("duplicate-keys.zkclfasl");
    write_literal(
        &keys,
        vec![
            LiteralNode::HashTable {
                test: HashTableTest::Equalp,
                weakness: None,
                rehash_size: zkcl_bytecode::RehashHint::integer(8),
                rehash_threshold: zkcl_bytecode::RehashHint::integer(1),
                entries: vec![(1, 3), (2, 3)],
            },
            LiteralNode::String("Alpha".into()),
            LiteralNode::String("ALPHA".into()),
            LiteralNode::Nil,
        ],
    );
    let error = runtime.load_file(&keys).unwrap_err();
    assert!(error.to_string().contains("duplicate keys"));

    let cyclic_keys = temp_dir.path().join("duplicate-cycles.zkclfasl");
    let table = |test, entries| LiteralNode::HashTable {
        test,
        weakness: None,
        rehash_size: zkcl_bytecode::RehashHint::integer(8),
        rehash_threshold: zkcl_bytecode::RehashHint::integer(1),
        entries,
    };
    write_literal(
        &cyclic_keys,
        vec![
            table(HashTableTest::Equalp, vec![(1, 4), (2, 4)]),
            table(HashTableTest::Eql, vec![(3, 1)]),
            table(HashTableTest::Eql, vec![(3, 2)]),
            LiteralNode::Character('a'),
            LiteralNode::Nil,
        ],
    );
    let error = runtime.load_file(&cyclic_keys).unwrap_err();
    assert!(error.to_string().contains("duplicate keys"));

    assert_eq!(
        eval(runtime, "(car (loaded-payload))")[0].as_fixnum(),
        Some(42)
    );
}

fn write_literal(path: &Path, nodes: Vec<LiteralNode>) {
    let file = CompiledFile {
        target: zkcl::CompilationTarget::native(),
        source_files: Vec::new(),
        nodes,
        entries: vec![BytecodeFunction {
            name: None,
            source_file: None,
            debug: Default::default(),
            arity: Arity::ZERO,
            registers: 1,
            instructions: vec![Instruction::Constant(0, 0), Instruction::SetValues(0..1)],
            constants: vec![0],
            lambda_list: None,
            captures: Vec::new(),
            notinline: Vec::new(),
            vop_calls: Vec::new(),
            handlers: Vec::new(),
            restarts: Vec::new(),
            keywords: Vec::new(),
            jump_tables: Vec::new(),
            facts: Default::default(),
        }],
    };
    fs::write(path, file.encode().unwrap()).unwrap();
}

#[test]
fn load_forms_recreate_shared_instances_in_a_fresh_runtime() {
    let temp_dir = TempDir::new("fasl-load-forms");
    let source = temp_dir.path().join("externalize.lisp");
    fs::write(
        &source,
        r#"
      (eval-when (:compile-toplevel :load-toplevel :execute)
        (defclass saved-node () ((id :initarg :id :reader node-id) (next)))
        (defvar *phase*)
        (defvar *events* nil))
      (eval-when (:compile-toplevel)
        (defvar *calls* 0)
        (defmethod make-load-form ((node saved-node) &optional env)
          (declare (ignore env))
          (when (eq *phase* :abort) (throw 'abort-compile :method))
          (when (eq *phase* :cycle) (return-from make-load-form (list 'quote node)))
          (assert (and (eq *phase* :compiling) (pathnamep *compile-file-truename*)))
          (incf *calls*)
          (fasl-gc)
          (let ((id (node-id node)) (next (slot-value node 'next)))
            (values
              `(progn
                 (assert (and (eq *phase* :loading) (pathnamep *load-truename*)))
                 (when *abort-load* (throw 'abort-load :creation))
                 ,@(when (zerop id)
                     (list `(assert (slot-boundp ',next 'next))))
                 (push (list :create ,id) *events*)
                 (values (make-instance 'saved-node :id ,id) :ignored))
              `(progn (fasl-gc) (push (list :init ,id) *events*)
                      (setf (slot-value ',node 'next) ',next)))))
        (defvar *nodes*
          (let ((nodes (loop for i below 24 collect (make-instance 'saved-node :id i))))
            (loop for (a b) on nodes do (setf (slot-value a 'next) (or b (second nodes))))
            nodes))
        (defmacro saved-nodes () (list 'quote *nodes*)))
      (defun all-nodes () (saved-nodes))
      (fasl-gc)
      (defun same-nodes () (saved-nodes))"#,
    )
    .unwrap();
    let runtime = Runtime::start().unwrap();
    let register_gc = |rt: &Runtime| {
        rt.register_function("fasl-gc", |rt, _| {
            rt.collect();
            Ok(vec![])
        })
        .unwrap()
    };
    register_gc(&runtime);
    eval(
        &runtime,
        "(progn (defvar *phase*) (defvar *abort-load* nil))",
    );
    let compiled = temp_dir.path().join("externalize.zkclfasl");
    let compile = format!(
        "(compile-file #P\"{}\" :output-file #P\"{}\")",
        source.display(),
        compiled.display()
    );
    let aborted = eval(
        &runtime,
        &format!("(let ((*phase* :abort)) (catch 'abort-compile {compile}))"),
    );
    assert_eq!(runtime.symbol_name(&aborted[0]).as_deref(), Some("METHOD"));
    drop(aborted);
    let cycle = runtime
        .eval(&format!("(let ((*phase* :cycle)) {compile})"))
        .err()
        .expect("cyclic creation");
    assert!(cycle.to_string().contains("circular dependency"), "{cycle}");
    assert!(!compiled.exists());
    eval(&runtime, &format!("(let ((*phase* :compiling)) {compile})"));
    assert_eq!(eval(&runtime, "*calls*")[0].as_fixnum(), Some(24));
    fs::remove_file(&source).unwrap();
    drop(runtime);

    let fresh = Runtime::start().unwrap();
    register_gc(&fresh);
    eval(&fresh, "(progn (defvar *phase*) (defvar *abort-load* nil))");
    for abort in [true, false] {
        let result = eval(
            &fresh,
            &format!(
                "(let ((*phase* :loading) (*abort-load* {})) (catch 'abort-load (load #P\"{}\")))",
                if abort { "t" } else { "nil" },
                compiled.display()
            ),
        );
        if abort {
            assert_eq!(fresh.symbol_name(&result[0]).as_deref(), Some("CREATION"));
        }
    }
    let result = eval(
        &fresh,
        "
      (let ((nodes (all-nodes)))
        (and (eq nodes (same-nodes))
             (= (length *events*) 48)
             (loop for (a b) on nodes always
               (eq (slot-value a 'next) (or b (second nodes))))))",
    );
    assert!(!result[0].is_nil());
}
