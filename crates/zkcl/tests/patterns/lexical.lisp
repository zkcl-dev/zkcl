(in-package #:cl-user)

(assert (eq :long (zk-lex:lexmatch "==x" :longest-prefix
                    ("=" :short) ("==" :long) (_ :miss))))
(assert (eq :short (zk-lex:lexmatch "==x" :prefix
                     ("=" :short) ("==" :long) (_ :miss))))
(assert (eq :right-rule (zk-lex:lexmatch "a--b" :search
                          ("b" :right-rule) ("a" :left-rule) (_ :miss))))
(assert (eq :literal (zk-lex:lexmatch "a.b" ("a.b" :literal) (_ :miss))))
(assert (eq :miss (zk-lex:lexmatch "axb" ("a.b" :literal) (_ :miss))))
(assert (eq :regex (zk-lex:lexmatch "axb" ((:re "a.b") :regex) (_ :miss))))
(assert (eq :total (zk-lex:lexmatch (string #\Newline) ((:re ".*") :total))))

(assert (equal '(nil "")
               (zk-lex:lexmatch ""
                 ((:seq (:? (:capture missing "k")) (:capture empty (:* "k")))
                  (list missing empty))
                 (_ :no))))

(zk:defpattern unicode-name (name)
  `(:capture ,name (:seq (:property :xid-start) (:* (:property :xid-continue)))))

(defun token-result (input)
  (zk-lex:lexmatch input :longest-prefix
    ("if" :keyword)
    ((unicode-name name) (values :identifier name))
    (_ :miss)))

(assert (equal '(:identifier "if中") (multiple-value-list (token-result "if中!"))))
(assert (eq :keyword (token-result "if!")))
(assert (string= "12" (zk:match '(:field "id=12")
                         ((list :field (zk-lex:lex (:seq "id=" (:capture digits (:re "[0-9]+"))))) digits)
                         (_ ""))))

(let* ((original (copy-seq "abc!"))
       (outer (zk-text:snapshot original))
       (region (zk-text:span-slice outer 1 4))
       (tail (zk-lex:lexmatch region :prefix ((:with "bc" :after (rest :as :span)) rest) (_ nil))))
  (setf (char original 3) #\?)
  (zk:gc)
  (assert (eq (zk-text:span-source tail) (zk-text:span-source outer)))
  (assert (= (zk-text:span-start tail) 3))
  (assert (= (zk-text:span-end tail) 4))
  (assert (string= (zk-text:span-value tail) "!")))

(defun scan-frame-head (scanner)
  (zk-lex:lexscan (:bytes scanner)
    ((:seq (:octets #x6c #x65 #x6e #x3a) (:capture digits (:re "[0-9]+")) (:octets #x0a))
     (let ((length 0))
       (loop for digit across digits do (setf length (+ (* length 10) (- digit #x30))))
       length))
    ((:eof) :eof)
    ((:need-more) :need-more)
    (_ :invalid)))

(defun scripted-chunks (steps)
  (lambda ()
    (if steps
        (let ((step (pop steps)))
          (if (keywordp step) (values nil step) (values step :chunk)))
        (values nil :eof))))

(let* ((scanner (zk-lex:make-lexscanner
                 (scripted-chunks (list (zk-text:utf8-encode (format nil "len:3~%A"))
                                        :need-more (zk-text:utf8-encode "BC!")))
                 :domain :bytes)))
  (assert (= 3 (scan-frame-head scanner)))
  (assert (= 6 (zk-lex:scan-position scanner)))
  (multiple-value-bind (status span count) (zk-lex:scan-take scanner 0)
    (assert (and (eq status :ok) (zerop count) (= (zk-text:span-start span) 6) (= (zk-text:span-end span) 6))))
  (assert (equal '(:need-more nil 1) (multiple-value-list (zk-lex:scan-take scanner 3))))
  (assert (= 6 (zk-lex:scan-position scanner)))
  (multiple-value-bind (status payload count) (zk-lex:scan-take scanner 3)
    (assert (eq :ok status))
    (assert (= count 3))
    (assert (= 6 (zk-text:span-start payload)))
    (assert (= 9 (zk-text:span-end payload)))
    (assert (string= "ABC" (zk-text:utf8-decode payload)))
    (let ((decoded (zk-text:utf8-decode payload :as :span)))
      (assert (equal (list (zk-text:span-source payload) 6 7)
                     (multiple-value-list (zk-text:span-origin-byte-range (zk-text:span-slice decoded 0 1)))))))
  (assert (= 9 (zk-lex:scan-position scanner)))
  (assert (eq :punctuation (zk-lex:lexscan (:bytes scanner) ((:octets 33) :punctuation) (_ :miss)))))

(let ((scanner (zk-lex:make-lexscanner (zk-text:utf8-encode (format nil "len:3~%A")) :domain :bytes)))
  (assert (= 3 (scan-frame-head scanner)))
  (assert (equal '(:truncated nil 1) (multiple-value-list (zk-lex:scan-take scanner 3))))
  (assert (= 6 (zk-lex:scan-position scanner)))
  (assert (eq :still-buffered (zk-lex:lexscan (:bytes scanner) ((:octets 65) :still-buffered) (_ :miss)))))

(defun scan-unicode-token (scanner)
  (zk-lex:lexscan scanner
    ((:capture token "if" :as :span) (list :keyword token))
    ((:capture token (:seq (:property :xid-start) (:* (:property :xid-continue))) :as :span)
     (list :name token))
    ((:capture token "=" :as :span) (list :assign token))
    ((:capture token "==" :as :span) (list :equals token))
    ((:capture token (:re ".") :as :span) (list :bad token))
    ((:eof) :eof)
    ((:need-more) :need-more)))

(defun unicode-token-trace (chunks)
  (let* ((decoder (zk-text:utf8-source (scripted-chunks chunks) :on-error :replace))
         (scanner (zk-lex:make-lexscanner decoder))
         (tokens nil))
    (loop for result = (scan-unicode-token scanner) until (eq :eof result) do
      (unless (eq result :need-more) (push result tokens)))
    (zk:gc)
    (loop for (kind span) in (nreverse tokens) collect
      (multiple-value-bind (source low high) (zk-text:span-origin-byte-range span)
        (assert source)
        (list kind (zk-text:span-value span) (zk-text:span-start span) (zk-text:span-end span) low high)))))

(let ((input (zk-bits:octets #x69 #x66 #xe4 #xb8 #xad #x3d #x3d #xe1 #x80 #x41))
      (expected '((:name "if中" 0 3 0 5) (:equals "==" 3 5 5 7)
                  (:bad "�" 5 6 7 9) (:name "A" 6 7 9 10))))
  (assert (equal expected (unicode-token-trace (list input))))
  (assert (equal expected (unicode-token-trace (map 'list #'zk-bits:octets input))))
  (loop for split from 1 below (length input) do
    (assert (equal expected (unicode-token-trace (list (subseq input 0 split) :need-more (subseq input split)))))))

(let ((source (zk-text:utf8-source (scripted-chunks (list (zk-bits:octets #x41 #xe1 #x80) :need-more (zk-bits:octets #x41))))))
  (multiple-value-bind (span status) (funcall source)
    (assert (eq :chunk status))
    (assert (string= "A" (zk-text:span-value span))))
  (assert (equal '(nil :need-more) (multiple-value-list (funcall source))))
  (dotimes (attempt 2)
    (assert (equal '(1 3 (225 128))
             (handler-case (funcall source)
               (zk-text:unicode-decode-error (condition)
                 (let ((span (zk-text:unicode-decode-error-span condition)))
                   (list (zk-text:span-start span) (zk-text:span-end span) (coerce (zk-text:span-value span) 'list)))))))))

(let ((scanner (zk-lex:make-lexscanner "ab")))
  (assert (eq :first (zk-lex:lexscan scanner ("a" :first) (_ :miss))))
  (assert (eq :token-origin (zk-lex:lexscan scanner
                             ((:re "\\bb") :word-origin)
                             ((:re "^b") :token-origin)
                             (_ :miss)))))

(let ((scanner (zk-lex:make-lexscanner (scripted-chunks (list "e" :need-more "́!")))))
  (flet ((next () (zk-lex:lexscan scanner
                    ((:capture cluster (:re "\\X")) cluster)
                    ((:need-more) :need-more)
                    (_ :miss))))
    (assert (eq :need-more (next)))
    (assert (string= "é" (next)))
    (assert (string= "!" (next)))))

(let ((scanner (zk-lex:make-lexscanner (scripted-chunks (list "x" :need-more)))))
  (flet ((next ()
           (zk-lex:lexscan scanner
             ((:or (:seq (:capture at-end "x") (:re "$")) (:capture otherwise "x"))
              (list at-end otherwise))
             ((:need-more) :need-more))))
    (assert (eq :need-more (next)))
    (assert (zerop (zk-lex:scan-position scanner)))
    (assert (equal '("x" nil) (next)))))

(let ((scanner (zk-lex:make-lexscanner (scripted-chunks (list "x" :need-more "y")))))
  (flet ((next ()
           (zk-lex:lexscan scanner :prefix
             ((:seq "x" (:or (:seq (:re "\\b") (:capture boundary ""))
                             (:seq (:re "\\B") (:capture interior ""))))
              (list boundary interior))
             ((:need-more) :need-more))))
    (assert (eq :need-more (next)))
    (assert (zerop (zk-lex:scan-position scanner)))
    (assert (equal '(nil "") (next)))
    (assert (= 1 (zk-lex:scan-position scanner)))))

(assert (string= "é" (zk-text:normalize "é" :nfc)))
(assert (string= "strasse σσ" (zk-text:case-fold "Straße Σς")))
(assert (zk-text:unicode-property-p #\中 :xid-start))
(assert (equal '("A" "中" "😀" "é")
               (map 'list #'zk-text:span-value (zk-text:segment-text "A中😀é" :grapheme))))

;; Plan switching on :need-more preserves buffered lookahead and discards old pending state.
(let* ((scanner (zk-lex:make-lexscanner (scripted-chunks (list "=" :need-more "=x"))))
       (plan-long (zk-lex:compile-lex-rules '("==" "=") :mode :longest-prefix))
       (plan-short (zk-lex:compile-lex-rules '("=" "==") :mode :prefix)))
  (assert (eq :need-more (zk-lex:scan-step scanner plan-long)))
  (multiple-value-bind (status match) (zk-lex:scan-step scanner plan-short)
    (assert (eq :match status))
    (assert (string= "=" (zk-text:span-value (zk-lex:lex-match-span match))))
    (assert (= 1 (zk-lex:scan-position scanner))))
  (multiple-value-bind (status match) (zk-lex:scan-step scanner plan-short)
    (assert (eq :match status))
    (assert (string= "=" (zk-text:span-value (zk-lex:lex-match-span match))))
    (assert (= 2 (zk-lex:scan-position scanner)))))

;; Zero-length :bytes requires byte alignment, and (zk-lex:lex ...) inside OR preserves bindings.
(let ((unaligned (zk:match (zk-bits:octets #xab) ((zk-bits:bits (u4be #xa) (:rest-bits r)) r) (_ nil))))
  (assert (eq :miss (zk:match unaligned ((zk-bits:bits (:bytes 0 _) (:rest-bits _)) :bad) (_ :miss))))
  (multiple-value-bind (tag bytes)
      (zk:match (zk-bits:octets #x10)
        ((zk-bits:bits (u8be tag) (:bytes 0 empty)) (values tag empty))
        (_ nil))
    (assert (eql tag 16))
    (assert (typep bytes '(vector (unsigned-byte 8) 0)))))

(assert (equal '("12")
               (list (zk:match "id=12"
                       ((or (zk-lex:lex (:seq "num=" (:capture v (:re "[0-9]+"))))
                            (zk-lex:lex (:seq "id=" (:capture v (:re "[0-9]+")))))
                        v)
                       (_ nil)))))

;; Strict utf8-decode on a byte span preserves absolute origin coordinates on error.
(let* ((frame (zk-text:snapshot (zk-bits:octets #x00 #x41 #xe1 #x80 #x42)))
       (payload (zk-text:span-slice frame 1 5)))
  (handler-case (progn (zk-text:utf8-decode payload :on-error :error :as :span) (error "expected error"))
    (zk-text:unicode-decode-error (condition)
      (let ((span (zk-text:unicode-decode-error-span condition)))
        (assert (eq (zk-text:span-source span) (zk-text:span-source frame)))
        (assert (= 2 (zk-text:span-start span)))
        (assert (= 4 (zk-text:span-end span)))))))

;; :start / :end evaluate in source order once, and :unit / :value / :span captures agree.
(let ((order nil))
  (assert (equal '(#\b "cd")
                 (zk-lex:lexmatch (:text (progn (push :input order) "abcdef")
                               :end (progn (push :end order) 4)
                               :start (progn (push :start order) 1))
                   ((:seq (:capture head (:re ".") :as :unit)
                          (:capture tail (:re "[a-z]+")))
                    (list head tail))
                   (_ nil))))
  (assert (equal '(:start :end :input) order)))

;; Pure literal and prefix-trie specialization matches reference semantics across modes and domains.
(assert (eq :mid (zk-lex:lexmatch "ab" :prefix ("abc" :long) ("a" :mid) ("ab" :two) (_ :miss))))
(assert (eq :two (zk-lex:lexmatch "ab" :longest-prefix ("abc" :long) ("a" :mid) ("ab" :two) (_ :miss))))
(assert (eq :else (zk-lex:lexmatch "else" :full ("if" :if) ("else" :else) (_ :miss))))
(assert (eq :eq (zk-lex:lexmatch (:bytes (zk-bits:octets #x3d #x3d #x78)) :longest-prefix
                  ((:octets #x3d) :assign)
                  ((:octets #x3d #x3d) :eq)
                  (_ :miss))))
(assert (string= "=x"
                 (zk-lex:lexmatch "===x" :longest-prefix
                   ((:with "=" :after tail) tail)
                   ((:with "==" :after tail) tail)
                   (_ ""))))
(assert (string= "!" (zk-lex:lexmatch "ab!" :prefix
                       ((:with (:or "ab" "a") :after tail) tail)
                       (_ nil))))
(assert (string= "b!" (zk-lex:lexmatch "ab!" :prefix
                        ((:with (:or "a" "ab") :after tail) tail)
                        (_ nil))))
(assert (zk-lex:lexmatch "ab!" :prefix
          ((:with "ab" :before (before :as :span) :after (after :as :span))
           (eq (zk-text:span-source before) (zk-text:span-source after)))
          (_ nil)))
(assert (string= "!" (zk-lex:lexmatch (zk-text:snapshot "ab!") :prefix
                       ((:with "ab" :after tail) tail)
                       (_ nil))))

(let* ((input (zk-bits:octets #xff 97 98 33 #xff))
       (view (make-array 4 :element-type '(unsigned-byte 8) :displaced-to input
                         :displaced-index-offset 1 :fill-pointer 3))
       (plan (zk-lex:compile-lex-rules '((:octets 98)) :domain :bytes :mode :prefix))
       (result (zk-lex:run-lex-rules plan view :start 1)))
  (assert (= 2 (zk-text:span-end (zk-lex:lex-match-span result))))
  (assert (equal '(98) (coerce (zk-text:span-value (zk-lex:lex-match-span result)) 'list))))

;; Async channel-backed scanners suspend until chunks arrive and handle mixed/UTF-8 streams.
(let ((tokens nil))
  (zk-async:with-task-group (group)
    (let ((ch (zk-async:make-channel)))
      (zk-async:spawn-task (group)
        (zk-async:channel-send ch (zk-bits:octets #x69 #x66 #xe4))
        (zk-async:yield)
        (zk-async:channel-send ch (zk-bits:octets #xb8 #xad #x3d #x3d))
        (zk-async:close-channel ch))
      (let ((scanner (zk-lex:make-lexscanner (zk-text:utf8-source ch))))
        (loop
          (let ((tok (zk-lex:lexscan scanner
                       ((:capture w "if" :as :span) (list :kw (zk-text:span-value w)))
                       ((:capture w (:seq (:property :xid-start) (:* (:property :xid-continue))) :as :span)
                        (list :id (zk-text:span-value w) (zk-text:span-start w) (zk-text:span-end w)))
                       ((:capture w "==" :as :span) (list :eq (zk-text:span-value w)))
                       ((:eof) :eof))))
            (when (eq tok :eof) (return))
            (push tok tokens))))))
  (assert (equal '((:id "if中" 0 3) (:eq "==")) (nreverse tokens))))
