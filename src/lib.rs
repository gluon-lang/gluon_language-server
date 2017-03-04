#![cfg_attr(feature = "serde_macros", feature(custom_derive, plugin))]
#![cfg_attr(feature = "serde_macros", plugin(serde_macros))]

extern crate serde;
extern crate serde_json;

extern crate jsonrpc_core;
extern crate futures;

#[macro_use]
extern crate log;
extern crate env_logger;
extern crate gluon;
extern crate url;

extern crate languageserver_types;

pub mod rpc;

use jsonrpc_core::{IoHandler, RpcNotificationSimple, Params, Value};

use url::Url;

use gluon::base::ast::SpannedExpr;
use gluon::base::fnv::FnvMap;
use gluon::base::metadata::Metadata;
use gluon::base::pos::{self, BytePos, Line, Span};
use gluon::base::source;
use gluon::base::symbol::Symbol;
use gluon::check::completion;
use gluon::import::{Import, Importer};
use gluon::vm::internal::Value as GluonValue;
use gluon::vm::thread::{Thread, ThreadInternal};
use gluon::vm::macros::Error as MacroError;
use gluon::{Compiler, Error as GluonError, Result as GluonResult, RootedThread, new_vm,
            filename_to_module};

use std::collections::VecDeque;
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

use futures::{BoxFuture, Future, IntoFuture};

use rpc::*;

impl<T, P> RpcNotificationSimple for ServerCommand<T, P>
    where T: LanguageServerNotification<P>,
          P: serde::Deserialize + 'static,
{
    fn execute(&self, param: Params) {
        match param {
            Params::Map(map) => {
                match serde_json::from_value(Value::Object(map)) {
                    Ok(value) => {
                        self.0.execute(value);
                    }
                    Err(err) => log_message(format!("Invalid parameters. Reason: {}", err)),
                }
            }
            _ => log_message(format!("Invalid parameters: {:?}", param)),
        }
    }
}

fn log_message(message: String) {
    debug!("{}", message);
    let r = format!(r#"{{"jsonrpc": "2.0", "method": "window/logMessage", "params": {} }}"#,
                    serde_json::to_value(&LogMessageParams {
                            typ: MessageType::Log,
                            message: message,
                        })
                        .unwrap());
    print!("Content-Length: {}\r\n\r\n{}", r.len(), r);
}

#[derive(Clone)]
pub struct CheckImporter(pub Arc<Mutex<FnvMap<String, (source::Lines, SpannedExpr<Symbol>)>>>);
impl CheckImporter {
    pub fn new() -> CheckImporter {
        CheckImporter(Arc::new(Mutex::new(FnvMap::default())))
    }
}
impl Importer for CheckImporter {
    fn import(&self,
              compiler: &mut Compiler,
              vm: &Thread,
              module_name: &str,
              input: &str,
              expr: SpannedExpr<Symbol>)
              -> Result<(), MacroError> {
        use gluon::compiler_pipeline::*;

        let macro_value = MacroValue { expr: expr };
        let TypecheckValue { expr, typ } =
            try!(macro_value.typecheck(compiler, vm, module_name, input));

        let lines = source::Lines::new(input);
        self.0.lock().unwrap().insert(module_name.into(), (lines, expr));
        let metadata = Metadata::default();
        // Insert a global to ensure the globals type can be looked up
        try!(vm.global_env()
            .set_global(Symbol::from(module_name), typ, metadata, GluonValue::Int(0)));
        Ok(())
    }
}

struct Initialize(RootedThread);
impl LanguageServerCommand<InitializeParams> for Initialize {
    type Output = InitializeResult;
    type Error = InitializeError;
    fn execute(&self,
               change: InitializeParams)
               -> BoxFuture<InitializeResult, ServerError<InitializeError>> {
        let import = self.0.get_macros().get("import").expect("Import macro");
        let import = import.downcast_ref::<Import<CheckImporter>>()
            .expect("Check importer");
        if let Some(ref path) = change.root_path {
            import.add_path(path);
        }
        Ok(InitializeResult {
                capabilities: ServerCapabilities {
                    text_document_sync: Some(TextDocumentSyncKind::Full),
                    completion_provider: Some(CompletionOptions {
                        resolve_provider: Some(true),
                        trigger_characters: vec![".".into()],
                    }),
                    hover_provider: Some(true),
                    ..ServerCapabilities::default()
                },
            })
            .into_future()
            .boxed()
    }

    fn invalid_params(&self) -> Option<Self::Error> {
        Some(InitializeError { retry: false })
    }
}

struct Completion(RootedThread);
impl LanguageServerCommand<TextDocumentPositionParams> for Completion {
    type Output = Vec<CompletionItem>;
    type Error = ();
    fn execute(&self,
               change: TextDocumentPositionParams)
               -> BoxFuture<Vec<CompletionItem>, ServerError<()>> {
        (|| -> Result<_, _> {
                let thread = &self.0;
                let module = strip_file_prefix_with_thread(thread, &change.text_document.uri);
                let import = thread.get_macros().get("import").expect("Import macro");
                let import = import.downcast_ref::<Import<CheckImporter>>()
                    .expect("Check importer");
                let importer = import.importer.0.lock().unwrap();
                let &(ref line_map, ref expr) = try!(importer.get(&module).ok_or_else(|| {
                    ServerError {
                        message: format!("Module `{}` is not defined", module),
                        data: None,
                    }
                }));

                let line_pos = try!(line_map.line(Line::from(change.position.line as usize))
                    .ok_or_else(|| {
                        ServerError {
                            message: format!("Position ({}, {}) is out of range",
                                             change.position.line,
                                             change.position.character),
                            data: None,
                        }
                    }));
                let byte_pos = line_pos + BytePos::from(change.position.character as usize);
                let suggestions = completion::suggest(&*thread.get_env(), expr, byte_pos);

                let mut items: Vec<_> = suggestions.into_iter()
                    .map(|ident| {
                        // Remove the `:Line x, Row y suffix`
                        let name: &str = ident.name.as_ref();
                        let label = String::from(name.split(':')
                            .next()
                            .unwrap_or(ident.name.as_ref()));
                        CompletionItem {
                            label: label,
                            detail: Some(format!("{}", ident.typ)),
                            kind: Some(CompletionItemKind::Variable),
                            ..CompletionItem::default()
                        }
                    })
                    .collect();

                items.sort_by(|l, r| l.label.cmp(&r.label));

                Ok(items)
            })()
            .into_future()
            .boxed()
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
        (|| -> Result<_, _> {
                let thread = &self.0;
                let module = strip_file_prefix_with_thread(thread, &change.text_document.uri);
                let import = thread.get_macros().get("import").expect("Import macro");
                let import = import.downcast_ref::<Import<CheckImporter>>()
                    .expect("Check importer");
                let importer = import.importer.0.lock().unwrap();
                let &(ref line_map, ref expr) = try!(importer.get(&module).ok_or_else(|| {
                    ServerError {
                        message: format!("Module `{}` is not defined", module),
                        data: None,
                    }
                }));

                let line_pos = try!(line_map.line(Line::from(change.position.line as usize))
                    .ok_or_else(|| {
                        ServerError {
                            message: format!("Position ({}, {}) is out of range",
                                             change.position.line,
                                             change.position.character),
                            data: None,
                        }
                    }));
                let byte_pos = line_pos + BytePos::from(change.position.character as usize);
                completion::find(&*thread.get_env(), expr, byte_pos)
            .map(|typ| {
                Hover {
                    contents: vec![MarkedString::String(format!("{}", typ))],
                    range: None,
                }
            })
            .map_err(|()| {
                ServerError {
                    message: format!("Completion not found at: Line {}, Column {}",
                                     change.position.line + 1,
                                     change.position.character + 1),
                    data: None,
                }
            })
            })()
            .into_future()
            .boxed()
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

struct TextDocumentDidOpen(RootedThread);
impl LanguageServerNotification<DidOpenTextDocumentParams> for TextDocumentDidOpen {
    fn execute(&self, change: DidOpenTextDocumentParams) {
        run_diagnostics(&self.0,
                        &change.text_document.uri,
                        &change.text_document.text);
    }
}

struct TextDocumentDidChange(Arc<UniqueQueue<Url, String>>);
impl LanguageServerNotification<DidChangeTextDocumentParams> for TextDocumentDidChange {
    fn execute(&self, mut change: DidChangeTextDocumentParams) {
        use std::mem::replace;
        self.0.add_work(change.text_document.uri,
                        replace(&mut change.content_changes[0].text, String::new()))
    }
}

fn strip_file_prefix_with_thread(thread: &Thread, url: &Url) -> String {
    let import = thread.get_macros()
        .get("import")
        .expect("Import macro");
    let import = import.downcast_ref::<Import<CheckImporter>>()
        .expect("Check importer");
    let paths = import.paths.read().unwrap();
    strip_file_prefix(&paths, url).unwrap()
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

        let result = canonicalized.as_ref()
            .and_then(|path| name.strip_prefix(path).ok())
            .and_then(|path| path.to_str());
        if let Some(path) = result {
            return Ok(format!("{}", path));
        }
    }
    Ok(format!("{}", name.strip_prefix(&env::current_dir()?)?.display()))
}

fn typecheck(thread: &Thread, filename: &Url, fileinput: &str) -> GluonResult<()> {
    use gluon::compiler_pipeline::*;

    let filename = strip_file_prefix_with_thread(thread, filename);
    let name = filename_to_module(&filename);
    let mut compiler = Compiler::new();
    // The parser may find parse errors but still produce an expression
    // For that case still typecheck the expression but return the parse error afterwards
    let (mut expr, parse_result): (_, GluonResult<()>) =
        match compiler.parse_partial_expr(&name, fileinput) {
            Ok(expr) => (expr, Ok(())),
            Err((None, err)) => return Err(err.into()),
            Err((Some(expr), err)) => (expr, Err(err.into())),
        };
    try!(expr.expand_macro(&mut compiler, thread, &name));
    let result = match compiler.typecheck_expr(thread, &name, fileinput, &mut expr) {
        Ok(typ) => {
            let metadata = Metadata::default();
            try!(thread.global_env()
                .set_global(Symbol::from(&filename[..]),
                            typ,
                            metadata,
                            GluonValue::Int(0)));
            Ok(())
        }
        Err(err) => Err(err),
    };
    let import = thread.get_macros().get("import").expect("Import macro");
    let import = import.downcast_ref::<Import<CheckImporter>>()
        .expect("Check importer");
    let mut importer = import.importer.0.lock().unwrap();

    let lines = source::Lines::new(fileinput);
    importer.insert(filename.into(), (lines, expr));
    parse_result.and(result)
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
    where K: PartialEq,
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
        }
    }
}

fn run_diagnostics(thread: &Thread, filename: &Url, fileinput: &str) {
    info!("Running diagnostics on {}", filename);

    fn into_diagnostic<T>(err: pos::Spanned<T, pos::Location>) -> Diagnostic
        where T: fmt::Display,
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
        Ok(url::Url::from_file_path(path).or_else(|_| url::Url::from_file_path(s))
            .map_err(|_| format!("Unable to convert module name to a url: `{}`", s))?)
    }

    fn module_name_to_file(s: &str) -> Url {
        module_name_to_file_(s).unwrap()
    }

    let (source_name, diagnostics) = match typecheck(thread, filename, fileinput) {
        Ok(_) => (filename.clone(), vec![]),
        Err(err) => {
            match err {
                GluonError::Typecheck(err) => {
                    (module_name_to_file(&err.source_name),
                     err.errors()
                         .into_iter()
                         .map(into_diagnostic)
                         .collect())
                }
                GluonError::Parse(err) => {
                    (module_name_to_file(&err.source_name),
                     err.errors()
                         .into_iter()
                         .map(into_diagnostic)
                         .collect())
                }
                err => {
                    (filename.clone(),
                     vec![Diagnostic {
                              message: format!("{}", err),
                              severity: Some(DiagnosticSeverity::Error),
                              ..Diagnostic::default()
                          }])
                }
            }
        }
    };
    let r = format!(r#"{{
                        "jsonrpc": "2.0",
                        "method": "textDocument/publishDiagnostics",
                        "params": {}
                    }}"#,
                    serde_json::to_value(&PublishDiagnosticsParams {
                            uri: source_name,
                            diagnostics: diagnostics,
                        })
                        .unwrap());
    print!("Content-Length: {}\r\n\r\n{}", r.len(), r);
}

pub fn run() {
    ::env_logger::init().unwrap();

    let thread = new_vm();
    let work_queue = Arc::new(UniqueQueue {
        queue: Mutex::new(VecDeque::new()),
        new_work: Condvar::new(),
    });

    let handle = {
        let work_queue = work_queue.clone();
        let thread = thread.clone();
        thread::spawn(move || {

            let import = Import::new(CheckImporter::new());
            thread.get_macros().insert("import".into(), import);

            let mut io = IoHandler::new();
            io.add_async_method("initialize", ServerCommand::new(Initialize(thread.clone())));
            io.add_async_method("textDocument/completion",
                                ServerCommand::new(Completion(thread.clone())));
            io.add_async_method("textDocument/hover",
                                ServerCommand::new(HoverCommand(thread.clone())));
            io.add_async_method("shutdown", |_| futures::finished(Value::from(0)).boxed());
            let exit_token = Arc::new(AtomicBool::new(false));
            {
                let exit_token = exit_token.clone();
                io.add_notification("exit",
                                    move |_| exit_token.store(true, atomic::Ordering::SeqCst));
            }
            io.add_notification("textDocument/didOpen",
                                ServerCommand::new(TextDocumentDidOpen(thread.clone())));
            io.add_notification("textDocument/didChange",
                                ServerCommand::new(TextDocumentDidChange(work_queue.clone())));

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
        })
    };

    // Spawn a separate thread which runs a returns diagnostic information
    thread::Builder::new()
        .name("diagnostics".to_string())
        .spawn(move || {
            let diagnostics = DiagnosticProcessor {
                thread: thread,
                work_queue: work_queue,
            };
            diagnostics.run();
        })
        .unwrap();

    if let Err(err) = handle.join() {
        let msg = err.downcast_ref::<&'static str>()
            .cloned()
            .or_else(|| err.downcast_ref::<String>().map(|s| &s[..]))
            .unwrap_or("Any");
        log_message(format!("Panic: `{}`", msg));
    }
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::path::PathBuf;

    use url::Url;

    use super::strip_file_prefix;

    #[test]
    fn test_strip_file_prefix() {
        let renamed =
            strip_file_prefix(&[PathBuf::from(".")],
                              &Url::from_file_path(env::current_dir().unwrap().join("test"))
                                  .unwrap())
                .unwrap();
        assert_eq!(renamed, "test");
    }
}
