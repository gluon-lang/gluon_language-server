use std::{
    collections::{hash_map, BTreeMap},
    fmt,
    marker::Unpin,
};

use gluon::{
    base::{
        filename_to_module,
        pos::{self, ByteIndex, ByteOffset},
        source::{self, Source},
    },
    import::Import,
    query::{AsyncCompilation, CompilationBase},
    Error as GluonError, Result as GluonResult, RootedThread, Thread, ThreadExt,
};

use {
    codespan_reporting::diagnostic::{Diagnostic, Severity},
    futures::{channel::mpsc, prelude::*},
    jsonrpc_core::IoHandler,
    lsp_types::*,
    url::Url,
};

use crate::{
    byte_span_to_range, cancelable,
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
    diagnostics: &'a mut BTreeMap<Url, Vec<lsp_types::Diagnostic>>,
    importer: &'a CheckImporter,
    filename: &'a Url,
    err: &'a GluonError,
) -> futures::future::BoxFuture<'a, Result<(), ServerError<()>>> {
    create_diagnostics_(diagnostics, importer, filename, err).boxed()
}
async fn create_diagnostics_(
    diagnostics: &mut BTreeMap<Url, Vec<lsp_types::Diagnostic>>,
    importer: &CheckImporter,
    filename: &Url,
    err: &GluonError,
) -> Result<(), ServerError<()>> {
    use gluon::base::error::AsDiagnostic;
    fn into_diagnostic<T>(
        code_map: &source::CodeMap,
        err: &pos::Spanned<T, pos::BytePos>,
    ) -> Result<lsp_types::Diagnostic, ServerError<()>>
    where
        T: fmt::Debug + fmt::Display + AsDiagnostic,
    {
        Ok(lsp_types::Diagnostic {
            source: Some("gluon".to_string()),
            ..make_lsp_diagnostic(code_map, err.as_diagnostic(&code_map), |filename| {
                codespan_name_to_file(filename).map_err(|err| {
                    error!("Could not find file: {}", err);
                })
            })?
        })
    }

    async fn insert_in_file_error<T>(
        diagnostics: &mut BTreeMap<Url, Vec<lsp_types::Diagnostic>>,
        importer: &CheckImporter,
        in_file_error: &gluon::base::error::InFile<T>,
    ) -> Result<(), ServerError<()>>
    where
        T: fmt::Debug + fmt::Display + AsDiagnostic,
    {
        let errors = diagnostics
            .entry(module_name_to_file(importer, &in_file_error.source_name()).await)
            .or_default();
        for err in in_file_error.errors() {
            errors.push(into_diagnostic(in_file_error.source(), &err)?);
        }
        Ok(())
    }

    match err {
        GluonError::Typecheck(err) => insert_in_file_error(diagnostics, importer, err).await?,

        GluonError::Parse(err) => insert_in_file_error(diagnostics, importer, err).await?,

        GluonError::Macro(err) => insert_in_file_error(diagnostics, importer, err).await?,

        GluonError::Multiple(errors) => {
            for err in errors {
                create_diagnostics(diagnostics, importer, filename, err).await?;
            }
        }

        err => diagnostics
            .entry(filename.clone())
            .or_default()
            .push(lsp_types::Diagnostic {
                message: format!("{}", err),
                severity: Some(DiagnosticSeverity::Error),
                source: Some("gluon".to_string()),
                ..Default::default()
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
        version: Option<i64>,
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

                let result =
                    create_diagnostics(&mut diagnostics, &import.importer, uri_filename, &err)
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
                    version,
                },
            )
            .await;
        }
    }

    async fn typecheck(
        &mut self,
        uri_filename: &Url,
        name: &str,
        version: Option<i64>,
    ) -> GluonResult<()> {
        let result = self
            .thread
            .get_database()
            .typechecked_source_module(name.into(), None)
            .await;

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
        S: Sink<Entry<Url, String, i64>, Error = ()> + Send + Unpin + 'static,
    {
        // If it does not exist in sources it should exist in the `import` macro
        let import = thread.get_macros().get("import").expect("Import macro");
        let import = import
            .downcast_ref::<Import<CheckImporter>>()
            .expect("Check importer");
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
        let result = {
            let mut source = thread
                .get_database()
                .get_filemap(&module_name)
                .map(|filemap| filemap.src().to_string())
                .unwrap_or_default();
            debug!("Change source {}:\n{}", uri, source);

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
                module_state.version = Some(new_version);
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

pub fn make_lsp_severity(severity: Severity) -> lsp_types::DiagnosticSeverity {
    match severity {
        Severity::Error | Severity::Bug => lsp_types::DiagnosticSeverity::Error,
        Severity::Warning => lsp_types::DiagnosticSeverity::Warning,
        Severity::Note => lsp_types::DiagnosticSeverity::Information,
        Severity::Help => lsp_types::DiagnosticSeverity::Hint,
    }
}

const UNKNOWN_POS: lsp_types::Position = lsp_types::Position {
    character: 0,
    line: 0,
};

const UNKNOWN_RANGE: lsp_types::Range = lsp_types::Range {
    start: UNKNOWN_POS,
    end: UNKNOWN_POS,
};

/// Translates a `codespan_reporting::Diagnostic` to a `languageserver_types::Diagnostic`.
///
/// Since the language client requires `Url`s to locate the errors `codespan_name_to_file` is
/// necessary to resolve codespan `FileName`s
///
/// `code` and `source` are left empty by this function
pub fn make_lsp_diagnostic<F>(
    code_map: &source::CodeMap,
    diagnostic: Diagnostic<ByteIndex>,
    mut codespan_name_to_file: F,
) -> Result<lsp_types::Diagnostic, anyhow::Error>
where
    F: FnMut(&str) -> Result<Url, ()>,
{
    use codespan_reporting::diagnostic::LabelStyle;

    let find_file = |index| {
        code_map
            .get(index)
            .ok_or_else(|| anyhow::anyhow!("Span is outside codemap {}", index))
    };

    // We need a position for the primary error so take the span from the first primary label
    let (primary_file_map, primary_label_range) = {
        let first_primary_label = diagnostic
            .labels
            .iter()
            .find(|label| label.style == LabelStyle::Primary);

        match first_primary_label {
            Some(label) => {
                let file_map = find_file(label.file_id)?;
                let start = file_map.span().start();
                let span = pos::Span::new(
                    start + ByteOffset::from(label.range.start as i64),
                    start + ByteOffset::from(label.range.end as i64),
                );
                (Some(file_map), byte_span_to_range(&file_map, span)?)
            }
            None => (None, UNKNOWN_RANGE),
        }
    };

    let related_information = diagnostic
        .labels
        .into_iter()
        .map(|label| {
            let (file_map, range) = match primary_file_map {
                // If the label's span does not point anywhere, assume it comes from the same file
                // as the primary label
                Some(file_map) if label.file_id == ByteIndex::default() => {
                    (file_map, UNKNOWN_RANGE)
                }
                Some(_) | None => {
                    let file_map = find_file(label.file_id)?;
                    let start = file_map.span().start();
                    let span = pos::Span::new(
                        start + ByteOffset::from(label.range.start as i64),
                        start + ByteOffset::from(label.range.end as i64),
                    );
                    let range = byte_span_to_range(file_map, span)?;

                    (file_map, range)
                }
            };

            let uri = codespan_name_to_file(file_map.name()).map_err(|()| {
                anyhow::anyhow!("Unable to correlate filename: {}", file_map.name())
            })?;

            Ok(lsp_types::DiagnosticRelatedInformation {
                location: lsp_types::Location { uri, range },
                message: label.message,
            })
        })
        .collect::<Result<Vec<_>, anyhow::Error>>()?;

    Ok(lsp_types::Diagnostic {
        message: diagnostic.message,
        range: primary_label_range,
        severity: Some(make_lsp_severity(diagnostic.severity)),
        related_information: if related_information.is_empty() {
            None
        } else {
            Some(related_information)
        },
        ..lsp_types::Diagnostic::default()
    })
}
