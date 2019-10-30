use std::{
    collections::{hash_map, BTreeMap},
    fmt,
};

use gluon::{
    self,
    base::{filename_to_module, pos},
    import::Import,
    query::{Compilation, CompilationBase},
    Error as GluonError, Result as GluonResult, RootedThread, Thread, ThreadExt,
};

use futures::{
    future::{self, Either},
    prelude::*,
    stream,
    sync::mpsc,
};

use tokio;

use codespan;
use codespan_lsp;

use url::Url;

use jsonrpc_core::IoHandler;

use languageserver_types::*;

use crate::{
    cancelable,
    check_importer::{CheckImporter, State},
    name::{
        codespan_name_to_file, module_name_to_file, strip_file_prefix,
        strip_file_prefix_with_thread,
    },
    rpc::{self, send_response, Entry, ServerError},
    server::{Handler, ShutdownReceiver},
    text_edit::TextChanges,
    BoxFuture,
};

fn create_diagnostics(
    diagnostics: &mut BTreeMap<Url, Vec<Diagnostic>>,
    code_map: &codespan::CodeMap,
    importer: &CheckImporter,
    filename: &Url,
    err: &GluonError,
) -> Result<(), ServerError<()>> {
    use gluon::base::error::AsDiagnostic;
    fn into_diagnostic<T>(
        code_map: &codespan::CodeMap,
        err: &pos::Spanned<T, pos::BytePos>,
    ) -> Result<Diagnostic, ServerError<()>>
    where
        T: fmt::Display + AsDiagnostic,
    {
        Ok(Diagnostic {
            source: Some("gluon".to_string()),
            ..codespan_lsp::make_lsp_diagnostic(code_map, err.as_diagnostic(), |filename| {
                codespan_name_to_file(filename).map_err(|err| {
                    error!("{}", err);
                })
            })?
        })
    }

    fn insert_in_file_error<T>(
        diagnostics: &mut BTreeMap<Url, Vec<Diagnostic>>,
        code_map: &codespan::CodeMap,
        importer: &CheckImporter,
        in_file_error: &gluon::base::error::InFile<T>,
    ) -> Result<(), ServerError<()>>
    where
        T: fmt::Display + AsDiagnostic,
    {
        let errors = diagnostics
            .entry(module_name_to_file(importer, &in_file_error.source_name()))
            .or_default();
        for err in in_file_error.errors() {
            errors.push(into_diagnostic(code_map, &err)?);
        }
        Ok(())
    }

    match err {
        GluonError::Typecheck(err) => insert_in_file_error(diagnostics, code_map, importer, err)?,

        GluonError::Parse(err) => insert_in_file_error(diagnostics, code_map, importer, err)?,

        GluonError::Macro(err) => insert_in_file_error(diagnostics, code_map, importer, err)?,

        GluonError::Multiple(errors) => {
            for err in errors {
                create_diagnostics(diagnostics, code_map, importer, filename, err)?;
            }
        }

        err => diagnostics
            .entry(filename.clone())
            .or_default()
            .push(Diagnostic {
                message: format!("{}", err),
                severity: Some(DiagnosticSeverity::Error),
                source: Some("gluon".to_string()),
                ..Diagnostic::default()
            }),
    }
    Ok(())
}

struct DiagnosticsWorker {
    thread: RootedThread,
    message_log: mpsc::Sender<String>,
}

impl DiagnosticsWorker {
    pub fn new(thread: RootedThread, message_log: mpsc::Sender<String>) -> Self {
        DiagnosticsWorker {
            thread,
            message_log,
        }
    }

    pub fn run_diagnostics(
        &mut self,
        uri_filename: &Url,
        version: Option<u64>,
        fileinput: &str,
    ) -> BoxFuture<(), ()> {
        info!("Running diagnostics on {}", uri_filename);

        let filename = strip_file_prefix_with_thread(&self.thread, uri_filename);
        let name = filename_to_module(&filename);

        self.thread.get_database().update_filemap(&name, fileinput);

        let diagnostics = match self.typecheck(uri_filename, &name, version) {
            Ok(_) => Some((uri_filename.clone(), vec![])).into_iter().collect(),
            Err(err) => {
                debug!("Diagnostics result on `{}`: {}", uri_filename, err);
                let mut diagnostics = BTreeMap::new();

                let import = self
                    .thread
                    .get_macros()
                    .get("import")
                    .expect("Import macro");
                let import = import
                    .downcast_ref::<Import<CheckImporter>>()
                    .expect("Check importer");

                let result = create_diagnostics(
                    &mut diagnostics,
                    &self.thread.get_database().code_map(),
                    &import.importer,
                    uri_filename,
                    &err,
                );
                if let Err(err) = result {
                    error!("Unable to create diagnostics: {}", err.message);
                    return Box::new(Err(()).into_future());
                }
                diagnostics
            }
        };

        let message_log = self.message_log.clone();
        Box::new(
            stream::futures_ordered(diagnostics.into_iter().map(|(source_name, diagnostic)| {
                send_response(
                    message_log.clone(),
                    notification!("textDocument/publishDiagnostics"),
                    PublishDiagnosticsParams {
                        uri: source_name,
                        diagnostics: diagnostic,
                    },
                )
            }))
            .for_each(|_| Ok(())),
        )
    }

    fn typecheck(
        &mut self,
        uri_filename: &Url,
        name: &str,
        version: Option<u64>,
    ) -> GluonResult<()> {
        let result = self
            .thread
            .get_database()
            .typechecked_module(name.into(), None)
            .map_err(|(_, err)| err);

        let import = self
            .thread
            .get_macros()
            .get("import")
            .expect("Import macro");
        let import = import
            .downcast_ref::<Import<CheckImporter>>()
            .expect("Check importer");
        let mut importer = import.importer.0.lock().unwrap();

        match importer.entry(name.into()) {
            hash_map::Entry::Occupied(mut entry) => {
                let module = entry.get_mut();

                if version.is_some() {
                    module.version = version;
                }

                module.uri = uri_filename.clone();
            }
            hash_map::Entry::Vacant(entry) => {
                entry.insert(self::State {
                    uri: uri_filename.clone(),
                    version: version,
                    text_changes: TextChanges::new(),
                });
            }
        }
        result?;
        Ok(())
    }
}

pub fn register(
    io: &mut IoHandler,
    thread: &RootedThread,
    message_log: &mpsc::Sender<String>,
    shutdown: ShutdownReceiver,
) {
    let work_queue = {
        let (diagnostic_sink, diagnostic_stream) = rpc::unique_queue();

        let mut diagnostics_runner = DiagnosticsWorker::new(thread.clone(), message_log.clone());

        tokio::spawn(cancelable(
            shutdown,
            future::lazy(move || {
                diagnostic_stream.for_each(move |entry: Entry<Url, String, _>| {
                    diagnostics_runner.run_diagnostics(
                        &entry.key,
                        Some(entry.version),
                        &entry.value,
                    )
                })
            }),
        ));

        diagnostic_sink
    };

    {
        let work_queue = work_queue.clone();
        let thread = thread.clone();

        let f = move |change: DidOpenTextDocumentParams| {
            let work_queue = work_queue.clone();
            let thread = thread.clone();
            tokio::spawn({
                let filename = strip_file_prefix_with_thread(&thread, &change.text_document.uri);
                let module = filename_to_module(&filename);
                thread
                    .get_database_mut()
                    .add_module(module.into(), &change.text_document.text);
                work_queue
                    .send(Entry {
                        key: change.text_document.uri,
                        value: change.text_document.text,

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
        S: Sink<SinkItem = Entry<Url, String, u64>, SinkError = ()> + Send + 'static,
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
        let module_state = modules
            .entry(module_name.clone())
            .or_insert_with(|| State::empty(change.text_document.uri.clone()));

        module_state.text_changes.add(
            change.text_document.version.expect("version"),
            change.content_changes,
        );

        let uri = change.text_document.uri;
        // If the module was loaded via `import!` before we open it in the editor
        // `module.uri` has been set by looking at the current working directory which is
        // not necessarily correct (works in VS code but not with (neo)vim) so update the
        // uri to match the one supplied by the client to ensure errors show up.
        if module_state.uri != uri {
            module_state.uri.clone_from(&uri);
        }
        let result = {
            let mut source = module_state
                .module(thread, &module_name)
                .map(|m| m.source.src().to_string())
                .unwrap_or_default();
            debug!("Change source {}:\n{}", uri, source);

            match module_state.version {
                Some(current_version) => match module_state
                    .text_changes
                    .apply_changes(&mut source, current_version)
                {
                    Ok(version) if version == current_version => return Either::A(future::ok(())),
                    Ok(version) => Ok((version, source)),
                    Err(err) => Err(err),
                },
                None => {
                    return Either::A(future::ok(()));
                }
            }
        };
        match result {
            Ok((new_version, source)) => {
                module_state.version = Some(new_version);
                eprintln!("ADD 2 {}", module_name);
                thread
                    .get_database_mut()
                    .add_module(module_name.into(), &source);
                debug!("Changed to\n{}", source);
                Either::B(Either::A(
                    work_queue
                        .send(Entry {
                            key: uri,
                            value: source,
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
}
