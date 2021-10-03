use std::fmt;

use crate::{
    completion::{CompletionSymbol, CompletionSymbolContent},
    either,
};

use gluon::{
    self,
    base::{
        ast::{Expr, SpannedExpr},
        filename_to_module,
        kind::ArcKind,
        pos::{BytePos, Spanned},
        symbol::Symbol,
        types::{ArcType, BuiltinType, Type, TypeExt, TypePtr},
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
        _ if typ.remove_forall().as_function().is_some() => CompletionItemKind::Function,
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
        CompletionSymbolContent::Type { typ } => match &**typ {
            Type::Variant(_) => SymbolKind::Enum,
            _ => SymbolKind::Class,
        },
        CompletionSymbolContent::Value { typ, kind: _, expr } => match expr {
            Some(expr) => expr_to_kind(expr, typ),
            None => SymbolKind::Variable,
        },
    }
}

fn completion_symbols_to_document_symbols(
    source: &gluon::base::source::FileMap,
    symbols: &[Spanned<CompletionSymbol<'_, '_>, BytePos>],
) -> Result<Vec<DocumentSymbol>, ServerError<()>> {
    completion_symbols_to_document_symbols_inner(source, symbols, None)
}

fn completion_symbols_to_document_symbols_inner(
    source: &gluon::base::source::FileMap,
    symbols: &[Spanned<CompletionSymbol<'_, '_>, BytePos>],
    parent_kind: Option<SymbolKind>,
) -> Result<Vec<DocumentSymbol>, ServerError<()>> {
    symbols
        .iter()
        .filter(|symbol| match &symbol.value.content {
            CompletionSymbolContent::Value { kind, .. } => match kind {
                // Skip function parameters
                gluon_completion::CompletionValueKind::Parameter => false,
                gluon_completion::CompletionValueKind::Binding => true,
            },
            CompletionSymbolContent::Type { .. } => true,
        })
        .map(|symbol| completion_symbol_to_document_symbol(source, symbol, parent_kind))
        .collect()
}

fn completion_symbol_to_document_symbol(
    source: &gluon::base::source::FileMap,
    symbol: &Spanned<CompletionSymbol<'_, '_>, BytePos>,
    parent_kind: Option<SymbolKind>,
) -> Result<DocumentSymbol, ServerError<()>> {
    let kind = parent_kind
        .and_then(|parent_kind| match parent_kind {
            SymbolKind::Enum => Some(SymbolKind::EnumMember),
            _ => None,
        })
        .unwrap_or_else(|| completion_symbol_kind(&symbol.value));
    let range = byte_span_to_range(source, symbol.span)?;
    #[allow(deprecated)]
    Ok(DocumentSymbol {
        kind,
        range,
        selection_range: range,
        name: symbol.value.name.declared_name().to_string(),
        detail: {
            let detail = match symbol.value.content {
                CompletionSymbolContent::Type { typ } => typ.display(1000000).to_string(),
                CompletionSymbolContent::Value { typ, .. } => typ.display(1000000).to_string(),
            };
            // Multiline renders badly in VS Code so skip detail if we happen to get newlines (such as for variant types)
            if detail.contains('\n') {
                None
            } else {
                Some(detail)
            }
        },
        deprecated: Default::default(),
        children: {
            let children = completion_symbols_to_document_symbols_inner(
                source,
                &symbol.value.children,
                Some(kind),
            )?;
            if children.is_empty() {
                None
            } else {
                Some(children)
            }
        },
        tags: Default::default(),
    })
}

fn completion_symbol_to_symbol_information(
    source: &gluon::base::source::FileMap,
    symbol: Spanned<CompletionSymbol<'_, '_>, BytePos>,
    uri: Url,
) -> Result<SymbolInformation, ServerError<()>> {
    let kind = completion_symbol_kind(&symbol.value);
    #[allow(deprecated)]
    Ok(SymbolInformation {
        kind,
        location: Location {
            uri,
            range: byte_span_to_range(source, symbol.span)?,
        },
        name: symbol.value.name.declared_name().to_string(),
        container_name: None,
        deprecated: Default::default(),
        tags: Default::default(),
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
        _ if typ.remove_forall().as_function().is_some() => SymbolKind::Function,
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
    if let Some(ref source_module) = import.importer.module(&thread, &module).await {
        return f(source_module);
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

async fn retrieve_module_from_url(
    thread: &Thread,
    text_document_uri: &Url,
) -> Result<Module, ServerError<()>> {
    let filename = strip_file_prefix_with_thread(&thread, &text_document_uri);
    let module = filename_to_module(&filename);
    retrieve_module(thread, &module).await
}

async fn retrieve_module(thread: &Thread, module: &str) -> Result<Module, ServerError<()>> {
    let import = thread.get_macros().get("import").expect("Import macro");
    let import = import
        .downcast_ref::<Import<CheckImporter>>()
        .expect("Check importer");
    if let Some(source_module) = import.importer.module(&thread, &module).await {
        Ok(source_module)
    } else {
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
}
