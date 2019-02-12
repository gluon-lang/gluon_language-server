use std::{
    collections::{hash_map, BTreeMap},
    fmt, mem,
    sync::Arc,
};

use gluon::{
    self,
    base::{
        ast::{Expr, SpannedExpr, Typed},
        error::Errors,
        filename_to_module,
        metadata::Metadata,
        pos,
        symbol::Symbol,
        types::{Type, TypeCache},
    },
    compiler_pipeline::{MacroExpandable, MacroValue, Typecheckable},
    import::Import,
    Compiler, Error as GluonError, Result as GluonResult, RootedThread, Thread,
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

use {
    cancelable,
    check_importer::{CheckImporter, Module},
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
    err: GluonError,
) -> Result<(), ServerError<()>> {
    use gluon::base::error::AsDiagnostic;
    fn into_diagnostic<T>(
        code_map: &codespan::CodeMap,
        err: pos::Spanned<T, pos::BytePos>,
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
        in_file_error: gluon::base::error::InFile<T>,
    ) -> Result<(), ServerError<()>>
    where
        T: fmt::Display + AsDiagnostic,
    {
        diagnostics
            .entry(module_name_to_file(importer, &in_file_error.source_name()))
            .or_insert(Vec::new())
            .extend(
                in_file_error
                    .errors()
                    .into_iter()
                    .map(|err| into_diagnostic(code_map, err))
                    .collect::<Result<Vec<_>, _>>()?,
            );
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
            .or_insert(Vec::new())
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
    compiler: Compiler,
}

impl DiagnosticsWorker {
    pub fn new(thread: RootedThread, message_log: mpsc::Sender<String>) -> Self {
        DiagnosticsWorker {
            thread,
            compiler: Compiler::new(),
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

        self.compiler.update_filemap(&name, fileinput);

        let diagnostics = match self.typecheck(uri_filename, &name, version, fileinput) {
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
                    &self.compiler.code_map(),
                    &import.importer,
                    uri_filename,
                    err,
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
        fileinput: &str,
    ) -> GluonResult<()> {
        let (expr_opt, errors) = self.typecheck_(&name, fileinput);

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

                if let Some(expr) = expr_opt {
                    module.expr = expr;
                }
                module.uri = uri_filename.clone();

                if version.is_some() {
                    module.version = version;
                }

                module.source = self.compiler.get_filemap(&name).expect("FileMap").clone();

                module.dirty = false;

                let waiters = mem::replace(&mut module.waiters, Vec::new());
                tokio::spawn({
                    future::join_all(waiters.into_iter().map(|sender| sender.send(())))
                        .map(|_| ())
                        .map_err(|_| ())
                });
            }
            hash_map::Entry::Vacant(entry) => {
                entry.insert(self::Module {
                    expr: expr_opt
                        .unwrap_or_else(|| pos::spanned2(0.into(), 0.into(), Expr::Error(None))),
                    source: self.compiler.get_filemap(&name).unwrap().clone(),
                    uri: uri_filename.clone(),
                    dirty: false,
                    waiters: Vec::new(),
                    version: version,
                    text_changes: TextChanges::new(),
                });
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.into())
        }
    }

    fn typecheck_(
        &mut self,
        name: &str,
        fileinput: &str,
    ) -> (Option<SpannedExpr<Symbol>>, Errors<GluonError>) {
        debug!("Loading: `{}`", name);
        let mut errors = Errors::new();
        // The parser may find parse errors but still produce an expression
        // For that case still typecheck the expression but return the parse error afterwards
        let mut expr = match self
            .compiler
            .parse_partial_expr(&TypeCache::new(), &name, fileinput)
        {
            Ok(expr) => expr,
            Err((None, err)) => {
                errors.push(err.into());
                return (None, errors);
            }
            Err((Some(expr), err)) => {
                errors.push(err.into());
                expr
            }
        };

        if let Err((_, err)) =
            (&mut expr).expand_macro(&mut self.compiler, &self.thread, &name, fileinput)
        {
            errors.push(err);
        }

        let check_result = (MacroValue { expr: &mut expr })
            .typecheck(&mut self.compiler, &self.thread, &name, fileinput)
            .map(|value| value.typ);
        let typ = match check_result {
            Ok(typ) => typ,
            Err(err) => {
                errors.push(err);
                expr.try_type_of(&*self.thread.global_env().get_env())
                    .unwrap_or_else(|_| Type::hole())
            }
        };
        let metadata = Metadata::default();
        if let Err(err) = self
            .thread
            .global_env()
            .set_dummy_global(name, typ, metadata)
        {
            errors.push(err.into());
        }

        (Some(expr), errors)
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
                diagnostic_stream.for_each(move |entry: Entry<Url, Arc<codespan::FileMap>, _>| {
                    diagnostics_runner.run_diagnostics(
                        &entry.key,
                        Some(entry.version),
                        &entry.value.src(),
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
}
