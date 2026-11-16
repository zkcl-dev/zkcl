use std::sync::LazyLock;

use rustc_hash::FxHashMap;

use super::{
    arrays, association, characters, coerce, debugging, deduplicate, environment,
    find_position_count, format, function_definitions, hash_tables, instances, list, mapping,
    numbers, packages, pathnames, plists, predicate_remove, printer, random, readtables, reduce,
    restart, search_mismatch, sequence, sequence_map, sets, simd, sort, streams, strings,
    task_scheduler, tree, treelist,
};
use crate::heap::Word;

#[derive(Clone, Copy)]
pub(crate) enum Primitive {
    Array(arrays::Op),
    Treelist(treelist::Op),
    Simd(simd::Op),
    Association(association::Op),
    Coerce(coerce::Op),
    Deduplicate(deduplicate::Op),
    PredicateRemove(predicate_remove::Op),
    Printer(printer::PrinterPrimitive),
    Format(format::OutputPrimitive),
    Environment(environment::Op),
    Debug(debugging::Op),
    Async(task_scheduler::TaskPrimitive),
    List(list::Op),
    Mapping(mapping::Op),
    HashTable(hash_tables::Op),
    Instance(instances::Op),
    Package(packages::Op),
    Pathname(pathnames::Op),
    Plist(plists::Op),
    Reduce(reduce::Op),
    ElementSearch(find_position_count::Op),
    SubsequenceComparison(search_mismatch::Op),
    Sets(sets::Op),
    Sort(sort::Op),
    Sequence(sequence::Op),
    SequenceMap(sequence_map::Op),
    Tree(tree::Op),
    String(strings::Op),
    Character(characters::Op),
    Readtable(readtables::Op),
    Identity,
    Eq,
    Eql,
    Equal,
    Equalp,
    Sxhash,
    Number(numbers::Op),
    Random(random::Op),
    Values,
    ValuesList,
    Constantly,
    Complement,
    Funcall,
    Apply,
    Eval,
    Load,
    LoadLogicalPathnameTranslations,
    Provide,
    Require,
    LispImplementationType,
    LispImplementationVersion,
    CompileFile,
    Compile,
    FunctionDefinition(function_definitions::DefinitionOp),
    SetFdefinition,
    SetSymbolFunction,
    Fmakunbound,
    Constantp,
    Proclaim,
    FunctionLambdaExpression,
    FunctionLambdaList,
    RegisterDeclarationHandler,
    DeclarationInformation,
    FunctionP,
    ReadFromString,
    Stream(streams::Op),
    SymbolValue,
    SymbolName,
    SymbolP,
    KeywordP,
    SymbolPackage,
    MakeSymbol,
    CopySymbol,
    Gensym,
    Gentemp,
    Boundp,
    Makunbound,
    MacroFunction,
    Macroexpand,
    MacroexpandOne,
    CompilerMacroFunction,
    SetCompilerMacroFunction,
    GetSetfExpansion,
    Set,
    MakeCondition,
    Signal,
    Error,
    Cerror,
    InvokeDebugger,
    Warn,
    Typep,
    Subtypep,
    TypeOf,
    PatternTag,
    PatternField,
    ClassOf,
    ClassName,
    FindClass,
    DefineNamedClass,
    CopyStructure,
    Documentation,
    SetDocumentation,
    FormatControl,
    FormatArguments,
    TypeErrorDatum,
    TypeErrorExpectedType,
    CellErrorName,
    PackageErrorPackage,
    PrintNotReadableObject,
    FileErrorPathname,
    StreamErrorStream,
    ArithmeticErrorOperation,
    ArithmeticErrorOperands,
    Truename,
    ProbeFile,
    Directory,
    DeleteFile,
    FileWriteDate,
    FileAuthor,
    RenameFile,
    EnsureDirectoriesExist,
    FindRestart,
    ComputeRestarts,
    InvokeRestart,
    InvokeRestartInteractively,
    RestartName,
    CallWithSimpleRestart,
    StandardRestart(restart::StandardRestart),
}

pub(crate) fn setf_primitive(accessor: &str) -> Option<Word> {
    arrays::setf_primitive(accessor)
        .or_else(|| hash_tables::setf_primitive(accessor))
        .or_else(|| characters::setf_primitive(accessor))
        .or_else(|| readtables::setf_primitive(accessor))
        .or_else(|| pathnames::setf_primitive(accessor))
        .or_else(|| instances::setf_primitive(accessor))
        .or_else(|| plists::setf_primitive(accessor))
        .or_else(|| match accessor {
            "FDEFINITION" => Primitive::named("(SETF FDEFINITION)"),
            "SYMBOL-FUNCTION" => Primitive::named("(SETF SYMBOL-FUNCTION)"),
            "SUBSEQ" => Primitive::named("(SETF SUBSEQ)"),
            "DOCUMENTATION" => Primitive::named("(SETF DOCUMENTATION)"),
            "COMPILER-MACRO-FUNCTION" => Primitive::named("(SETF COMPILER-MACRO-FUNCTION)"),
            _ => None,
        })
}

const PRIMITIVES: &[(&str, Primitive)] = &[
    (
        "%DEFAULT-PRINT-OBJECT",
        Primitive::Printer(printer::PrinterPrimitive::DefaultMethod),
    ),
    (
        "WRITE-TO-STRING",
        Primitive::Printer(printer::PrinterPrimitive::Write),
    ),
    (
        "PRIN1-TO-STRING",
        Primitive::Printer(printer::PrinterPrimitive::Prin1),
    ),
    (
        "PRINC-TO-STRING",
        Primitive::Printer(printer::PrinterPrimitive::Princ),
    ),
    ("FORMAT", Primitive::Format(format::OutputPrimitive::Format)),
    ("PRIN1", Primitive::Format(format::OutputPrimitive::Prin1)),
    ("PRINC", Primitive::Format(format::OutputPrimitive::Princ)),
    ("PRINT", Primitive::Format(format::OutputPrimitive::Print)),
    ("TERPRI", Primitive::Format(format::OutputPrimitive::Terpri)),
    (
        "FRESH-LINE",
        Primitive::Format(format::OutputPrimitive::FreshLine),
    ),
    (
        "PRINT-NOT-READABLE-OBJECT",
        Primitive::PrintNotReadableObject,
    ),
    ("IDENTITY", Primitive::Identity),
    ("EQ", Primitive::Eq),
    ("EQL", Primitive::Eql),
    ("EQUAL", Primitive::Equal),
    ("EQUALP", Primitive::Equalp),
    ("SXHASH", Primitive::Sxhash),
    ("VALUES", Primitive::Values),
    ("VALUES-LIST", Primitive::ValuesList),
    ("CONSTANTLY", Primitive::Constantly),
    ("COMPLEMENT", Primitive::Complement),
    ("FUNCALL", Primitive::Funcall),
    ("APPLY", Primitive::Apply),
    ("EVAL", Primitive::Eval),
    ("LOAD", Primitive::Load),
    (
        "LOAD-LOGICAL-PATHNAME-TRANSLATIONS",
        Primitive::LoadLogicalPathnameTranslations,
    ),
    ("PROVIDE", Primitive::Provide),
    ("REQUIRE", Primitive::Require),
    (
        "LISP-IMPLEMENTATION-TYPE",
        Primitive::LispImplementationType,
    ),
    (
        "LISP-IMPLEMENTATION-VERSION",
        Primitive::LispImplementationVersion,
    ),
    ("COMPILE-FILE", Primitive::CompileFile),
    ("COMPILE", Primitive::Compile),
    (
        "FBOUNDP",
        Primitive::FunctionDefinition(function_definitions::DefinitionOp::Fboundp),
    ),
    (
        "SPECIAL-OPERATOR-P",
        Primitive::FunctionDefinition(function_definitions::DefinitionOp::SpecialOperatorP),
    ),
    (
        "SYMBOL-FUNCTION",
        Primitive::FunctionDefinition(function_definitions::DefinitionOp::SymbolFunction),
    ),
    (
        "FDEFINITION",
        Primitive::FunctionDefinition(function_definitions::DefinitionOp::Fdefinition),
    ),
    ("(SETF FDEFINITION)", Primitive::SetFdefinition),
    ("(SETF SYMBOL-FUNCTION)", Primitive::SetSymbolFunction),
    ("FMAKUNBOUND", Primitive::Fmakunbound),
    ("CONSTANTP", Primitive::Constantp),
    ("PROCLAIM", Primitive::Proclaim),
    ("ZK:FUNCTION-LAMBDA-LIST", Primitive::FunctionLambdaList),
    (
        "ZK:%REGISTER-DECLARATION-HANDLER",
        Primitive::RegisterDeclarationHandler,
    ),
    (
        "ZK:DECLARATION-INFORMATION",
        Primitive::DeclarationInformation,
    ),
    (
        "FUNCTION-LAMBDA-EXPRESSION",
        Primitive::FunctionLambdaExpression,
    ),
    ("COMPILED-FUNCTION-P", Primitive::FunctionP),
    ("FUNCTIONP", Primitive::FunctionP),
    ("READ-FROM-STRING", Primitive::ReadFromString),
    ("SYMBOL-VALUE", Primitive::SymbolValue),
    ("SYMBOL-NAME", Primitive::SymbolName),
    ("SYMBOLP", Primitive::SymbolP),
    ("KEYWORDP", Primitive::KeywordP),
    ("SYMBOL-PACKAGE", Primitive::SymbolPackage),
    ("MAKE-SYMBOL", Primitive::MakeSymbol),
    ("COPY-SYMBOL", Primitive::CopySymbol),
    ("GENSYM", Primitive::Gensym),
    ("GENTEMP", Primitive::Gentemp),
    ("BOUNDP", Primitive::Boundp),
    ("MAKUNBOUND", Primitive::Makunbound),
    ("MACRO-FUNCTION", Primitive::MacroFunction),
    ("MACROEXPAND", Primitive::Macroexpand),
    ("MACROEXPAND-1", Primitive::MacroexpandOne),
    ("COMPILER-MACRO-FUNCTION", Primitive::CompilerMacroFunction),
    (
        "(SETF COMPILER-MACRO-FUNCTION)",
        Primitive::SetCompilerMacroFunction,
    ),
    ("GET-SETF-EXPANSION", Primitive::GetSetfExpansion),
    ("SET", Primitive::Set),
    ("MAKE-CONDITION", Primitive::MakeCondition),
    ("SIGNAL", Primitive::Signal),
    ("INVOKE-DEBUGGER", Primitive::InvokeDebugger),
    ("ERROR", Primitive::Error),
    ("CERROR", Primitive::Cerror),
    ("WARN", Primitive::Warn),
    ("TYPEP", Primitive::Typep),
    ("SUBTYPEP", Primitive::Subtypep),
    ("TYPE-OF", Primitive::TypeOf),
    ("ZK::%PATTERN-TAG", Primitive::PatternTag),
    ("ZK::%PATTERN-FIELD", Primitive::PatternField),
    ("CLASS-OF", Primitive::ClassOf),
    ("CLASS-NAME", Primitive::ClassName),
    ("FIND-CLASS", Primitive::FindClass),
    ("COPY-STRUCTURE", Primitive::CopyStructure),
    ("DOCUMENTATION", Primitive::Documentation),
    ("(SETF DOCUMENTATION)", Primitive::SetDocumentation),
    ("SIMPLE-CONDITION-FORMAT-CONTROL", Primitive::FormatControl),
    (
        "SIMPLE-CONDITION-FORMAT-ARGUMENTS",
        Primitive::FormatArguments,
    ),
    ("TYPE-ERROR-DATUM", Primitive::TypeErrorDatum),
    ("TYPE-ERROR-EXPECTED-TYPE", Primitive::TypeErrorExpectedType),
    ("CELL-ERROR-NAME", Primitive::CellErrorName),
    ("PACKAGE-ERROR-PACKAGE", Primitive::PackageErrorPackage),
    ("FILE-ERROR-PATHNAME", Primitive::FileErrorPathname),
    ("STREAM-ERROR-STREAM", Primitive::StreamErrorStream),
    (
        "ARITHMETIC-ERROR-OPERATION",
        Primitive::ArithmeticErrorOperation,
    ),
    (
        "ARITHMETIC-ERROR-OPERANDS",
        Primitive::ArithmeticErrorOperands,
    ),
    ("TRUENAME", Primitive::Truename),
    ("PROBE-FILE", Primitive::ProbeFile),
    ("DIRECTORY", Primitive::Directory),
    ("DELETE-FILE", Primitive::DeleteFile),
    ("FILE-WRITE-DATE", Primitive::FileWriteDate),
    ("FILE-AUTHOR", Primitive::FileAuthor),
    ("RENAME-FILE", Primitive::RenameFile),
    (
        "ENSURE-DIRECTORIES-EXIST",
        Primitive::EnsureDirectoriesExist,
    ),
    ("FIND-RESTART", Primitive::FindRestart),
    ("COMPUTE-RESTARTS", Primitive::ComputeRestarts),
    ("INVOKE-RESTART", Primitive::InvokeRestart),
    (
        "INVOKE-RESTART-INTERACTIVELY",
        Primitive::InvokeRestartInteractively,
    ),
    ("RESTART-NAME", Primitive::RestartName),
    (
        "%CALL-WITH-SIMPLE-RESTART",
        Primitive::CallWithSimpleRestart,
    ),
    (
        restart::StandardRestart::Abort.name(),
        Primitive::StandardRestart(restart::StandardRestart::Abort),
    ),
    (
        restart::StandardRestart::Continue.name(),
        Primitive::StandardRestart(restart::StandardRestart::Continue),
    ),
    (
        restart::StandardRestart::MuffleWarning.name(),
        Primitive::StandardRestart(restart::StandardRestart::MuffleWarning),
    ),
    (
        restart::StandardRestart::StoreValue.name(),
        Primitive::StandardRestart(restart::StandardRestart::StoreValue),
    ),
    (
        restart::StandardRestart::UseValue.name(),
        Primitive::StandardRestart(restart::StandardRestart::UseValue),
    ),
];

impl Primitive {
    pub(crate) fn guarded_integer_bits(op: zkcl_bytecode::native_abi::IntegerBitPrimitive) -> Word {
        use zkcl_bytecode::native_abi::IntegerBitPrimitive;

        static BINDINGS: LazyLock<[Word; IntegerBitPrimitive::COUNT]> = LazyLock::new(|| {
            IntegerBitPrimitive::ALL
                .map(|op| Primitive::named(op.name()).expect("registered integer bit primitive"))
        });
        BINDINGS[op as usize]
    }

    /// Shared by interpreted and native tests of a live function binding.
    pub(crate) fn guarded_binding(index: usize) -> Option<Word> {
        static PRIMITIVES: LazyLock<[Word; 19]> = LazyLock::new(|| {
            [
                "+",
                "-",
                "*",
                "/",
                "TYPEP",
                "FUNCALL",
                "1+",
                "1-",
                "=",
                "/=",
                "<",
                "<=",
                ">",
                ">=",
                "ZK-SIMD:ADD",
                "ZK-SIMD:SUB",
                "ZK-SIMD:BIT-AND",
                "ZK-SIMD:BIT-OR",
                "ZK-SIMD:BIT-XOR",
            ]
            .map(|name| Primitive::named(name).expect("known primitive"))
        });
        PRIMITIVES.get(index).copied()
    }

    pub(crate) fn named(name: &str) -> Option<Word> {
        static BY_NAME: LazyLock<FxHashMap<&'static str, usize>> = LazyLock::new(|| {
            Primitive::table()
                .iter()
                .enumerate()
                .map(|(index, &(key, _))| (key, index))
                .collect()
        });
        BY_NAME.get(name).copied().map(Word::primitive)
    }

    pub(crate) fn compiled_name(word: Word) -> Option<&'static str> {
        Self::table()
            .get(word.primitive_index()?)
            .map(|(name, _)| *name)
    }

    pub(super) fn decode(word: Word) -> Option<Self> {
        Self::table()
            .get(word.primitive_index()?)
            .map(|(_, op)| *op)
    }

    #[inline(always)]
    fn table() -> &'static [(&'static str, Self)] {
        static TABLE: LazyLock<Vec<(&'static str, Primitive)>> = LazyLock::new(|| {
            fn group<T: Copy>(
                operations: &[(&'static str, T)],
                wrap: fn(T) -> Primitive,
            ) -> impl Iterator<Item = (&'static str, Primitive)> {
                operations.iter().map(move |&(name, op)| (name, wrap(op)))
            }
            let mut entries = PRIMITIVES.to_vec();
            entries.extend(group(format::PRIMITIVES, Primitive::Format));
            entries.extend(group(environment::PRIMITIVES, Primitive::Environment));
            entries.extend(group(numbers::PRIMITIVES, Primitive::Number));
            entries.extend(group(list::PRIMITIVES, Primitive::List));
            entries.extend(group(arrays::PRIMITIVES, Primitive::Array));
            entries.extend(group(association::PRIMITIVES, Primitive::Association));
            entries.extend(group(deduplicate::PRIMITIVES, Primitive::Deduplicate));
            entries.extend(group(
                find_position_count::PRIMITIVES,
                Primitive::ElementSearch,
            ));
            entries.extend(group(sets::PRIMITIVES, Primitive::Sets));
            entries.extend(group(sort::PRIMITIVES, Primitive::Sort));
            entries.extend(group(reduce::PRIMITIVES, Primitive::Reduce));
            entries.extend(group(
                predicate_remove::PRIMITIVES,
                Primitive::PredicateRemove,
            ));
            entries.extend(group(packages::PRIMITIVES, Primitive::Package));
            entries.extend(group(pathnames::PRIMITIVES, Primitive::Pathname));
            entries.extend(group(strings::PRIMITIVES, Primitive::String));
            entries.extend(group(characters::PRIMITIVES, Primitive::Character));
            entries.extend(group(mapping::PRIMITIVES, Primitive::Mapping));
            entries.extend(group(hash_tables::PRIMITIVES, Primitive::HashTable));
            entries.extend(group(instances::PRIMITIVES, Primitive::Instance));
            entries.extend(group(plists::PRIMITIVES, Primitive::Plist));
            entries.extend(group(streams::PRIMITIVES, Primitive::Stream));
            entries.extend(group(readtables::PRIMITIVES, Primitive::Readtable));
            entries.extend(group(sequence::PRIMITIVES, Primitive::Sequence));
            entries.extend(group(sequence_map::PRIMITIVES, Primitive::SequenceMap));
            entries.extend(group(random::PRIMITIVES, Primitive::Random));
            entries.extend(group(coerce::PRIMITIVES, Primitive::Coerce));
            entries.extend(group(tree::PRIMITIVES, Primitive::Tree));
            entries.extend(group(
                search_mismatch::PRIMITIVES,
                Primitive::SubsequenceComparison,
            ));
            entries.extend(group(task_scheduler::PRIMITIVES, Primitive::Async));
            entries.extend(group(simd::PRIMITIVES, Primitive::Simd));
            entries.extend(group(treelist::PRIMITIVES, Primitive::Treelist));
            entries.push(("ZK-MOP::%DEFINE-NAMED-CLASS", Primitive::DefineNamedClass));
            entries.extend(group(debugging::PRIMITIVES, Primitive::Debug));
            entries
        });
        &TABLE
    }

    pub(super) fn accepts(self, count: usize) -> bool {
        match self {
            Self::EnsureDirectoriesExist => count >= 1,
            Self::Number(op) => op.accepts(count),
            Self::Random(op) => op.accepts(count),
            Self::Stream(op) => op.accepts(count),
            Self::Array(op) => op.accepts(count),
            Self::Treelist(op) => op.accepts(count),
            Self::Simd(op) => op.accepts(count),
            Self::Association(_) => count >= 2,
            Self::Deduplicate(_) => count >= 1,
            Self::PredicateRemove(op) => op.accepts(count),
            Self::Sort(op) => op.accepts(count),
            Self::Printer(op) => op.accepts(count),
            Self::Format(op) => op.accepts(count),
            Self::Environment(op) => op.accepts(count),
            Self::Debug(op) => op.arity() == count,
            Self::Async(op) => op.accepts(count),
            Self::List(op) => op.accepts(count),
            Self::HashTable(op) => op.accepts(count),
            Self::Instance(op) => op.accepts(count),
            Self::Package(op) => op.accepts(count),
            Self::Pathname(op) => op.accepts(count),
            Self::Plist(op) => op.accepts(count),
            Self::ElementSearch(_) => count >= 2,
            Self::SubsequenceComparison(op) => op.accepts(count),
            Self::Tree(op) => op.accepts(count),
            Self::Sets(op) => op.accepts(count),
            Self::Reduce(op) => op.accepts(count),
            Self::Sequence(op) => op.accepts(count),
            Self::SequenceMap(op) => op.accepts(count),
            Self::Coerce(op) => op.accepts(count),
            Self::String(op) => op.accepts(count),
            Self::Character(op) => op.accepts(count),
            Self::Readtable(op) => op.accepts(count),
            Self::PatternField
            | Self::Eq
            | Self::Eql
            | Self::Equal
            | Self::Equalp
            | Self::Set
            | Self::SetFdefinition
            | Self::SetSymbolFunction
            | Self::RenameFile
            | Self::DefineNamedClass
            | Self::Documentation => count == 2,
            Self::SetDocumentation => count == 3,
            Self::Typep | Self::Subtypep => (2..=3).contains(&count),
            Self::FindClass => (1..=3).contains(&count),
            Self::SymbolValue
            | Self::Identity
            | Self::TypeOf
            | Self::PatternTag
            | Self::ClassOf
            | Self::ClassName
            | Self::CopyStructure
            | Self::Sxhash
            | Self::SymbolName
            | Self::SymbolP
            | Self::KeywordP
            | Self::FunctionP
            | Self::ValuesList
            | Self::Constantly
            | Self::Complement
            | Self::Fmakunbound
            | Self::FunctionLambdaExpression
            | Self::FunctionLambdaList
            | Self::FunctionDefinition(_)
            | Self::SymbolPackage
            | Self::MakeSymbol
            | Self::Boundp
            | Self::Makunbound
            | Self::FormatControl
            | Self::FormatArguments
            | Self::TypeErrorDatum
            | Self::TypeErrorExpectedType
            | Self::CellErrorName
            | Self::PackageErrorPackage
            | Self::PrintNotReadableObject
            | Self::FileErrorPathname
            | Self::StreamErrorStream
            | Self::ArithmeticErrorOperation
            | Self::ArithmeticErrorOperands
            | Self::Truename
            | Self::ProbeFile
            | Self::DeleteFile
            | Self::FileWriteDate
            | Self::FileAuthor
            | Self::InvokeDebugger
            | Self::RestartName
            | Self::InvokeRestartInteractively
            | Self::Proclaim
            | Self::Eval => count == 1,
            Self::Compile
            | Self::CopySymbol
            | Self::Constantp
            | Self::FindRestart
            | Self::MacroFunction
            | Self::CompilerMacroFunction
            | Self::Macroexpand
            | Self::GetSetfExpansion
            | Self::MacroexpandOne => (1..=2).contains(&count),
            Self::SetCompilerMacroFunction => (2..=3).contains(&count),
            Self::ComputeRestarts | Self::Gensym | Self::Gentemp => {
                count <= 1 || matches!(self, Self::Gentemp if count == 2)
            }
            Self::StandardRestart(restart) => restart.accepts(count),
            Self::Apply => count >= 2,
            Self::CallWithSimpleRestart => count == 3,
            Self::RegisterDeclarationHandler => count == 2,
            Self::DeclarationInformation => (1..=2).contains(&count),
            Self::Cerror => count >= 2,
            Self::Mapping(_) => count >= 2,
            Self::Load | Self::CompileFile => count >= 1,
            Self::LoadLogicalPathnameTranslations => count == 1,
            Self::Provide => count == 1,
            Self::Require => (1..=2).contains(&count),
            Self::LispImplementationType | Self::LispImplementationVersion => count == 0,
            Self::Funcall
            | Self::ReadFromString
            | Self::Directory
            | Self::MakeCondition
            | Self::Signal
            | Self::Error
            | Self::Warn
            | Self::InvokeRestart => count != 0,
            _ => true,
        }
    }

    pub(super) fn is_direct_leaf(self) -> bool {
        // Package operations dispatch through `packages::invoke` so that a
        // correctable package error can signal with a CONTINUE restart.
        if streams::gray_input::reader_stream_argument(self).is_some() {
            return false;
        }
        if let Self::Stream(op) = self
            && op.gray_entry().is_some()
        {
            return false;
        }
        !matches!(
            self,
            Self::Printer(_)
                | Self::Debug(_)
                | Self::Format(_)
                | Self::Stream(streams::Op::Princ | streams::Op::Prin1 | streams::Op::Print)
                | Self::Package(_)
                | Self::Funcall
                | Self::Apply
                | Self::Association(_)
                | Self::Deduplicate(_)
                | Self::PredicateRemove(_)
                | Self::ElementSearch(_)
                | Self::SubsequenceComparison(_)
                | Self::Tree(_)
                | Self::Sets(_)
                | Self::Sort(_)
                | Self::Reduce(_)
                | Self::Typep
                | Self::MakeCondition
                | Self::Signal
                | Self::InvokeDebugger
                | Self::Error
                | Self::CallWithSimpleRestart
                | Self::DefineNamedClass
                | Self::Cerror
                | Self::Warn
                | Self::FindRestart
                | Self::ComputeRestarts
                | Self::InvokeRestart
                | Self::InvokeRestartInteractively
                | Self::StandardRestart(_)
                | Self::Macroexpand
                | Self::MacroexpandOne
                | Self::Mapping(_)
                | Self::SequenceMap(_)
                | Self::HashTable(hash_tables::Op::Map)
                | Self::Instance(_)
                | Self::Eval
                | Self::Load
                | Self::LoadLogicalPathnameTranslations
                | Self::Require
                | Self::Compile
                | Self::CompileFile
                | Self::Environment(environment::Op::Sleep)
                | Self::Environment(environment::Op::Inspect)
                | Self::Async(_)
        )
    }
}
