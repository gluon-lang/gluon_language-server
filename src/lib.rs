#![cfg_attr(feature = "serde_macros", feature(custom_derive, plugin))]
#![cfg_attr(feature = "serde_macros", plugin(serde_macros))]

extern crate clap;

extern crate serde;
#[macro_use]
extern crate serde_derive;
extern crate serde_json;

extern crate futures;
extern crate jsonrpc_core;

extern crate env_logger;
extern crate gluon;
extern crate gluon_completion as completion;
extern crate gluon_format;
#[macro_use]
extern crate log;
extern crate url;
extern crate url_serde;

extern crate languageserver_types;

macro_rules! log_message {
    ($($ts: tt)+) => {
        if log_enabled!(::log::LogLevel::Debug) {
            ::log_message(format!( $($ts)+ ))
        }
    }
}

pub mod rpc;

use jsonrpc_core::{IoHandler, Value};

use url::Url;

use gluon::base::ast::{Expr, SpannedExpr, Typed};
use gluon::base::error::Errors;
use gluon::base::fnv::FnvMap;
use gluon::base::metadata::Metadata;
use gluon::base::pos::{self, BytePos, Line, Span};
use gluon::base::source;
use gluon::base::symbol::Symbol;
use gluon::base::types::{ArcType, BuiltinType, Type, TypeCache};
use gluon::import::{Import, Importer};
use gluon::vm::internal::Value as GluonValue;
use gluon::vm::thread::{Thread, ThreadInternal};
use gluon::vm::macros::Error as MacroError;
use gluon::compiler_pipeline::{MacroExpandable, MacroValue, TypecheckValue, Typecheckable};
use gluon::{filename_to_module, new_vm, Compiler, Error as GluonError, Result as GluonResult,
            RootedThread};

use std::collections::{BTreeMap, VecDeque};
use std::env;
use std::error::Error as StdError;
use std::fmt;
use std::fs;
use std::io::{self, BufReader, Write};
use std::path::PathBuf;
use std::str;
use std::sync::{Arc, Condvar, Mutex};
use std::sync::atomic;
use std::sync::atomic::AtomicBool;
use std::thread;

use languageserver_types::*;

use futures::{Future, IntoFuture};

pub type BoxFuture<I, E> = Box<Future<Item = I, Error = E> + Send + 'static>;

use rpc::*;

fn log_message(message: String) {
    debug!("{}", message);
    let r = format!(
        r#"{{"jsonrpc": "2.0", "method": "window/logMessage", "params": {} }}"#,
        serde_json::to_value(&LogMessageParams {
            typ: MessageType::Log,
            message: message,
        }).unwrap()
    );
    print!("Content-Length: {}\r\n\r\n{}", r.len(), r);
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

struct Module {
    lines: source::Lines,
    expr: SpannedExpr<Symbol>,
    source_string: String,
}

pub struct ChainImporter<I>(CheckImporter, I);

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
        module_name: &str,
        input: &str,
        expr: SpannedExpr<Symbol>,
    ) -> Result<(), MacroError> {
        let macro_value = MacroValue { expr: expr };
        let TypecheckValue { expr, typ } =
            try!(macro_value.typecheck(compiler, vm, module_name, input));

        let lines = source::Lines::new(input.as_bytes().iter().cloned());
        let (metadata, _) = gluon::check::metadata::metadata(&*vm.global_env().get_env(), &expr);
        self.0.lock().unwrap().insert(
            module_name.into(),
            self::Module {
                lines: lines,
                expr: expr,
                source_string: input.into(),
            },
        );
        // Insert a global to ensure the globals type can be looked up
        try!(vm.global_env().set_global(
            Symbol::from(module_name),
            typ,
            metadata,
            GluonValue::Int(0)
        ));
        Ok(())
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
                    text_document_sync: Some(TextDocumentSyncKind::Full),
                    completion_provider: Some(CompletionOptions {
                        resolve_provider: Some(true),
                        trigger_characters: vec![".".into()],
                    }),
                    hover_provider: Some(true),
                    document_formatting_provider: Some(true),
                    document_highlight_provider: Some(true),
                    document_symbol_provider: Some(true),
                    ..ServerCapabilities::default()
                },
            }).into_future(),
        )
    }

    fn invalid_params(&self) -> Option<Self::Error> {
        Some(InitializeError { retry: false })
    }
}

fn retrieve_expr<F, R>(thread: &Thread, text_document_uri: &Url, f: F) -> Result<R, ServerError<()>>
where
    F: FnOnce(&Module) -> Result<R, ServerError<()>>,
{
    let filename = strip_file_prefix_with_thread(thread, text_document_uri);
    let module = filename_to_module(&filename);
    let import = thread.get_macros().get("import").expect("Import macro");
    let import = import
        .downcast_ref::<Import<CheckImporter>>()
        .expect("Check importer");
    let importer = import.importer.0.lock().unwrap();
    let source_module = try!(importer.get(&module).ok_or_else(|| {
        ServerError {
            message: format!(
                "Module `{}` is not defined\n{:?}",
                module,
                importer.keys().collect::<Vec<_>>()
            ),
            data: None,
        }
    }));
    f(source_module)
}

fn retrieve_expr_with_pos<F, R>(
    thread: &Thread,
    text_document_uri: &Url,
    position: &Position,
    f: F,
) -> Result<R, ServerError<()>>
where
    F: FnOnce(&SpannedExpr<Symbol>, BytePos) -> Result<R, ServerError<()>>,
{
    retrieve_expr(thread, text_document_uri, |module| {
        let Module {
            ref expr,
            ref lines,
            ..
        } = *module;
        let byte_pos = position_to_byte_pos(lines, position)?;

        f(expr, byte_pos)
    })
}

#[derive(Serialize, Deserialize)]
pub struct CompletionData {
    #[serde(with = "url_serde")] pub text_document_uri: Url,
    pub position: Position,
}

struct Completion(RootedThread);
impl LanguageServerCommand<TextDocumentPositionParams> for Completion {
    type Output = Vec<CompletionItem>;
    type Error = ();
    fn execute(
        &self,
        change: TextDocumentPositionParams,
    ) -> BoxFuture<Vec<CompletionItem>, ServerError<()>> {
        Box::new(
            (|| -> Result<_, _> {
                let thread = &self.0;
                let suggestions = retrieve_expr_with_pos(
                    thread,
                    &change.text_document.uri,
                    &change.position,
                    |expr, byte_pos| Ok(completion::suggest(&*thread.get_env(), expr, byte_pos)),
                )?;

                let mut items: Vec<_> = suggestions
                    .into_iter()
                    .map(|ident| {
                        // Remove the `:Line x, Row y suffix`
                        let name: &str = ident.name.as_ref();
                        let label =
                            String::from(name.split(':').next().unwrap_or(ident.name.as_ref()));
                        CompletionItem {
                            label: label,
                            detail: Some(format!("{}", ident.typ)),
                            kind: Some(CompletionItemKind::Variable),
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

                Ok(items)
            })()
                .into_future(),
        )
    }

    fn invalid_params(&self) -> Option<Self::Error> {
        None
    }
}

struct HoverCommand(RootedThread);
impl LanguageServerCommand<TextDocumentPositionParams> for HoverCommand {
    type Output = Hover;
    type Error = ();
    fn execute(&self, change: TextDocumentPositionParams) -> BoxFuture<Hover, ServerError<()>> {
        Box::new(
            (|| -> Result<_, _> {
                let thread = &self.0;
                retrieve_expr(thread, &change.text_document.uri, |module| {
                    let expr = &module.expr;
                    let byte_pos = position_to_byte_pos(&module.lines, &change.position)?;

                    let env = thread.get_env();
                    let (_, metadata_map) = gluon::check::metadata::metadata(&*env, &expr);
                    let opt_metadata = completion::get_metadata(&metadata_map, expr, byte_pos);
                    let extract = (completion::TypeAt { env: &*env }, completion::SpanAt);
                    Ok(
                        completion::completion(extract, expr, byte_pos)
                            .map(|(typ, span)| {
                                let contents = match opt_metadata.and_then(|m| m.comment.as_ref()) {
                                    Some(comment) => format!("{}\n\n{}", typ, comment),
                                    None => format!("{}", typ),
                                };
                                Hover {
                                    contents: vec![MarkedString::String(contents)],
                                    range: byte_span_to_range(&module.lines, span).ok(),
                                }
                            })
                            .unwrap_or_else(|()| {
                                Hover {
                                    contents: vec![],
                                    range: None,
                                }
                            }),
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

fn location_to_position(loc: &pos::Location) -> Position {
    Position {
        line: loc.line.to_usize() as u64,
        character: loc.column.to_usize() as u64,
    }
}
fn span_to_range(span: &Span<pos::Location>) -> Range {
    Range {
        start: location_to_position(&span.start),
        end: location_to_position(&span.end),
    }
}

fn byte_pos_to_position(lines: &source::Lines, pos: BytePos) -> Result<Position, ServerError<()>> {
    Ok(location_to_position(&lines.location(pos).ok_or_else(|| {
        ServerError::from(&"Unable to translate index to location")
    })?))
}

fn byte_span_to_range(
    lines: &source::Lines,
    span: Span<BytePos>,
) -> Result<Range, ServerError<()>> {
    Ok(Range {
        start: byte_pos_to_position(lines, span.start)?,
        end: byte_pos_to_position(lines, span.end)?,
    })
}

fn position_to_byte_pos(
    lines: &source::Lines,
    position: &Position,
) -> Result<BytePos, ServerError<()>> {
    let line_pos = try!(lines.line(Line::from(position.line as usize),).ok_or_else(
        || {
            ServerError {
                message: format!(
                    "Position ({}, {}) is out of range",
                    position.line,
                    position.character
                ),
                data: None,
            }
        },
    ));
    Ok(line_pos + BytePos::from(position.character as usize))
}

struct TextDocumentDidOpen(RootedThread);
impl LanguageServerNotification<DidOpenTextDocumentParams> for TextDocumentDidOpen {
    fn execute(&self, change: DidOpenTextDocumentParams) {
        run_diagnostics(
            &self.0,
            &change.text_document.uri,
            &change.text_document.text,
        );
    }
}

struct TextDocumentDidChange(Arc<UniqueQueue<Url, String>>);
impl LanguageServerNotification<DidChangeTextDocumentParams> for TextDocumentDidChange {
    fn execute(&self, mut change: DidChangeTextDocumentParams) {
        use std::mem::replace;
        self.0.add_work(
            change.text_document.uri,
            replace(&mut change.content_changes[0].text, String::new()),
        )
    }
}

fn strip_file_prefix_with_thread(thread: &Thread, url: &Url) -> String {
    let import = thread.get_macros().get("import").expect("Import macro");
    let import = import
        .downcast_ref::<Import<CheckImporter>>()
        .expect("Check importer");
    let paths = import.paths.read().unwrap();
    strip_file_prefix(&paths, url).unwrap_or_else(|err| panic!("{}", err))
}

pub fn strip_file_prefix(paths: &[PathBuf], url: &Url) -> Result<String, Box<StdError>> {
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

fn typecheck(thread: &Thread, filename: &Url, fileinput: &str) -> GluonResult<()> {
    let filename = strip_file_prefix_with_thread(thread, filename);
    let name = filename_to_module(&filename);
    debug!("Loading: `{}`", name);
    let mut errors = Errors::new();
    let mut compiler = Compiler::new();
    // The parser may find parse errors but still produce an expression
    // For that case still typecheck the expression but return the parse error afterwards
    let mut expr = match compiler.parse_partial_expr(&TypeCache::new(), &name, fileinput) {
        Ok(expr) => expr,
        Err((None, err)) => return Err(err.into()),
        Err((Some(expr), err)) => {
            errors.push(err.into());
            expr
        }
    };
    if let Err(err) = expr.expand_macro(&mut compiler, thread, &name) {
        errors.push(err);
    }

    let check_result = (MacroValue { expr: &mut expr })
        .typecheck(&mut compiler, thread, &name, fileinput)
        .map(|value| value.typ);
    let typ = match check_result {
        Ok(typ) => typ,
        Err(err) => {
            errors.push(err);
            expr.env_type_of(&*thread.global_env().get_env())
        }
    };
    let metadata = Metadata::default();
    try!(thread.global_env().set_global(
        Symbol::from(&name[..]),
        typ,
        metadata,
        GluonValue::Int(0)
    ));
    let import = thread.get_macros().get("import").expect("Import macro");
    let import = import
        .downcast_ref::<Import<CheckImporter>>()
        .expect("Check importer");
    let mut importer = import.importer.0.lock().unwrap();

    let lines = source::Lines::new(fileinput.as_bytes().iter().cloned());
    importer.insert(
        name.into(),
        self::Module {
            lines: lines,
            expr: expr,
            source_string: fileinput.into(),
        },
    );
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.into())
    }
}

struct Entry<K, V> {
    key: K,
    value: V,
}

/// Queue which only keeps the latest work item for each key
struct UniqueQueue<K, V> {
    queue: Mutex<VecDeque<Entry<K, V>>>,
    new_work: Condvar,
}

impl<K, V> UniqueQueue<K, V>
where
    K: PartialEq,
{
    fn add_work(&self, key: K, value: V) {
        let mut queue = self.queue.lock().unwrap();
        // Overwrite the previous item if one exists already for this key
        if let Some(entry) = queue.iter_mut().find(|entry| entry.key == key) {
            entry.value = value;
            return;
        }
        queue.push_back(Entry {
            key: key,
            value: value,
        });
        self.new_work.notify_one();
    }

    fn stop(&self) {
        self.new_work.notify_one();
    }
}

struct DiagnosticProcessor {
    thread: RootedThread,
    work_queue: Arc<UniqueQueue<Url, String>>,
}

impl DiagnosticProcessor {
    fn run(&self) {
        let mut work_queue = self.work_queue.queue.lock().unwrap();
        loop {
            while let Some(entry) = work_queue.pop_front() {
                // Don't block the producers while we run diagnostics
                drop(work_queue);
                run_diagnostics(&self.thread, &entry.key, &entry.value);
                work_queue = self.work_queue.queue.lock().unwrap();
            }
            work_queue = self.work_queue.new_work.wait(work_queue).unwrap();
            if work_queue.is_empty() {
                break;
            }
        }
    }
}

fn create_diagnostics(
    diagnostics: &mut BTreeMap<Url, Vec<Diagnostic>>,
    filename: &Url,
    err: GluonError,
) {
    fn into_diagnostic<T>(err: pos::Spanned<T, pos::Location>) -> Diagnostic
    where
        T: fmt::Display,
    {
        Diagnostic {
            message: format!("{}", err.value),
            severity: Some(DiagnosticSeverity::Error),
            range: span_to_range(&err.span),
            ..Diagnostic::default()
        }
    }

    fn module_name_to_file_(s: &str) -> Result<Url, Box<StdError>> {
        let mut result = s.replace(".", "/");
        result.push_str(".glu");
        let path = fs::canonicalize(&*result).or_else(|err| match env::current_dir() {
            Ok(path) => Ok(path.join(result)),
            Err(_) => Err(err),
        })?;
        Ok(url::Url::from_file_path(path)
            .or_else(|_| url::Url::from_file_path(s))
            .map_err(|_| {
                format!("Unable to convert module name to a url: `{}`", s)
            })?)
    }

    fn module_name_to_file(s: &str) -> Url {
        module_name_to_file_(s).unwrap()
    }

    match err {
        GluonError::Typecheck(err) => diagnostics
            .entry(module_name_to_file(&err.source_name))
            .or_insert(Vec::new())
            .extend(err.errors().into_iter().map(into_diagnostic)),
        GluonError::Parse(err) => diagnostics
            .entry(module_name_to_file(&err.source_name))
            .or_insert(Vec::new())
            .extend(err.errors().into_iter().map(into_diagnostic)),
        GluonError::Multiple(errors) => for err in errors {
            create_diagnostics(diagnostics, filename, err);
        },
        err => diagnostics
            .entry(filename.clone())
            .or_insert(Vec::new())
            .push(Diagnostic {
                message: format!("{}", err),
                severity: Some(DiagnosticSeverity::Error),
                ..Diagnostic::default()
            }),
    }
}

fn run_diagnostics(thread: &Thread, filename: &Url, fileinput: &str) {
    info!("Running diagnostics on {}", filename);

    let diagnostics = match typecheck(thread, filename, fileinput) {
        Ok(_) => Some((filename.clone(), vec![])).into_iter().collect(),
        Err(err) => {
            debug!("Diagnostics result on `{}`: {}", filename, err);
            let mut diagnostics = BTreeMap::new();
            create_diagnostics(&mut diagnostics, filename, err);
            diagnostics
        }
    };
    for (source_name, diagnostic) in diagnostics {
        let r = format!(
            r#"{{
                            "jsonrpc": "2.0",
                            "method": "textDocument/publishDiagnostics",
                            "params": {}
                        }}"#,
            serde_json::to_value(&PublishDiagnosticsParams {
                uri: source_name,
                diagnostics: diagnostic,
            }).unwrap()
        );
        print!("Content-Length: {}\r\n\r\n{}", r.len(), r);
    }
}

pub fn run() {
    ::env_logger::init().unwrap();

    let _matches = clap::App::new("debugger")
        .version(env!("CARGO_PKG_VERSION"))
        .get_matches();

    let thread = new_vm();
    {
        let macros = thread.get_macros();
        let import = Import::new(CheckImporter::new());
        macros.insert("import".into(), import);
    }
    let handle = start_server(thread);

    if let Err(err) = handle.join() {
        let msg = err.downcast_ref::<&'static str>()
            .cloned()
            .or_else(|| err.downcast_ref::<String>().map(|s| &s[..]))
            .unwrap_or("Any");
        log_message!("Panic: `{}`", msg);
    }
}

pub fn start_server(thread: RootedThread) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let (io, exit_token, work_queue) = initialize_rpc(&thread);

        // Spawn a separate thread which runs a returns diagnostic information
        let diagnostics_handle = {
            let work_queue = work_queue.clone();
            thread::Builder::new()
                .name("diagnostics".to_string())
                .spawn(move || {
                    let diagnostics = DiagnosticProcessor {
                        thread: thread,
                        work_queue: work_queue,
                    };
                    diagnostics.run();
                })
                .unwrap()
        };


        let mut input = BufReader::new(io::stdin());
        let mut output = io::stdout();

        (|| -> Result<(), Box<StdError>> {
            while !exit_token.load(atomic::Ordering::SeqCst) {
                match try!(read_message(&mut input)) {
                    Some(json) => {
                        debug!("Handle: {}", json);
                        if let Some(response) = io.handle_request_sync(&json) {
                            try!(write_message_str(&mut output, &response));
                            try!(output.flush());
                        }
                    }
                    None => return Ok(()),
                }
            }
            Ok(())
        })()
            .unwrap();
        work_queue.stop();
        diagnostics_handle.join().unwrap();
    })
}

fn initialize_rpc(
    thread: &RootedThread,
) -> (IoHandler, Arc<AtomicBool>, Arc<UniqueQueue<Url, String>>) {
    let work_queue = Arc::new(UniqueQueue {
        queue: Mutex::new(VecDeque::new()),
        new_work: Condvar::new(),
    });

    let mut io = IoHandler::new();
    io.add_async_method(
        "initialize",
        ServerCommand::method(Initialize(thread.clone())),
    );
    io.add_async_method(
        "textDocument/completion",
        ServerCommand::method(Completion(thread.clone())),
    );

    {
        let thread = thread.clone();
        let resolve = move |mut item: CompletionItem| -> BoxFuture<CompletionItem, _> {
            let data: CompletionData =
                serde_json::from_value(item.data.clone().unwrap()).expect("CompletionData");

            log_message(format!("{:?}", data.text_document_uri));
            Box::new(
                retrieve_expr_with_pos(
                    &thread,
                    &data.text_document_uri,
                    &data.position,
                    |expr, byte_pos| {
                        let type_env = thread.global_env().get_env();
                        let (_, metadata_map) = gluon::check::metadata::metadata(&*type_env, expr);
                        log_message(format!("{}  {:?}", item.label, metadata_map));
                        Ok(
                            completion::suggest_metadata(
                                &metadata_map,
                                &*type_env,
                                expr,
                                byte_pos,
                                &item.label,
                            ).and_then(
                                |metadata| metadata.comment.clone(),
                            ),
                        )
                    },
                ).map(|comment| {
                    log_message(format!("{:?}", comment));
                    item.documentation = comment;
                    item
                })
                    .into_future(),
            )
        };
        io.add_async_method("completionItem/resolve", ServerCommand::method(resolve));
    }

    io.add_async_method(
        "textDocument/hover",
        ServerCommand::method(HoverCommand(thread.clone())),
    );

    {
        let thread = thread.clone();
        let format = move |params: DocumentFormattingParams| -> BoxFuture<Vec<_>, _> {
            Box::new(
                retrieve_expr(&thread, &params.text_document.uri, |module| {
                    let formatted = gluon_format::format_expr(&module.source_string)?;
                    Ok(vec![
                        TextEdit {
                            range: Range {
                                start: location_to_position(
                                    &module.lines.location(0.into()).ok_or_else(|| {
                                        ServerError::from(&"Unable to translate index to location")
                                    })?,
                                ),
                                end: location_to_position(&module
                                    .lines
                                    .location(module.source_string.len().into())
                                    .ok_or_else(|| {
                                        ServerError::from(&"Unable to translate index to location")
                                    })?),
                            },
                            new_text: formatted,
                        },
                    ])
                }).into_future(),
            )
        };
        io.add_async_method("textDocument/formatting", ServerCommand::method(format));
    }

    {
        let thread = thread.clone();
        let f = move |params: TextDocumentPositionParams| -> BoxFuture<Vec<_>, _> {
            Box::new(
                retrieve_expr(&thread, &params.text_document.uri, |module| {
                    let expr = &module.expr;
                    let byte_pos = position_to_byte_pos(&module.lines, &params.position)?;

                    let symbol_spans = completion::find_all_symbols(expr, byte_pos)
                        .map(|t| t.1)
                        .unwrap_or(Vec::new());

                    symbol_spans
                        .into_iter()
                        .map(|span| {
                            Ok(DocumentHighlight {
                                kind: None,
                                range: byte_span_to_range(&module.lines, span)?,
                            })
                        })
                        .collect::<Result<_, _>>()
                }).into_future(),
            )
        };
        io.add_async_method("textDocument/documentHighlight", ServerCommand::method(f));
    }

    {
        let thread = thread.clone();
        let f = move |params: DocumentSymbolParams| -> BoxFuture<Vec<_>, _> {
            Box::new(
                retrieve_expr(&thread, &params.text_document.uri, |module| {
                    let expr = &module.expr;

                    let symbols = completion::all_symbols(expr);

                    symbols
                        .into_iter()
                        .map(|symbol| {
                            use completion::CompletionSymbol;
                            let (kind, name) = match symbol.value {
                                CompletionSymbol::Type { ref name, .. } => {
                                    (SymbolKind::Class, name)
                                }
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
                                    uri: params.text_document.uri.clone(),

                                    range: byte_span_to_range(&module.lines, symbol.span)?,
                                },
                                name: name.declared_name().to_string(),
                                container_name: None,
                            })
                        })
                        .collect::<Result<_, _>>()
                }).into_future(),
            )
        };
        io.add_async_method("textDocument/documentSymbol", ServerCommand::method(f));
    }

    io.add_async_method("shutdown", |_| Box::new(futures::finished(Value::from(0))));
    let exit_token = Arc::new(AtomicBool::new(false));
    {
        let exit_token = exit_token.clone();
        io.add_notification("exit", move |_| {
            exit_token.store(true, atomic::Ordering::SeqCst)
        });
    }
    io.add_notification(
        "textDocument/didOpen",
        ServerCommand::notification(TextDocumentDidOpen(thread.clone())),
    );
    io.add_notification(
        "textDocument/didChange",
        ServerCommand::notification(TextDocumentDidChange(work_queue.clone())),
    );
    (io, exit_token, work_queue)
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::path::PathBuf;

    use url::Url;

    use super::strip_file_prefix;

    #[test]
    fn test_strip_file_prefix() {
        let renamed = strip_file_prefix(
            &[PathBuf::from(".")],
            &Url::from_file_path(env::current_dir().unwrap().join("test")).unwrap(),
        ).unwrap();
        assert_eq!(renamed, "test");
    }
}
