use std::{
    collections::{hash_map, BTreeMap},
    fmt,
    marker::Unpin,
};

use gluon::{
    self,
    base::{filename_to_module, pos},
    import::Import,
    query::{Compilation, CompilationBase},
    Error as GluonError, Result as GluonResult, RootedThread, Thread, ThreadExt,
};

use {
    futures::{channel::mpsc, prelude::*},
    jsonrpc_core::IoHandler,
    languageserver_types::*,
    url::Url,
};

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
};

fn create_diagnostics<'a>(
    diagnostics: &'a mut BTreeMap<Url, Vec<Diagnostic>>,
    code_map: &'a codespan::CodeMap,
    importer: &'a CheckImporter,
    filename: &'a Url,
    err: &'a GluonError,
) -> futures::future::BoxFuture<'a, Result<(), ServerError<()>>> {
    create_diagnostics_(diagnostics, code_map, importer, filename, err).boxed()
}
async fn create_diagnostics_(
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

    async fn insert_in_file_error<T>(
        diagnostics: &mut BTreeMap<Url, Vec<Diagnostic>>,
        code_map: &codespan::CodeMap,
        importer: &CheckImporter,
        in_file_error: &gluon::base::error::InFile<T>,
    ) -> Result<(), ServerError<()>>
    where
        T: fmt::Display + AsDiagnostic,
    {
        let errors = diagnostics
            .entry(module_name_to_file(importer, &in_file_error.source_name()).await)
            .or_default();
        for err in in_file_error.errors() {
            errors.push(into_diagnostic(code_map, &err)?);
        }
        Ok(())
    }

    match err {
        GluonError::Typecheck(err) => {
            insert_in_file_error(diagnostics, code_map, importer, err).await?
        }

        GluonError::Parse(err) => {
            insert_in_file_error(diagnostics, code_map, importer, err).await?
        }

        GluonError::Macro(err) => {
            insert_in_file_error(diagnostics, code_map, importer, err).await?
        }

        GluonError::Multiple(errors) => {
            for err in errors {
                create_diagnostics(diagnostics, code_map, importer, filename, err).await?;
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

    pub async fn run_diagnostics(
        &mut self,
        uri_filename: &Url,
        version: Option<u64>,
        fileinput: &str,
    ) {
        info!("Running diagnostics on {}", uri_filename);

        let filename = strip_file_prefix_with_thread(&self.thread, uri_filename);
        let name = filename_to_module(&filename);

        self.thread.get_database().update_filemap(&name, fileinput);

        let diagnostics = match self.typecheck(uri_filename, &name, version).await {
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
                )
                .await;
                if let Err(err) = result {
                    error!("Unable to create diagnostics: {}", err.message);
                    return;
                }
                diagnostics
            }
        };

        let message_log = self.message_log.clone();

        for (source_name, diagnostic) in diagnostics {
            send_response(
                message_log.clone(),
                notification!("textDocument/publishDiagnostics"),
                PublishDiagnosticsParams {
                    uri: source_name,
                    diagnostics: diagnostic,
                },
            )
            .await;
        }
    }

    async fn typecheck(
        &mut self,
        uri_filename: &Url,
        name: &str,
        version: Option<u64>,
    ) -> GluonResult<()> {
        let result = self
            .thread
            .get_database()
            .typechecked_source_module(name.into(), None)
            .await
            .map_err(|(_, err)| err);

        let import = self
            .thread
            .get_macros()
            .get("import")
            .expect("Import macro");
        let import = import
            .downcast_ref::<Import<CheckImporter>>()
            .expect("Check importer");
        let mut importer = import.importer.0.lock().await;

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

        tokio::spawn(cancelable(shutdown, async move {
            futures::pin_mut!(diagnostic_stream);
            while let Some(entry) = diagnostic_stream.next().await {
                let entry: Entry<Url, String, _> = entry;
                diagnostics_runner
                    .run_diagnostics(&entry.key, Some(entry.version), &entry.value)
                    .await;
            }
        }));

        diagnostic_sink
    };

    {
        let work_queue = work_queue.clone();
        let thread = thread.clone();

        let f = move |change: DidOpenTextDocumentParams| {
            let mut work_queue = work_queue.clone();
            let thread = thread.clone();
            tokio::spawn(async move {
                let filename = strip_file_prefix_with_thread(&thread, &change.text_document.uri);
                let module = filename_to_module(&filename);
                thread
                    .get_database_mut()
                    .add_module(module.into(), &change.text_document.text);
                let _ = work_queue
                    .send(Entry {
                        key: change.text_document.uri,
                        value: change.text_document.text,

                        version: change.text_document.version,
                    })
                    .await;
            });
        };
        io.add_notification(notification!("textDocument/didOpen"), f);
    }
    {
        let f = move |_: DidSaveTextDocumentParams| {};
        io.add_notification(notification!("textDocument/didSave"), f);
    }

    async fn did_change<S>(
        thread: &Thread,
        message_log: mpsc::Sender<String>,
        mut work_queue: S,
        change: DidChangeTextDocumentParams,
    ) where
        S: Sink<Entry<Url, String, u64>, Error = ()> + Send + Unpin + 'static,
    {
        // If it does not exist in sources it should exist in the `import` macro
        let import = thread.get_macros().get("import").expect("Import macro");
        let import = import
            .downcast_ref::<Import<CheckImporter>>()
            .expect("Check importer");
        let (module_uri, uri, module_name) = {
            let mut modules = import.importer.0.lock().await;
            let module_name = {
                let paths = import.paths.read().unwrap();
                strip_file_prefix(&paths, &change.text_document.uri)
                    .unwrap_or_else(|err| panic!("{}", err))
            };
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

            (module_state.uri.clone(), uri, module_name)
        };

        let result = {
            let mut source = State::module(module_uri, thread, &module_name)
                .await
                .map(|m| m.source.src().to_string())
                .unwrap_or_default();
            debug!("Change source {}:\n{}", uri, source);
            let mut modules = import.importer.0.lock().await;
            let module_state = modules.get_mut(&module_name).unwrap();

            match module_state.version {
                Some(current_version) => match module_state
                    .text_changes
                    .apply_changes(&mut source, current_version)
                {
                    Ok(version) if version == current_version => return,
                    Ok(version) => Ok((version, source)),
                    Err(err) => Err(err),
                },
                None => return,
            }
        };
        match result {
            Ok((new_version, source)) => {
                {
                    let mut modules = import.importer.0.lock().await;
                    let module_state = modules.get_mut(&module_name).unwrap();

                    module_state.version = Some(new_version);
                }
                thread
                    .get_database_mut()
                    .add_module(module_name.into(), &source);
                debug!("Changed to\n{}", source);
                work_queue
                    .send(Entry {
                        key: uri,
                        value: source,
                        version: new_version,
                    })
                    .await
                    .unwrap()
            }
            Err(err) => log_message!(message_log.clone(), "{}", err.message).await,
        }
    }

    {
        let thread = thread.clone();
        let message_log = message_log.clone();

        let f = move |change: DidChangeTextDocumentParams| {
            let work_queue = work_queue.clone();
            let thread = thread.clone();
            let message_log = message_log.clone();
            tokio::spawn(async move {
                if let Err(err) = ::std::panic::AssertUnwindSafe(did_change(
                    &thread,
                    message_log.clone(),
                    work_queue.clone().sink_map_err(|_| ()),
                    change,
                ))
                .catch_unwind()
                .await
                {
                    error!("{:?}", err);
                }
            });
        };

        io.add_notification(notification!("textDocument/didChange"), f);
    }
}
