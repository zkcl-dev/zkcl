use zkcl::Runtime;

#[test]
fn user_streams_use_the_clos_protocol_through_common_lisp_io() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(include_str!("common/gray_streams.lisp"))
        .unwrap();
}

mod common;

#[test]
fn gray_refills_can_change_reader_bindings_between_tokens() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"(require :zk-gray)
               (defclass changing-reader-input (zk-gray:fundamental-character-input-stream)
                 ((text :initform "(before 10 @after 10)") (position :initform 0)))
               (defmethod zk-gray:stream-read-char ((input changing-reader-input))
                 (with-slots (text position) input
                   (if (= position (length text)) :eof
                       (let ((character (char text position)))
                         (incf position)
                         (if (char= character #\@)
                             (progn
                               (setq *read-base* 16 *package* (find-package :keyword))
                               (setf (readtable-case *readtable*) :preserve)
                               #\Space)
                             character)))))
               (defmethod zk-gray:stream-unread-char ((input changing-reader-input) character)
                 (declare (ignore character))
                 (decf (slot-value input 'position)))
               (let ((*readtable* (copy-readtable nil))
                     (*read-base* 10) (*package* (find-package :cl-user)))
                 (let ((form (read (make-instance 'changing-reader-input))))
                   (assert (eq (first form) 'cl-user::before))
                   (assert (= (second form) 10))
                   (assert (eq (symbol-package (third form)) (find-package :keyword)))
                   (assert (string= (symbol-name (third form)) "after"))
                   (assert (= (fourth form) 16))))"#,
        )
        .unwrap();
}

#[test]
fn load_reads_a_compiled_file_from_gray_octets() {
    let dir = common::files::TempDir::new("gray-fasl");
    let source = dir.write("source.lisp", "(setq *gray-fasl-answer* 41)");
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(&format!(
            r#"
        (require :zk-gray)
        (defvar *gray-fasl-answer* nil)
        (defclass octet-source (zk-gray:fundamental-binary-input-stream)
          ((data :initarg :data) (index :initform 0)))
        (defmethod zk-gray:stream-read-byte ((s octet-source))
          (with-slots (data index) s
            (if (= index (length data)) :eof
                (prog1 (aref data index) (incf index)))))
        (let* ((compiled (compile-file "{path}"))
               (bytes (with-open-file (f compiled :element-type '(unsigned-byte 8))
                        (let ((v (make-array (file-length f)))) (read-sequence v f) v)))
               (s (make-instance 'octet-source :data bytes)))
          (assert (load (make-concatenated-stream s)))
          (assert (= *gray-fasl-answer* 41))
          (assert (open-stream-p s)))
        (let ((s (make-instance 'octet-source :data #(1 2 3))))
          (assert (handler-case (progn (load s) nil)
                    (stream-error (c) (eq s (stream-error-stream c))))))"#,
            path = source.display()
        ))
        .unwrap();
}
