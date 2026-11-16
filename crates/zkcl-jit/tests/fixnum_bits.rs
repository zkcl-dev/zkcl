#![cfg(feature = "cranelift")]

use std::sync::Arc;
use std::time::{Duration, Instant};

use zkcl_jit::CraneliftJitBackend;
use zkcl_opt::{BitwiseOp, SsaOp, lift::FaslSpecializationInputs, lift_code, optimize};
use zkcl_runtime::{Runtime, SourceDocument};

const SOURCE: &str = "
(defvar *bit-inputs* 0)
(defun bit-kernel (x y)
  (logxor (logior (logand (progn (incf *bit-inputs*) x) y) 1) -2))
(defun ordinary-logand (x y)
  (declare (notinline logand))
  (logand x y))";

#[test]
fn fixnum_bitwise_native_keeps_bignums_errors_and_live_bindings() {
    let rt = Runtime::builder().jit_threshold(u32::MAX).start().unwrap();
    let file = rt
        .compile_document_to_bytecode(&SourceDocument::input(SOURCE))
        .unwrap();
    for (name, expected) in [
        (
            "BIT-KERNEL",
            &[BitwiseOp::And, BitwiseOp::Or, BitwiseOp::Xor][..],
        ),
        ("ORDINARY-LOGAND", &[][..]),
    ] {
        let code = file
            .code_entries()
            .find(|code| code.name.as_deref() == Some(name))
            .unwrap();
        let inputs = FaslSpecializationInputs::new(&file, code);
        let mut ssa = lift_code(code, &inputs).unwrap();
        optimize(&mut ssa, &inputs);
        let operations = ssa
            .blocks
            .iter()
            .flat_map(|block| &block.insts)
            .filter_map(|inst| match inst.op {
                SsaOp::BitwiseI64 { op, .. } => Some(op),
                _ => None,
            });
        assert_eq!(operations.collect::<Vec<_>>(), expected, "{name}");
    }

    rt.load_str(SOURCE).unwrap();
    rt.set_jit_backend(Arc::new(CraneliftJitBackend::new().unwrap()));
    let function = rt.eval("#'bit-kernel").unwrap().pop().unwrap();
    rt.eval("(compile nil #'bit-kernel)").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while rt.machine_code(&function).unwrap().is_none() {
        assert!(Instant::now() < deadline, "bitwise entry did not install");
        std::thread::sleep(Duration::from_millis(1));
    }

    rt.load_str(
        r#"
        (assert (= -11 (bit-kernel -6 11)))
        (let ((wide (ash 1 80)))
          (assert (= (1- (- wide)) (bit-kernel -1 wide))))
        (assert (eq :type-error
                    (handler-case (bit-kernel :bad 11)
                      (type-error () :type-error))))
        (let ((*trace-output* (make-string-output-stream)))
          (unwind-protect
              (progn
                (trace logand)
                (assert (= -11 (bit-kernel -6 11)))
                (assert (search "LOGAND" (get-output-stream-string *trace-output*))))
            (untrace logand)))
        (assert (= 4 *bit-inputs*))
        "#,
    )
    .unwrap();
}
