//! Run explicitly against upstream source distributions, outside the zkcl checkout.
use std::path::{Path, PathBuf};
use zkcl::Runtime;

mod common;
use common::files::TempDir;
use common::lisp::lisp_path;

fn start_runtime(cache: &TempDir) -> Runtime {
    let rt = Runtime::start().unwrap();
    rt.eval("(require :asdf)").unwrap();
    rt.eval(&format!(
        "(asdf:initialize-output-translations
            (list :output-translations (list t (uiop:ensure-directory-pathname {}))
                  :ignore-inherited-configuration))",
        lisp_path(cache.path())
    ))
    .unwrap();
    rt
}

fn upstream_file(variable: &str, filename: &str) -> PathBuf {
    PathBuf::from(
        std::env::var_os(variable)
            .unwrap_or_else(|| panic!("set {variable} to the upstream source directory")),
    )
    .join(filename)
    .canonicalize()
    .unwrap()
}

fn load_asd(rt: &Runtime, path: &Path) {
    rt.eval(&format!("(asdf:load-asd #p{})", lisp_path(path)))
        .unwrap();
}

#[test]
#[ignore = "requires ZKCL_CLOSER_MOP_DIR with the original Closer to MOP source"]
fn cached_closer_mop_load_preserves_method_protocol() {
    let asd = upstream_file("ZKCL_CLOSER_MOP_DIR", "closer-mop.asd");
    let cache = TempDir::new("closer-mop-integration");
    for _ in 0..2 {
        let rt = start_runtime(&cache);
        load_asd(&rt, &asd);
        rt.eval("(asdf:load-system :closer-mop)").unwrap();
        rt.load_str(r#"
            (assert (member :closer-mop *features*))
            (assert (eq 'c2mop:compute-effective-method 'zk-mop:compute-effective-method))
            (defclass checked-method (standard-method) ())
            (c2cl:defgeneric ported (object) (:method-class checked-method))
            (let ((m (c2mop:ensure-method #'ported '(lambda (object) (values (1+ object) :library))
                                          :specializers (list (find-class 'integer)))))
              (assert (typep m 'checked-method))
              (assert (eq m (find-method #'ported nil (list (find-class 'integer)))))
              (assert (equal (multiple-value-list (ported 26)) '(27 :library)))
              (multiple-value-bind (form options)
                  (c2mop:compute-effective-method #'ported (c2mop:generic-function-method-combination #'ported) (list m))
                (assert (equal (multiple-value-list
                                 (funcall (c2mop:compute-effective-method-function #'ported form options) 30))
                               '(31 :library)))))
            (defvar *method-lookups* 0)
            (defmacro expand-call-method (method) `(call-method ,method))
            (let* ((m (find-method #'ported nil (list (find-class 'integer))))
                   (emf (c2mop:compute-effective-method-function #'ported
                          (list 'expand-call-method m) nil))
                   (hook (defmethod c2mop:compute-applicable-methods-using-classes :around
                             ((gf (eql #'ported)) classes)
                           (incf *method-lookups*)
                           (call-next-method))))
              (setq *method-lookups* 0)
              (assert (= (funcall emf 8) 9))
              (assert (plusp *method-lookups*))
              (remove-method #'c2mop:compute-applicable-methods-using-classes hook)
              (assert (= (funcall emf 9) 10)))
            (defvar *expansions* 0)
            (defmacro once-per-effective-function ()
              (incf *expansions*)
              '(values :compiled (load-time-value (list :shared))))
            (let ((entry (c2mop:compute-effective-method-function #'ported '(once-per-effective-function) nil)))
              (assert (= 1 *expansions*))
              (let ((first (multiple-value-list (funcall entry 1)))
                    (second (multiple-value-list (funcall entry 2))))
                (assert (equal first '(:compiled (:shared))))
                (assert (eq (second first) (second second)))
                (assert (= 1 *expansions*))))
            (assert (equal (c2mop:fix-slot-initargs '(:name x :tag a)) '(:name x :tag a)))
            "#).unwrap();
    }
}

#[test]
#[ignore = "requires ZKCL_MOP_FEATURES_DIR and ZKCL_LW_COMPAT_DIR at the verified upstream revisions"]
fn all_upstream_standard_features_are_acknowledged() {
    let asd = upstream_file("ZKCL_MOP_FEATURES_DIR", "mop-feature-tests.asd");
    let dir = asd.parent().unwrap();
    let cache = TempDir::new("mop-feature-integration");
    let rt = start_runtime(&cache);
    load_asd(&rt, &upstream_file("ZKCL_LW_COMPAT_DIR", "lw-compat.asd"));
    rt.eval("(asdf:load-system :lw-compat)").unwrap();
    load_asd(&rt, &asd);
    rt.load_file(&dir.join("mop-features-packages.lisp"))
        .unwrap();
    // The driver's implementation table predates zkcl. Supply its package
    // name; the upstream discovery, assertions and 28 test files are unchanged.
    let driver = std::fs::read_to_string(dir.join("mop-feature-tests.lisp")).unwrap();
    let parameter = "(defparameter *mop-package-name*";
    assert_eq!(driver.matches(parameter).count(), 1);
    rt.load_str(&driver.replacen(
        parameter,
        "(defparameter *mop-package-name* #+zkcl \"ZK-MOP\"",
        1,
    ))
    .unwrap();
    rt.load_str(
        "(in-package :mop-feature-tests)
                 (assert (run-feature-tests \"ZK-MOP\"))
                 (assert (= 247 (length *mop-known-standard-features*)))
                 (assert (null (set-difference *mop-known-standard-features* *mop-features*)))",
    )
    .unwrap();
}
