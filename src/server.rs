use std::{
    fmt,
    sync::{Arc, Mutex},
};

use completion::{self, CompletionSymbol};
use gluon::{
    base::{
        ast::{Expr, SpannedExpr},
        filename_to_module,
        pos::{BytePos, Spanned},
        symbol::Symbol,
        types::{ArcType, BuiltinType, Type},
    },
    import::Import,
    Compiler, RootedThread, Thread,
};

use futures::{
    future::{self, Either},
    prelude::*,
    sync::{mpsc, oneshot},
};

use tokio;

use jsonrpc_core::{IoHandler, MetaIoHandler};

use url::Url;

use codespan_lsp::{byte_span_to_range, position_to_byte_index};

use languageserver_types::*;

use {
    check_importer::{CheckImporter, Module},
    diagnostics::DiagnosticsWorker,
    retrieve_expr, retrieve_expr_with_pos,
    rpc::{self, *},
    strip_file_prefix, strip_file_prefix_with_thread, BoxFuture, Completion, CompletionData,
    HoverCommand, Initialize,
};

trait Handler {
    fn add_async_method<T, U>(&mut self, _: Option<T>, method: U)
    where
        T: ::languageserver_types::request::Request,
        U: LanguageServerCommand<T::Params, Output = T::Result>,
        <U::Future as IntoFuture>::Future: Send + 'static,
        T::Params: serde::de::DeserializeOwned + 'static,
        T::Result: serde::Serialize;
    fn add_notification<T, U>(&mut self, _: Option<T>, notification: U)
    where
        T: ::languageserver_types::notification::Notification,
        T::Params: serde::de::DeserializeOwned + 'static,
        U: LanguageServerNotification<T::Params>;
}

impl Handler for IoHandler {
    fn add_async_method<T, U>(&mut self, _: Option<T>, method: U)
    where
        T: ::languageserver_types::request::Request,
        U: LanguageServerCommand<T::Params, Output = T::Result>,
        <U::Future as IntoFuture>::Future: Send + 'static,
        T::Params: serde::de::DeserializeOwned + 'static,
        T::Result: serde::Serialize,
    {
        MetaIoHandler::add_method(self, T::METHOD, ServerCommand::method(method))
    }
    fn add_notification<T, U>(&mut self, _: Option<T>, notification: U)
    where
        T: ::languageserver_types::notification::Notification,
        T::Params: serde::de::DeserializeOwned + 'static,
        U: LanguageServerNotification<T::Params>,
    {
        MetaIoHandler::add_notification(self, T::METHOD, ServerCommand::notification(notification))
    }
}

macro_rules! request {
    ($t:tt) => {
        ::std::option::Option::None::<lsp_request!($t)>
    };
}
macro_rules! notification {
    ($t:tt) => {
        ::std::option::Option::None::<lsp_notification!($t)>
    };
}

pub(crate) fn initialize(
    thread: &RootedThread,
) -> (
    IoHandler,
    future::Shared<oneshot::Receiver<()>>,
    mpsc::Receiver<String>,
    mpsc::Sender<String>,
) {
    let (message_log, message_log_receiver) = mpsc::channel(1);

    let (exit_sender, exit_receiver) = oneshot::channel();
    let exit_receiver = exit_receiver.shared();

    let work_queue = {
        let (diagnostic_sink, diagnostic_stream) = rpc::unique_queue();

        let mut diagnostics_runner = DiagnosticsWorker::new(thread.clone(), message_log.clone());

        tokio::spawn(
            future::lazy(move || {
                diagnostic_stream.for_each(move |entry: Entry<Url, Arc<codespan::FileMap>, _>| {
                    diagnostics_runner.run_diagnostics(
                        &entry.key,
                        Some(entry.version),
                        &entry.value.src(),
                    )
                })
            })
            .select(exit_receiver.clone().then(|_| future::ok(())))
            .then(|_| future::ok(())),
        );

        diagnostic_sink
    };

    let mut io = IoHandler::new();
    io.add_async_method(request!("initialize"), Initialize(thread.clone()));
    io.add_async_method(
        request!("textDocument/completion"),
        Completion(thread.clone()),
    );

    {
        let thread = thread.clone();
        let message_log = message_log.clone();
        let resolve = move |mut item: CompletionItem| {
            let data: CompletionData =
                serde_json::from_value(item.data.clone().unwrap()).expect("CompletionData");

            let message_log2 = message_log.clone();
            let thread = thread.clone();
            let label = item.label.clone();
            log_message!(message_log.clone(), "{:?}", data.text_document_uri)
                .then(move |_| {
                    retrieve_expr_with_pos(
                        &thread,
                        &data.text_document_uri,
                        &data.position,
                        |module, byte_index| {
                            let type_env = thread.global_env().get_env();
                            let (_, metadata_map) =
                                gluon::check::metadata::metadata(&*type_env, &module.expr);
                            Ok(completion::suggest_metadata(
                                &metadata_map,
                                &*type_env,
                                module.source.span(),
                                &module.expr,
                                byte_index,
                                &label,
                            )
                            .and_then(|metadata| metadata.comment.clone()))
                        },
                    )
                })
                .and_then(move |comment| {
                    log_message!(message_log2, "{:?}", comment)
                        .map(move |()| {
                            item.documentation = Some(make_documentation(
                                None::<&str>,
                                comment.as_ref().map_or("", |comment| &comment.content),
                            ));
                            item
                        })
                        .map_err(|_| panic!("Unable to send log message"))
                })
        };
        io.add_async_method(request!("completionItem/resolve"), resolve);
    }

    io.add_async_method(request!("textDocument/hover"), HoverCommand(thread.clone()));

    {
        let thread = thread.clone();
        let format = move |params: DocumentFormattingParams| {
            retrieve_expr(&thread, &params.text_document.uri, |module| {
                let formatted = gluon_format::format_expr(
                    &mut Compiler::new(),
                    &thread,
                    &module.source.name().to_string(),
                    module.source.src(),
                )?;
                let range = byte_span_to_range(&module.source, module.source.span())?;
                Ok(Some(vec![TextEdit {
                    range,
                    new_text: formatted,
                }]))
            })
        };
        io.add_async_method(request!("textDocument/formatting"), format);
    }

    {
        let thread = thread.clone();
        let f = move |params: TextDocumentPositionParams| {
            retrieve_expr(&thread, &params.text_document.uri, |module| {
                let expr = &module.expr;

                let source = &module.source;

                let byte_index = position_to_byte_index(&source, &params.position)?;

                let symbol_spans = completion::find_all_symbols(source.span(), expr, byte_index)
                    .map(|t| t.1)
                    .unwrap_or(Vec::new());

                symbol_spans
                    .into_iter()
                    .map(|span| {
                        Ok(DocumentHighlight {
                            kind: None,
                            range: byte_span_to_range(&source, span)?,
                        })
                    })
                    .collect::<Result<_, _>>()
                    .map(Some)
            })
        };
        io.add_async_method(request!("textDocument/documentHighlight"), f);
    }

    {
        let thread = thread.clone();
        let f = move |params: DocumentSymbolParams| {
            retrieve_expr(&thread, &params.text_document.uri, |module| {
                let expr = &module.expr;

                let symbols = completion::all_symbols(module.source.span(), expr);

                let source = &module.source;

                symbols
                    .into_iter()
                    .map(|symbol| {
                        completion_symbol_to_symbol_information(
                            &source,
                            symbol,
                            params.text_document.uri.clone(),
                        )
                    })
                    .collect::<Result<_, _>>()
                    .map(Some)
            })
        };
        io.add_async_method(request!("textDocument/documentSymbol"), f);
    }

    {
        let thread = thread.clone();
        let f = move |params: WorkspaceSymbolParams| -> _ {
            let import = thread.get_macros().get("import").expect("Import macro");
            let import = import
                .downcast_ref::<Import<CheckImporter>>()
                .expect("Check importer");
            let modules = import.importer.0.lock().unwrap();

            let mut symbols = Vec::<SymbolInformation>::new();

            for module in modules.values() {
                let source = &module.source;

                symbols.extend(
                    completion::all_symbols(module.source.span(), &module.expr)
                        .into_iter()
                        .filter(|symbol| match symbol.value {
                            CompletionSymbol::Value { ref name, .. }
                            | CompletionSymbol::Type { ref name, .. } => {
                                name.declared_name().contains(&params.query)
                            }
                        })
                        .map(|symbol| {
                            completion_symbol_to_symbol_information(
                                &source,
                                symbol,
                                module.uri.clone(),
                            )
                        })
                        .collect::<Result<Vec<_>, _>>()?,
                );
            }

            Ok(Some(symbols))
        };
        io.add_async_method(request!("workspace/symbol"), f);
    }

    io.add_async_method(request!("shutdown"), |_| Ok::<(), ServerError<()>>(()));

    let exit_sender = Mutex::new(Some(exit_sender));
    io.add_notification(notification!("exit"), move |_| {
        if let Some(exit_sender) = exit_sender.lock().unwrap().take() {
            exit_sender.send(()).unwrap()
        }
    });
    {
        let work_queue = work_queue.clone();
        let thread = thread.clone();

        let f = move |change: DidOpenTextDocumentParams| {
            let work_queue = work_queue.clone();
            let thread = thread.clone();
            tokio::spawn({
                let filename = strip_file_prefix_with_thread(&thread, &change.text_document.uri);
                let module = filename_to_module(&filename);
                work_queue
                    .send(Entry {
                        key: change.text_document.uri,
                        value: Arc::new(codespan::FileMap::new(
                            module.into(),
                            change.text_document.text,
                        )),
                        version: change.text_document.version,
                    })
                    .map(|_| ())
                    .map_err(|_| ())
            });
        };
        io.add_notification(notification!("textDocument/didOpen"), f);
    }
    {
        let f = move |_: DidSaveTextDocumentParams| {};
        io.add_notification(notification!("textDocument/didSave"), f);
    }

    fn did_change<S>(
        thread: &Thread,
        message_log: mpsc::Sender<String>,
        work_queue: S,
        change: DidChangeTextDocumentParams,
    ) -> impl Future<Item = (), Error = ()> + Send + 'static
    where
        S: Sink<SinkItem = Entry<Url, Arc<codespan::FileMap>, u64>, SinkError = ()>
            + Send
            + 'static,
    {
        // If it does not exist in sources it should exist in the `import` macro
        let import = thread.get_macros().get("import").expect("Import macro");
        let import = import
            .downcast_ref::<Import<CheckImporter>>()
            .expect("Check importer");
        let mut modules = import.importer.0.lock().unwrap();
        let paths = import.paths.read().unwrap();
        let module_name = strip_file_prefix(&paths, &change.text_document.uri)
            .unwrap_or_else(|err| panic!("{}", err));
        let module_name = filename_to_module(&module_name);
        let module = modules
            .entry(module_name)
            .or_insert_with(|| Module::empty(change.text_document.uri.clone()));

        module.text_changes.add(
            change.text_document.version.expect("version"),
            change.content_changes,
        );

        module.dirty = true;

        let uri = change.text_document.uri;
        // If the module was loaded via `import!` before we open it in the editor
        // `module.uri` has been set by looking at the current working directory which is
        // not necessarily correct (works in VS code but not with (neo)vim) so update the
        // uri to match the one supplied by the client to ensure errors show up.
        if module.uri != uri {
            module.uri.clone_from(&uri);
        }
        let result = {
            let mut source = module.source.src().to_string();
            debug!("Change source {}:\n{}", uri, source);

            match module.version {
                Some(current_version) => match module
                    .text_changes
                    .apply_changes(&mut source, current_version)
                {
                    Ok(version) if version == current_version => return Either::A(future::ok(())),
                    Ok(version) => {
                        module.source =
                            Arc::new(codespan::FileMap::new(module.source.name().clone(), source));
                        Ok(version)
                    }
                    Err(err) => Err(err),
                },
                None => return Either::A(future::ok(())),
            }
        };
        match result {
            Ok(new_version) => {
                module.version = Some(new_version);
                let arc_source = module.source.clone();
                debug!("Changed to\n{}", arc_source.src());
                Either::B(Either::A(
                    work_queue
                        .send(Entry {
                            key: uri,
                            value: arc_source,
                            version: new_version,
                        })
                        .map(|_| ()),
                ))
            }
            Err(err) => Either::B(Either::B(
                log_message!(message_log.clone(), "{}", err.message).then(|_| Err(())),
            )),
        }
    }
    {
        let thread = thread.clone();
        let message_log = message_log.clone();

        let f = move |change: DidChangeTextDocumentParams| {
            let work_queue = work_queue.clone();
            let thread = thread.clone();
            let message_log = message_log.clone();
            tokio::spawn({
                let work_queue = work_queue.clone();
                ::std::panic::AssertUnwindSafe(did_change(
                    &thread,
                    message_log.clone(),
                    work_queue.clone().sink_map_err(|_| ()),
                    change,
                ))
                .catch_unwind()
                .map_err(|err| {
                    error!("{:?}", err);
                })
                .and_then(|result| result)
            });
        };

        io.add_notification(notification!("textDocument/didChange"), f);
    }
    {
        let thread = thread.clone();

        io.add_async_method(
            request!("textDocument/signatureHelp"),
            move |params: TextDocumentPositionParams| -> BoxFuture<_, _> {
                let result = retrieve_expr(&thread, &params.text_document.uri, |module| {
                    let expr = &module.expr;

                    let source = &module.source;
                    let byte_pos = position_to_byte_index(&source, &params.position)?;

                    let env = thread.get_env();

                    Ok(
                        completion::signature_help(&*env, module.source.span(), expr, byte_pos)
                            .map(|help| {
                                let (_, metadata_map) =
                                    gluon::check::metadata::metadata(&*env, expr);
                                let comment = if help.name.is_empty() {
                                    None
                                } else {
                                    completion::suggest_metadata(
                                        &metadata_map,
                                        &*env,
                                        module.source.span(),
                                        expr,
                                        byte_pos,
                                        &help.name,
                                    )
                                    .and_then(|metadata| metadata.comment.clone())
                                };

                                SignatureHelp {
                                    signatures: vec![SignatureInformation {
                                        label: help.name,
                                        documentation: Some(make_documentation(
                                            Some(&help.typ),
                                            &comment.as_ref().map_or("", |c| &c.content),
                                        )),
                                        parameters: Some(
                                            ::gluon::base::types::arg_iter(&help.typ)
                                                .map(|typ| ParameterInformation {
                                                    label: "".to_string(),
                                                    documentation: Some(make_documentation(
                                                        Some(typ),
                                                        "",
                                                    )),
                                                })
                                                .collect(),
                                        ),
                                    }],
                                    active_signature: None,
                                    active_parameter: help.index.map(u64::from),
                                }
                            }),
                    )
                });

                Box::new(result.into_future())
            },
        );
    }
    (io, exit_receiver, message_log_receiver, message_log)
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
