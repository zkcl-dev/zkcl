use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Fixture(PathBuf);

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn success(output: Output) {
    assert!(
        output.status.success(),
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn rust_emitted_method_combinations_run_without_string_code_generation() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let fixture = Fixture(
        workspace
            .join(".agents/tmp/zkcl/js-method-combinations")
            .join(format!("fixture-{}", std::process::id())),
    );
    fs::create_dir_all(&fixture.0).unwrap();
    let source = fixture.0.join("combinations.lisp");
    let module = fixture.0.join("combinations.mjs");
    let checks = fixture.0.join("checks.mjs");
    fs::write(
        &source,
        r#"
(define-method-combination keep :operator list :identity-with-one-argument t)
(defgeneric retain (value) (:method-combination keep))
(defmethod retain keep (value) (values value 42))

(define-method-combination ordered (&optional (direction :most-specific-first))
    ((around (:around)) (primary (:run . *) :required t :order direction))
  (:arguments &whole arguments value &optional optional &key key)
  (let ((body `(values ,arguments ,value ,optional ,key
                       (list ,@(mapcar (lambda (method) `(call-method ,method)) primary)))))
    `(call-method ,(first around) ((make-method ,body)))))
(defgeneric combine (value &optional optional &key key)
  (:method-combination ordered :most-specific-last))
(defmethod combine :around (value &optional optional &key key)
  (declare (ignore optional key))
  (call-next-method (1+ value) 30 :key 50))
(defmethod combine :run ((value number) &optional optional &key key)
  (declare (ignore optional key))
  value)
(defmethod combine :run 7 ((value integer) &optional optional &key key)
  (declare (ignore value optional key))
  7)
(defun answer () (multiple-value-call #'values (retain 41) (combine 10)))
"#,
    )
    .unwrap();
    success(
        Command::new(env!("CARGO_BIN_EXE_zkcl"))
            .arg("build")
            .arg(&source)
            .args(["--target", "js", "-o"])
            .arg(&module)
            .output()
            .unwrap(),
    );
    fs::write(
        &checks,
        r#"
import assert from 'node:assert/strict';
import { instantiate } from './combinations.mjs';
const runtime = instantiate();
const [short, extra, actual, value, optional, key, methodValues] = runtime.call('ANSWER').values;
assert.deepEqual([short, extra, value, optional, key], [41, 42, 11, 30, 50]);
assert.equal(runtime.print(actual), '(11 30 :KEY 50)');
assert.equal(runtime.print(methodValues), '(11 7)');
"#,
    )
    .unwrap();
    success(
        Command::new("node")
            .arg("--disallow-code-generation-from-strings")
            .arg(&checks)
            .output()
            .unwrap(),
    );
}
