//! Standard signatures displayed by source analysis and editor tooling.

use super::{BuiltinKind, BuiltinSignature};

macro_rules! join_params {
    () => { "" };
    ($first:literal $(, $rest:literal)*) => {
        concat!($first $(, " ", $rest)*)
    };
}

macro_rules! define_signatures {
    ($($name:literal: $kind:ident [$($param:literal),* $(,)?], $summary:literal;)*) => {
        pub(super) fn explicit_signature(name: &str) -> Option<BuiltinSignature> {
            Some(match name {
                $(
                    $name => BuiltinSignature {
                        name: $name,
                        kind: BuiltinKind::$kind,
                        params: &[$($param),*],
                        params_display: join_params!($($param),*),
                        summary: $summary,
                    },
                )*
                _ => return None,
            })
        }
    };
}

define_signatures! {
    // 25 Special Operators
    "BLOCK": SpecialOperator ["name", "&body", "forms"],
        "Establishes a lexical exit point named NAME around FORMS.";
    "CATCH": SpecialOperator ["tag", "&body", "forms"],
        "Establishes a dynamic catch tag for THROW around FORMS.";
    "EVAL-WHEN": SpecialOperator ["situations", "&body", "forms"],
        "Controls evaluation of FORMS at compile, load, or execute time.";
    "FLET": SpecialOperator ["definitions", "&body", "body"],
        "Binds local functions whose bodies do not see the local bindings.";
    "FUNCTION": SpecialOperator ["name"],
        "Returns the functional value of a function name or lambda expression.";
    "GO": SpecialOperator ["tag"],
        "Transfers control to TAG in an enclosing TAGBODY.";
    "IF": SpecialOperator ["test-form", "then-form", "&optional", "else-form"],
        "Evaluates THEN-FORM if TEST-FORM is true, otherwise ELSE-FORM.";
    "LABELS": SpecialOperator ["definitions", "&body", "body"],
        "Binds local mutually recursive functions visible within their own bodies.";
    "LET": SpecialOperator ["bindings", "&body", "body"],
        "Binds variables in parallel for the scope of BODY.";
    "LET*": SpecialOperator ["bindings", "&body", "body"],
        "Binds variables sequentially for the scope of BODY.";
    "LOAD-TIME-VALUE": SpecialOperator ["form", "&optional", "read-only-p"],
        "Arranges for FORM to be evaluated at load time.";
    "LOCALLY": SpecialOperator ["&body", "body"],
        "Executes forms in a lexical environment with local declarations.";
    "MACROLET": SpecialOperator ["definitions", "&body", "body"],
        "Establishes local macro definitions for the lexical scope of BODY.";
    "MULTIPLE-VALUE-CALL": SpecialOperator ["function-form", "&rest", "forms"],
        "Applies FUNCTION-FORM to all values produced by each form in FORMS.";
    "MULTIPLE-VALUE-PROG1": SpecialOperator ["first-form", "&rest", "forms"],
        "Evaluates FIRST-FORM and FORMS, returning all values of FIRST-FORM.";
    "PROGN": SpecialOperator ["&rest", "forms"],
        "Evaluates FORMS sequentially from left to right, returning the last form's values.";
    "PROGV": SpecialOperator ["symbols", "values", "&body", "forms"],
        "Dynamically binds SYMBOLS to VALUES during execution of FORMS.";
    "QUOTE": SpecialOperator ["object"],
        "Returns OBJECT without evaluating it.";
    "RETURN-FROM": SpecialOperator ["name", "&optional", "result"],
        "Returns values of RESULT from the enclosing lexical block NAME.";
    "SETQ": SpecialOperator ["&rest", "pairs"],
        "Assigns evaluated values sequentially to variables.";
    "SYMBOL-MACROLET": SpecialOperator ["macrobindings", "&body", "body"],
        "Establishes symbol macro expansions for the lexical scope of BODY.";
    "TAGBODY": SpecialOperator ["&rest", "statements"],
        "Executes statements sequentially with lexical tags reachable via GO.";
    "THE": SpecialOperator ["value-type", "form"],
        "Declares that the values of FORM conform to VALUE-TYPE.";
    "THROW": SpecialOperator ["tag", "result-form"],
        "Transfers control dynamically to the matching CATCH for TAG.";
    "UNWIND-PROTECT": SpecialOperator ["protected-form", "&body", "cleanup-forms"],
        "Evaluates PROTECTED-FORM and guarantees CLEANUP-FORMS run on exit.";

    // Declarations
    "DECLARE": Declaration ["&rest", "declaration-specifiers"],
        "Supplies declarations at the head of a binding or body form.";

    // Defining Forms and Standard Macros
    "DEFUN": Macro ["function-name", "lambda-list", "&body", "body"],
        "Defines a global function named FUNCTION-NAME.";
    "DEFMACRO": Macro ["name", "lambda-list", "&body", "body"],
        "Defines a global macro named NAME.";
    "DEFGENERIC": Macro ["function-name", "gf-lambda-list", "&rest", "options-and-methods"],
        "Defines a generic function or updates its options and methods.";
    "DEFMETHOD": Macro ["function-name", "&rest", "args"],
        "Defines a method on a generic function.";
    "DEFVAR": Macro ["name", "&optional", "initial-value", "documentation"],
        "Declares a special variable and initializes it if unbound.";
    "DEFPARAMETER": Macro ["name", "initial-value", "&optional", "documentation"],
        "Declares a special variable and unconditionally assigns INITIAL-VALUE.";
    "DEFCONSTANT": Macro ["name", "initial-value", "&optional", "documentation"],
        "Defines a global named constant.";
    "DEFPACKAGE": Macro ["defined-package-name", "&rest", "options"],
        "Defines a package or updates an existing package definition.";
    "IN-PACKAGE": Macro ["name"],
        "Sets the current *PACKAGE* to the package designated by NAME.";
    "DEFCLASS": Macro ["class-name", "superclasses", "slots", "&rest", "options"],
        "Defines a new CLOS class.";
    "DEFINE-CONDITION": Macro ["name", "parent-types", "slot-specs", "&rest", "options"],
        "Defines a new condition type.";
    "DEFSTRUCT": Macro ["name-and-options", "&rest", "slot-descriptions"],
        "Defines a structure type with constructor, predicate, and accessors.";
    "DEFTYPE": Macro ["name", "lambda-list", "&body", "body"],
        "Defines a derived type specifier.";
    "DEFINE-SYMBOL-MACRO": Macro ["symbol", "expansion"],
        "Defines a global symbol macro for SYMBOL.";
    "DEFINE-COMPILER-MACRO": Macro ["name", "lambda-list", "&body", "body"],
        "Defines a compiler macro for function or macro NAME.";
    "DEFINE-SETF-EXPANDER": Macro ["access-fn", "lambda-list", "&body", "body"],
        "Defines how SETF updates places of the form (ACCESS-FN ...).";
    "DEFSETF": Macro ["access-fn", "&rest", "args"],
        "Defines a short or long form SETF expansion for ACCESS-FN.";
    "DEFINE-MODIFY-MACRO": Macro [
        "name",
        "lambda-list",
        "function",
        "&optional",
        "documentation",
    ],
        "Defines a read-modify-write macro on a Generalized Reference place.";
    "DEFINE-METHOD-COMBINATION": Macro ["name", "&rest", "args"],
        "Defines a new CLOS method combination type.";
    "CALL-METHOD": Macro ["method", "&optional", "next-method-list"],
        "Invokes a method inside an effective method form.";
    "MAKE-METHOD": Macro ["form"],
        "Creates a pseudo-method object for CALL-METHOD inside an effective method.";
    "DESTRUCTURING-BIND": Macro ["lambda-list", "expression", "&body", "body"],
        "Binds variables in LAMBDA-LIST to the destructured tree of EXPRESSION.";
    "MULTIPLE-VALUE-BIND": Macro ["vars", "values-form", "&body", "body"],
        "Binds VARS to the multiple values returned by VALUES-FORM.";
    "MULTIPLE-VALUE-LIST": Macro ["form"],
        "Evaluates FORM and returns a list of the multiple values it produced.";
    "MULTIPLE-VALUE-SETQ": Macro ["vars", "form"],
        "Assigns multiple values returned by FORM to the variables in VARS.";
    "NTH-VALUE": Macro ["n", "form"],
        "Returns the Nth (zero-indexed) value produced by FORM.";
    "AND": Macro ["&rest", "forms"],
        "Evaluates FORMS left to right until one returns NIL; returns last form's values.";
    "OR": Macro ["&rest", "forms"],
        "Evaluates FORMS left to right until one returns non-NIL.";
    "COND": Macro ["&rest", "clauses"],
        "Evaluates test clauses sequentially until one succeeds.";
    "WHEN": Macro ["test-form", "&body", "forms"],
        "Evaluates FORMS in an implicit PROGN when TEST-FORM yields true.";
    "UNLESS": Macro ["test-form", "&body", "forms"],
        "Evaluates FORMS in an implicit PROGN when TEST-FORM yields false.";
    "CASE": Macro ["keyform", "&rest", "clauses"],
        "Dispatches on KEYFORM using EQL comparison against unevaluated keys.";
    "ECASE": Macro ["keyform", "&rest", "clauses"],
        "Like CASE, signaling a type-error if no clause matches.";
    "CCASE": Macro ["keyplace", "&rest", "clauses"],
        "Continuable CASE on KEYPLACE, offering a store-value restart on mismatch.";
    "TYPECASE": Macro ["keyform", "&rest", "clauses"],
        "Dispatches on KEYFORM by testing membership in each clause's type specifier.";
    "ETYPECASE": Macro ["keyform", "&rest", "clauses"],
        "Like TYPECASE, signaling a type-error if no clause matches.";
    "CTYPECASE": Macro ["keyplace", "&rest", "clauses"],
        "Continuable TYPECASE on KEYPLACE, offering a store-value restart on mismatch.";
    "LOOP": Macro ["&rest", "keywords-and-forms"],
        "Executes an iteration construct controlled by loop clauses or simple loop body.";
    "LOOP-FINISH": Macro [],
        "Terminates a LOOP iteration and runs any epilogue/finally clauses.";
    "DO": Macro ["varlist", "endlist", "&body", "body"],
        "Iterates with parallel variable stepping until end-test in ENDLIST is true.";
    "DO*": Macro ["varlist", "endlist", "&body", "body"],
        "Iterates with sequential variable stepping until end-test in ENDLIST is true.";
    "DOLIST": Macro ["(var list-form &optional result-form)", "&body", "body"],
        "Iterates VAR over each element of LIST-FORM.";
    "DOTIMES": Macro ["(var count-form &optional result-form)", "&body", "body"],
        "Iterates VAR over integers from 0 below COUNT-FORM.";
    "DO-SYMBOLS": Macro ["(var &optional package result-form)", "&body", "body"],
        "Iterates over symbols accessible in PACKAGE.";
    "DO-EXTERNAL-SYMBOLS": Macro ["(var &optional package result-form)", "&body", "body"],
        "Iterates over external symbols of PACKAGE.";
    "DO-ALL-SYMBOLS": Macro ["(var &optional result-form)", "&body", "body"],
        "Iterates over symbols in all registered packages.";
    "PROG": Macro ["varlist", "&body", "body"],
        "Binds variables in parallel inside an implicit BLOCK NIL and TAGBODY.";
    "PROG*": Macro ["varlist", "&body", "body"],
        "Binds variables sequentially inside an implicit BLOCK NIL and TAGBODY.";
    "PROG1": Macro ["first-form", "&rest", "forms"],
        "Evaluates FIRST-FORM and FORMS, returning the primary value of FIRST-FORM.";
    "PROG2": Macro ["first-form", "second-form", "&rest", "forms"],
        "Evaluates forms sequentially, returning the primary value of SECOND-FORM.";
    "SETF": Macro ["&rest", "pairs"],
        "Updates generalized reference places sequentially.";
    "PSETF": Macro ["&rest", "pairs"],
        "Updates generalized reference places in parallel.";
    "PSETQ": Macro ["&rest", "pairs"],
        "Assigns values to variables in parallel.";
    "INCF": Macro ["place", "&optional", "delta-form"],
        "Increments the value stored in PLACE by DELTA-FORM (default 1).";
    "DECF": Macro ["place", "&optional", "delta-form"],
        "Decrements the value stored in PLACE by DELTA-FORM (default 1).";
    "PUSH": Macro ["item", "place"],
        "Prepends ITEM onto the list stored in PLACE.";
    "PUSHNEW": Macro ["item", "place", "&key", "key", "test", "test-not"],
        "Prepends ITEM onto the list in PLACE if not already present.";
    "POP": Macro ["place"],
        "Removes and returns the CAR of the list stored in PLACE.";
    "REMF": Macro ["place", "indicator"],
        "Removes the property with INDICATOR from the plist stored in PLACE.";
    "SHIFTF": Macro ["&rest", "places-and-newvalue"],
        "Shifts values left across places and returns the old value of the first place.";
    "ROTATEF": Macro ["&rest", "places"],
        "Rotates values left across generalized reference places.";
    "HANDLER-BIND": Macro ["bindings", "&body", "forms"],
        "Establishes dynamic condition handlers without unwinding the stack.";
    "HANDLER-CASE": Macro ["expression", "&rest", "clauses"],
        "Evaluates EXPRESSION with condition handlers that unwind before running clauses.";
    "IGNORE-ERRORS": Macro ["&rest", "forms"],
        "Evaluates FORMS, returning NIL and the condition if an ERROR is signaled.";
    "RESTART-BIND": Macro ["bindings", "&body", "forms"],
        "Establishes dynamic restarts during execution of FORMS.";
    "RESTART-CASE": Macro ["restartable-form", "&rest", "clauses"],
        "Establishes restarts that unwind to CLAUSES when invoked.";
    "WITH-SIMPLE-RESTART": Macro [
        "(name format-control &rest format-arguments)",
        "&body",
        "forms",
    ],
        "Establishes a restart that returns NIL and T when invoked.";
    "WITH-CONDITION-RESTARTS": Macro ["condition-form", "restarts-form", "&body", "forms"],
        "Associates restarts with a condition during execution of FORMS.";
    "WITH-OPEN-FILE": Macro ["(stream filespec &rest options)", "&body", "body"],
        "Opens a file stream for BODY and guarantees it is closed on exit.";
    "WITH-OPEN-STREAM": Macro ["(var stream)", "&body", "body"],
        "Binds VAR to STREAM and ensures STREAM is closed on exit.";
    "WITH-INPUT-FROM-STRING": Macro ["(var string &key index start end)", "&body", "body"],
        "Binds VAR to a character input stream reading from STRING.";
    "WITH-OUTPUT-TO-STRING": Macro [
        "(var &optional string-form &key element-type)",
        "&body",
        "body",
    ],
        "Binds VAR to a character output stream accumulating a string.";
    "WITH-SLOTS": Macro ["slot-entries", "instance-form", "&body", "body"],
        "Establishes symbol macros accessing slots of INSTANCE-FORM via SLOT-VALUE.";
    "WITH-ACCESSORS": Macro ["slot-entries", "instance-form", "&body", "body"],
        "Establishes symbol macros accessing INSTANCE-FORM via accessor functions.";
    "WITH-HASH-TABLE-ITERATOR": Macro ["(name hash-table)", "&body", "body"],
        "Defines local macro NAME to iterate entries of HASH-TABLE.";
    "WITH-PACKAGE-ITERATOR": Macro [
        "(name package-list-form &rest symbol-types)",
        "&body",
        "body",
    ],
        "Defines local macro NAME to iterate symbols in packages.";
    "WITH-STANDARD-IO-SYNTAX": Macro ["&body", "forms"],
        "Executes FORMS with standard reader and printer control variables bound.";
    "WITH-COMPILATION-UNIT": Macro ["options", "&body", "forms"],
        "Executes FORMS within a single compilation unit for deferred warnings.";
    "PRINT-UNREADABLE-OBJECT": Macro ["(object stream &key type identity)", "&body", "body"],
        "Outputs OBJECT in #<...> unreadable notation to STREAM.";
    "PPRINT-LOGICAL-BLOCK": Macro [
        "(stream-symbol object &key prefix per-line-prefix suffix)",
        "&body",
        "body",
    ],
        "Groups pretty-printer output into a logical block.";
    "PPRINT-EXIT-IF-LIST-EXHAUSTED": Macro [],
        "Exits the enclosing PPRINT-LOGICAL-BLOCK when its list is exhausted.";
    "PPRINT-POP": Macro [],
        "Pops one element from the list being printed in PPRINT-LOGICAL-BLOCK.";
    "ASSERT": Macro [
        "test-form",
        "&optional",
        "places",
        "datum-form",
        "&rest",
        "argument-forms",
    ],
        "Signals a continuable error if TEST-FORM evaluates to false.";
    "CHECK-TYPE": Macro ["place", "typespec", "&optional", "string"],
        "Signals a continuable type-error if PLACE is not of type TYPESPEC.";
    "DECLAIM": Macro ["&rest", "declaration-specifiers"],
        "Establishes global declarations for the compilation and execution environment.";
    "RETURN": Macro ["&optional", "result"],
        "Returns RESULT from the enclosing block named NIL.";
    "LAMBDA": Macro ["lambda-list", "&body", "body"],
        "Denotes an anonymous function with LAMBDA-LIST and BODY.";
    "FORMATTER": Macro ["control-string"],
        "Compiles a FORMAT control string into a function.";
    "TIME": Macro ["form"],
        "Evaluates FORM, prints timing information, and returns FORM's values.";
    "TRACE": Macro ["&rest", "function-names"],
        "Enables tracing for the specified functions.";
    "UNTRACE": Macro ["&rest", "function-names"],
        "Disables tracing for the specified functions.";
    "STEP": Macro ["form"],
        "Evaluates FORM with interactive single-stepping enabled.";

    // Arithmetic and Numeric Functions
    "+": Function ["&rest", "numbers"],
        "Returns the sum of NUMBERS (0 if none given).";
    "-": Function ["number", "&rest", "more-numbers"],
        "Negates NUMBER or subtracts MORE-NUMBERS from NUMBER.";
    "*": Function ["&rest", "numbers"],
        "Returns the product of NUMBERS (1 if none given).";
    "/": Function ["number", "&rest", "more-numbers"],
        "Returns the reciprocal of NUMBER or divides NUMBER by MORE-NUMBERS.";
    "=": Function ["number", "&rest", "more-numbers"],
        "Returns true if all numbers are numerically equal.";
    "/=": Function ["number", "&rest", "more-numbers"],
        "Returns true if no two numbers are numerically equal.";
    "<": Function ["number", "&rest", "more-numbers"],
        "Returns true if numbers are in strictly monotonically increasing order.";
    ">": Function ["number", "&rest", "more-numbers"],
        "Returns true if numbers are in strictly monotonically decreasing order.";
    "<=": Function ["number", "&rest", "more-numbers"],
        "Returns true if numbers are in monotonically nondecreasing order.";
    ">=": Function ["number", "&rest", "more-numbers"],
        "Returns true if numbers are in monotonically nonincreasing order.";
    "1+": Function ["number"],
        "Returns NUMBER plus 1.";
    "1-": Function ["number"],
        "Returns NUMBER minus 1.";
    "MAX": Function ["real", "&rest", "more-reals"],
        "Returns the greatest of its real arguments.";
    "MIN": Function ["real", "&rest", "more-reals"],
        "Returns the least of its real arguments.";
    "ABS": Function ["number"],
        "Returns the absolute value (magnitude) of NUMBER.";
    "MOD": Function ["number", "divisor"],
        "Returns the second value of (FLOOR NUMBER DIVISOR).";
    "REM": Function ["number", "divisor"],
        "Returns the second value of (TRUNCATE NUMBER DIVISOR).";
    "FLOOR": Function ["number", "&optional", "divisor"],
        "Truncates toward negative infinity, returning quotient and remainder.";
    "CEILING": Function ["number", "&optional", "divisor"],
        "Truncates toward positive infinity, returning quotient and remainder.";
    "TRUNCATE": Function ["number", "&optional", "divisor"],
        "Truncates toward zero, returning quotient and remainder.";
    "ROUND": Function ["number", "&optional", "divisor"],
        "Rounds to nearest even integer, returning quotient and remainder.";
    "FFLOOR": Function ["number", "&optional", "divisor"],
        "Like FLOOR, returning the quotient as a float.";
    "FCEILING": Function ["number", "&optional", "divisor"],
        "Like CEILING, returning the quotient as a float.";
    "FTRUNCATE": Function ["number", "&optional", "divisor"],
        "Like TRUNCATE, returning the quotient as a float.";
    "FROUND": Function ["number", "&optional", "divisor"],
        "Like ROUND, returning the quotient as a float.";
    "ASH": Function ["integer", "count"],
        "Shifts INTEGER arithmetically left by COUNT bits (right if negative).";
    "LOGAND": Function ["&rest", "integers"],
        "Returns the bit-wise logical AND of INTEGERS.";
    "LOGIOR": Function ["&rest", "integers"],
        "Returns the bit-wise logical inclusive OR of INTEGERS.";
    "LOGXOR": Function ["&rest", "integers"],
        "Returns the bit-wise logical exclusive OR of INTEGERS.";
    "LOGEQV": Function ["&rest", "integers"],
        "Returns the bit-wise logical equivalence (XNOR) of INTEGERS.";
    "LOGNAND": Function ["integer-1", "integer-2"],
        "Returns the bit-wise logical NAND of two integers.";
    "LOGNOR": Function ["integer-1", "integer-2"],
        "Returns the bit-wise logical NOR of two integers.";
    "LOGANDC1": Function ["integer-1", "integer-2"],
        "Returns the bit-wise AND of (LOGNOT INTEGER-1) with INTEGER-2.";
    "LOGANDC2": Function ["integer-1", "integer-2"],
        "Returns the bit-wise AND of INTEGER-1 with (LOGNOT INTEGER-2).";
    "LOGORC1": Function ["integer-1", "integer-2"],
        "Returns the bit-wise OR of (LOGNOT INTEGER-1) with INTEGER-2.";
    "LOGORC2": Function ["integer-1", "integer-2"],
        "Returns the bit-wise OR of INTEGER-1 with (LOGNOT INTEGER-2).";
    "LOGNOT": Function ["integer"],
        "Returns the bit-wise logical complement of INTEGER.";
    "LOGTEST": Function ["integer-1", "integer-2"],
        "Returns true if any bit is set in both INTEGER-1 and INTEGER-2.";
    "LOGBITP": Function ["index", "integer"],
        "Returns true if bit INDEX of INTEGER is 1.";
    "LOGCOUNT": Function ["integer"],
        "Counts 1 bits in non-negative INTEGER or 0 bits in negative INTEGER.";
    "INTEGER-LENGTH": Function ["integer"],
        "Returns the number of bits needed to represent INTEGER in two's complement.";
    "BYTE": Function ["size", "position"],
        "Returns a byte specifier of SIZE bits at POSITION.";
    "BYTE-SIZE": Function ["bytespec"],
        "Returns the size of a byte specifier.";
    "BYTE-POSITION": Function ["bytespec"],
        "Returns the position of a byte specifier.";
    "LDB": Function ["bytespec", "integer"],
        "Extracts the byte specified by BYTESPEC from INTEGER.";
    "LDB-TEST": Function ["bytespec", "integer"],
        "Returns true if any bit in the byte specified by BYTESPEC is 1.";
    "MASK-FIELD": Function ["bytespec", "integer"],
        "Masks the byte specified by BYTESPEC within INTEGER.";
    "DPB": Function ["newbyte", "bytespec", "integer"],
        "Deposits NEWBYTE into the byte specified by BYTESPEC of INTEGER.";
    "DEPOSIT-FIELD": Function ["newbyte", "bytespec", "integer"],
        "Replaces the byte specified by BYTESPEC in INTEGER from aligned NEWBYTE.";
    "BOOLE": Function ["op", "integer-1", "integer-2"],
        "Performs the bit-wise boolean operation OP on two integers.";
    "EXPT": Function ["base-number", "power-number"],
        "Returns BASE-NUMBER raised to the power POWER-NUMBER.";
    "SQRT": Function ["number"],
        "Returns the principal square root of NUMBER.";
    "ISQRT": Function ["natural"],
        "Returns the greatest integer less than or equal to the square root of NATURAL.";
    "SIN": Function ["radians"],
        "Returns the sine of RADIANS.";
    "COS": Function ["radians"],
        "Returns the cosine of RADIANS.";
    "TAN": Function ["radians"],
        "Returns the tangent of RADIANS.";
    "ASIN": Function ["number"],
        "Returns the arc sine of NUMBER.";
    "ACOS": Function ["number"],
        "Returns the arc cosine of NUMBER.";
    "ATAN": Function ["number1", "&optional", "number2"],
        "Returns the arc tangent of NUMBER1 or NUMBER1/NUMBER2.";
    "SINH": Function ["number"],
        "Returns the hyperbolic sine of NUMBER.";
    "COSH": Function ["number"],
        "Returns the hyperbolic cosine of NUMBER.";
    "TANH": Function ["number"],
        "Returns the hyperbolic tangent of NUMBER.";
    "ASINH": Function ["number"],
        "Returns the hyperbolic arc sine of NUMBER.";
    "ACOSH": Function ["number"],
        "Returns the hyperbolic arc cosine of NUMBER.";
    "ATANH": Function ["number"],
        "Returns the hyperbolic arc tangent of NUMBER.";
    "EXP": Function ["number"],
        "Returns e raised to the power NUMBER.";
    "LOG": Function ["number", "&optional", "base"],
        "Returns the logarithm of NUMBER in BASE (natural logarithm by default).";
    "GCD": Function ["&rest", "integers"],
        "Returns the greatest common divisor of INTEGERS.";
    "LCM": Function ["&rest", "integers"],
        "Returns the least common multiple of INTEGERS.";
    "SIGNUM": Function ["number"],
        "Returns -1, 0, or 1 (or unit phase) indicating the sign of NUMBER.";
    "CIS": Function ["radians"],
        "Returns e^(i * RADIANS) as a complex number.";
    "COMPLEX": Function ["realpart", "&optional", "imagpart"],
        "Constructs a number with REALPART and IMAGPART.";
    "REALPART": Function ["number"],
        "Returns the real part of NUMBER.";
    "IMAGPART": Function ["number"],
        "Returns the imaginary part of NUMBER.";
    "CONJUGATE": Function ["number"],
        "Returns the complex conjugate of NUMBER.";
    "PHASE": Function ["number"],
        "Returns the angle part of the polar representation of NUMBER.";
    "NUMERATOR": Function ["rational"],
        "Returns the numerator of canonical RATIONAL.";
    "DENOMINATOR": Function ["rational"],
        "Returns the denominator of canonical RATIONAL.";
    "RATIONAL": Function ["number"],
        "Converts a real NUMBER to a rational exactly.";
    "RATIONALIZE": Function ["number"],
        "Approximates a real NUMBER as a rational.";
    "FLOAT": Function ["number", "&optional", "prototype"],
        "Converts a real NUMBER to a float.";
    "DECODE-FLOAT": Function ["float"],
        "Returns significand, exponent, and sign of FLOAT.";
    "SCALE-FLOAT": Function ["float", "integer"],
        "Scales FLOAT by radix^INTEGER.";
    "FLOAT-RADIX": Function ["float"],
        "Returns the radix of FLOAT.";
    "FLOAT-SIGN": Function ["float-1", "&optional", "float-2"],
        "Returns a float with the sign of FLOAT-1 and magnitude of FLOAT-2.";
    "FLOAT-DIGITS": Function ["float"],
        "Returns the number of radix digits in the representation of FLOAT.";
    "FLOAT-PRECISION": Function ["float"],
        "Returns the number of significant radix digits in FLOAT.";
    "INTEGER-DECODE-FLOAT": Function ["float"],
        "Returns integer significand, exponent, and sign of FLOAT.";
    "RANDOM": Function ["limit", "&optional", "random-state"],
        "Returns a pseudo-random number in [0, LIMIT).";
    "MAKE-RANDOM-STATE": Function ["&optional", "state"],
        "Creates or copies a random-state object.";

    // Conses and Lists
    "CONS": Function ["object-1", "object-2"],
        "Creates a fresh cons cell with CAR OBJECT-1 and CDR OBJECT-2.";
    "CAR": Function ["list"],
        "Returns the CAR of LIST (or NIL if LIST is NIL).";
    "CDR": Function ["list"],
        "Returns the CDR of LIST (or NIL if LIST is NIL).";
    "FIRST": Function ["list"],
        "Returns the 1st element of LIST.";
    "SECOND": Function ["list"],
        "Returns the 2nd element of LIST.";
    "THIRD": Function ["list"],
        "Returns the 3rd element of LIST.";
    "FOURTH": Function ["list"],
        "Returns the 4th element of LIST.";
    "FIFTH": Function ["list"],
        "Returns the 5th element of LIST.";
    "SIXTH": Function ["list"],
        "Returns the 6th element of LIST.";
    "SEVENTH": Function ["list"],
        "Returns the 7th element of LIST.";
    "EIGHTH": Function ["list"],
        "Returns the 8th element of LIST.";
    "NINTH": Function ["list"],
        "Returns the 9th element of LIST.";
    "TENTH": Function ["list"],
        "Returns the 10th element of LIST.";
    "REST": Function ["list"],
        "Returns the tail of LIST after the first element (same as CDR).";
    "LIST": Function ["&rest", "objects"],
        "Constructs and returns a proper list of OBJECTS.";
    "LIST*": Function ["&rest", "objects"],
        "Constructs a list with the last argument as the final CDR.";
    "APPEND": Function ["&rest", "lists"],
        "Concatenates LISTS by copying all but the last list.";
    "NCONC": Function ["&rest", "lists"],
        "Destructively concatenates LISTS by splicing their CDRs.";
    "REVAPPEND": Function ["list", "tail"],
        "Prepends a reversed copy of LIST onto TAIL.";
    "NRECONC": Function ["list", "tail"],
        "Destructively reverses LIST and appends TAIL.";
    "REVERSE": Function ["sequence"],
        "Returns a new sequence containing the elements of SEQUENCE in reverse order.";
    "NREVERSE": Function ["sequence"],
        "Destructively reverses SEQUENCE.";
    "MAKE-LIST": Function ["size", "&key", "initial-element"],
        "Creates a list of length SIZE filled with INITIAL-ELEMENT.";
    "COPY-LIST": Function ["list"],
        "Copies the top-level cons Spine of LIST.";
    "COPY-ALIST": Function ["alist"],
        "Copies the spine of ALIST and each non-atom association pair.";
    "COPY-TREE": Function ["tree"],
        "Recursively copies all cons cells in TREE.";
    "TREE-EQUAL": Function ["tree-1", "tree-2", "&key", "test", "test-not"],
        "Compares two trees for isomorphic cons structure and leaf equality.";
    "LAST": Function ["list", "&optional", "n"],
        "Returns the last N conses (default 1) of LIST.";
    "BUTLAST": Function ["list", "&optional", "n"],
        "Returns a copy of LIST omitting the last N conses.";
    "NBUTLAST": Function ["list", "&optional", "n"],
        "Destructively omits the last N conses of LIST.";
    "LDIFF": Function ["list", "object"],
        "Returns a copy of the prefix of LIST before tail OBJECT.";
    "TAILP": Function ["object", "list"],
        "Returns true if OBJECT is EQL to a tail of LIST.";
    "NTH": Function ["n", "list"],
        "Returns the Nth element (zero-indexed) of LIST.";
    "NTHCDR": Function ["n", "list"],
        "Applies CDR N times to LIST.";
    "MEMBER": Function ["item", "list", "&key", "key", "test", "test-not"],
        "Searches LIST for ITEM, returning the tail starting at the match.";
    "MEMBER-IF": Function ["predicate", "list", "&key", "key"],
        "Returns the tail of LIST starting at the first element satisfying PREDICATE.";
    "MEMBER-IF-NOT": Function ["predicate", "list", "&key", "key"],
        "Returns the tail of LIST starting at the first element not satisfying PREDICATE.";
    "ASSOC": Function ["item", "alist", "&key", "key", "test", "test-not"],
        "Looks up the first pair in ALIST whose CAR matches ITEM.";
    "ASSOC-IF": Function ["predicate", "alist", "&key", "key"],
        "Looks up the first pair in ALIST whose CAR satisfies PREDICATE.";
    "ASSOC-IF-NOT": Function ["predicate", "alist", "&key", "key"],
        "Looks up the first pair in ALIST whose CAR does not satisfy PREDICATE.";
    "RASSOC": Function ["item", "alist", "&key", "key", "test", "test-not"],
        "Looks up the first pair in ALIST whose CDR matches ITEM.";
    "RASSOC-IF": Function ["predicate", "alist", "&key", "key"],
        "Looks up the first pair in ALIST whose CDR satisfies PREDICATE.";
    "RASSOC-IF-NOT": Function ["predicate", "alist", "&key", "key"],
        "Looks up the first pair in ALIST whose CDR does not satisfy PREDICATE.";
    "ACONS": Function ["key", "datum", "alist"],
        "Prepends (CONS KEY DATUM) onto ALIST.";
    "PAIRLIS": Function ["keys", "data", "&optional", "alist"],
        "Prepends pairs of KEYS and DATA onto ALIST.";
    "SUBLIS": Function ["alist", "tree", "&key", "key", "test", "test-not"],
        "Substitutes subtrees in TREE according to ALIST.";
    "NSUBLIS": Function ["alist", "tree", "&key", "key", "test", "test-not"],
        "Destructively substitutes subtrees in TREE according to ALIST.";
    "SUBST": Function ["new", "old", "tree", "&key", "key", "test", "test-not"],
        "Substitutes NEW for occurrences of OLD throughout TREE.";
    "SUBST-IF": Function ["new", "predicate", "tree", "&key", "key"],
        "Substitutes NEW for subtrees of TREE satisfying PREDICATE.";
    "SUBST-IF-NOT": Function ["new", "predicate", "tree", "&key", "key"],
        "Substitutes NEW for subtrees of TREE not satisfying PREDICATE.";
    "NSUBST": Function ["new", "old", "tree", "&key", "key", "test", "test-not"],
        "Destructively substitutes NEW for occurrences of OLD throughout TREE.";
    "NSUBST-IF": Function ["new", "predicate", "tree", "&key", "key"],
        "Destructively substitutes NEW for subtrees satisfying PREDICATE.";
    "NSUBST-IF-NOT": Function ["new", "predicate", "tree", "&key", "key"],
        "Destructively substitutes NEW for subtrees not satisfying PREDICATE.";
    "GETF": Function ["plist", "indicator", "&optional", "default"],
        "Finds property INDICATOR on PLIST, or returns DEFAULT.";
    "GET-PROPERTIES": Function ["plist", "indicator-list"],
        "Searches PLIST for any indicator in INDICATOR-LIST.";
    "UNION": Function ["list-1", "list-2", "&key", "key", "test", "test-not"],
        "Returns the set union of two lists.";
    "NUNION": Function ["list-1", "list-2", "&key", "key", "test", "test-not"],
        "Destructively returns the set union of two lists.";
    "INTERSECTION": Function ["list-1", "list-2", "&key", "key", "test", "test-not"],
        "Returns the set intersection of two lists.";
    "NINTERSECTION": Function ["list-1", "list-2", "&key", "key", "test", "test-not"],
        "Destructively returns the set intersection of two lists.";
    "SET-DIFFERENCE": Function ["list-1", "list-2", "&key", "key", "test", "test-not"],
        "Returns elements of LIST-1 that do not appear in LIST-2.";
    "NSET-DIFFERENCE": Function ["list-1", "list-2", "&key", "key", "test", "test-not"],
        "Destructively returns elements of LIST-1 not in LIST-2.";
    "SET-EXCLUSIVE-OR": Function ["list-1", "list-2", "&key", "key", "test", "test-not"],
        "Returns elements appearing in exactly one of LIST-1 and LIST-2.";
    "NSET-EXCLUSIVE-OR": Function ["list-1", "list-2", "&key", "key", "test", "test-not"],
        "Destructively returns symmetric difference of LIST-1 and LIST-2.";
    "SUBSETP": Function ["list-1", "list-2", "&key", "key", "test", "test-not"],
        "Returns true if every element of LIST-1 is a member of LIST-2.";
    "ADJOIN": Function ["item", "list", "&key", "key", "test", "test-not"],
        "Prepends ITEM onto LIST only if it is not already a member.";
    "RPLACA": Function ["cons", "object"],
        "Replaces the CAR of CONS with OBJECT and returns CONS.";
    "RPLACD": Function ["cons", "object"],
        "Replaces the CDR of CONS with OBJECT and returns CONS.";
    "ENDP": Function ["list"],
        "Returns true if LIST is NIL, false if a cons, or signals a type-error.";
    "LIST-LENGTH": Function ["list"],
        "Returns the length of proper LIST, or NIL if LIST is circular.";
    "ATOM": Function ["object"],
        "Returns true if OBJECT is not a cons.";

    // Sequences
    "ELT": Function ["sequence", "index"],
        "Returns the element of SEQUENCE at INDEX.";
    "LENGTH": Function ["sequence"],
        "Returns the number of elements in SEQUENCE.";
    "SUBSEQ": Function ["sequence", "start", "&optional", "end"],
        "Returns a copy of the subsequence of SEQUENCE from START to END.";
    "COPY-SEQ": Function ["sequence"],
        "Returns a shallow copy of SEQUENCE.";
    "FILL": Function ["sequence", "item", "&key", "start", "end"],
        "Destructively replaces elements of SEQUENCE with ITEM.";
    "REPLACE": Function [
        "sequence-1",
        "sequence-2",
        "&key",
        "start1",
        "end1",
        "start2",
        "end2",
    ],
        "Destructively copies elements from SEQUENCE-2 into SEQUENCE-1.";
    "COUNT": Function [
        "item",
        "sequence",
        "&key",
        "from-end",
        "start",
        "end",
        "key",
        "test",
        "test-not",
    ],
        "Counts elements in SEQUENCE matching ITEM.";
    "COUNT-IF": Function ["predicate", "sequence", "&key", "from-end", "start", "end", "key"],
        "Counts elements in SEQUENCE satisfying PREDICATE.";
    "COUNT-IF-NOT": Function [
        "predicate",
        "sequence",
        "&key",
        "from-end",
        "start",
        "end",
        "key",
    ],
        "Counts elements in SEQUENCE not satisfying PREDICATE.";
    "FIND": Function [
        "item",
        "sequence",
        "&key",
        "from-end",
        "test",
        "test-not",
        "start",
        "end",
        "key",
    ],
        "Returns the first element in SEQUENCE matching ITEM, or NIL.";
    "FIND-IF": Function ["predicate", "sequence", "&key", "from-end", "start", "end", "key"],
        "Returns the first element in SEQUENCE satisfying PREDICATE, or NIL.";
    "FIND-IF-NOT": Function [
        "predicate",
        "sequence",
        "&key",
        "from-end",
        "start",
        "end",
        "key",
    ],
        "Returns the first element in SEQUENCE not satisfying PREDICATE, or NIL.";
    "POSITION": Function [
        "item",
        "sequence",
        "&key",
        "from-end",
        "test",
        "test-not",
        "start",
        "end",
        "key",
    ],
        "Returns the index of the first element matching ITEM, or NIL.";
    "POSITION-IF": Function [
        "predicate",
        "sequence",
        "&key",
        "from-end",
        "start",
        "end",
        "key",
    ],
        "Returns the index of the first element satisfying PREDICATE, or NIL.";
    "POSITION-IF-NOT": Function [
        "predicate",
        "sequence",
        "&key",
        "from-end",
        "start",
        "end",
        "key",
    ],
        "Returns the index of the first element not satisfying PREDICATE, or NIL.";
    "SEARCH": Function [
        "sequence-1",
        "sequence-2",
        "&key",
        "from-end",
        "test",
        "test-not",
        "key",
        "start1",
        "start2",
        "end1",
        "end2",
    ],
        "Searches SEQUENCE-2 for a subsequence matching SEQUENCE-1.";
    "MISMATCH": Function [
        "sequence-1",
        "sequence-2",
        "&key",
        "from-end",
        "test",
        "test-not",
        "key",
        "start1",
        "start2",
        "end1",
        "end2",
    ],
        "Returns the first index where SEQUENCE-1 and SEQUENCE-2 differ, or NIL.";
    "REMOVE": Function [
        "item",
        "sequence",
        "&key",
        "from-end",
        "test",
        "test-not",
        "start",
        "end",
        "count",
        "key",
    ],
        "Returns a copy of SEQUENCE with elements matching ITEM removed.";
    "REMOVE-IF": Function [
        "test",
        "sequence",
        "&key",
        "from-end",
        "start",
        "end",
        "count",
        "key",
    ],
        "Returns a copy of SEQUENCE with elements satisfying TEST removed.";
    "REMOVE-IF-NOT": Function [
        "test",
        "sequence",
        "&key",
        "from-end",
        "start",
        "end",
        "count",
        "key",
    ],
        "Returns a copy of SEQUENCE keeping only elements satisfying TEST.";
    "DELETE": Function [
        "item",
        "sequence",
        "&key",
        "from-end",
        "test",
        "test-not",
        "start",
        "end",
        "count",
        "key",
    ],
        "Destructively removes elements matching ITEM from SEQUENCE.";
    "DELETE-IF": Function [
        "test",
        "sequence",
        "&key",
        "from-end",
        "start",
        "end",
        "count",
        "key",
    ],
        "Destructively removes elements satisfying TEST from SEQUENCE.";
    "DELETE-IF-NOT": Function [
        "test",
        "sequence",
        "&key",
        "from-end",
        "start",
        "end",
        "count",
        "key",
    ],
        "Destructively removes elements not satisfying TEST from SEQUENCE.";
    "REMOVE-DUPLICATES": Function [
        "sequence",
        "&key",
        "from-end",
        "test",
        "test-not",
        "start",
        "end",
        "key",
    ],
        "Returns a copy of SEQUENCE with duplicate elements removed.";
    "DELETE-DUPLICATES": Function [
        "sequence",
        "&key",
        "from-end",
        "test",
        "test-not",
        "start",
        "end",
        "key",
    ],
        "Destructively removes duplicate elements from SEQUENCE.";
    "SUBSTITUTE": Function [
        "newitem",
        "olditem",
        "sequence",
        "&key",
        "from-end",
        "test",
        "test-not",
        "start",
        "end",
        "count",
        "key",
    ],
        "Returns a copy of SEQUENCE replacing OLDITEM with NEWITEM.";
    "SUBSTITUTE-IF": Function [
        "newitem",
        "predicate",
        "sequence",
        "&key",
        "from-end",
        "start",
        "end",
        "count",
        "key",
    ],
        "Returns a copy of SEQUENCE replacing elements satisfying PREDICATE.";
    "SUBSTITUTE-IF-NOT": Function [
        "newitem",
        "predicate",
        "sequence",
        "&key",
        "from-end",
        "start",
        "end",
        "count",
        "key",
    ],
        "Returns a copy of SEQUENCE replacing elements not satisfying PREDICATE.";
    "NSUBSTITUTE": Function [
        "newitem",
        "olditem",
        "sequence",
        "&key",
        "from-end",
        "test",
        "test-not",
        "start",
        "end",
        "count",
        "key",
    ],
        "Destructively replaces OLDITEM with NEWITEM in SEQUENCE.";
    "NSUBSTITUTE-IF": Function [
        "newitem",
        "predicate",
        "sequence",
        "&key",
        "from-end",
        "start",
        "end",
        "count",
        "key",
    ],
        "Destructively replaces elements satisfying PREDICATE in SEQUENCE.";
    "NSUBSTITUTE-IF-NOT": Function [
        "newitem",
        "predicate",
        "sequence",
        "&key",
        "from-end",
        "start",
        "end",
        "count",
        "key",
    ],
        "Destructively replaces elements not satisfying PREDICATE in SEQUENCE.";
    "CONCATENATE": Function ["result-type", "&rest", "sequences"],
        "Concatenates SEQUENCES into a new sequence of RESULT-TYPE.";
    "MERGE": Function ["result-type", "sequence-1", "sequence-2", "predicate", "&key", "key"],
        "Destructively merges two sorted sequences into RESULT-TYPE.";
    "MAP": Function ["result-type", "function", "first-sequence", "&rest", "more-sequences"],
        "Applies FUNCTION element-wise across sequences, returning RESULT-TYPE.";
    "MAP-INTO": Function ["result-sequence", "function", "&rest", "sequences"],
        "Destructively stores results of applying FUNCTION into RESULT-SEQUENCE.";
    "SOME": Function ["predicate", "first-seq", "&rest", "more-seqs"],
        "Returns the first non-NIL result of applying PREDICATE across sequences.";
    "EVERY": Function ["predicate", "first-seq", "&rest", "more-seqs"],
        "Returns true if PREDICATE returns true for every element tuple.";
    "NOTANY": Function ["predicate", "first-seq", "&rest", "more-seqs"],
        "Returns true if PREDICATE returns false for every element tuple.";
    "NOTEVERY": Function ["predicate", "first-seq", "&rest", "more-seqs"],
        "Returns true if PREDICATE returns false for at least one element tuple.";
    "REDUCE": Function [
        "function",
        "sequence",
        "&key",
        "key",
        "from-end",
        "start",
        "end",
        "initial-value",
    ],
        "Combines elements of SEQUENCE using binary FUNCTION.";
    "SORT": Function ["sequence", "predicate", "&key", "key"],
        "Destructively sorts SEQUENCE according to PREDICATE.";
    "STABLE-SORT": Function ["sequence", "predicate", "&key", "key"],
        "Destructively sorts SEQUENCE while preserving relative order of equal elements.";
    "MAKE-SEQUENCE": Function ["result-type", "size", "&key", "initial-element"],
        "Creates a sequence of RESULT-TYPE and length SIZE.";

    // Control, Mapping, and Equality
    "FUNCALL": Function ["function", "&rest", "args"],
        "Calls FUNCTION with ARGS.";
    "APPLY": Function ["function", "&rest", "args+"],
        "Applies FUNCTION to arguments with the last argument spread as a list.";
    "VALUES": Function ["&rest", "objects"],
        "Returns OBJECTS as multiple values.";
    "VALUES-LIST": Function ["list"],
        "Returns the elements of LIST as multiple values.";
    "MAPCAR": Function ["function", "list", "&rest", "more-lists"],
        "Applies FUNCTION to successive elements of lists, collecting results.";
    "MAPLIST": Function ["function", "list", "&rest", "more-lists"],
        "Applies FUNCTION to successive CDRs of lists, collecting results.";
    "MAPC": Function ["function", "list", "&rest", "more-lists"],
        "Applies FUNCTION to successive elements for side effects, returning LIST.";
    "MAPL": Function ["function", "list", "&rest", "more-lists"],
        "Applies FUNCTION to successive CDRs for side effects, returning LIST.";
    "MAPCAN": Function ["function", "list", "&rest", "more-lists"],
        "Maps FUNCTION over elements and splices results with NCONC.";
    "MAPCON": Function ["function", "list", "&rest", "more-lists"],
        "Maps FUNCTION over successive CDRs and splices results with NCONC.";
    "IDENTITY": Function ["object"],
        "Returns OBJECT unchanged.";
    "CONSTANTLY": Function ["value"],
        "Returns a function that accepts any arguments and always returns VALUE.";
    "COMPLEMENT": Function ["function"],
        "Returns a function that returns the boolean negation of FUNCTION.";
    "EQ": Function ["x", "y"],
        "Returns true if X and Y are the exact same identical object.";
    "EQL": Function ["x", "y"],
        "Returns true if X and Y are EQ, or same-type numbers/characters with equal value.";
    "EQUAL": Function ["x", "y"],
        "Returns true if X and Y are structurally isomorphic (conses, strings, bit-vectors, pathnames).";
    "EQUALP": Function ["x", "y"],
        "Returns true if X and Y are equal under case-insensitive, numeric-type-insensitive structural comparison.";
    "NOT": Function ["x"],
        "Returns T if X is NIL, otherwise NIL.";
    "NULL": Function ["object"],
        "Returns T if OBJECT is the empty list NIL, otherwise NIL.";

    // Arrays, Vectors, Strings, and Characters
    "AREF": Function ["array", "&rest", "subscripts"],
        "Accesses the element of ARRAY at SUBSCRIPTS.";
    "SVREF": Function ["simple-vector", "index"],
        "Accesses the element of SIMPLE-VECTOR at INDEX.";
    "CHAR": Function ["string", "index"],
        "Accesses character INDEX of STRING.";
    "SCHAR": Function ["simple-string", "index"],
        "Accesses character INDEX of SIMPLE-STRING.";
    "BIT": Function ["bit-array", "&rest", "subscripts"],
        "Accesses the element of BIT-ARRAY at SUBSCRIPTS.";
    "SBIT": Function ["simple-bit-array", "&rest", "subscripts"],
        "Accesses the element of SIMPLE-BIT-ARRAY at SUBSCRIPTS.";
    "ROW-MAJOR-AREF": Function ["array", "index"],
        "Accesses the element of ARRAY at row-major INDEX.";
    "ARRAY-ROW-MAJOR-INDEX": Function ["array", "&rest", "subscripts"],
        "Computes the row-major index for SUBSCRIPTS in ARRAY.";
    "MAKE-ARRAY": Function [
        "dimensions",
        "&key",
        "element-type",
        "initial-element",
        "initial-contents",
        "adjustable",
        "fill-pointer",
        "displaced-to",
        "displaced-index-offset",
    ],
        "Creates and returns an array with DIMENSIONS.";
    "ADJUST-ARRAY": Function [
        "array",
        "new-dimensions",
        "&key",
        "element-type",
        "initial-element",
        "initial-contents",
        "fill-pointer",
        "displaced-to",
        "displaced-index-offset",
    ],
        "Resizes or reconfigures ARRAY to NEW-DIMENSIONS.";
    "VECTOR": Function ["&rest", "objects"],
        "Creates a fresh simple general vector containing OBJECTS.";
    "VECTOR-PUSH": Function ["new-element", "vector"],
        "Appends NEW-ELEMENT at VECTOR's fill pointer if space remains.";
    "VECTOR-PUSH-EXTEND": Function ["new-element", "vector", "&optional", "extension"],
        "Appends NEW-ELEMENT at VECTOR's fill pointer, extending VECTOR if full.";
    "VECTOR-POP": Function ["vector"],
        "Decrements VECTOR's fill pointer and returns the popped element.";
    "FILL-POINTER": Function ["vector"],
        "Returns the fill pointer of VECTOR.";
    "ARRAY-DIMENSIONS": Function ["array"],
        "Returns a list of the dimensions of ARRAY.";
    "ARRAY-DIMENSION": Function ["array", "axis-number"],
        "Returns the length of dimension AXIS-NUMBER of ARRAY.";
    "ARRAY-RANK": Function ["array"],
        "Returns the number of dimensions of ARRAY.";
    "ARRAY-TOTAL-SIZE": Function ["array"],
        "Returns the total number of elements in ARRAY.";
    "ARRAY-ELEMENT-TYPE": Function ["array"],
        "Returns the upgraded element type of ARRAY.";
    "ARRAY-DISPLACEMENT": Function ["array"],
        "Returns the target array and offset if ARRAY is displaced.";
    "ARRAY-HAS-FILL-POINTER-P": Function ["array"],
        "Returns true if ARRAY has a fill pointer.";
    "ARRAY-IN-BOUNDS-P": Function ["array", "&rest", "subscripts"],
        "Returns true if SUBSCRIPTS are valid indices for ARRAY.";
    "ADJUSTABLE-ARRAY-P": Function ["array"],
        "Returns true if ARRAY is expressly adjustable.";
    "UPGRADED-ARRAY-ELEMENT-TYPE": Function ["typespec", "&optional", "environment"],
        "Returns the upgraded array element type for TYPESPEC.";
    "UPGRADED-COMPLEX-PART-TYPE": Function ["typespec", "&optional", "environment"],
        "Returns the upgraded complex part type for TYPESPEC.";
    "BIT-AND": Function ["bit-array1", "bit-array2", "&optional", "opt-arg"],
        "Performs element-wise AND on two bit arrays.";
    "BIT-IOR": Function ["bit-array1", "bit-array2", "&optional", "opt-arg"],
        "Performs element-wise inclusive OR on two bit arrays.";
    "BIT-XOR": Function ["bit-array1", "bit-array2", "&optional", "opt-arg"],
        "Performs element-wise exclusive OR on two bit arrays.";
    "BIT-EQV": Function ["bit-array1", "bit-array2", "&optional", "opt-arg"],
        "Performs element-wise equivalence on two bit arrays.";
    "BIT-NAND": Function ["bit-array1", "bit-array2", "&optional", "opt-arg"],
        "Performs element-wise NAND on two bit arrays.";
    "BIT-NOR": Function ["bit-array1", "bit-array2", "&optional", "opt-arg"],
        "Performs element-wise NOR on two bit arrays.";
    "BIT-ANDC1": Function ["bit-array1", "bit-array2", "&optional", "opt-arg"],
        "Performs element-wise AND of complement of BIT-ARRAY1 with BIT-ARRAY2.";
    "BIT-ANDC2": Function ["bit-array1", "bit-array2", "&optional", "opt-arg"],
        "Performs element-wise AND of BIT-ARRAY1 with complement of BIT-ARRAY2.";
    "BIT-ORC1": Function ["bit-array1", "bit-array2", "&optional", "opt-arg"],
        "Performs element-wise OR of complement of BIT-ARRAY1 with BIT-ARRAY2.";
    "BIT-ORC2": Function ["bit-array1", "bit-array2", "&optional", "opt-arg"],
        "Performs element-wise OR of BIT-ARRAY1 with complement of BIT-ARRAY2.";
    "BIT-NOT": Function ["bit-array", "&optional", "opt-arg"],
        "Performs element-wise bit complement on BIT-ARRAY.";
    "STRING": Function ["x"],
        "Coerces a string, symbol, or character designator X into a string.";
    "STRING=": Function ["string1", "string2", "&key", "start1", "end1", "start2", "end2"],
        "Compares two strings for case-sensitive equality.";
    "STRING/=": Function ["string1", "string2", "&key", "start1", "end1", "start2", "end2"],
        "Compares two strings for case-sensitive inequality.";
    "STRING<": Function ["string1", "string2", "&key", "start1", "end1", "start2", "end2"],
        "Compares two strings for lexicographic less-than.";
    "STRING>": Function ["string1", "string2", "&key", "start1", "end1", "start2", "end2"],
        "Compares two strings for lexicographic greater-than.";
    "STRING<=": Function ["string1", "string2", "&key", "start1", "end1", "start2", "end2"],
        "Compares two strings for lexicographic less-than-or-equal.";
    "STRING>=": Function ["string1", "string2", "&key", "start1", "end1", "start2", "end2"],
        "Compares two strings for lexicographic greater-than-or-equal.";
    "STRING-EQUAL": Function ["string1", "string2", "&key", "start1", "end1", "start2", "end2"],
        "Compares two strings for case-insensitive equality.";
    "STRING-NOT-EQUAL": Function [
        "string1",
        "string2",
        "&key",
        "start1",
        "end1",
        "start2",
        "end2",
    ],
        "Compares two strings for case-insensitive inequality.";
    "STRING-LESSP": Function ["string1", "string2", "&key", "start1", "end1", "start2", "end2"],
        "Compares two strings for case-insensitive less-than.";
    "STRING-GREATERP": Function [
        "string1",
        "string2",
        "&key",
        "start1",
        "end1",
        "start2",
        "end2",
    ],
        "Compares two strings for case-insensitive greater-than.";
    "STRING-NOT-GREATERP": Function [
        "string1",
        "string2",
        "&key",
        "start1",
        "end1",
        "start2",
        "end2",
    ],
        "Compares two strings for case-insensitive less-than-or-equal.";
    "STRING-NOT-LESSP": Function [
        "string1",
        "string2",
        "&key",
        "start1",
        "end1",
        "start2",
        "end2",
    ],
        "Compares two strings for case-insensitive greater-than-or-equal.";
    "STRING-UPCASE": Function ["string", "&key", "start", "end"],
        "Returns a copy of STRING with characters converted to uppercase.";
    "STRING-DOWNCASE": Function ["string", "&key", "start", "end"],
        "Returns a copy of STRING with characters converted to lowercase.";
    "STRING-CAPITALIZE": Function ["string", "&key", "start", "end"],
        "Returns a copy of STRING with words capitalized.";
    "NSTRING-UPCASE": Function ["string", "&key", "start", "end"],
        "Destructively converts characters of STRING to uppercase.";
    "NSTRING-DOWNCASE": Function ["string", "&key", "start", "end"],
        "Destructively converts characters of STRING to lowercase.";
    "NSTRING-CAPITALIZE": Function ["string", "&key", "start", "end"],
        "Destructively capitalizes words in STRING.";
    "STRING-TRIM": Function ["character-bag", "string"],
        "Strips characters in CHARACTER-BAG from both ends of STRING.";
    "STRING-LEFT-TRIM": Function ["character-bag", "string"],
        "Strips characters in CHARACTER-BAG from the beginning of STRING.";
    "STRING-RIGHT-TRIM": Function ["character-bag", "string"],
        "Strips characters in CHARACTER-BAG from the end of STRING.";
    "MAKE-STRING": Function ["size", "&key", "initial-element", "element-type"],
        "Creates a simple string of length SIZE.";
    "CHAR=": Function ["&rest", "characters+"],
        "Returns true if all characters are identical.";
    "CHAR/=": Function ["&rest", "characters+"],
        "Returns true if no two characters are the same.";
    "CHAR<": Function ["&rest", "characters+"],
        "Returns true if characters are in strictly increasing code order.";
    "CHAR>": Function ["&rest", "characters+"],
        "Returns true if characters are in strictly decreasing code order.";
    "CHAR<=": Function ["&rest", "characters+"],
        "Returns true if characters are in nondecreasing code order.";
    "CHAR>=": Function ["&rest", "characters+"],
        "Returns true if characters are in nonincreasing code order.";
    "CHAR-EQUAL": Function ["&rest", "characters+"],
        "Compares characters for case-insensitive equality.";
    "CHAR-NOT-EQUAL": Function ["&rest", "characters+"],
        "Compares characters for case-insensitive distinctness.";
    "CHAR-LESSP": Function ["&rest", "characters+"],
        "Compares characters for case-insensitive strictly increasing order.";
    "CHAR-GREATERP": Function ["&rest", "characters+"],
        "Compares characters for case-insensitive strictly decreasing order.";
    "CHAR-NOT-GREATERP": Function ["&rest", "characters+"],
        "Compares characters for case-insensitive nondecreasing order.";
    "CHAR-NOT-LESSP": Function ["&rest", "characters+"],
        "Compares characters for case-insensitive nonincreasing order.";
    "CHAR-CODE": Function ["character"],
        "Returns the integer code attribute of CHARACTER.";
    "CHAR-INT": Function ["character"],
        "Returns a non-negative integer encoding of CHARACTER.";
    "CODE-CHAR": Function ["code"],
        "Returns the character with code attribute CODE, or NIL.";
    "CHAR-NAME": Function ["character"],
        "Returns the name string of CHARACTER, or NIL.";
    "NAME-CHAR": Function ["name"],
        "Returns the character with the given NAME designator, or NIL.";
    "CHAR-UPCASE": Function ["character"],
        "Returns the uppercase equivalent of CHARACTER.";
    "CHAR-DOWNCASE": Function ["character"],
        "Returns the lowercase equivalent of CHARACTER.";
    "DIGIT-CHAR": Function ["weight", "&optional", "radix"],
        "Returns the digit character for WEIGHT in RADIX, or NIL.";
    "DIGIT-CHAR-P": Function ["char", "&optional", "radix"],
        "Returns the numeric weight of digit CHAR in RADIX, or NIL.";

    // Hash Tables
    "MAKE-HASH-TABLE": Function ["&key", "test", "size", "rehash-size", "rehash-threshold", "weakness"],
        "Creates and returns a new hash table.";
    "GETHASH": Function ["key", "hash-table", "&optional", "default"],
        "Looks up KEY in HASH-TABLE, returning value and present-p.";
    "REMHASH": Function ["key", "hash-table"],
        "Removes any entry for KEY in HASH-TABLE, returning true if found.";
    "CLRHASH": Function ["hash-table"],
        "Removes all entries from HASH-TABLE and returns it.";
    "MAPHASH": Function ["function", "hash-table"],
        "Calls FUNCTION with (key value) for each entry in HASH-TABLE.";
    "HASH-TABLE-COUNT": Function ["hash-table"],
        "Returns the number of entries currently in HASH-TABLE.";
    "HASH-TABLE-SIZE": Function ["hash-table"],
        "Returns the current capacity of HASH-TABLE.";
    "HASH-TABLE-REHASH-SIZE": Function ["hash-table"],
        "Returns the rehash-size of HASH-TABLE.";
    "HASH-TABLE-REHASH-THRESHOLD": Function ["hash-table"],
        "Returns the rehash-threshold of HASH-TABLE.";
    "HASH-TABLE-TEST": Function ["hash-table"],
        "Returns the test function name used by HASH-TABLE.";
    "SXHASH": Function ["object"],
        "Computes a hash code for OBJECT compatible with EQUAL.";

    // Symbols and Packages
    "INTERN": Function ["string", "&optional", "package"],
        "Finds or interns a symbol named STRING in PACKAGE.";
    "FIND-SYMBOL": Function ["string", "&optional", "package"],
        "Looks up a symbol named STRING accessible in PACKAGE.";
    "UNINTERN": Function ["symbol", "&optional", "package"],
        "Removes SYMBOL from PACKAGE.";
    "IMPORT": Function ["symbols", "&optional", "package"],
        "Imports SYMBOLS into PACKAGE as internal symbols.";
    "SHADOWING-IMPORT": Function ["symbols", "&optional", "package"],
        "Imports SYMBOLS into PACKAGE, shadowing conflicting symbols.";
    "EXPORT": Function ["symbols", "&optional", "package"],
        "Makes SYMBOLS external in PACKAGE.";
    "UNEXPORT": Function ["symbols", "&optional", "package"],
        "Reverts external SYMBOLS to internal status in PACKAGE.";
    "SHADOW": Function ["symbol-names", "&optional", "package"],
        "Ensures shadowing symbols exist in PACKAGE for SYMBOL-NAMES.";
    "USE-PACKAGE": Function ["packages-to-use", "&optional", "package"],
        "Causes PACKAGE to inherit external symbols from PACKAGES-TO-USE.";
    "UNUSE-PACKAGE": Function ["packages-to-unuse", "&optional", "package"],
        "Removes PACKAGES-TO-UNUSE from the use-list of PACKAGE.";
    "MAKE-PACKAGE": Function ["package-name", "&key", "nicknames", "use"],
        "Creates and returns a new package.";
    "FIND-PACKAGE": Function ["name"],
        "Finds the package designated by NAME, or returns NIL.";
    "DELETE-PACKAGE": Function ["package"],
        "Deletes PACKAGE from the package system.";
    "RENAME-PACKAGE": Function ["package", "new-name", "&optional", "new-nicknames"],
        "Replaces the name and nicknames of PACKAGE.";
    "PACKAGE-NAME": Function ["package"],
        "Returns the name string of PACKAGE, or NIL if deleted.";
    "PACKAGE-NICKNAMES": Function ["package"],
        "Returns the list of nickname strings of PACKAGE.";
    "PACKAGE-USE-LIST": Function ["package"],
        "Returns the list of packages used by PACKAGE.";
    "PACKAGE-USED-BY-LIST": Function ["package"],
        "Returns the list of packages that use PACKAGE.";
    "PACKAGE-SHADOWING-SYMBOLS": Function ["package"],
        "Returns the shadowing symbols list of PACKAGE.";
    "LIST-ALL-PACKAGES": Function [],
        "Returns a list of all registered packages.";
    "FIND-ALL-SYMBOLS": Function ["string"],
        "Returns all symbols named STRING across all registered packages.";
    "SYMBOL-NAME": Function ["symbol"],
        "Returns the print name string of SYMBOL.";
    "SYMBOL-PACKAGE": Function ["symbol"],
        "Returns the home package of SYMBOL, or NIL.";
    "SYMBOL-VALUE": Function ["symbol"],
        "Returns the current dynamic value of SYMBOL.";
    "SYMBOL-FUNCTION": Function ["symbol"],
        "Returns the global function definition of SYMBOL.";
    "SYMBOL-PLIST": Function ["symbol"],
        "Returns the property list of SYMBOL.";
    "BOUNDP": Function ["symbol"],
        "Returns true if SYMBOL has a dynamic value binding.";
    "FBOUNDP": Function ["name"],
        "Returns true if NAME is globally bound in the function namespace.";
    "MAKUNBOUND": Function ["symbol"],
        "Makes SYMBOL unbound in the value namespace.";
    "FMAKUNBOUND": Function ["name"],
        "Removes the global function or macro definition of NAME.";
    "SET": Function ["symbol", "value"],
        "Sets the dynamic value cell of SYMBOL to VALUE.";
    "GET": Function ["symbol", "indicator", "&optional", "default"],
        "Looks up INDICATOR on SYMBOL's property list.";
    "REMPROP": Function ["symbol", "indicator"],
        "Removes property INDICATOR from SYMBOL's property list.";
    "GENSYM": Function ["&optional", "x"],
        "Creates a fresh uninterned symbol with prefix/counter from X.";
    "GENTEMP": Function ["&optional", "prefix", "package"],
        "Creates a fresh symbol interned in PACKAGE.";
    "MAKE-SYMBOL": Function ["name"],
        "Creates a fresh uninterned symbol with print name NAME.";
    "COPY-SYMBOL": Function ["symbol", "&optional", "copy-properties"],
        "Creates a fresh uninterned symbol with the same name as SYMBOL.";
    "KEYWORDP": Function ["object"],
        "Returns true if OBJECT is a symbol in the KEYWORD package.";

    // Types, CLOS, and Conditions
    "TYPEP": Function ["object", "type-specifier", "&optional", "environment"],
        "Returns true if OBJECT is of type TYPE-SPECIFIER.";
    "SUBTYPEP": Function ["type-1", "type-2", "&optional", "environment"],
        "Tests whether TYPE-1 is a recognizable subtype of TYPE-2.";
    "TYPE-OF": Function ["object"],
        "Returns a type specifier for OBJECT.";
    "COERCE": Function ["object", "result-type"],
        "Coerces OBJECT to RESULT-TYPE.";
    "CLASS-OF": Function ["object"],
        "Returns the CLOS class of OBJECT.";
    "FIND-CLASS": Function ["symbol", "&optional", "errorp", "environment"],
        "Finds the class named by SYMBOL.";
    "CLASS-NAME": Function ["class"],
        "Returns the name of CLASS.";
    "MAKE-INSTANCE": Function ["class", "&rest", "initargs"],
        "Creates and initializes a new instance of CLASS.";
    "ALLOCATE-INSTANCE": Function ["class", "&rest", "initargs"],
        "Allocates an uninitialized instance of CLASS.";
    "INITIALIZE-INSTANCE": Function ["instance", "&rest", "initargs"],
        "Initializes a newly allocated CLOS INSTANCE.";
    "REINITIALIZE-INSTANCE": Function ["instance", "&rest", "initargs"],
        "Updates slots of INSTANCE according to INITARGS.";
    "SHARED-INITIALIZE": Function ["instance", "slot-names", "&rest", "initargs"],
        "Fills slots of INSTANCE from INITARGS and initforms.";
    "UPDATE-INSTANCE-FOR-DIFFERENT-CLASS": Function [
        "previous",
        "current",
        "&rest",
        "initargs",
    ],
        "Initializes slots after CHANGE-CLASS.";
    "UPDATE-INSTANCE-FOR-REDEFINED-CLASS": Function [
        "instance",
        "added-slots",
        "discarded-slots",
        "property-list",
        "&rest",
        "initargs",
    ],
        "Updates an instance after its class is redefined.";
    "MAKE-INSTANCES-OBSOLETE": Function ["class"],
        "Marks instances of CLASS for lazy redefinition update.";
    "SLOT-VALUE": Function ["object", "slot-name"],
        "Returns the value of slot SLOT-NAME in OBJECT.";
    "SLOT-BOUNDP": Function ["instance", "slot-name"],
        "Returns true if SLOT-NAME is bound in INSTANCE.";
    "SLOT-MAKUNBOUND": Function ["instance", "slot-name"],
        "Makes slot SLOT-NAME unbound in INSTANCE.";
    "SLOT-EXISTS-P": Function ["object", "slot-name"],
        "Returns true if OBJECT has a slot named SLOT-NAME.";
    "SLOT-MISSING": Function [
        "class",
        "object",
        "slot-name",
        "operation",
        "&optional",
        "new-value",
    ],
        "Called when an accessed slot does not exist on OBJECT.";
    "SLOT-UNBOUND": Function ["class", "instance", "slot-name"],
        "Called when an unbound slot is read from INSTANCE.";
    "CHANGE-CLASS": Function ["instance", "new-class", "&rest", "initargs"],
        "Changes the class of INSTANCE to NEW-CLASS in place.";
    "ENSURE-GENERIC-FUNCTION": Function ["function-name", "&rest", "all-keys"],
        "Defines or updates a global generic function named FUNCTION-NAME.";
    "ADD-METHOD": Function ["generic-function", "method"],
        "Adds METHOD to GENERIC-FUNCTION.";
    "REMOVE-METHOD": Function ["generic-function", "method"],
        "Removes METHOD from GENERIC-FUNCTION.";
    "FIND-METHOD": Function [
        "generic-function",
        "method-qualifiers",
        "specializers",
        "&optional",
        "errorp",
    ],
        "Finds a method on GENERIC-FUNCTION matching qualifiers and specializers.";
    "COMPUTE-APPLICABLE-METHODS": Function ["generic-function", "function-arguments"],
        "Returns the sorted list of methods applicable to FUNCTION-ARGUMENTS.";
    "FUNCTION-KEYWORDS": Function ["method"],
        "Returns the keyword parameters and allow-other-keys flag for METHOD.";
    "METHOD-QUALIFIERS": Function ["method"],
        "Returns the list of qualifiers of METHOD.";
    "CALL-NEXT-METHOD": Function ["&rest", "args"],
        "Invokes the next most specific applicable method inside a method body.";
    "NEXT-METHOD-P": Function [],
        "Returns true if a next method exists in the current method context.";
    "NO-APPLICABLE-METHOD": Function ["generic-function", "&rest", "function-arguments"],
        "Called when no method applies to a generic function call.";
    "NO-NEXT-METHOD": Function ["generic-function", "method", "&rest", "args"],
        "Called when CALL-NEXT-METHOD is invoked with no next method.";
    "INVALID-METHOD-ERROR": Function ["method", "format-control", "&rest", "args"],
        "Signals an invalid method error during method combination.";
    "METHOD-COMBINATION-ERROR": Function ["format-control", "&rest", "args"],
        "Signals an error encountered during method combination.";
    "COPY-STRUCTURE": Function ["structure"],
        "Returns a shallow copy of a structure-object.";
    "MAKE-LOAD-FORM": Function ["object", "&optional", "environment"],
        "Returns creation and initialization forms for externalizing OBJECT.";
    "MAKE-LOAD-FORM-SAVING-SLOTS": Function ["object", "&key", "slot-names", "environment"],
        "Returns forms to reconstruct OBJECT with its slot values.";
    "ERROR": Function ["datum", "&rest", "arguments"],
        "Signals a non-continuable error condition.";
    "CERROR": Function ["continue-format-control", "datum", "&rest", "arguments"],
        "Signals a continuable error with a CONTINUE restart.";
    "WARN": Function ["datum", "&rest", "arguments"],
        "Signals a warning condition and prints it if not muffled.";
    "SIGNAL": Function ["datum", "&rest", "arguments"],
        "Signals a condition to active handlers, returning NIL if unhandled.";
    "MAKE-CONDITION": Function ["type", "&rest", "slot-initializations"],
        "Instantiates a condition of TYPE.";
    "INVOKE-DEBUGGER": Function ["condition"],
        "Enters the debugger with CONDITION.";
    "BREAK": Function ["&optional", "format-control", "&rest", "format-arguments"],
        "Enters the debugger directly with a CONTINUE restart.";
    "FIND-RESTART": Function ["identifier", "&optional", "condition"],
        "Searches for an active restart matching IDENTIFIER.";
    "COMPUTE-RESTARTS": Function ["&optional", "condition"],
        "Returns a list of all currently active restarts.";
    "INVOKE-RESTART": Function ["restart", "&rest", "arguments"],
        "Calls the function associated with RESTART.";
    "INVOKE-RESTART-INTERACTIVELY": Function ["restart"],
        "Prompts for arguments interactively and invokes RESTART.";
    "RESTART-NAME": Function ["restart"],
        "Returns the name symbol of RESTART.";
    "ABORT": Function ["&optional", "condition"],
        "Invokes the most recent ABORT restart.";
    "CONTINUE": Function ["&optional", "condition"],
        "Invokes the most recent CONTINUE restart, or returns NIL.";
    "MUFFLE-WARNING": Function ["&optional", "condition"],
        "Invokes the MUFFLE-WARNING restart.";
    "STORE-VALUE": Function ["value", "&optional", "condition"],
        "Invokes the STORE-VALUE restart with VALUE.";
    "USE-VALUE": Function ["value", "&optional", "condition"],
        "Invokes the USE-VALUE restart with VALUE.";
    "SIMPLE-CONDITION-FORMAT-CONTROL": Function ["condition"],
        "Returns the format control of a simple-condition.";
    "SIMPLE-CONDITION-FORMAT-ARGUMENTS": Function ["condition"],
        "Returns the format arguments list of a simple-condition.";
    "TYPE-ERROR-DATUM": Function ["condition"],
        "Returns the offending datum of a type-error.";
    "TYPE-ERROR-EXPECTED-TYPE": Function ["condition"],
        "Returns the expected type specifier of a type-error.";
    "CELL-ERROR-NAME": Function ["condition"],
        "Returns the cell name of a cell-error.";
    "UNBOUND-SLOT-INSTANCE": Function ["condition"],
        "Returns the instance that had an unbound slot.";
    "PACKAGE-ERROR-PACKAGE": Function ["condition"],
        "Returns the offending package designator of a package-error.";
    "FILE-ERROR-PATHNAME": Function ["condition"],
        "Returns the offending pathname designator of a file-error.";
    "STREAM-ERROR-STREAM": Function ["condition"],
        "Returns the offending stream of a stream-error.";
    "PRINT-NOT-READABLE-OBJECT": Function ["condition"],
        "Returns the object that could not be printed readably.";
    "ARITHMETIC-ERROR-OPERATION": Function ["condition"],
        "Returns the operation of an arithmetic-error.";
    "ARITHMETIC-ERROR-OPERANDS": Function ["condition"],
        "Returns the operands list of an arithmetic-error.";

    // I/O, Streams, Files, Reader, Printer, and Evaluation
    "READ": Function ["&optional", "input-stream", "eof-error-p", "eof-value", "recursive-p"],
        "Parses and returns one Lisp object from INPUT-STREAM.";
    "READ-PRESERVING-WHITESPACE": Function [
        "&optional",
        "input-stream",
        "eof-error-p",
        "eof-value",
        "recursive-p",
    ],
        "Reads an object from INPUT-STREAM without consuming trailing whitespace.";
    "READ-FROM-STRING": Function [
        "string",
        "&optional",
        "eof-error-p",
        "eof-value",
        "&key",
        "start",
        "end",
        "preserve-whitespace",
    ],
        "Reads a Lisp object from STRING, returning the object and next index.";
    "READ-DELIMITED-LIST": Function ["char", "&optional", "input-stream", "recursive-p"],
        "Reads objects from INPUT-STREAM until delimiter CHAR is encountered.";
    "READ-LINE": Function [
        "&optional",
        "input-stream",
        "eof-error-p",
        "eof-value",
        "recursive-p",
    ],
        "Reads a line of text terminated by newline from INPUT-STREAM.";
    "READ-CHAR": Function [
        "&optional",
        "input-stream",
        "eof-error-p",
        "eof-value",
        "recursive-p",
    ],
        "Reads and returns the next character from INPUT-STREAM.";
    "READ-CHAR-NO-HANG": Function [
        "&optional",
        "input-stream",
        "eof-error-p",
        "eof-value",
        "recursive-p",
    ],
        "Reads a character from INPUT-STREAM if immediately available, else NIL.";
    "UNREAD-CHAR": Function ["character", "&optional", "input-stream"],
        "Pushes CHARACTER back onto the front of INPUT-STREAM.";
    "PEEK-CHAR": Function [
        "&optional",
        "peek-type",
        "input-stream",
        "eof-error-p",
        "eof-value",
        "recursive-p",
    ],
        "Peeks at the next character in INPUT-STREAM without consuming it.";
    "LISTEN": Function ["&optional", "input-stream"],
        "Returns true if a character is immediately available on INPUT-STREAM.";
    "READ-BYTE": Function ["stream", "&optional", "eof-error-p", "eof-value"],
        "Reads and returns one byte integer from binary STREAM.";
    "READ-SEQUENCE": Function ["sequence", "stream", "&key", "start", "end"],
        "Destructively fills SEQUENCE with elements read from STREAM.";
    "PARSE-INTEGER": Function ["string", "&key", "start", "end", "radix", "junk-allowed"],
        "Parses an integer from STRING in RADIX.";
    "WRITE": Function [
        "object",
        "&key",
        "array",
        "base",
        "case",
        "circle",
        "escape",
        "gensym",
        "length",
        "level",
        "lines",
        "miser-width",
        "pprint-dispatch",
        "pretty",
        "radix",
        "readably",
        "right-margin",
        "stream",
    ],
        "Outputs OBJECT to STREAM using the specified printer control parameters.";
    "PRIN1": Function ["object", "&optional", "output-stream"],
        "Outputs OBJECT with escape characters suitable for READ.";
    "PRINC": Function ["object", "&optional", "output-stream"],
        "Outputs OBJECT without escape characters for human reading.";
    "PRINT": Function ["object", "&optional", "output-stream"],
        "Outputs a newline, PRIN1 of OBJECT, and a space.";
    "PPRINT": Function ["object", "&optional", "output-stream"],
        "Outputs a newline and pretty-prints OBJECT, returning no values.";
    "WRITE-TO-STRING": Function [
        "object",
        "&key",
        "array",
        "base",
        "case",
        "circle",
        "escape",
        "gensym",
        "length",
        "level",
        "lines",
        "miser-width",
        "pprint-dispatch",
        "pretty",
        "radix",
        "readably",
        "right-margin",
    ],
        "Formats OBJECT to a string using the specified printer parameters.";
    "PRIN1-TO-STRING": Function ["object"],
        "Formats OBJECT to a string using PRIN1.";
    "PRINC-TO-STRING": Function ["object"],
        "Formats OBJECT to a string using PRINC.";
    "WRITE-CHAR": Function ["character", "&optional", "output-stream"],
        "Outputs CHARACTER to OUTPUT-STREAM.";
    "WRITE-STRING": Function ["string", "&optional", "output-stream", "&key", "start", "end"],
        "Outputs characters of STRING to OUTPUT-STREAM.";
    "WRITE-LINE": Function ["string", "&optional", "output-stream", "&key", "start", "end"],
        "Outputs characters of STRING followed by a newline to OUTPUT-STREAM.";
    "WRITE-BYTE": Function ["byte", "stream"],
        "Outputs one integer BYTE to binary STREAM.";
    "WRITE-SEQUENCE": Function ["sequence", "stream", "&key", "start", "end"],
        "Writes elements of SEQUENCE to STREAM.";
    "TERPRI": Function ["&optional", "output-stream"],
        "Outputs a newline to OUTPUT-STREAM.";
    "FRESH-LINE": Function ["&optional", "output-stream"],
        "Outputs a newline only if OUTPUT-STREAM is not already at the start of a line.";
    "FINISH-OUTPUT": Function ["&optional", "output-stream"],
        "Flushes buffered output on OUTPUT-STREAM and waits for completion.";
    "FORCE-OUTPUT": Function ["&optional", "output-stream"],
        "Initiates flushing of buffered output on OUTPUT-STREAM.";
    "CLEAR-OUTPUT": Function ["&optional", "output-stream"],
        "Aborts pending buffered output on OUTPUT-STREAM.";
    "CLEAR-INPUT": Function ["&optional", "input-stream"],
        "Clears buffered input on INPUT-STREAM.";
    "FORMAT": Function ["destination", "control-string", "&rest", "format-arguments"],
        "Produces formatted output to a stream or string according to CONTROL-STRING.";
    "PRINT-OBJECT": Function ["object", "stream"],
        "Generic function called by the Lisp printer to display OBJECT on STREAM.";
    "PPRINT-DISPATCH": Function ["object", "&optional", "table"],
        "Retrieves the pretty-print dispatch function for OBJECT.";
    "COPY-PPRINT-DISPATCH": Function ["&optional", "table"],
        "Copies a pprint dispatch table.";
    "SET-PPRINT-DISPATCH": Function [
        "type-specifier",
        "function",
        "&optional",
        "priority",
        "table",
    ],
        "Sets the pretty-print dispatch entry for TYPE-SPECIFIER.";
    "PPRINT-FILL": Function ["stream", "object", "&optional", "colon-p", "at-sign-p"],
        "Pretty-prints a list in fill style.";
    "PPRINT-LINEAR": Function ["stream", "object", "&optional", "colon-p", "at-sign-p"],
        "Pretty-prints a list in linear style.";
    "PPRINT-TABULAR": Function [
        "stream",
        "object",
        "&optional",
        "colon-p",
        "at-sign-p",
        "tabsize",
    ],
        "Pretty-prints a list in tabular columns.";
    "PPRINT-INDENT": Function ["relative-to", "n", "&optional", "stream"],
        "Sets indentation within a pretty-printing logical block.";
    "PPRINT-NEWLINE": Function ["kind", "&optional", "stream"],
        "Outputs a conditional newline to pretty-printing STREAM.";
    "PPRINT-TAB": Function ["kind", "colnum", "colinc", "&optional", "stream"],
        "Outputs a tabulation to pretty-printing STREAM.";
    "OPEN": Function [
        "filespec",
        "&key",
        "direction",
        "element-type",
        "if-exists",
        "if-does-not-exist",
        "external-format",
    ],
        "Opens a file stream for FILESPEC.";
    "CLOSE": Function ["stream", "&key", "abort"],
        "Closes an open STREAM.";
    "STREAM-ELEMENT-TYPE": Function ["stream"],
        "Returns the element type of STREAM.";
    "STREAM-EXTERNAL-FORMAT": Function ["stream"],
        "Returns the external file format of STREAM.";
    "FILE-POSITION": Function ["stream", "&optional", "position-spec"],
        "Gets or sets the current byte/character position within STREAM.";
    "FILE-LENGTH": Function ["stream"],
        "Returns the length of the file associated with STREAM.";
    "FILE-STRING-LENGTH": Function ["stream", "object"],
        "Returns the delta in FILE-POSITION that writing OBJECT would cause.";
    "MAKE-STRING-INPUT-STREAM": Function ["string", "&optional", "start", "end"],
        "Creates an input stream that reads from STRING.";
    "MAKE-STRING-OUTPUT-STREAM": Function ["&key", "element-type"],
        "Creates an output stream that accumulates characters into a string.";
    "GET-OUTPUT-STREAM-STRING": Function ["string-output-stream"],
        "Returns and clears the string accumulated in STRING-OUTPUT-STREAM.";
    "MAKE-BROADCAST-STREAM": Function ["&rest", "streams"],
        "Creates an output stream that broadcasts to all STREAMS.";
    "BROADCAST-STREAM-STREAMS": Function ["broadcast-stream"],
        "Returns the component streams of BROADCAST-STREAM.";
    "MAKE-CONCATENATED-STREAM": Function ["&rest", "input-streams"],
        "Creates an input stream that reads sequentially from INPUT-STREAMS.";
    "CONCATENATED-STREAM-STREAMS": Function ["concatenated-stream"],
        "Returns the remaining input streams of CONCATENATED-STREAM.";
    "MAKE-TWO-WAY-STREAM": Function ["input-stream", "output-stream"],
        "Creates a bidirectional stream from INPUT-STREAM and OUTPUT-STREAM.";
    "TWO-WAY-STREAM-INPUT-STREAM": Function ["two-way-stream"],
        "Returns the input stream of TWO-WAY-STREAM.";
    "TWO-WAY-STREAM-OUTPUT-STREAM": Function ["two-way-stream"],
        "Returns the output stream of TWO-WAY-STREAM.";
    "MAKE-ECHO-STREAM": Function ["input-stream", "output-stream"],
        "Creates a stream that echoes input read from INPUT-STREAM to OUTPUT-STREAM.";
    "ECHO-STREAM-INPUT-STREAM": Function ["echo-stream"],
        "Returns the input stream of ECHO-STREAM.";
    "ECHO-STREAM-OUTPUT-STREAM": Function ["echo-stream"],
        "Returns the output stream of ECHO-STREAM.";
    "MAKE-SYNONYM-STREAM": Function ["symbol"],
        "Creates a stream that delegates operations to the dynamic value of SYMBOL.";
    "SYNONYM-STREAM-SYMBOL": Function ["synonym-stream"],
        "Returns the target symbol of SYNONYM-STREAM.";
    "PATHNAME": Function ["pathspec"],
        "Coerces PATHSPEC into a pathname object.";
    "MAKE-PATHNAME": Function [
        "&key",
        "host",
        "device",
        "directory",
        "name",
        "type",
        "version",
        "defaults",
        "case",
    ],
        "Constructs a pathname from component values.";
    "PARSE-NAMESTRING": Function [
        "thing",
        "&optional",
        "host",
        "default-pathname",
        "&key",
        "start",
        "end",
        "junk-allowed",
    ],
        "Parses a pathname designator into a pathname object and index.";
    "MERGE-PATHNAMES": Function [
        "pathname",
        "&optional",
        "default-pathname",
        "default-version",
    ],
        "Fills in missing components of PATHNAME from DEFAULT-PATHNAME.";
    "NAMESTRING": Function ["pathname"],
        "Returns the full namestring representation of PATHNAME.";
    "FILE-NAMESTRING": Function ["pathname"],
        "Returns the name, type, and version portion of PATHNAME as a string.";
    "DIRECTORY-NAMESTRING": Function ["pathname"],
        "Returns the directory portion of PATHNAME as a string.";
    "HOST-NAMESTRING": Function ["pathname"],
        "Returns the host name of PATHNAME as a string.";
    "ENOUGH-NAMESTRING": Function ["pathname", "&optional", "defaults"],
        "Returns a namestring sufficient to identify PATHNAME relative to DEFAULTS.";
    "PATHNAME-HOST": Function ["pathname", "&key", "case"],
        "Returns the host component of PATHNAME.";
    "PATHNAME-DEVICE": Function ["pathname", "&key", "case"],
        "Returns the device component of PATHNAME.";
    "PATHNAME-DIRECTORY": Function ["pathname", "&key", "case"],
        "Returns the directory component of PATHNAME.";
    "PATHNAME-NAME": Function ["pathname", "&key", "case"],
        "Returns the name component of PATHNAME.";
    "PATHNAME-TYPE": Function ["pathname", "&key", "case"],
        "Returns the type component of PATHNAME.";
    "PATHNAME-VERSION": Function ["pathname"],
        "Returns the version component of PATHNAME.";
    "LOGICAL-PATHNAME": Function ["pathspec"],
        "Converts PATHSPEC to a logical-pathname.";
    "LOGICAL-PATHNAME-TRANSLATIONS": Function ["host"],
        "Returns the list of translations for logical pathname HOST.";
    "LOAD-LOGICAL-PATHNAME-TRANSLATIONS": Function ["host"],
        "Loads logical pathname translations for HOST.";
    "TRANSLATE-LOGICAL-PATHNAME": Function ["pathname", "&key"],
        "Translates a logical pathname into a physical pathname.";
    "TRANSLATE-PATHNAME": Function ["source", "from-wildcard", "to-wildcard", "&key"],
        "Translates SOURCE pathname matching FROM-WILDCARD into TO-WILDCARD.";
    "PATHNAME-MATCH-P": Function ["pathname", "wildcard"],
        "Returns true if PATHNAME matches WILDCARD.";
    "WILD-PATHNAME-P": Function ["pathname", "&optional", "field-key"],
        "Returns true if PATHNAME contains wild components.";
    "TRUENAME": Function ["filespec"],
        "Resolves FILESPEC to its canonical existing file system pathname.";
    "PROBE-FILE": Function ["pathspec"],
        "Returns the truename of PATHSPEC if it exists, or NIL.";
    "ENSURE-DIRECTORIES-EXIST": Function ["pathspec", "&key", "verbose"],
        "Creates all missing directories for PATHSPEC.";
    "DELETE-FILE": Function ["filespec"],
        "Deletes the file specified by FILESPEC.";
    "RENAME-FILE": Function ["filespec", "new-name"],
        "Renames FILESPEC to NEW-NAME.";
    "FILE-WRITE-DATE": Function ["pathspec"],
        "Returns the universal time when PATHSPEC was last modified.";
    "FILE-AUTHOR": Function ["pathspec"],
        "Returns the file author string of PATHSPEC, or NIL.";
    "DIRECTORY": Function ["pathspec", "&key"],
        "Returns a list of truenames matching wildcard PATHSPEC.";
    "USER-HOMEDIR-PATHNAME": Function ["&optional", "host"],
        "Returns the user's home directory pathname.";
    "EVAL": Function ["form"],
        "Evaluates FORM in the current dynamic environment and null lexical environment.";
    "LOAD": Function [
        "filespec",
        "&key",
        "verbose",
        "print",
        "if-does-not-exist",
        "external-format",
    ],
        "Loads and executes a source or compiled file.";
    "COMPILE": Function ["name", "&optional", "definition"],
        "Compiles a lambda expression or the function named by NAME.";
    "COMPILE-FILE": Function [
        "input-file",
        "&key",
        "output-file",
        "verbose",
        "print",
        "external-format",
    ],
        "Compiles a Lisp source file into a FASL file.";
    "COMPILE-FILE-PATHNAME": Function [
        "input-file",
        "&key",
        "output-file",
        "&allow-other-keys",
    ],
        "Returns the pathname that COMPILE-FILE would write.";
    "DISASSEMBLE": Function ["fn"],
        "Prints the disassembled representation of function FN.";
    "MACROEXPAND": Function ["form", "&optional", "env"],
        "Repeatedly expands FORM until it is no longer a macro form.";
    "MACROEXPAND-1": Function ["form", "&optional", "env"],
        "Expands FORM once if it is a macro form.";
    "MACRO-FUNCTION": Function ["symbol", "&optional", "environment"],
        "Returns the macro expansion function of SYMBOL, or NIL.";
    "COMPILER-MACRO-FUNCTION": Function ["name", "&optional", "environment"],
        "Returns the compiler macro function for NAME, or NIL.";
    "SPECIAL-OPERATOR-P": Function ["symbol"],
        "Returns true if SYMBOL names a special operator.";
    "FDEFINITION": Function ["function-name"],
        "Returns the global function definition of FUNCTION-NAME.";
    "FUNCTION-LAMBDA-EXPRESSION": Function ["function"],
        "Returns source lambda expression, closure-p, and name for FUNCTION.";
    "PROCLAIM": Function ["declaration-specifier"],
        "Establishes a global declaration specified by DECLARATION-SPECIFIER.";
    "CONSTANTP": Function ["form", "&optional", "environment"],
        "Returns true if FORM is a constant form in ENVIRONMENT.";
    "GET-SETF-EXPANSION": Function ["place", "&optional", "environment"],
        "Returns the five-value SETF expansion of PLACE.";
    "COPY-READTABLE": Function ["&optional", "from-readtable", "to-readtable"],
        "Copies FROM-READTABLE into TO-READTABLE or a fresh readtable.";
    "GET-MACRO-CHARACTER": Function ["char", "&optional", "readtable"],
        "Returns the reader macro function and non-terminating-p for CHAR.";
    "SET-MACRO-CHARACTER": Function [
        "char",
        "new-function",
        "&optional",
        "non-terminating-p",
        "readtable",
    ],
        "Associates reader macro function NEW-FUNCTION with CHAR.";
    "MAKE-DISPATCH-MACRO-CHARACTER": Function [
        "char",
        "&optional",
        "non-terminating-p",
        "readtable",
    ],
        "Makes CHAR a dispatching macro character in READTABLE.";
    "GET-DISPATCH-MACRO-CHARACTER": Function [
        "disp-char",
        "sub-char",
        "&optional",
        "readtable",
    ],
        "Returns the dispatch reader macro function for DISP-CHAR and SUB-CHAR.";
    "SET-DISPATCH-MACRO-CHARACTER": Function [
        "disp-char",
        "sub-char",
        "new-function",
        "&optional",
        "readtable",
    ],
        "Sets the dispatch reader macro function for DISP-CHAR and SUB-CHAR.";
    "SET-SYNTAX-FROM-CHAR": Function [
        "to-char",
        "from-char",
        "&optional",
        "to-readtable",
        "from-readtable",
    ],
        "Copies character syntax of FROM-CHAR to TO-CHAR.";
    "READTABLE-CASE": Function ["readtable"],
        "Returns the case-sensitivity mode (:UPCASE, :DOWNCASE, :PRESERVE, :INVERT) of READTABLE.";
    "DOCUMENTATION": Function ["x", "doc-type"],
        "Returns the documentation string of X for DOC-TYPE, or NIL.";
    "DESCRIBE": Function ["object", "&optional", "stream"],
        "Prints a description of OBJECT to STREAM.";
    "DESCRIBE-OBJECT": Function ["object", "stream"],
        "Generic function called by DESCRIBE to display information about OBJECT.";
    "INSPECT": Function ["object"],
        "Interactively inspects OBJECT.";
    "APROPOS": Function ["string", "&optional", "package"],
        "Prints symbols whose print names contain STRING.";
    "APROPOS-LIST": Function ["string", "&optional", "package"],
        "Returns a list of symbols whose print names contain STRING.";
    "DRIBBLE": Function ["&optional", "pathname"],
        "Starts or stops recording interactive I/O to PATHNAME.";
    "ED": Function ["&optional", "x"],
        "Invokes the implementation's editor if provided.";
    "ROOM": Function ["&optional", "x"],
        "Prints information about internal memory usage.";
    "SLEEP": Function ["seconds"],
        "Suspends execution for approximately SECONDS.";
    "GET-UNIVERSAL-TIME": Function [],
        "Returns the current Universal Time in seconds since 1900-01-01 00:00:00 UTC.";
    "GET-DECODED-TIME": Function [],
        "Returns the current time decoded into nine calendar components.";
    "DECODE-UNIVERSAL-TIME": Function ["universal-time", "&optional", "time-zone"],
        "Decodes UNIVERSAL-TIME into second, minute, hour, date, month, year, day, dst-p, zone.";
    "ENCODE-UNIVERSAL-TIME": Function [
        "second",
        "minute",
        "hour",
        "date",
        "month",
        "year",
        "&optional",
        "time-zone",
    ],
        "Converts decoded calendar time components into Universal Time.";
    "GET-INTERNAL-REAL-TIME": Function [],
        "Returns internal wall-clock time in internal time units.";
    "GET-INTERNAL-RUN-TIME": Function [],
        "Returns internal CPU run time in internal time units.";
    "LISP-IMPLEMENTATION-TYPE": Function [],
        "Returns a string identifying the Common Lisp implementation type.";
    "LISP-IMPLEMENTATION-VERSION": Function [],
        "Returns a string identifying the Common Lisp implementation version.";
    "MACHINE-INSTANCE": Function [],
        "Returns a string identifying the computer hardware instance.";
    "MACHINE-TYPE": Function [],
        "Returns a string identifying the computer hardware type.";
    "MACHINE-VERSION": Function [],
        "Returns a string identifying the computer hardware version.";
    "SOFTWARE-TYPE": Function [],
        "Returns a string identifying the operating system type.";
    "SOFTWARE-VERSION": Function [],
        "Returns a string identifying the operating system version.";
    "SHORT-SITE-NAME": Function [],
        "Returns the short site name string, or NIL.";
    "LONG-SITE-NAME": Function [],
        "Returns the long site name string, or NIL.";
    "PROVIDE": Function ["module-name"],
        "Adds MODULE-NAME to *MODULES*.";
    "REQUIRE": Function ["module-name", "&optional", "pathname-list"],
        "Loads a module if not already present in *MODULES*.";
    "Y-OR-N-P": Function ["&optional", "control", "&rest", "arguments"],
        "Prompts the user on *QUERY-IO* for a Y or N response.";
    "YES-OR-NO-P": Function ["&optional", "control", "&rest", "arguments"],
        "Prompts the user on *QUERY-IO* for a YES or NO response.";
}
