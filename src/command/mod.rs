use std::fmt;

use futures::{future::Either, prelude::*};

use either;

use completion::CompletionSymbol;
use gluon::{self,
    base::{
        ast::{Expr, SpannedExpr},
        filename_to_module,
        kind::ArcKind,
        pos::{BytePos, Spanned},
        symbol::Symbol,
        types::{ArcType, BuiltinType, Type},
    },
    import::Import,
    RootedThread, Thread,
};

use jsonrpc_core::IoHandler;

use codespan;

use url::Url;

use languageserver_types::{
    CompletionItemKind, Documentation, Location, MarkupContent, MarkupKind, Position,
    SymbolInformation, SymbolKind,
};

use codespan_lsp::{byte_span_to_range, position_to_byte_index};

use {
    check_importer::{CheckImporter, Module},
    name::strip_file_prefix_with_thread,
    rpc::ServerError,
    server::Handler,
};

pub mod completion;
pub mod document_highlight;
pub mod document_symbols;
pub mod formatting;
pub mod hover;
pub mod initialize;
pub mod signature_help;
pub mod symbol;

fn type_to_completion_item_kind(typ: &ArcType) -> CompletionItemKind {
    match **typ {
        _ if typ.as_function().is_some() => CompletionItemKind::Function,
        Type::Alias(ref alias) => type_to_completion_item_kind(alias.unresolved_type()),
        Type::App(ref f, _) => type_to_completion_item_kind(f),
        Type::Variant(_) => CompletionItemKind::Enum,
        Type::Record(_) => CompletionItemKind::Module,
        _ => CompletionItemKind::Variable,
    }
}

fn ident_to_completion_item_kind(
    id: &str,
    typ_or_kind: either::Either<&ArcKind, &ArcType>,
) -> CompletionItemKind {
    match typ_or_kind {
        either::Either::Left(_) => CompletionItemKind::Class,
        either::Either::Right(typ) => {
            if id.starts_with(char::is_uppercase) {
                CompletionItemKind::Constructor
            } else {
                type_to_completion_item_kind(typ)
            }
        }
    }
}

fn make_documentation<T>(typ: Option<T>, comment: &str) -> Documentation
where
    T: fmt::Display,
{
    use std::fmt::Write;
    let mut value = String::new();
    if let Some(typ) = typ {
        write!(value, "```gluon\n{}\n```\n", typ).unwrap();
    }
    value.push_str(comment);

    Documentation::MarkupContent(MarkupContent {
        kind: MarkupKind::Markdown,
        value,
    })
}

fn completion_symbol_to_symbol_information(
    source: &codespan::FileMap,
    symbol: Spanned<CompletionSymbol, BytePos>,
    uri: Url,
) -> Result<SymbolInformation, ServerError<()>> {
    let (kind, name) = match symbol.value {
        CompletionSymbol::Type { ref name, .. } => (SymbolKind::Class, name),
        CompletionSymbol::Value {
            ref name,
            ref typ,
            ref expr,
        } => {
            let kind = expr_to_kind(expr, typ);
            (kind, name)
        }
    };
    Ok(SymbolInformation {
        kind,
        location: Location {
            uri,
            range: byte_span_to_range(source, symbol.span)?,
        },
        name: name.declared_name().to_string(),
        container_name: None,
    })
}

fn expr_to_kind(expr: &SpannedExpr<Symbol>, typ: &ArcType) -> SymbolKind {
    match expr.value {
        // import! "std/prelude.glu" will replace itself with a symbol like `std.prelude
        Expr::Ident(ref id) if id.name.declared_name().contains('.') => SymbolKind::Module,
        _ => type_to_kind(typ),
    }
}

fn type_to_kind(typ: &ArcType) -> SymbolKind {
    match **typ {
        _ if typ.as_function().is_some() => SymbolKind::Function,
        Type::Ident(ref id) if id.declared_name() == "Bool" => SymbolKind::Boolean,
        Type::Alias(ref alias) if alias.name.declared_name() == "Bool" => SymbolKind::Boolean,
        Type::Builtin(builtin) => match builtin {
            BuiltinType::Char | BuiltinType::String => SymbolKind::String,
            BuiltinType::Byte | BuiltinType::Int | BuiltinType::Float => SymbolKind::Number,
            BuiltinType::Array => SymbolKind::Array,
            BuiltinType::Function => SymbolKind::Function,
        },
        _ => SymbolKind::Variable,
    }
}

fn retrieve_expr_future<'a, 'b, F, Q, R>(
    thread: &'a Thread,
    text_document_uri: &'b Url,
    f: F,
) -> impl Future<Item = R, Error = ServerError<()>> + 'static
where
    F: FnOnce(&mut Module) -> Q,
    Q: IntoFuture<Item = R, Error = ServerError<()>>,
    Q::Future: Send + 'static,
    R: Send + 'static,
{
    let filename = strip_file_prefix_with_thread(thread, text_document_uri);
    let module = filename_to_module(&filename);
    let import = thread.get_macros().get("import").expect("Import macro");
    let import = import
        .downcast_ref::<Import<CheckImporter>>()
        .expect("Check importer");
    let mut importer = import.importer.0.lock().unwrap();
    match importer.get_mut(&module) {
        Some(source_module) => return Either::A(f(source_module).into_future()),
        None => (),
    }
    Either::B(
        Err(ServerError {
            message: format!(
                "Module `{}` is not defined\n{:?}",
                module,
                importer.keys().collect::<Vec<_>>()
            ),
            data: None,
        })
        .into_future(),
    )
}

fn retrieve_expr<F, R>(thread: &Thread, text_document_uri: &Url, f: F) -> Result<R, ServerError<()>>
where
    F: FnOnce(&mut Module) -> Result<R, ServerError<()>>,
{
    let filename = strip_file_prefix_with_thread(thread, text_document_uri);
    let module = filename_to_module(&filename);
    let import = thread.get_macros().get("import").expect("Import macro");
    let import = import
        .downcast_ref::<Import<CheckImporter>>()
        .expect("Check importer");
    let mut importer = import.importer.0.lock().unwrap();
    match importer.get_mut(&module) {
        Some(source_module) => return f(source_module),
        None => (),
    }
    Err(ServerError {
        message: format!(
            "Module `{}` is not defined\n{:?}",
            module,
            importer.keys().collect::<Vec<_>>()
        ),
        data: None,
    })
}

fn retrieve_expr_with_pos<F, R>(
    thread: &Thread,
    text_document_uri: &Url,
    position: &Position,
    f: F,
) -> Result<R, ServerError<()>>
where
    F: FnOnce(&Module, BytePos) -> Result<R, ServerError<()>>,
{
    retrieve_expr(thread, text_document_uri, |module| {
        let byte_index = position_to_byte_index(&module.source, position)?;

        f(module, byte_index)
    })
}
