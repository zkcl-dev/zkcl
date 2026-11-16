(defpackage #:zk-syntax
  (:use #:cl)
  (:export #:parse-source #:make-workspace #:set-readtable #:set-document #:replace-text #:remove-document
           #:snapshot #:snapshot-equal-p #:text #:root #:node-at #:kind #:range #:node-text
           #:node-snapshot #:node-equal-p
           #:parent #:children #:forms #:previous-sibling #:next-sibling
           #:diagnostics #:byte-offset #:character-index #:selection-ranges
           #:move-cursor #:insert-delimiter
           #:structural-edit #:make-edit #:edit-source #:edit-changes #:edit-selection
           #:apply-edit #:apply-edit-to-document
           #:edit-error #:edit-error-reason #:edit-error-message
           #:workspacep #:snapshotp #:nodep #:editp))

(in-package #:zk-syntax)

(define-condition edit-error (error)
  ((reason :initarg :reason :reader edit-error-reason)
   (message :initarg :message :reader edit-error-message))
  (:documentation "A structural edit refusal, invalid edit range, or stale source edit.")
  (:report (lambda (condition stream)
             (write-string (edit-error-message condition) stream))))

(defun %checked-edit-result (value reason message)
  (if reason
      (error 'edit-error :reason reason :message message)
      value))

(defun parse-source (text &key (readtable :standard))
  "Return an immutable source snapshot of TEXT without creating a workspace.
READTABLE is :STANDARD or :RAW-STRINGS. No reader macro is executed."
  (zk::%syntax-parse-source text readtable))

(defun make-workspace (&key (readtable :standard))
  "Create a source workspace. READTABLE is :STANDARD or :RAW-STRINGS.
Source analysis preserves incomplete input and never executes reader macros."
  (zk::%syntax-make-workspace readtable))

(defun set-readtable (workspace readtable)
  "Select :STANDARD or :RAW-STRINGS for WORKSPACE; return whether it changed.
Existing snapshots remain immutable. Source readtable directives may override
the initial readtable for following forms."
  (zk::%syntax-set-readtable workspace readtable))

(defun set-document (workspace name text)
  "Install string TEXT under string NAME and return its immutable snapshot.
A byte-identical update preserves the current snapshot identity."
  (zk::%syntax-set-document workspace name text))

(defun replace-text (workspace name start end replacement)
  "Replace the half-open UTF-8 byte range [START, END) in document NAME.
Both endpoints must be character boundaries. Return the current immutable
snapshot after the replacement; byte-identical changes preserve its identity."
  (zk::%syntax-replace-text workspace name start end replacement))

(defun remove-document (workspace name)
  "Remove NAME from WORKSPACE and return true if it existed.
Previously returned snapshots, nodes, and edits remain usable."
  (zk::%syntax-remove-document workspace name))

(defun snapshot (workspace name)
  "Return the current immutable snapshot for NAME, or NIL if absent."
  (zk::%syntax-snapshot workspace name))

(defun snapshot-equal-p (left right)
  "Return true when LEFT and RIGHT denote the same immutable source snapshot.
Lisp EQ compares wrapper handles; this predicate compares source identity."
  (zk::%syntax-snapshot-equal-p left right))

(defun text (snapshot)
  "Return the original source text of SNAPSHOT as a fresh string."
  (zk::%syntax-text snapshot))

(defun root (snapshot)
  "Return the lossless source-file node of SNAPSHOT."
  (zk::%syntax-root snapshot))

(defun node-at (snapshot byte)
  "Return the token touching UTF-8 BYTE, including trivia, or NIL for empty text.
At a boundary prefer the token on the right; at end of source use the last
token. BYTE must be within the source and on a character boundary."
  (zk::%syntax-node-at snapshot byte))

(defun kind (node)
  "Return NODE's syntax kind as a keyword, such as :LIST or :LINE-COMMENT."
  (zk::%syntax-kind node))

(defun range (node)
  "Return NODE's half-open UTF-8 byte range as two values, start and end."
  (zk::%syntax-range node))

(defun node-text (node)
  "Return NODE's exact original spelling, including retained whitespace."
  (zk::%syntax-node-text node))

(defun node-snapshot (node)
  "Return the immutable source snapshot retained by NODE."
  (zk::%syntax-node-snapshot node))

(defun node-equal-p (left right)
  "Return true when LEFT and RIGHT have the same snapshot, kind, and byte range.
Use this predicate to compare nodes obtained through separate tree queries."
  (zk::%syntax-node-equal-p left right))

(defun parent (node)
  "Return NODE's enclosing syntax node, or NIL for the source-file root."
  (zk::%syntax-parent node))

(defun children (node)
  "Return NODE's direct lossless children, including delimiters and comments.
A token has no children. Every returned node keeps its snapshot alive."
  (zk::%syntax-children node))

(defun forms (node)
  "Return NODE's direct source-form children, excluding trivia and delimiters.
The dot in an improper list is excluded. Incomplete forms and recovery nodes
are included; a token has no children."
  (zk::%syntax-forms node))

(defun previous-sibling (node)
  "Return the preceding source-form sibling of NODE, skipping trivia and delimiters."
  (zk::%syntax-previous-sibling node))

(defun next-sibling (node)
  "Return the following source-form sibling of NODE, skipping trivia and delimiters."
  (zk::%syntax-next-sibling node))

(defun diagnostics (snapshot)
  "Return structural diagnostic plists for SNAPSHOT in source order.
Each record has :KIND, :START, :END, :MESSAGE, and :NEEDS-MORE-INPUT.
START and END delimit a half-open UTF-8 byte range."
  (zk::%syntax-diagnostics snapshot))

(defun byte-offset (snapshot character-index)
  "Convert a zero-based Lisp character index to a UTF-8 byte offset.
The index may equal the character count to designate end of source."
  (zk::%syntax-byte-offset snapshot character-index))

(defun character-index (snapshot byte)
  "Convert a UTF-8 byte offset to a zero-based Lisp character index.
Reject offsets outside the source or inside a multibyte character."
  (zk::%syntax-character-index snapshot byte))

(defun selection-ranges (snapshot byte)
  "Return enclosing (START END) byte ranges, innermost first, at UTF-8 BYTE."
  (zk::%syntax-selection-ranges snapshot byte))

(defun move-cursor (snapshot byte motion)
  "Return a UTF-8 byte offset in SNAPSHOT, or NIL if MOTION has no target.
MOTION is :FORWARD-FORM, :BACKWARD-FORM, :UP-LIST, :BACKWARD-UP-LIST,
or :DOWN-LIST. No text is changed."
  (multiple-value-bind (value reason message)
      (zk::%syntax-move-cursor snapshot byte motion)
    (%checked-edit-result value reason message)))

(defun insert-delimiter (snapshot anchor head character)
  "Create a text or caret edit for an explicit (, ), or double quote command."
  (multiple-value-bind (value reason message)
      (zk::%syntax-insert-delimiter snapshot anchor head character)
    (%checked-edit-result value reason message)))

(defun structural-edit (source anchor head operation)
  "Create a structural edit of a directed UTF-8 byte selection in SOURCE.
ANCHOR and HEAD may be reversed; equal endpoints designate a caret.
OPERATION is :WRAP, :SPLICE, :RAISE, :SLURP-FORWARD, :SLURP-BACKWARD,
:BARF-FORWARD, :BARF-BACKWARD, :SPLIT, :JOIN-FORWARD, :JOIN-BACKWARD,
:TRANSPOSE-FORWARD, :TRANSPOSE-BACKWARD, :DELETE-FORWARD, or
:DELETE-BACKWARD, :INDENT, or :INFER-PARENS.
Return an immutable edit without changing SOURCE or its workspace."
  (multiple-value-bind (value reason message)
      (zk::%syntax-structural-edit source anchor head operation)
    (%checked-edit-result value reason message)))

(defun make-edit (source changes anchor head)
  "Create a custom immutable edit against SOURCE.
CHANGES is a proper list of (START END REPLACEMENT) triples in source byte
coordinates, in ascending source order. Ranges must be disjoint and lie on
UTF-8 character boundaries.
ANCHOR and HEAD select the resulting edited text, preserve direction, and
must be valid UTF-8 boundaries in that result. This function does not parse
the source, so it also supports ordinary text and comment editing."
  (multiple-value-bind (value reason message)
      (zk::%syntax-make-edit source changes anchor head)
    (%checked-edit-result value reason message)))

(defun edit-source (edit)
  "Return the immutable source snapshot retained by EDIT."
  (zk::%syntax-edit-source edit))

(defun edit-changes (edit)
  "Return EDIT's changes as (START END REPLACEMENT) lists in source coordinates.
START and END are half-open UTF-8 byte offsets in EDIT-SOURCE."
  (zk::%syntax-edit-changes edit))

(defun edit-selection (edit)
  "Return the resulting selection's anchor and head as two UTF-8 byte offsets.
The offsets refer to the edited text returned by APPLY-EDIT. A reversed
selection retains its direction."
  (zk::%syntax-edit-selection edit))

(defun apply-edit (edit)
  "Apply EDIT to its retained source and return a new string."
  (multiple-value-bind (value reason message)
      (zk::%syntax-apply-edit edit)
    (%checked-edit-result value reason message)))

(defun apply-edit-to-document (workspace name edit)
  "Apply EDIT to current document NAME and return its new immutable snapshot.
Signal EDIT-ERROR with reason :UNKNOWN-DOCUMENT if NAME is absent, or
:STALE-SNAPSHOT if its current snapshot differs from EDIT's source.
No document is changed when the edit is stale."
  (multiple-value-bind (value reason message)
      (zk::%syntax-apply-edit-to-document workspace name edit)
    (%checked-edit-result value reason message)))

(defun workspacep (value)
  "Return true if VALUE is a source workspace belonging to this runtime."
  (zk::%syntax-workspacep value))

(defun snapshotp (value)
  "Return true if VALUE is a source snapshot belonging to this runtime."
  (zk::%syntax-snapshotp value))

(defun nodep (value)
  "Return true if VALUE is a snapshot-owning syntax node or token."
  (zk::%syntax-nodep value))

(defun editp (value)
  "Return true if VALUE is an immutable source edit."
  (zk::%syntax-editp value))

(provide :zk-syntax)
