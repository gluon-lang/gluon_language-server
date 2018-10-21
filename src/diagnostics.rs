use std::{
    collections::{hash_map, BTreeMap},
    fmt, mem,
};

use gluon::{
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
    Compiler, Error as GluonError, Result as GluonResult, RootedThread,
};

use futures::{future, prelude::*, stream, sync::mpsc};

use url::Url;

use languageserver_types::*;

use {
    check_importer::{CheckImporter, Module},
    filename_to_url, module_name_to_file, module_name_to_file_,
    rpc::ServerError,
    strip_file_prefix_with_thread,
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

pub(crate) struct DiagnosticsWorker {
    thread: RootedThread,
    message_log: mpsc::Sender<String>,
    compiler: Compiler,
}

impl DiagnosticsWorker {
    pub(crate) fn new(thread: RootedThread, message_log: mpsc::Sender<String>) -> Self {
        DiagnosticsWorker {
            thread,
            compiler: Compiler::new(),
            message_log,
        }
    }

    pub(crate) fn run_diagnostics(
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
                    self.compiler.code_map(),
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

        let diagnostics_stream =
            stream::futures_ordered(diagnostics.into_iter().map(|(source_name, diagnostic)| {
                Ok(format!(
                    r#"{{
                            "jsonrpc": "2.0",
                            "method": "textDocument/publishDiagnostics",
                            "params": {}
                        }}"#,
                    serde_json::to_value(&PublishDiagnosticsParams {
                        uri: source_name,
                        diagnostics: diagnostic,
                    })
                    .unwrap()
                ))
            }));

        Box::new(
            self.message_log
                .clone()
                .send_all(diagnostics_stream)
                .map(|_| ())
                .map_err(|_| ()),
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

fn codespan_name_to_file(name: &codespan::FileName) -> Result<Url, failure::Error> {
    match *name {
        codespan::FileName::Virtual(ref s) => module_name_to_file_(s),
        codespan::FileName::Real(ref p) => filename_to_url(p),
    }
}
