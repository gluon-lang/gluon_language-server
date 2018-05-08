#![cfg_attr(feature = "serde_macros", feature(custom_derive, plugin))]
#![cfg_attr(feature = "serde_macros", plugin(serde_macros))]

extern crate clap;

extern crate serde;
#[macro_use]
extern crate serde_derive;
extern crate serde_json;

extern crate futures;
extern crate jsonrpc_core;
extern crate tokio;
extern crate tokio_io;

extern crate env_logger;

#[macro_use]
extern crate log;
extern crate url;
extern crate url_serde;

extern crate bytes;

extern crate codespan;
extern crate codespan_lsp;
extern crate codespan_reporting;

#[macro_use]
extern crate languageserver_types;

extern crate gluon;
extern crate gluon_completion as completion;
extern crate gluon_format;

macro_rules! log_message {
    ($sender: expr, $($ts: tt)+) => {
        if log_enabled!(::log::Level::Debug) {
            ::log_message($sender, format!( $($ts)+ ))
        } else {
            Box::new(Ok(()).into_future())
        }
    }
}

macro_rules! try_future {
    ($e:expr) => {

        match $e {
            Ok(x) => x,
            Err(err) => return Box::new(Err(err.into()).into_future()),
        }
    }
}

pub mod debugger;
pub mod rpc;
mod text_edit;

use jsonrpc_core::{IoHandler, MetaIoHandler};

use url::Url;

use gluon::base::ast::{Expr, SpannedExpr, Typed};
use gluon::base::error::Errors;
use gluon::base::filename_to_module;
use gluon::base::fnv::FnvMap;
use gluon::base::kind::ArcKind;
use gluon::base::metadata::Metadata;
use gluon::base::pos::{self, BytePos, Spanned};
use gluon::base::symbol::Symbol;
use gluon::base::types::{ArcType, BuiltinType, Type, TypeCache};
use gluon::compiler_pipeline::{MacroExpandable, MacroValue, Typecheckable};
use gluon::either;
use gluon::import::{Import, Importer};
use gluon::vm::macros::Error as MacroError;
use gluon::vm::thread::{Thread, ThreadInternal};
use gluon::{new_vm, Compiler, Error as GluonError, Result as GluonResult, RootedThread};

use completion::CompletionSymbol;

use std::collections::{hash_map, BTreeMap};
use std::env;
use std::error::Error as StdError;
use std::fmt;
use std::fs;
use std::io::BufReader;
use std::mem;
use std::path::{Path, PathBuf};
use std::str;
use std::sync::{Arc, Mutex, RwLock};

use languageserver_types::*;

use futures::future::Either;
use futures::stream;
use futures::sync::mpsc;
use futures::sync::oneshot;
use futures::{future, Future, IntoFuture, Sink, Stream};

use tokio_io::codec::{Framed, FramedParts};

use bytes::BytesMut;

pub type BoxFuture<I, E> = Box<Future<Item = I, Error = E> + Send + 'static>;

use codespan_lsp::{byte_span_to_range, position_to_byte_index};
use rpc::*;
use text_edit::TextChanges;

fn log_message(sender: mpsc::Sender<String>, message: String) -> BoxFuture<(), ()> {
    debug!("{}", message);
    let r = format!(
        r#"{{"jsonrpc": "2.0", "method": "window/logMessage", "params": {} }}"#,
        serde_json::to_value(&LogMessageParams {
            typ: MessageType::Log,
            message: message,
        }).unwrap()
    );
    Box::new(sender.send(r).map(|_| ()).map_err(|_| ()))
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

struct Module {
    source: Arc<codespan::FileMap>,
    expr: SpannedExpr<Symbol>,
    uri: Url,
    dirty: bool,
    waiters: Vec<oneshot::Sender<()>>,
    version: Option<u64>,
    text_changes: TextChanges,
}

impl Module {
    fn empty(uri: Url) -> Module {
        Module {
            source: Arc::new(codespan::FileMap::new("".into(), "".into())),
            expr: pos::spanned2(0.into(), 0.into(), Expr::Error(None)),
            uri,
            dirty: false,
            waiters: Vec::new(),
            version: None,
            text_changes: TextChanges::new(),
        }
    }
}

#[derive(Clone)]
struct CheckImporter(Arc<Mutex<FnvMap<String, Module>>>);
impl CheckImporter {
    fn new() -> CheckImporter {
        CheckImporter(Arc::new(Mutex::new(FnvMap::default())))
    }
}
impl Importer for CheckImporter {
    fn import(
        &self,
        compiler: &mut Compiler,
        vm: &Thread,
        _earlier_errors_exist: bool,
        module_name: &str,
        input: &str,
        mut expr: SpannedExpr<Symbol>,
    ) -> Result<(), (Option<ArcType>, MacroError)> {
        let result = MacroValue { expr: &mut expr }
            .typecheck(compiler, vm, module_name, input)
            .map(|res| res.typ);

        let typ = result.as_ref().ok().map_or_else(
            || {
                expr.try_type_of(&*vm.get_env())
                    .unwrap_or_else(|_| Type::hole())
            },
            |typ| typ.clone(),
        );

        let (metadata, _) = gluon::check::metadata::metadata(&*vm.global_env().get_env(), &expr);

        let previous = self.0.lock().unwrap().insert(
            module_name.into(),
            self::Module {
                expr: expr,
                source: compiler.get_filemap(&module_name).unwrap().clone(),
                uri: module_name_to_file_(module_name).map_err(|err| (None, err.into()))?,
                dirty: false,
                waiters: Vec::new(),
                version: None,
                text_changes: TextChanges::new(),
            },
        );
        if let Some(previous_module) = previous {
            tokio::spawn({
                future::join_all(
                    previous_module
                        .waiters
                        .into_iter()
                        .map(|sender| sender.send(())),
                ).map(|_| ())
                    .map_err(|_| ())
            });
        }
        // Insert a global to ensure the globals type can be looked up
        vm.global_env()
            .set_dummy_global(module_name, typ.clone(), metadata)
            .map_err(|err| (None, err.into()))?;

        result.map(|_| ()).map_err(|err| (Some(typ), err.into()))
    }
}

struct Initialize(RootedThread);
impl LanguageServerCommand<InitializeParams> for Initialize {
    type Output = InitializeResult;
    type Error = InitializeError;
    fn execute(
        &self,
        change: InitializeParams,
    ) -> BoxFuture<InitializeResult, ServerError<InitializeError>> {
        let import = self.0.get_macros().get("import").expect("Import macro");
        let import = import
            .downcast_ref::<Import<CheckImporter>>()
            .expect("Check importer");
        if let Some(ref path) = change.root_path {
            import.add_path(path);
        }
        Box::new(
            Ok(InitializeResult {
                capabilities: ServerCapabilities {
                    text_document_sync: Some(TextDocumentSyncCapability::Kind(
                        TextDocumentSyncKind::Incremental,
                    )),
                    completion_provider: Some(CompletionOptions {
                        resolve_provider: Some(true),
                        trigger_characters: Some(vec![".".into()]),
                    }),
                    signature_help_provider: Some(SignatureHelpOptions {
                        trigger_characters: None,
                    }),
                    hover_provider: Some(true),
                    document_formatting_provider: Some(true),
                    document_highlight_provider: Some(true),
                    document_symbol_provider: Some(true),
                    workspace_symbol_provider: Some(true),
                    ..ServerCapabilities::default()
                },
            }).into_future(),
        )
    }

    fn invalid_params(&self) -> Option<Self::Error> {
        Some(InitializeError { retry: false })
    }
}

fn retrieve_expr_future<F, R>(
    thread: &Thread,
    text_document_uri: &Url,
    f: F,
) -> BoxFuture<R, ServerError<()>>
where
    F: FnOnce(&mut Module) -> BoxFuture<R, ServerError<()>>,
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
        Some(source_module) => return f(source_module),
        None => (),
    }
    Box::new(
        Err(ServerError {
            message: format!(
                "Module `{}` is not defined\n{:?}",
                module,
                importer.keys().collect::<Vec<_>>()
            ),
            data: None,
        }).into_future(),
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

#[derive(Serialize, Deserialize)]
pub struct CompletionData {
    #[serde(with = "url_serde")]
    pub text_document_uri: Url,
    pub position: Position,
}

#[derive(Clone)]
struct Completion(RootedThread);
impl LanguageServerCommand<CompletionParams> for Completion {
    type Output = Option<CompletionResponse>;
    type Error = ();
    fn execute(&self, change: CompletionParams) -> BoxFuture<Self::Output, ServerError<()>> {
        let thread = &self.0;
        let self_ = self.clone();
        let text_document_uri = change.text_document.uri.clone();
        let result = retrieve_expr_future(&thread, &text_document_uri, |module| {
            let Module {
                ref expr,
                ref source,
                dirty,
                ref mut waiters,
                ..
            } = *module;

            if dirty {
                let (sender, receiver) = oneshot::channel();
                waiters.push(sender);
                return Box::new(
                    receiver
                        .map_err(|_| {
                            let msg = "Completion sender was unexpectedly dropped";
                            error!("{}", msg);
                            ServerError::from(msg.to_string())
                        })
                        .and_then(move |_| self_.clone().execute(change)),
                );
            }

            let byte_index = try_future!(position_to_byte_index(&source, &change.position));

            let query = completion::SuggestionQuery {
                modules: with_import(thread, |import| import.modules()),
                ..completion::SuggestionQuery::default()
            };

            let suggestions = query
                .suggest(&*thread.get_env(), source.span(), expr, byte_index)
                .into_iter()
                .filter(|suggestion| !suggestion.name.starts_with("__"))
                .collect::<Vec<_>>();

            let mut items: Vec<_> = suggestions
                .into_iter()
                .map(|ident| {
                    // Remove the `:Line x, Row y suffix`
                    let name: &str = ident.name.as_ref();
                    let label = String::from(name.split(':').next().unwrap_or(ident.name.as_ref()));
                    CompletionItem {
                        insert_text: if label.starts_with(char::is_alphabetic) {
                            None
                        } else {
                            Some(format!("({})", label))
                        },
                        kind: Some(ident_to_completion_item_kind(&label, ident.typ.as_ref())),
                        label,
                        detail: match ident.typ {
                            either::Either::Right(ref typ) => match **typ {
                                Type::Hole => None,
                                _ => Some(format!("{}", ident.typ)),
                            },
                            either::Either::Left(_) => Some(format!("{}", ident.typ)),
                        },
                        data: Some(
                            serde_json::to_value(CompletionData {
                                text_document_uri: change.text_document.uri.clone(),
                                position: change.position,
                            }).expect("CompletionData"),
                        ),
                        ..CompletionItem::default()
                    }
                })
                .collect();

            items.sort_by(|l, r| l.label.cmp(&r.label));

            Box::new(Ok(Some(CompletionResponse::Array(items))).into_future())
        });
        Box::new(result.into_future())
    }

    fn invalid_params(&self) -> Option<Self::Error> {
        None
    }
}

struct HoverCommand(RootedThread);
impl LanguageServerCommand<TextDocumentPositionParams> for HoverCommand {
    type Output = Option<Hover>;
    type Error = ();
    fn execute(
        &self,
        change: TextDocumentPositionParams,
    ) -> BoxFuture<Option<Hover>, ServerError<()>> {
        Box::new(
            (|| -> Result<_, _> {
                let thread = &self.0;
                retrieve_expr(thread, &change.text_document.uri, |module| {
                    let expr = &module.expr;

                    let source = &module.source;
                    let byte_index = position_to_byte_index(&source, &change.position)?;

                    let env = thread.get_env();
                    let (_, metadata_map) = gluon::check::metadata::metadata(&*env, &expr);
                    let opt_metadata =
                        completion::get_metadata(&metadata_map, source.span(), expr, byte_index);
                    let extract = (completion::TypeAt { env: &*env }, completion::SpanAt);
                    Ok(
                        completion::completion(extract, source.span(), expr, byte_index)
                            .map(|(typ, span)| {
                                let contents = match opt_metadata.and_then(|m| m.comment.as_ref()) {
                                    Some(comment) => format!("{}\n\n{}", typ, comment),
                                    None => format!("{}", typ),
                                };
                                Some(Hover {
                                    contents: HoverContents::Scalar(MarkedString::String(contents)),
                                    range: byte_span_to_range(&source, span).ok(),
                                })
                            })
                            .unwrap_or_else(|()| None),
                    )
                })
            })()
                .into_future(),
        )
    }

    fn invalid_params(&self) -> Option<Self::Error> {
        None
    }
}

fn codespan_name_to_file(name: &codespan::FileName) -> Result<Url, Box<StdError + Send + Sync>> {
    match *name {
        codespan::FileName::Virtual(ref s) => module_name_to_file_(s),
        codespan::FileName::Real(ref p) => filename_to_url(p),
    }
}

fn codspan_name_to_module(name: &codespan::FileName) -> String {
    match *name {
        codespan::FileName::Virtual(ref s) => s.to_string(),
        codespan::FileName::Real(ref p) => filename_to_module(&p.display().to_string()),
    }
}

fn module_name_to_file_(s: &str) -> Result<Url, Box<StdError + Send + Sync>> {
    let mut result = s.replace(".", "/");
    result.push_str(".glu");
    Ok(filename_to_url(Path::new(&result))
        .or_else(|_| url::Url::from_file_path(s))
        .map_err(|_| format!("Unable to convert module name to a url: `{}`", s))?)
}

fn filename_to_url(result: &Path) -> Result<Url, Box<StdError + Send + Sync>> {
    let path = fs::canonicalize(&*result).or_else(|err| match env::current_dir() {
        Ok(path) => Ok(path.join(result)),
        Err(_) => Err(err),
    })?;
    Ok(url::Url::from_file_path(path).map_err(|_| {
        format!(
            "Unable to convert module name to a url: `{}`",
            result.display()
        )
    })?)
}

fn module_name_to_file(importer: &CheckImporter, name: &codespan::FileName) -> Url {
    let s = codspan_name_to_module(name);
    importer
        .0
        .lock()
        .unwrap()
        .get(&s)
        .map(|source| source.uri.clone())
        .unwrap_or_else(|| module_name_to_file_(&s).unwrap())
}

fn with_import<F, R>(thread: &Thread, f: F) -> R
where
    F: FnOnce(&Import<CheckImporter>) -> R,
{
    let import = thread.get_macros().get("import").expect("Import macro");
    let import = import
        .downcast_ref::<Import<CheckImporter>>()
        .expect("Check importer");
    f(import)
}

fn strip_file_prefix_with_thread(thread: &Thread, url: &Url) -> String {
    with_import(thread, |import| {
        let paths = import.paths.read().unwrap();
        strip_file_prefix(&paths, url).unwrap_or_else(|err| panic!("{}", err))
    })
}

pub fn strip_file_prefix(
    paths: &[PathBuf],
    url: &Url,
) -> Result<String, Box<StdError + Send + Sync>> {
    use std::env;

    let path = url.to_file_path().map_err(|_| "Expected a file uri")?;
    let name = match fs::canonicalize(&*path) {
        Ok(name) => name,
        Err(_) => env::current_dir()?.join(&*path),
    };

    for path in paths {
        let canonicalized = fs::canonicalize(path).ok();

        let result = canonicalized
            .as_ref()
            .and_then(|path| name.strip_prefix(path).ok())
            .and_then(|path| path.to_str());
        if let Some(path) = result {
            return Ok(format!("{}", path));
        }
    }
    Ok(format!(
        "{}",
        name.strip_prefix(&env::current_dir()?)
            .unwrap_or_else(|_| &name)
            .display()
    ))
}

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
            .extend(in_file_error
                .errors()
                .into_iter()
                .map(|err| into_diagnostic(code_map, err))
                .collect::<Result<Vec<_>, _>>()?);
        Ok(())
    }

    match err {
        GluonError::Typecheck(err) => insert_in_file_error(diagnostics, code_map, importer, err)?,

        GluonError::Parse(err) => insert_in_file_error(diagnostics, code_map, importer, err)?,

        GluonError::Macro(err) => insert_in_file_error(diagnostics, code_map, importer, err)?,

        GluonError::Multiple(errors) => for err in errors {
            create_diagnostics(diagnostics, code_map, importer, filename, err)?;
        },

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
    fn run_diagnostics(
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

                let import = self.thread
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
                    }).unwrap()
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

        let import = self.thread
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
        let mut expr = match self.compiler
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
        if let Err(err) = self.thread
            .global_env()
            .set_dummy_global(name, typ, metadata)
        {
            errors.push(err.into());
        }

        (Some(expr), errors)
    }
}

pub fn run() {
    ::env_logger::init();

    let _matches = clap::App::new("debugger")
        .version(env!("CARGO_PKG_VERSION"))
        .get_matches();

    let thread = new_vm();

    tokio::run(future::lazy(move || {
        start_server(thread, tokio::io::stdin(), tokio::io::stdout())
            .map_err(|err| panic!("{}", err))
    }))
}

pub fn start_server<R, W>(
    thread: RootedThread,
    input: R,
    mut output: W,
) -> BoxFuture<(), Box<StdError + Send + Sync>>
where
    R: tokio::io::AsyncRead + Send + 'static,
    W: tokio::io::AsyncWrite + Send + 'static,
{
    let _ = ::env_logger::try_init();

    {
        let macros = thread.get_macros();
        let mut check_import = Import::new(CheckImporter::new());
        {
            let import = macros.get("import").expect("Import macro");
            let import = import.downcast_ref::<Import>().expect("Importer");
            check_import.paths = RwLock::new((*import.paths.read().unwrap()).clone());
            check_import.loaders = RwLock::new(
                import
                    .loaders
                    .read()
                    .unwrap()
                    .iter()
                    .map(|(k, v)| (k.clone(), *v))
                    .collect(),
            );
        }
        macros.insert("import".into(), check_import);
    }

    let (io, exit_receiver, message_log_receiver, message_log) = initialize_rpc(&thread);

    let input = BufReader::new(input);

    let parts = FramedParts {
        inner: input,
        readbuf: BytesMut::default(),
        writebuf: BytesMut::default(),
    };
    let future: BoxFuture<(), Box<StdError + Send + Sync>> = Box::new(
        Framed::from_parts(parts, rpc::LanguageServerDecoder::new())
            .map_err(|err| panic!("{}", err))
            .for_each(move |json| {
                debug!("Handle: {}", json);
                let message_log = message_log.clone();
                io.handle_request(&json).then(move |result| {
                    if let Ok(Some(response)) = result {
                        Either::A(
                            message_log
                                .clone()
                                .send(response)
                                .map(|_| ())
                                .map_err(|_| "Unable to send".into()),
                        )
                    } else {
                        Either::B(Ok(()).into_future())
                    }
                })
            }),
    );

    let log_messages = Box::new(
        message_log_receiver
            .map_err(|_| {
                let x: Box<StdError + Send + Sync> = "Unable to log message".into();
                x
            })
            .for_each(move |message| -> Result<(), Box<StdError + Send + Sync>> {
                Ok(write_message_str(&mut output, &message)?)
            }),
    );

    Box::new(
        future::select_all(vec![
            future,
            Box::new(
                exit_receiver
                    .map(|_| {
                        info!("Exiting");
                    })
                    .map_err(|_| "Exit was canceled".into()),
            ),
            log_messages,
        ]).map(|t| t.0)
            .map_err(|t| panic!("{}", t.0)),
    )
}

trait Handler {
    fn add_async_method<T, U>(&mut self, _: Option<T>, method: U)
    where
        T: ::languageserver_types::request::Request,
        U: LanguageServerCommand<T::Params, Output = T::Result>,
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
    }
}
macro_rules! notification {
    ($t:tt) => {

        ::std::option::Option::None::<lsp_notification!($t)>
    }
}

fn initialize_rpc(
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

        let mut diagnostics_runner = DiagnosticsWorker {
            thread: thread.clone(),
            compiler: Compiler::new(),
            message_log: message_log.clone(),
        };

        tokio::spawn(
            future::lazy(move || {
                diagnostic_stream.for_each(move |entry: Entry<Url, Arc<codespan::FileMap>, _>| {
                    diagnostics_runner.run_diagnostics(
                        &entry.key,
                        Some(entry.version),
                        &entry.value.src(),
                    )
                })
            }).select(exit_receiver.clone().then(|_| future::ok(())))
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
        let resolve = move |mut item: CompletionItem| -> BoxFuture<CompletionItem, _> {
            let data: CompletionData =
                serde_json::from_value(item.data.clone().unwrap()).expect("CompletionData");

            let message_log2 = message_log.clone();
            let thread = thread.clone();
            let label = item.label.clone();
            Box::new(
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
                                ).and_then(|metadata| metadata.comment.clone()))
                            },
                        )
                    })
                    .and_then(move |comment| {
                        log_message!(message_log2, "{:?}", comment)
                            .map(move |()| {
                                item.documentation = Some(make_documentation(
                                    None::<&str>,
                                    comment.as_ref().map_or("", |comment| comment),
                                ));
                                item
                            })
                            .map_err(|_| panic!("Unable to send log message"))
                    }),
            )
        };
        io.add_async_method(request!("completionItem/resolve"), resolve);
    }

    io.add_async_method(request!("textDocument/hover"), HoverCommand(thread.clone()));

    {
        let thread = thread.clone();
        let format = move |params: DocumentFormattingParams| -> BoxFuture<Option<Vec<_>>, _> {
            Box::new(
                retrieve_expr(&thread, &params.text_document.uri, |module| {
                    let formatted = gluon_format::format_expr(
                        &mut Compiler::new(),
                        &thread,
                        &module.source.name().to_string(),
                        module.source.src(),
                    )?;
                    let range = byte_span_to_range(&module.source, module.source.span())?;
                    Ok(Some(vec![
                        TextEdit {
                            range,
                            new_text: formatted,
                        },
                    ]))
                }).into_future(),
            )
        };
        io.add_async_method(request!("textDocument/formatting"), format);
    }

    {
        let thread = thread.clone();
        let f = move |params: TextDocumentPositionParams| -> BoxFuture<Option<Vec<_>>, _> {
            Box::new(
                retrieve_expr(&thread, &params.text_document.uri, |module| {
                    let expr = &module.expr;

                    let source = &module.source;

                    let byte_index = position_to_byte_index(&source, &params.position)?;

                    let symbol_spans =
                        completion::find_all_symbols(source.span(), expr, byte_index)
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
                }).into_future(),
            )
        };
        io.add_async_method(request!("textDocument/documentHighlight"), f);
    }

    {
        let thread = thread.clone();
        let f = move |params: DocumentSymbolParams| -> BoxFuture<Option<Vec<_>>, _> {
            Box::new(
                retrieve_expr(&thread, &params.text_document.uri, |module| {
                    let expr = &module.expr;

                    let symbols = completion::all_symbols(expr);

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
                }).into_future(),
            )
        };
        io.add_async_method(request!("textDocument/documentSymbol"), f);
    }

    {
        let thread = thread.clone();
        let f = move |params: WorkspaceSymbolParams| -> BoxFuture<Option<Vec<_>>, ServerError<()>> {
            let thread = thread.clone();
            Box::new(future::lazy(move || {
                let import = thread.get_macros().get("import").expect("Import macro");
                let import = import
                    .downcast_ref::<Import<CheckImporter>>()
                    .expect("Check importer");
                let modules = import.importer.0.lock().unwrap();

                let mut symbols = Vec::<SymbolInformation>::new();

                for module in modules.values() {
                    let source = &module.source;

                    symbols.extend(completion::all_symbols(&module.expr)
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
                        .collect::<Result<Vec<_>, _>>()?);
                }

                Ok(Some(symbols))
            }))
        };
        io.add_async_method(request!("workspace/symbol"), f);
    }

    io.add_async_method(
        request!("shutdown"),
        |_| -> BoxFuture<(), ServerError<()>> { Box::new(futures::finished(())) },
    );

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

    fn did_change<S>(
        thread: &Thread,
        message_log: mpsc::Sender<String>,
        work_queue: S,
        change: DidChangeTextDocumentParams,
    ) -> BoxFuture<(), ()>
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
            .or_insert_with(|| self::Module::empty(change.text_document.uri.clone()));

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
                    Ok(version) => {
                        module.source =
                            Arc::new(codespan::FileMap::new(module.source.name().clone(), source));
                        Ok(version)
                    }
                    Err(err) => Err(err),
                },
                None => return Box::new(Ok(()).into_future()),
            }
        };
        match result {
            Ok(new_version) => {
                if Some(new_version) == module.version {
                    return Box::new(Ok(()).into_future());
                }
                module.version = Some(new_version);
                let arc_source = module.source.clone();
                Box::new(
                    work_queue
                        .send(Entry {
                            key: uri,
                            value: arc_source,
                            version: new_version,
                        })
                        .map(|_| ()),
                )
            }
            Err(err) => {
                Box::new(log_message!(message_log.clone(), "{}", err.message).then(|_| Err(())))
            }
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
                )).catch_unwind()
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

                    let source = source::Source::with_lines(&module.source, module.lines.clone());
                    let byte_pos = position_to_byte_pos(&source, &params.position)?;

                    let env = thread.get_env();

                    Ok(
                        completion::signature_help(&*env, expr, byte_pos).map(|help| {
                            let (_, metadata_map) = gluon::check::metadata::metadata(&*env, expr);
                            let comment = if help.name.is_empty() {
                                None
                            } else {
                                completion::suggest_metadata(
                                    &metadata_map,
                                    &*env,
                                    expr,
                                    byte_pos,
                                    &help.name,
                                ).and_then(|metadata| metadata.comment.clone())
                            };

                            SignatureHelp {
                                signatures: vec![
                                    SignatureInformation {
                                        label: help.name,
                                        documentation: Some(make_documentation(
                                            Some(&help.typ),
                                            &comment.unwrap_or("".to_string()),
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
                                    },
                                ],
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
    (
        io,
        exit_receiver,
        message_log_receiver,
        message_log,
    )
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::path::PathBuf;

    use url::Url;

    use super::*;

    #[test]
    fn test_strip_file_prefix() {
        let renamed = strip_file_prefix(
            &[PathBuf::from(".")],
            &Url::from_file_path(env::current_dir().unwrap().join("test")).unwrap(),
        ).unwrap();
        assert_eq!(renamed, "test");
    }
}
