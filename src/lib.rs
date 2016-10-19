#![cfg_attr(feature = "serde_macros", feature(custom_derive, plugin))]
#![cfg_attr(feature = "serde_macros", plugin(serde_macros))]

extern crate serde;
extern crate serde_json;

extern crate jsonrpc_core;

#[macro_use]
extern crate log;
extern crate env_logger;
extern crate gluon;
extern crate url;

extern crate vscode_languageserver_types;

use jsonrpc_core::{Error, ErrorCode, IoHandler, MethodCommand, MethodResult, NotificationCommand,
                   Params, Value};
use serde_json::value::{from_value, to_value};

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

use std::error::Error as StdError;
use std::fs;
use std::io;
use std::io::{Read, BufRead, Write};
use std::str;
use std::sync::{Arc, Mutex};
use std::sync::atomic;
use std::sync::atomic::AtomicBool;

use vscode_languageserver_types::*;

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

struct ServerError<E> {
    message: String,
    data: Option<E>,
}

trait LanguageServerCommand: Send + Sync {
    type Param: serde::Deserialize;
    type Output: serde::Serialize;
    type Error: serde::Serialize;
    fn execute(&self, param: Self::Param) -> Result<Self::Output, ServerError<Self::Error>>;

    fn invalid_params(&self) -> Option<Self::Error>;
}

trait LanguageServerNotification: Send + Sync {
    type Param: serde::Deserialize;
    fn execute(&self, param: Self::Param);
}

struct ServerCommand<T>(T);

impl<T> NotificationCommand for ServerCommand<T>
    where T: LanguageServerNotification,
{
    fn execute(&self, param: Params) {
        match param {
            Params::Map(ref map) => {
                match from_value(Value::Object(map.clone())) {
                    Ok(value) => {
                        self.0.execute(value);
                    }
                    Err(_) => log_message(format!("Invalid parameters: {:?}", map)),
                }
            }
            _ => log_message(format!("Invalid parameters: {:?}", param)),
        }
    }
}

impl<T> MethodCommand for ServerCommand<T>
    where T: LanguageServerCommand,
{
    fn execute(&self, param: Params) -> MethodResult {
        match param {
            Params::Map(ref map) => {
                match from_value(Value::Object(map.clone())) {
                    Ok(value) => {
                        let result = match self.0.execute(value) {
                            Ok(value) => Ok(to_value(&value)),
                            Err(error) => {
                                Err(Error {
                                    code: ErrorCode::InternalError,
                                    message: error.message,
                                    data: error.data.as_ref().map(to_value),
                                })
                            }
                        };
                        return MethodResult::Sync(result);
                    }
                    Err(_) => (),
                }
            }
            _ => (),
        }
        let data = self.0.invalid_params();
        MethodResult::Sync(Err(Error {
            code: ErrorCode::InvalidParams,
            message: format!("Invalid params: {:?}", param),
            data: data.as_ref().map(to_value),
        }))
    }
}

struct Initialize(RootedThread);
impl LanguageServerCommand for Initialize {
    type Param = InitializeParams;
    type Output = InitializeResult;
    type Error = InitializeError;
    fn execute(&self,
               change: InitializeParams)
               -> Result<InitializeResult, ServerError<InitializeError>> {
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
    }

    fn invalid_params(&self) -> Option<Self::Error> {
        Some(InitializeError { retry: false })
    }
}

struct Completion(RootedThread);
impl LanguageServerCommand for Completion {
    type Param = TextDocumentPositionParams;
    type Output = Vec<CompletionItem>;
    type Error = ();
    fn execute(&self,
               change: TextDocumentPositionParams)
               -> Result<Vec<CompletionItem>, ServerError<()>> {
        let thread = &self.0;
        let module = strip_file_prefix(thread, &change.text_document.uri);
        let import = thread.get_macros().get("import").expect("Import macro");
        let import = import.downcast_ref::<Import<CheckImporter>>().expect("Check importer");
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

        let items: Vec<_> = suggestions.into_iter()
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
        Ok(items)
    }

    fn invalid_params(&self) -> Option<Self::Error> {
        None
    }
}

struct HoverCommand(RootedThread);
impl LanguageServerCommand for HoverCommand {
    type Param = TextDocumentPositionParams;
    type Output = Hover;
    type Error = ();
    fn execute(&self, change: TextDocumentPositionParams) -> Result<Hover, ServerError<()>> {
        let thread = &self.0;
        let module = strip_file_prefix(thread, &change.text_document.uri);
        let import = thread.get_macros().get("import").expect("Import macro");
        let import = import.downcast_ref::<Import<CheckImporter>>().expect("Check importer");
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
                                     change.position.line,
                                     change.position.character),
                    data: None,
                }
            })
    }

    fn invalid_params(&self) -> Option<Self::Error> {
        None
    }
}

fn location_to_position(loc: &pos::Location) -> Position {
    Position {
        line: loc.line.to_usize() as u64 + 1,
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
impl LanguageServerNotification for TextDocumentDidOpen {
    type Param = DidOpenTextDocumentParams;

    fn execute(&self, change: DidOpenTextDocumentParams) {
        run_diagnostics(&self.0,
                        &change.text_document.uri,
                        &change.text_document.text);
    }
}

struct TextDocumentDidChange(RootedThread);
impl LanguageServerNotification for TextDocumentDidChange {
    type Param = DidChangeTextDocumentParams;

    fn execute(&self, change: DidChangeTextDocumentParams) {
        run_diagnostics(&self.0,
                        &change.text_document.uri,
                        &change.content_changes[0].text);
    }
}

fn strip_file_prefix(thread: &Thread, filename: &str) -> String {
    let import = thread.get_macros()
        .get("import")
        .expect("Import macro");
    let import = import.downcast_ref::<Import<CheckImporter>>()
        .expect("Check importer");
    let paths = import.paths.read().unwrap();

    let file = url::percent_encoding::percent_decode(filename.as_bytes()).decode_utf8_lossy();
    let url = url::Url::parse(&file).ok();
    let name = url.and_then(|url| url.to_file_path().ok())
        .and_then(|path| fs::canonicalize(path).ok());
    let name = match name {
        Some(name) => name,
        None => return filename.to_string(),
    };

    for path in &*paths {
        let canonicalized = fs::canonicalize(path).ok();

        let result = canonicalized.as_ref()
            .and_then(|path| name.strip_prefix(path).ok())
            .and_then(|path| path.to_str());
        if let Some(path) = result {
            return path.to_string();
        }
    }
    filename.to_string()
}

fn typecheck(thread: &Thread, filename: &str, fileinput: &str) -> GluonResult<()> {
    use gluon::compiler_pipeline::*;

    let filename = strip_file_prefix(thread, filename);
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
    result.or(parse_result)
}

fn run_diagnostics(thread: &Thread, filename: &str, fileinput: &str) {
    let diagnostics = match typecheck(thread, filename, fileinput) {
        Ok(_) => vec![],
        Err(err) => {
            match err {
                GluonError::Typecheck(err) => {
                    err.errors()
                        .errors
                        .into_iter()
                        .map(|err| {
                            Diagnostic {
                                message: format!("{}", err.value),
                                severity: Some(DiagnosticSeverity::Error),
                                range: span_to_range(&err.span),
                                ..Diagnostic::default()
                            }
                        })
                        .collect()
                }
                GluonError::Parse(err) => {
                    err.errors()
                        .errors
                        .into_iter()
                        .map(|err| {
                            let p = Position {
                                line: err.span.start.line.to_usize() as u64 - 1,
                                character: err.span.start.column.to_usize() as u64,
                            };
                            Diagnostic {
                                message: format!("{}", err),
                                severity: Some(DiagnosticSeverity::Error),
                                range: Range { start: p, end: p },
                                ..Diagnostic::default()
                            }
                        })
                        .collect()
                }
                err => {
                    vec![Diagnostic {
                             message: format!("{}", err),
                             severity: Some(DiagnosticSeverity::Error),
                             ..Diagnostic::default()
                         }]
                }
            }
        }
    };
    let r = format!(r#"{{
                        "jsonrpc": "2.0",
                        "method": "textDocument/publishDiagnostics",
                        "params": {}
                    }}"#,
                    to_value(&PublishDiagnosticsParams {
                        uri: filename.into(),
                        diagnostics: diagnostics,
                    }));
    print!("Content-Length: {}\r\n\r\n{}", r.len(), r);
}

fn log_message(message: String) {
    debug!("{}", message);
    let r = format!(r#"{{"jsonrpc": "2.0", "method": "window/logMessage", "params": {} }}"#,
                    to_value(&LogMessageParams {
                        typ: MessageType::Log,
                        message: message,
                    }));
    print!("Content-Length: {}\r\n\r\n{}", r.len(), r);
}

pub fn read_message<R>(mut reader: R) -> Result<Option<String>, Box<StdError>>
    where R: BufRead + Read,
{
    let mut header = String::new();
    let n = try!(reader.read_line(&mut header));
    if n == 0 {
        // EOF
        return Ok(None);
    }
    if header.starts_with("Content-Length: ") {
        let content_length = {
            let len = header["Content-Length:".len()..].trim();
            debug!("{}", len);
            try!(len.parse::<usize>())
        };
        while header != "\r\n" {
            header.clear();
            try!(reader.read_line(&mut header));
        }
        let mut content = vec![0; content_length];
        try!(reader.read_exact(&mut content));
        Ok(Some(try!(String::from_utf8(content))))
    } else {
        Err(format!("Invalid message: `{}`", header).into())
    }
}

fn main_loop(io: &mut IoHandler, exit_token: Arc<AtomicBool>) -> Result<(), Box<StdError>> {
    let stdin = io::stdin();
    while !exit_token.load(atomic::Ordering::SeqCst) {
        match try!(read_message(stdin.lock())) {
            Some(json) => {
                debug!("Handle: {}", json);
                if let Some(response) = io.handle_request_sync(&json) {
                    print!("Content-Length: {}\r\n\r\n{}", response.len(), response);
                    try!(io::stdout().flush());
                }
            }
            None => return Ok(()),
        }
    }
    Ok(())
}

pub fn run() {
    ::env_logger::init().unwrap();
    let handle = ::std::thread::spawn(|| {
        let thread = new_vm();
        let import = Import::new(CheckImporter::new());
        thread.get_macros().insert("import".into(), import);

        let mut io = IoHandler::new();
        io.add_method("initialize", ServerCommand(Initialize(thread.clone())));
        io.add_method("textDocument/completion",
                      ServerCommand(Completion(thread.clone())));
        io.add_method("textDocument/hover",
                      ServerCommand(HoverCommand(thread.clone())));
        io.add_method("shutdown", |_| Ok(Value::I64(0)));
        let exit_token = Arc::new(AtomicBool::new(false));
        let exit_token2 = exit_token.clone();
        io.add_notification("exit",
                            move |_| exit_token.store(true, atomic::Ordering::SeqCst));
        io.add_notification("textDocument/didOpen",
                            ServerCommand(TextDocumentDidOpen(thread.clone())));
        io.add_notification("textDocument/didChange",
                            ServerCommand(TextDocumentDidChange(thread)));

        main_loop(&mut io, exit_token2).unwrap();
    });
    if let Err(err) = handle.join() {
        let msg = err.downcast_ref::<&'static str>()
            .cloned()
            .or_else(|| err.downcast_ref::<String>().map(|s| &s[..]))
            .unwrap_or("Any");
        log_message(format!("Panic: `{}`", msg));
    }
}
