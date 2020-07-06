use std::fmt;

use crate::completion::{CompletionSymbol, CompletionSymbolContent};
use crate::either;

use gluon::{
    self,
    base::{
        ast::{Expr, SpannedExpr},
        filename_to_module,
        kind::ArcKind,
        pos::{BytePos, Spanned},
        symbol::Symbol,
        types::{ArcType, BuiltinType, Type},
    },
    import::Import,
    RootedThread, Thread, ThreadExt,
};

use {
    futures::prelude::*,
    jsonrpc_core::IoHandler,
    lsp_types::{
        CompletionItemKind, DocumentSymbol, Documentation, Location, MarkupContent, MarkupKind,
        Position, SymbolInformation, SymbolKind,
    },
    url::Url,
};

use crate::{
    byte_span_to_range,
    check_importer::{CheckImporter, Module},
    name::strip_file_prefix_with_thread,
    position_to_byte_index,
    rpc::ServerError,
    server::Handler,
};

pub mod completion;
pub mod definition;
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

fn completion_symbol_kind(symbol: &CompletionSymbol<'_, '_>) -> SymbolKind {
    match symbol.content {
        CompletionSymbolContent::Type { .. } => SymbolKind::Class,
        CompletionSymbolContent::Value { typ, expr } => expr_to_kind(expr, typ),
    }
}

fn completion_symbol_to_document_symbol(
    source: &gluon::base::source::FileMap,
    symbol: &Spanned<CompletionSymbol<'_, '_>, BytePos>,
) -> Result<DocumentSymbol, ServerError<()>> {
    let kind = completion_symbol_kind(&symbol.value);
    let range = byte_span_to_range(source, symbol.span)?;
    Ok(DocumentSymbol {
        kind,
        range,
        selection_range: range,
        name: symbol.value.name.declared_name().to_string(),
        detail: Some(match &symbol.value.content {
            CompletionSymbolContent::Type { alias } => alias.unresolved_type().to_string(),
            CompletionSymbolContent::Value { typ, .. } => typ.to_string(),
        }),
        deprecated: Default::default(),
        children: if symbol.value.children.is_empty() {
            None
        } else {
            Some(
                symbol
                    .value
                    .children
                    .iter()
                    .map(|child| completion_symbol_to_document_symbol(source, child))
                    .collect::<Result<_, _>>()?,
            )
        },
    })
}

fn completion_symbol_to_symbol_information(
    source: &gluon::base::source::FileMap,
    symbol: Spanned<CompletionSymbol<'_, '_>, BytePos>,
    uri: Url,
) -> Result<SymbolInformation, ServerError<()>> {
    let kind = completion_symbol_kind(&symbol.value);
    Ok(SymbolInformation {
        kind,
        location: Location {
            uri,
            range: byte_span_to_range(source, symbol.span)?,
        },
        name: symbol.value.name.declared_name().to_string(),
        container_name: None,
        deprecated: Default::default(),
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
        Type::Ident(ref id) if id.name.declared_name() == "Bool" => SymbolKind::Boolean,
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

async fn retrieve_expr<F, R>(
    thread: &Thread,
    text_document_uri: &Url,
    f: F,
) -> Result<R, ServerError<()>>
where
    F: FnOnce(&Module) -> Result<R, ServerError<()>>,
{
    let filename = strip_file_prefix_with_thread(&thread, &text_document_uri);
    let module = filename_to_module(&filename);
    let import = thread.get_macros().get("import").expect("Import macro");
    let import = import
        .downcast_ref::<Import<CheckImporter>>()
        .expect("Check importer");
    match import.importer.module(&thread, &module).await {
        Some(ref source_module) => return f(source_module),
        None => (),
    }
    Err(ServerError {
        message: {
            let m = import.importer.0.lock().await;
            format!(
                "Module `{}` is not defined\n{:?}",
                module,
                m.keys().collect::<Vec<_>>()
            )
        },
        data: None,
    })
}

async fn retrieve_expr_with_pos<F, R>(
    thread: &Thread,
    text_document_uri: &Url,
    position: &Position,
    f: F,
) -> Result<R, ServerError<()>>
where
    F: FnOnce(&Module, BytePos) -> Result<R, ServerError<()>>,
{
    retrieve_expr(thread, text_document_uri, move |module| {
        let byte_index = position_to_byte_index(&*module.source, position)?;

        f(module, byte_index)
    })
    .await
}
