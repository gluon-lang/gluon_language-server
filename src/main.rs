#![feature(custom_derive, plugin)]
#![plugin(serde_macros)]

extern crate serde;
extern crate serde_json;

extern crate jsonrpc_core;

#[macro_use]
extern crate log;
extern crate env_logger;
extern crate gluon;

pub mod language_server;

use jsonrpc_core::{Error, ErrorCode, IoHandler, MethodCommand, NotificationCommand, Params, Value};
use serde_json::value::{from_value, to_value};

use gluon::base::ast;
use gluon::base::metadata::Metadata;
use gluon::base::symbol::Symbol;
use gluon::check::completion;
use gluon::import::{CheckImporter, Import};
use gluon::vm::internal::Value as GluonValue;
use gluon::vm::thread::ThreadInternal;
use gluon::{Compiler, Error as GluonError, Result as GluonResult, RootedThread, new_vm,
            filename_to_module};

use std::error::Error as StdError;
use std::io;
use std::io::{Read, Write};
use std::str;

use language_server::*;

struct Initialize(RootedThread);
impl MethodCommand for Initialize {
    fn execute(&self, _params: Params) -> Result<Value, Error> {
        let result = InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncKind::Full),
                completion_provider: Some(CompletionOptions {
                    resolve_provider: Some(true),
                    trigger_characters: vec![".".into()],
                }),
            },
        };
        Ok(to_value(&result))
    }
}

struct Completion(RootedThread);
impl MethodCommand for Completion {
    fn execute(&self, params: Params) -> Result<Value, Error> {
        let change: TextDocumentPositionParams = match params {
            Params::Map(map) => {
                match from_value(Value::Object(map)) {
                    Ok(change) => change,
                    Err(_) => return Err(Error::invalid_params()),
                }
            }
            _ => return Err(Error::invalid_params()),
        };
        let thread = &self.0;
        let module = change.text_document.uri;
        let import = thread.get_macros().get("import").expect("Import macro");
        let import = import.downcast_ref::<Import<CheckImporter>>().expect("Check importer");
        let importer = import.importer.0.lock().unwrap();
        let expr = try!(importer.get(&module).ok_or_else(|| {
            Error {
                code: ErrorCode::InternalError,
                message: format!("Module `{}` is not defined", module),
                data: None,
            }
        }));
        let suggestions = completion::suggest(&ast::EmptyEnv::new(),
                                              expr,
                                              ast::Location {
                                                  row: (change.position.line + 1) as i32,
                                                  column: change.position.character as i32,
                                                  absolute: 0,
                                              });
        let items: Vec<_> = suggestions.into_iter()
                                       .map(|symbol| {
                                           CompletionItem {
                                               label:
                                                   String::from(symbol.as_ref()
                                                                      .split(':')
                                                                      .next()
                                                                      .unwrap_or(symbol.as_ref())),
                                               detail: Some("comment".into()),
                                               kind: Some(CompletionItemKind::Variable),
                                               ..CompletionItem::default()
                                           }
                                       })
                                       .collect();
        Ok(to_value(&items))
    }
}

fn location_to_position(loc: &ast::Location) -> Position {
    Position {
        line: loc.row as u64 + 1,
        character: loc.column as u64,
    }
}
fn span_to_range(span: &ast::Span) -> Range {
    Range {
        start: location_to_position(&span.start),
        end: location_to_position(&span.end),
    }
}

struct TextDocumentDidChange(RootedThread);
impl NotificationCommand for TextDocumentDidChange {
    fn execute(&self, params: Params) {
        let change: DidChangeTextDocumentParams = match params {
            Params::Map(map) => {
                match from_value(Value::Object(map)) {
                    Ok(change) => change,
                    Err(_) => return,
                }
            }
            _ => return,
        };
        let diagnostics = match self.execute2(&change) {
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
        let filename = &change.text_document.uri;
        let r = format!(r#"{{
                            "jsonrpc": "2.0",
                            "method": "textDocument/publishDiagnostics",
                            "params": {}
                        }}"#,
                        to_value(&PublishDiagnosticsParams {
            uri: filename.clone(),
            diagnostics: diagnostics,
        }));
        print!("Content-Length: {}\r\n\r\n{}", r.len(), r);
    }
}

impl TextDocumentDidChange {
    fn execute2(&self, change: &DidChangeTextDocumentParams) -> GluonResult<()> {
        use gluon::compiler_pipeline::*;

        let thread = &self.0;
        let filename = &change.text_document.uri;
        let fileinput = &change.content_changes[0].text;
        let name = filename_to_module(filename);
        let mut compiler = Compiler::new();
        let MacroValue(mut expr) = try!(fileinput.expand_macro(&mut compiler, thread, &name));
        let result = match compiler.typecheck_expr(thread, &name, fileinput, &mut expr) {
            Ok(typ) => {
                let metadata = Metadata::default();
                try!(thread.global_env()
                           .set_global(Symbol::new(filename), typ, metadata, GluonValue::Int(0)));
                Ok(())
            }
            Err(err) => Err(err),
        };
        let import = thread.get_macros().get("import").expect("Import macro");
        let import = import.downcast_ref::<Import<CheckImporter>>()
                           .expect("Check importer");
        let mut importer = import.importer.0.lock().unwrap();
        importer.insert(filename.clone(), expr);
        result
    }
}

fn log_message(message: String) {
    let r = format!(r#"{{"jsonrpc": "2.0", "method": "window/logMessage", "params": {} }}"#,
                    to_value(&LogMessageParams {
                        typ: MessageType::Log,
                        message: message,
                    }));
    print!("Content-Length: {}\r\n\r\n{}", r.len(), r);
}

fn main_loop(io: &mut IoHandler) -> Result<(), Box<StdError>> {
    let stdin = io::stdin();
    loop {
        let mut header = String::new();
        let n = try!(stdin.read_line(&mut header));
        if n == 0 {
            // EOF
            return Ok(());
        }
        debug!("{}", header);
        if header.starts_with("Content-Length: ") {
            let content_length = {
                let len = header["Content-Length:".len()..].trim();
                debug!("{}", len);
                try!(len.parse::<usize>())
            };
            while header != "\r\n" {
                header.clear();
                try!(io::stdin().read_line(&mut header));
            }
            let mut content = vec![0; content_length];
            try!(stdin.lock().read_exact(&mut content));
            let json = try!(str::from_utf8(&content));
            if let Some(response) = io.handle_request(json) {
                print!("Content-Length: {}\r\n\r\n{}", response.len(), response);
                try!(io::stdout().flush());
            }
        }
    }
}

fn main() {
    ::env_logger::init().unwrap();
    let handle = ::std::thread::spawn(|| {
        let thread = new_vm();
        let import = Import::new(CheckImporter::new());
        thread.get_macros().insert("import".into(), import);

        let mut io = IoHandler::new();
        io.add_method("initialize", Initialize(thread.clone()));
        io.add_method("textDocument/completion", Completion(thread.clone()));
        io.add_notification("textDocument/didChange", TextDocumentDidChange(thread));

        main_loop(&mut io).unwrap();
    });
    if let Err(err) = handle.join() {
        let msg = err.downcast_ref::<&'static str>()
                     .cloned()
                     .or_else(|| err.downcast_ref::<String>().map(|s| &s[..]))
                     .unwrap_or("Any");
        log_message(format!("Panic: `{}`", msg));
    }
}
