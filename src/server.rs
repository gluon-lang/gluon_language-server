use std::sync::{Arc, Mutex};

use gluon::{base::filename_to_module, import::Import, RootedThread, Thread};

use futures::{
    future::{self, Either},
    prelude::*,
    sync::{mpsc, oneshot},
};

use tokio;

use jsonrpc_core::{IoHandler, MetaIoHandler};

use url::Url;

use languageserver_types::*;

use {
    check_importer::{CheckImporter, Module},
    diagnostics::DiagnosticsWorker,
    name::{strip_file_prefix, strip_file_prefix_with_thread},
    rpc::{self, *},
};

pub trait Handler {
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

pub(crate) struct Server {
    pub handlers: IoHandler,
    pub shutdown: future::Shared<oneshot::Receiver<()>>,
    pub message_receiver: mpsc::Receiver<String>,
    pub message_sender: mpsc::Sender<String>,
}

impl Server {
    pub(crate) fn new(thread: &RootedThread) -> Server {
        let (message_log, message_log_receiver) = mpsc::channel(1);

        let (exit_sender, exit_receiver) = oneshot::channel();
        let exit_receiver = exit_receiver.shared();

        let work_queue = {
            let (diagnostic_sink, diagnostic_stream) = rpc::unique_queue();

            let mut diagnostics_runner =
                DiagnosticsWorker::new(thread.clone(), message_log.clone());

            tokio::spawn(
                future::lazy(move || {
                    diagnostic_stream.for_each(
                        move |entry: Entry<Url, Arc<codespan::FileMap>, _>| {
                            diagnostics_runner.run_diagnostics(
                                &entry.key,
                                Some(entry.version),
                                &entry.value.src(),
                            )
                        },
                    )
                })
                .select(exit_receiver.clone().then(|_| future::ok(())))
                .then(|_| future::ok(())),
            );

            diagnostic_sink
        };

        let mut io = IoHandler::new();

        ::command::initialize::register(&mut io, thread);
        ::command::completion::register(&mut io, thread, &message_log);
        ::command::hover::register(&mut io, thread);
        ::command::signature_help::register(&mut io, thread);
        ::command::symbol::register(&mut io, thread);
        ::command::document_highlight::register(&mut io, thread);
        ::command::document_symbols::register(&mut io, thread);
        ::command::formatting::register(&mut io, thread);

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
                    let filename =
                        strip_file_prefix_with_thread(&thread, &change.text_document.uri);
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
                        Ok(version) if version == current_version => {
                            return Either::A(future::ok(()))
                        }
                        Ok(version) => {
                            module.source = Arc::new(codespan::FileMap::new(
                                module.source.name().clone(),
                                source,
                            ));
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

        Server {
            handlers: io,
            shutdown: exit_receiver,
            message_receiver: message_log_receiver,
            message_sender: message_log,
        }
    }
}
