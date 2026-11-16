use std::cell::Cell;
use std::path::PathBuf;
use std::process::Command;

use zkcl::{EvalError, ImageHooks, Root, Runtime};

const CHILD_MODE: &str = "ZKCL_IMAGE_REGISTRY_CHILD";
const CHILD_PATH: &str = "ZKCL_IMAGE_REGISTRY_PATH";

#[test]
fn core_image_preserves_class_structure_and_method_registries() {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../.agents/tmp/zkcl/repl-completion/image-registries")
        .join(format!("registry-roundtrip-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("registry.core");
    for mode in ["save", "restore"] {
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "image_registry_child", "--nocapture"])
            .env(CHILD_MODE, mode)
            .env(CHILD_PATH, &path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{mode} failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let bytes = std::fs::read(&path).unwrap();
    let head = b"\x89ZKCLIMAGE".len();
    let metadata_len = u64::from_le_bytes(bytes[head..head + 8].try_into().unwrap()) as usize;
    let graph_start = head + 8 + metadata_len;
    // A late metadata error must leave the candidate's existing state usable.
    let mut trailing_metadata = bytes[..graph_start].to_vec();
    trailing_metadata.push(0);
    trailing_metadata.extend_from_slice(&bytes[graph_start..]);
    trailing_metadata[head..head + 8].copy_from_slice(&((metadata_len + 1) as u64).to_le_bytes());
    let mut oversized_metadata = bytes.clone();
    oversized_metadata[head..head + 8].copy_from_slice(&u64::MAX.to_le_bytes());
    for (name, corrupt) in [
        ("truncated", bytes[..bytes.len() - 1].to_vec()),
        ("metadata-length", oversized_metadata),
        ("metadata-trailing", trailing_metadata),
    ] {
        let corrupt_path = directory.join(format!("{name}.core"));
        std::fs::write(&corrupt_path, corrupt).unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "image_registry_child", "--nocapture"])
            .env(CHILD_MODE, "reject")
            .env(CHILD_PATH, &corrupt_path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{name} rejection failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let resource_path = directory.join("resource.core");
    for mode in ["hooks-save", "hooks-restore"] {
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "image_registry_child", "--nocapture"])
            .env(CHILD_MODE, mode)
            .env(CHILD_PATH, &resource_path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{mode} failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    std::fs::remove_dir_all(directory).unwrap();
}

// Each phase owns its heap in a separate process; no source definition is
// executed in the restore phase.
#[test]
fn image_registry_child() {
    let Ok(mode) = std::env::var(CHILD_MODE) else {
        return;
    };
    let path = PathBuf::from(std::env::var_os(CHILD_PATH).unwrap());
    let mut runtime = Runtime::start().unwrap();
    if mode == "reject" {
        runtime.load_str("(defparameter *image-candidate-sentinel* (list :before)) (defun image-candidate-function () 19)").unwrap();
        assert!(runtime.restore_image(&path).is_err());
        runtime.collect();
        runtime.load_str("(assert (equal *image-candidate-sentinel* '(:before))) (assert (= (image-candidate-function) 19)) (defun image-after-failed-restore () 23) (assert (= (image-after-failed-restore) 23))").unwrap();
    } else if mode == "hooks-save" {
        let resource = runtime.foreign(42_u32).unwrap();
        let setter=runtime.eval("(lambda (resource) (defparameter *image-resource* nil) (let ((cycle (list resource resource))) (setf (cddr cycle) cycle *image-resource* cycle)))").unwrap().pop().unwrap();
        runtime.call(&setter, &[&resource]).unwrap();
        let hooks = ResourceHooks::default();
        runtime.save_image_with_hooks(&path, &hooks).unwrap();
        assert_eq!(hooks.saved.get(), 1);
    } else if mode == "hooks-restore" {
        let hooks = ResourceHooks::default();
        runtime.restore_image_with_hooks(&path, &hooks).unwrap();
        assert_eq!(hooks.restored.get(), 1);
        runtime.collect();
        runtime.load_str("(assert (eq (first *image-resource*) (second *image-resource*))) (assert (eq (cddr *image-resource*) *image-resource*))").unwrap();
        let resource = runtime
            .eval("(first *image-resource*)")
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(
            resource
                .try_with_foreign_ref(&runtime, |value: &u32| *value)
                .unwrap(),
            42
        );
    } else if mode == "save" {
        runtime.load_str(SETUP).unwrap();
        runtime.collect();
        runtime.load_str(CHECK_DISPATCH).unwrap();
        runtime.save_image(&path).unwrap();
    } else {
        runtime.restore_image(&path).unwrap();
        runtime.load_str(CHECK_DISPATCH).unwrap();
        runtime.collect();
        runtime.load_str(VERIFY).unwrap();
        runtime.collect();
        runtime.load_str(VERIFY_AFTER_CHANGE).unwrap();
    }
}

#[derive(Default)]
struct ResourceHooks {
    saved: Cell<usize>,
    restored: Cell<usize>,
}
impl ImageHooks for ResourceHooks {
    fn save_external(
        &self,
        runtime: &Runtime,
        value: &Root<'_>,
    ) -> Result<Option<(String, Vec<u8>)>, EvalError> {
        let Ok(value) = value.try_with_foreign_ref(runtime, |value: &u32| *value) else {
            return Ok(None);
        };
        self.saved.set(self.saved.get() + 1);
        // Moving collections inside the hook must not split object identities.
        runtime.collect();
        Ok(Some((
            "image-test-u32".into(),
            value.to_le_bytes().to_vec(),
        )))
    }
    fn restore_external<'rt>(
        &self,
        runtime: &'rt Runtime,
        key: &str,
        data: &[u8],
    ) -> Result<Root<'rt>, EvalError> {
        assert_eq!(key, "image-test-u32");
        let value = u32::from_le_bytes(data.try_into().unwrap());
        self.restored.set(self.restored.get() + 1);
        runtime.collect();
        Ok(runtime.foreign(value).unwrap())
    }
}

const SETUP: &str = r#"
    (defparameter *image-shared-inits* 0)
    (defparameter *image-updates* 0)
    (defclass image-base ()
      ((shared :allocation :class
               :initform (progn (incf *image-shared-inits*) (list :shared)))))
    (defclass image-record (image-base)
      ((kept :initarg :kept :accessor image-kept)
       (discarded :initform :old)
       (unbound)))
    (defparameter *image-class* (find-class 'image-record))
    (defparameter *image-object* (make-instance 'image-record :kept (list :kept)))
    (defparameter *image-alias* *image-object*)
    (setf (slot-value *image-object* 'shared) (list *image-object*))
    (defparameter *image-shared* (slot-value *image-object* 'shared))
    (setf (find-class 'image-class-alias) *image-class*)
    (defclass image-hidden () ())
    (setf (find-class 'image-hidden) nil)
    (defmethod update-instance-for-redefined-class :after
        ((object image-record) added removed properties &rest initargs)
      (declare (ignore object added removed properties initargs))
      (incf *image-updates*))
    ;; Leave the instance on its old layout until its first restored access.
    (defclass image-record (image-base)
      ((kept :accessor image-kept)
       (added :initform :new)
       (unbound :initform :must-stay-unbound)))

    (defclass image-meta (standard-class)
      ((stamp :initform (list :stamp) :accessor image-stamp)))
    (defmethod zk-mop:validate-superclass
        ((class image-meta) (parent standard-class)) t)
    (defclass image-custom () ((data :initform :custom)) (:metaclass image-meta))
    (defparameter *image-custom-class* (find-class 'image-custom))
    (defparameter *image-stamp* (image-stamp *image-custom-class*))
    (defparameter *image-custom* (make-instance 'image-custom))
    (defparameter *image-custom-slot*
      (first (zk-mop:class-direct-slots *image-custom-class*)))

    (defstruct image-structure-parent (value 17) (locked 8 :read-only t))
    (defstruct (image-structure-child (:include image-structure-parent)) extra)
    (defstruct (image-list (:type list) :named) value)
    (defstruct (image-vector (:type vector) :named) value)
    (defparameter *image-structure* (make-image-structure-child :extra *image-object*))
    (defparameter *image-structure-class* (class-of *image-structure*))
    (defparameter *image-structure-slot*
      (first (zk-mop:class-slots *image-structure-class*)))
    (defparameter *image-list* (make-image-list :value *image-structure*))
    (defparameter *image-vector* (make-image-vector :value *image-list*))

    (define-method-combination image-sum :operator + :identity-with-one-argument t)
    (defgeneric image-sum-call (object) (:method-combination image-sum))
    (defmethod image-sum-call image-sum ((object integer)) 40)
    (defmethod image-sum-call image-sum ((object number)) 2)
    (define-method-combination image-order
        (&optional (order :most-specific-first))
      ((primary (image-order) :order order :required t))
      `(list ,@(mapcar (lambda (method) `(call-method ,method)) primary)))
    (defgeneric image-order-call (object)
      (:method-combination image-order :most-specific-last))
    (defmethod image-order-call image-order ((object integer)) :integer)
    (defmethod image-order-call image-order ((object number)) :number)
    (defgeneric image-dispatch (object))
    (defmethod image-dispatch ((object integer)) :integer)
    (defmethod image-dispatch ((object string)) :string)
    (defgeneric image-protocol-call (object))
    (defmethod image-protocol-call ((object image-record)) :record)
    (defmethod zk-mop:compute-effective-method
        ((generic (eql #'image-protocol-call)) combination methods)
      (declare (ignore generic combination))
      `(list :protocol (call-method ,(first methods))))
    (setf (logical-pathname-translations "IMAGE")
          '(("**;*.*.*" "/image-root/**/*.*")))
    (defparameter *image-logical* (logical-pathname "IMAGE:SRC;FILE.LISP.3"))
    (defparameter *image-finalizer-runs* 0)
    (defparameter *image-finalizer-target* (list :retained))
    (zk:finalize *image-finalizer-target* (lambda () (incf *image-finalizer-runs*)))
    (defparameter *image-inline-expansions* 0)
    (defmacro image-inline-macro (value)
      (incf *image-inline-expansions*) `(+ ,value 1))
    (declaim (inline image-inline-function))
    (defun image-inline-function (value) (image-inline-macro value))
    (defun image-inline-old-caller (value) (image-inline-function value))
    (defparameter *image-inline-saved-expansions* *image-inline-expansions*)
    (defparameter *image-ltv-runs* 0)
    (defun image-ltv-function ()
      (load-time-value (progn (incf *image-ltv-runs*) (list :load-time))))
    (defparameter *image-ltv-object* (image-ltv-function))
    (setf *image-ltv-runs* 37)
"#;

const VERIFY: &str = r#"
    (assert (eq *image-object* *image-alias*))
    (assert (eq *image-class* (find-class 'image-record)))
    (assert (eq *image-class* (find-class 'image-class-alias)))
    (assert (null (find-class 'image-hidden nil)))
    (assert (eq *image-class* (class-of *image-object*)))
    (assert (= *image-updates* 0))
    (assert (equal (image-kept *image-object*) '(:kept)))
    (assert (= *image-updates* 1))
    (assert (eq (slot-value *image-object* 'added) :new))
    (assert (not (slot-boundp *image-object* 'unbound)))
    (assert (not (slot-exists-p *image-object* 'discarded)))
    (assert (eq *image-shared* (slot-value *image-object* 'shared)))
    (assert (eq *image-object* (car *image-shared*)))
    (assert (eq *image-shared* (slot-value (make-instance 'image-base) 'shared)))
    (assert (= *image-shared-inits* 1))
    (assert (member *image-class*
                    (zk-mop:class-direct-subclasses (find-class 'image-base))))
    (assert (eq (class-of *image-custom-class*) (find-class 'image-meta)))
    (assert (eq *image-custom-class* (class-of *image-custom*)))
    (assert (eq *image-stamp* (image-stamp *image-custom-class*)))
    (assert (eq *image-custom-slot*
                (first (zk-mop:class-direct-slots *image-custom-class*))))
    (assert (eq (slot-value *image-custom* 'data) :custom))
    (assert (eq *image-structure-class* (class-of *image-structure*)))
    (assert (eq *image-structure-slot*
                (first (zk-mop:class-slots *image-structure-class*))))
    (assert (typep *image-structure* 'image-structure-parent))
    (assert (eq (image-structure-child-extra *image-structure*) *image-object*))
    (assert (= (image-structure-child-value *image-structure*) 17))
    (setf (image-structure-child-value *image-structure*) 19)
    (assert (= (image-structure-parent-value *image-structure*) 19))
    (assert (image-list-p *image-list*))
    (assert (image-vector-p *image-vector*))
    (assert (eq (image-list-value *image-list*) *image-structure*))
    (assert (eq (image-vector-value *image-vector*) *image-list*))
    (assert (= (image-sum-call 1) 42))
    (assert (equal (image-order-call 1) '(:number :integer)))
    (assert (equal (image-protocol-call *image-object*) '(:protocol :record)))
    (assert (equal (namestring (translate-logical-pathname *image-logical*))
                   "/image-root/src/file.lisp"))
    (assert (= (pathname-version *image-logical*) 3))
    (assert (= *image-finalizer-runs* 0))
    (setq *image-finalizer-target* nil)
    (zk:gc)
    (assert (= *image-finalizer-runs* 1))
    (assert (= *image-ltv-runs* 37))
    (assert (eq *image-ltv-object* (image-ltv-function)))
    (defmacro image-inline-macro (value)
      (incf *image-inline-expansions*) `(+ ,value 100))
    (defun image-inline-new-caller (value) (image-inline-function value))
    (assert (= *image-inline-expansions* *image-inline-saved-expansions*))
    (trace image-inline-function)
    (assert (equal (with-output-to-string (*trace-output*)
                     (assert (= (image-inline-old-caller 1) 2))
                     (assert (= (image-inline-new-caller 1) 2))) ""))
    (assert (plusp (length (with-output-to-string (*trace-output*)
                             (funcall (fdefinition 'image-inline-function) 1)))))
    (untrace image-inline-function)
"#;

const CHECK_DISPATCH: &str = r#"
    (dotimes (i 3)
      (assert (eq (image-dispatch 1) :integer))
      (assert (eq (image-dispatch "one") :string)))
"#;

const VERIFY_AFTER_CHANGE: &str = r#"
    ;; A restored registry must support subsequent publication and migration.
    (defclass image-record (image-base)
      ((kept :accessor image-kept) (added) (unbound) (later :initform 43)))
    (assert (= (slot-value *image-object* 'later) 43))
    (assert (= *image-updates* 2))
    (assert (eq *image-class* (find-class 'image-record)))
    (assert (eq *image-shared* (slot-value *image-object* 'shared)))
    (defclass image-custom () ((data) (later :initform 44)) (:metaclass image-meta))
    (assert (eq *image-custom-class* (find-class 'image-custom)))
    (assert (eq *image-stamp* (image-stamp *image-custom-class*)))
    (assert (= (slot-value *image-custom* 'later) 44))
    (assert (eq (slot-value *image-custom* 'data) :custom))
    (defmethod image-sum-call image-sum ((object integer)) 41)
    (assert (= (image-sum-call 1) 43))
    (assert (= (image-structure-child-value (copy-image-structure-child *image-structure*)) 19))
    (zk:gc)
    (assert (= *image-finalizer-runs* 1))
"#;
