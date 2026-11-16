use zkcl::{Root, Runtime};

fn source(runtime: &Runtime, value: &Root<'_>) -> String {
    if let Some(integer) = value.as_fixnum() {
        return integer.to_string();
    }
    if let Some(name) = runtime.symbol_name(value) {
        return name;
    }
    let mut tail = value.clone();
    let mut elements = Vec::new();
    while let Some((car, cdr)) = runtime.cons_fields(&tail) {
        elements.push(source(runtime, &car));
        tail = cdr;
    }
    assert!(!elements.is_empty());
    let body = elements.join(" ");
    if tail.is_nil() {
        format!("({body})")
    } else {
        format!("({body} . {})", source(runtime, &tail))
    }
}

#[test]
fn quasiquote_evaluates_splices_in_template_order() {
    let runtime = Runtime::start().unwrap();
    let result = runtime
        .eval("(let ((n 0)) (values `(,(setq n 1) ,@(list (setq n 2) (setq n 3)) ,.(list (setq n 4)) . ,(setq n 5)) n))")
        .unwrap();
    assert_eq!(source(&runtime, &result[0]), "(1 2 3 4 . 5)");
    assert_eq!(result[1].as_fixnum(), Some(5));
}

#[test]
fn nested_quasiquote_preserves_pending_unquotes() {
    let runtime = Runtime::start().unwrap();
    let result = runtime.eval("(let ((x 17)) ``(value ,,x))").unwrap();
    let expansion = source(&runtime, &result[0]);
    let result = runtime.eval(&expansion).unwrap();
    assert_eq!(source(&runtime, &result[0]), "(VALUE 17)");

    let result = runtime.eval("(let ((xs '(11 12))) ``(a ,,@xs))").unwrap();
    let expansion = source(&runtime, &result[0]);
    let result = runtime.eval(&expansion).unwrap();
    assert_eq!(source(&runtime, &result[0]), "(A 11 12)");
}

#[test]
fn quasiquote_accepts_unquote_in_dotted_templates() {
    let runtime = Runtime::start().unwrap();
    let result = runtime.eval("(let ((x 9)) `(a . (b ,x)))").unwrap();
    assert_eq!(source(&runtime, &result[0]), "(A B 9)");
    let result = runtime.eval("(let ((x 9)) `,x)").unwrap();
    assert_eq!(result[0].as_fixnum(), Some(9));
}

#[test]
fn quasiquote_rejects_splices_outside_list_elements() {
    let runtime = Runtime::start().unwrap();
    assert!(runtime.eval(",x").is_err());
    assert!(runtime.eval("`,@(list 1)").is_err());
    assert!(runtime.eval("`(a . ,@(list 1))").is_err());
    assert!(runtime.eval("`(a ,@2)").is_err());
}
