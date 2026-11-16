//! Call Lisp from Rust and let Lisp call back for a UTF-8 byte count.

use std::error::Error;
use zkcl::Runtime;

fn main() -> Result<(), Box<dyn Error>> {
    let runtime = Runtime::start()?;
    runtime.load_str("(defpackage #:text-metrics (:use #:cl) (:export #:measure-text))")?;

    runtime.register_function("TEXT-METRICS::UTF8-LENGTH", |runtime, args| {
        let [text] = args else {
            return Err("utf8-length expects one argument".into());
        };
        let text = text
            .as_string(runtime)
            .ok_or("utf8-length expects a string")?;
        Ok(vec![runtime.integer_from(text.len())?])
    })?;

    runtime.load_str(
        r#"
        (in-package #:text-metrics)

        (defun measure-text (text)
          (values (length text) (utf8-length text)))
        "#,
    )?;

    let measure = runtime
        .lookup_symbol("TEXT-METRICS", "MEASURE-TEXT")?
        .ok_or("missing Lisp function TEXT-METRICS:MEASURE-TEXT")?;

    for text in ["Lisp", "λ😀"] {
        let argument = runtime.make_string(text)?;
        // The call preserves both values from Lisp, in their original order.
        let values = runtime.call(&measure, &[&argument])?;
        let [characters, bytes] = values.as_slice() else {
            return Err("measure-text must return character and byte counts".into());
        };
        let characters = characters.as_i64().ok_or("expected a character count")?;
        let bytes = bytes.as_i64().ok_or("expected a byte count")?;
        println!("{text:?} => {characters} characters, {bytes} UTF-8 bytes");
    }

    Ok(())
}
