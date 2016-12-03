extern crate debugserver_types;
extern crate env_logger;
extern crate futures;
extern crate jsonrpc_core;
extern crate serde;
extern crate serde_json;

extern crate gluon_language_server;
extern crate gluon;

use std::error::Error as StdError;
use std::fs::File;
use std::io::{BufReader, Read};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Condvar, Mutex};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::spawn;
use std::collections::{HashMap, HashSet};

use futures::{Async, BoxFuture, Future, IntoFuture};

use jsonrpc_core::IoHandler;

use serde_json::Value;

use debugserver_types::*;

use gluon::base::pos::Line;
use gluon::vm::api::{OpaqueValue, Hole};
use gluon::vm::Error as VmError;
use gluon::vm::thread::{RootedThread, ThreadInternal, LINE_FLAG};
use gluon::{Compiler, filename_to_module};

use gluon_language_server::rpc::{LanguageServerCommand, ServerCommand, ServerError, main_loop,
                                 write_message};

pub struct InitializeHandler {
    stream: Arc<TcpStream>,
    seq: Arc<AtomicUsize>,
}

impl LanguageServerCommand<InitializeRequestArguments> for InitializeHandler {
    type Output = Option<Capabilities>;
    type Error = ();
    fn execute(&self,
               _args: InitializeRequestArguments)
               -> BoxFuture<Option<Capabilities>, ServerError<Self::Error>> {

        let initialized = InitializedEvent {
            body: None,
            event: "initialized".into(),
            seq: self.seq.fetch_add(1, Ordering::SeqCst) as i64,
            type_: "event".into(),
        };
        write_message(&*self.stream, &initialized)
            .map_err(|err| {
                ServerError {
                    message: format!("Unable to write result to initialize: {}", err),
                    data: None,
                }
            })
            .map(|_| {
                Some(Capabilities {
                    supports_configuration_done_request: Some(true),
                    ..Capabilities::default()
                })
            })
            .into_future()
            .boxed()
    }
}

pub struct LaunchHandler {
    debugger: Arc<Debugger>,
    thread: RootedThread,
    stream: Arc<TcpStream>,
    seq: Arc<AtomicUsize>,
}

impl LanguageServerCommand<Value> for LaunchHandler {
    type Output = Option<Value>;
    type Error = ();
    fn execute(&self, args: Value) -> BoxFuture<Option<Value>, ServerError<()>> {
        self.execute_launch(args).into_future().boxed()
    }
}

impl LaunchHandler {
    fn execute_launch(&self, args: Value) -> Result<Option<Value>, ServerError<()>> {
        let program = args.get("program")
            .and_then(|s| s.as_str())
            .ok_or_else(|| {
                ServerError {
                    message: "No program argument found".into(),
                    data: None,
                }
            })?;
        let module = filename_to_module(program);
        let expr = {
            let mut file = File::open(program).map_err(|_| {
                    ServerError {
                        message: format!("Program does not exit: `{}`", program),
                        data: None,
                    }
                })?;
            let mut expr = String::new();
            file.read_to_string(&mut expr).expect("Could not read from file");
            expr
        };

        let debugger = self.debugger.clone();
        self.thread.context().set_hook(Some(Box::new(move |_, debug_info| {
            let stack_info = debug_info.stack_info(0).unwrap();
            if let Some(line) = stack_info.line() {
                if debugger.should_break(stack_info.source_name(), line) {
                    let mut continue_exection = debugger.continue_mutex.lock().unwrap();
                    *continue_exection = false;
                    write_message(&*debugger.stream,
                                  &StoppedEvent {
                                      body: StoppedEventBody {
                                          all_threads_stopped: Some(true),
                                          reason: "breakpoint".into(),
                                          text: None,
                                          thread_id: Some(1),
                                      },
                                      event: "stopped".to_string(),
                                      seq: debugger.seq(),
                                      type_: "event".to_string(),
                                  }).map_err(|err| VmError::Message(format!("{}", err)))?;
                    while !*continue_exection {
                        continue_exection = debugger.continue_var.wait(continue_exection).unwrap();
                    }
                }
            }
            Ok(Async::Ready(()))
        })));
        self.thread.context().set_hook_mask(LINE_FLAG);

        let thread = self.thread.clone();
        let seq = self.seq.clone();
        let stream = self.stream.clone();
        spawn(move || {
            let _result = Compiler::new()
                .run_expr::<OpaqueValue<RootedThread, Hole>>(&thread, &module, &expr);
            let seq = seq.fetch_add(1, Ordering::SeqCst) as i64;
            let terminated = TerminatedEvent {
                body: None,
                event: "terminated".into(),
                seq: seq,
                type_: "event".into(),
            };
            write_message(&*stream, &terminated).unwrap();
        });
        Ok(None)
    }
}

pub struct DisconnectHandler {
    exit_token: Arc<AtomicBool>,
}

impl LanguageServerCommand<DisconnectArguments> for DisconnectHandler {
    type Output = Option<Value>;
    type Error = ();
    fn execute(&self, _args: Value) -> BoxFuture<Option<Value>, ServerError<()>> {
        self.exit_token.store(true, Ordering::SeqCst);
        Ok(None).into_future().boxed()
    }
}

// Translate debug server messages into jsonrpc-2.0
fn translate_request(message: String) -> Result<String, Box<StdError>> {
    use serde_json::Value;
    let mut data: Value = try!(serde_json::from_str(&message));
    if data.is_object() {
        let data = data.as_object_mut().unwrap();
        let command = data.get("command").and_then(|c| c.as_str()).expect("command").to_string();
        let seq = data.get("seq")
            .and_then(|c| {
                c.as_str()
                    .map(|s| Value::String(s.to_string()))
                    .or_else(|| c.as_i64().map(Value::from))
            })
            .expect("seq");
        let arguments = data.remove("arguments").unwrap_or(Value::Object(Default::default()));
        let map = vec![("method".to_string(), Value::String(command)),
                       ("id".to_string(), seq),
                       ("params".to_string(), arguments),
                       ("jsonrpc".to_owned(), Value::String("2.0".to_string()))]
            .into_iter()
            .collect();
        return Ok(try!(serde_json::to_string(&Value::Object(map))));
    }
    Ok(message)
}

fn translate_response(debugger: &Debugger, message: String) -> Result<String, Box<StdError>> {
    use serde_json::Value;
    let mut data: Value = try!(serde_json::from_str(&message));
    if data.is_object() {
        let data = data.as_object_mut().unwrap();
        let seq = data.get("id")
            .and_then(|c| {
                c.as_str()
                    .map(|s| Value::String(s.to_string()))
                    .or_else(|| c.as_i64().map(Value::from))
            })
            .expect("id");
        let result = data.remove("result");
        let error = data.remove("error");

        let mut map = vec![("success".to_string(), Value::Bool(result.is_some())),
                           ("request_seq".to_string(), seq),
                           ("seq".to_string(), Value::from(debugger.seq())),
                           ("type".to_owned(), Value::String("response".to_string()))];

        if let Some(result) = result {
            map.push(("body".to_string(), result));
        }
        if let Some(mut error) = error {
            map.push(("message".to_string(),
                      error.as_object_mut().unwrap().remove("message").expect("error message")));
        }

        let map = map.into_iter().collect();
        return Ok(try!(serde_json::to_string(&Value::Object(map))));
    }
    Ok(message)
}

pub struct Debugger {
    stream: Arc<TcpStream>,
    breakpoints: Mutex<HashMap<String, HashSet<Line>>>,
    continue_mutex: Mutex<bool>,
    continue_var: Condvar,
    seq: Arc<AtomicUsize>,
}

impl Debugger {
    fn seq(&self) -> i64 {
        self.seq.fetch_add(1, Ordering::SeqCst) as i64
    }

    fn should_break(&self, source_name: &str, line: Line) -> bool {
        let breakpoints = self.breakpoints.lock().expect("breakpoints mutex is poisoned");
        breakpoints.get(source_name)
            .map_or(false, |lines| lines.contains(&line))
    }
}

pub fn main() {
    env_logger::init().unwrap();

    let listener = TcpListener::bind("127.0.0.1:4711").unwrap();
    for stream in listener.incoming() {
        let stream = Arc::new(stream.unwrap());

        let exit_token = Arc::new(AtomicBool::new(false));
        let seq = Arc::new(AtomicUsize::new(1));

        let thread = gluon::new_vm();

        let debugger = Arc::new(Debugger {
            stream: stream.clone(),
            breakpoints: Mutex::new(HashMap::new()),
            continue_mutex: Mutex::new(false),
            continue_var: Condvar::new(),
            seq: seq.clone(),
        });

        let mut io = IoHandler::new();
        io.add_async_method("initialize",
                            ServerCommand::new(InitializeHandler {
                                stream: stream.clone(),
                                seq: seq.clone(),
                            }));

        let configuration_done =
            move |_: ConfigurationDoneArguments| -> BoxFuture<(), ServerError<()>> {
                Ok(()).into_future().boxed()
            };
        io.add_async_method("configurationDone", ServerCommand::new(configuration_done));

        io.add_async_method("launch",
                            ServerCommand::new(LaunchHandler {
                                debugger: debugger.clone(),
                                thread: thread,
                                stream: stream.clone(),
                                seq: seq.clone(),
                            }));
        io.add_async_method("disconnect",
                            ServerCommand::new(DisconnectHandler {
                                exit_token: exit_token.clone(),
                            }));
        {
            let debugger = debugger.clone();
            let set_break = move |args: SetBreakpointsArguments| -> BoxFuture<_, ServerError<()>> {
                let mut breakpoint_map = debugger.breakpoints.lock().unwrap();
                let breakpoints =
                    args.breakpoints
                        .iter()
                        .flat_map(|bs| {
                            bs.iter().map(|breakpoint| Line::from(breakpoint.line as usize))
                        })
                        .collect();
                if let Some(path) = args.source.path {
                    breakpoint_map.insert(filename_to_module(path.trim_right_matches(".glu")),
                                          breakpoints);
                }
                Ok(SetBreakpointsResponseBody {
                        breakpoints: args.breakpoints
                            .into_iter()
                            .flat_map(|bs| bs)
                            .map(|breakpoint| {
                                Breakpoint {
                                    column: None,
                                    end_column: None,
                                    end_line: None,
                                    id: None,
                                    line: Some(breakpoint.line),
                                    message: None,
                                    source: None,
                                    verified: true,
                                }
                            })
                            .collect(),
                    })
                    .into_future()
                    .boxed()
            };

            io.add_async_method("setBreakpoints", ServerCommand::new(set_break));
        }

        let threads = move |_: Value| -> BoxFuture<ThreadsResponseBody, ServerError<()>> {
            Ok(ThreadsResponseBody {
                    threads: vec![Thread {
                                      id: 1,
                                      name: "main".to_string(),
                                  }],
                })
                .into_future()
                .boxed()
        };

        io.add_async_method("threads", ServerCommand::new(threads));

        let stack_trace = move |_: StackTraceArguments| -> BoxFuture<_, ServerError<()>> {
            Ok(StackTraceResponseBody {
                    stack_frames: vec![StackFrame {
                                           column: 0,
                                           end_column: None,
                                           end_line: None,
                                           id: 0,
                                           line: 0,
                                           module_id: None,
                                           name: "main".to_string(),
                                           source: None,
                                       }],
                    total_frames: Some(1),
                })
                .into_future()
                .boxed()
        };
        io.add_async_method("stackTrace", ServerCommand::new(stack_trace));

        let scopes = move |_: ScopesArguments| -> BoxFuture<_, ServerError<()>> {
            Ok(ScopesResponseBody {
                    scopes: vec![Scope {
                                     column: None,
                                     end_column: None,
                                     end_line: None,
                                     expensive: false,
                                     indexed_variables: None,
                                     line: None,
                                     name: "Locals".to_string(),
                                     named_variables: None,
                                     source: Some(Source {
                                         name: Some("main".to_string()),
                                         ..Source::default()
                                     }),
                                     variables_reference: 0,
                                 }],
                })
                .into_future()
                .boxed()
        };
        io.add_async_method("scopes", ServerCommand::new(scopes));

        {
            let debugger = debugger.clone();
            let cont = move |_: ContinueArguments| -> BoxFuture<Option<Value>, ServerError<()>> {
                *debugger.continue_mutex.lock().unwrap() = true;
                debugger.continue_var.notify_one();
                Ok(None).into_future().boxed()
            };
            io.add_async_method("continue", ServerCommand::new(cont));
        }

        spawn(move || {
            let read = BufReader::new(&*stream);
            main_loop(read,
                      &*stream,
                      &io,
                      exit_token,
                      translate_request,
                      |body| translate_response(&debugger, body))
                .unwrap()
        });
    }
}
