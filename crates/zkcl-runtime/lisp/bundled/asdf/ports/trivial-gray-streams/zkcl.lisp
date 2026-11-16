(in-package #:trivial-gray-streams)

(defmethod zk-gray:stream-read-sequence ((s fundamental-input-stream) seq start end)
  (or-fallback (stream-read-sequence s seq start end)))

(defmethod zk-gray:stream-write-sequence ((s fundamental-output-stream) seq start end)
  (or-fallback (stream-write-sequence s seq start end)))

(defmethod zk-gray:stream-file-position ((s fundamental-stream) &optional position)
  (if position (setf (stream-file-position s) position) (stream-file-position s)))
