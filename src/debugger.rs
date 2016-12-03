extern crate debugserver_types;
extern crate env_logger;
extern crate futures;
extern crate jsonrpc_core;
extern crate serde;
extern crate serde_json;
extern crate log;

extern crate gluon_language_server;
extern crate gluon;

use std::error::Error as StdError;
use std::fs::File;
use std::io::{BufReader, Read};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Barrier, Mutex};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::spawn;
use std::collections::{HashMap, HashSet};

use futures::{Async, BoxFuture, Future, IntoFuture};

use jsonrpc_core::IoHandler;

use serde_json::Value;

use debugserver_types::*;

use gluon::base::pos::Line;
use gluon::vm::api::{OpaqueValue, Hole};
use gluon::vm::thread::{DebugInfo, RootedThread, ThreadInternal, LINE_FLAG};
use gluon::{Compiler, filename_to_module};

use gluon_language_server::rpc::{LanguageServerCommand, ServerCommand, ServerError, main_loop,
                                 write_message};

pub struct InitializeHandler {
    debugger: Arc<Debugger>,
}

impl LanguageServerCommand<InitializeRequestArguments> for InitializeHandler {
    type Output = Option<Capabilities>;
    type Error = ();
    fn execute(&self,
               args: InitializeRequestArguments)
               -> BoxFuture<Option<Capabilities>, ServerError<Self::Error>> {
        self.debugger
            .lines_start_at_1
            .store(args.lines_start_at_1.unwrap_or(true), Ordering::Release);

        self.debugger.send_event(InitializedEvent {
            body: None,
            event: "initialized".into(),
            seq: self.debugger.seq(),
            type_: "event".into(),
        });

        Ok(Some(Capabilities {
                supports_configuration_done_request: Some(true),
                ..Capabilities::default()
            }))
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
                    debugger.store_stack_trace(&debug_info);

                    debugger.send_event(StoppedEvent {
                        body: StoppedEventBody {
                            all_threads_stopped: Some(true),
                            reason: "breakpoint".into(),
                            text: None,
                            thread_id: Some(1),
                        },
                        event: "stopped".to_string(),
                        seq: debugger.seq(),
                        type_: "event".to_string(),
                    });

                    debugger.continue_barrier.wait();
                }
            }
            Ok(Async::Ready(()))
        })));
        self.thread.context().set_hook_mask(LINE_FLAG);

        let debugger = self.debugger.clone();
        let thread = self.thread.clone();
        let seq = self.seq.clone();
        let stream = self.stream.clone();
        spawn(move || {
            // Wait for the initialization to finish
            debugger.continue_barrier.wait();

            let _result = Compiler::new()
                .implicit_prelude(false)
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

struct SourceData {
    source: Option<Source>,
    breakpoints: HashSet<Line>,
}

struct Debugger {
    stream: Arc<TcpStream>,
    sources: Mutex<HashMap<String, SourceData>>,
    continue_barrier: Barrier,
    seq: Arc<AtomicUsize>,
    lines_start_at_1: AtomicBool,
    // When continuing the response must be sent before actually conituing execution otherwise the `stopped`
    // action could be received by visual code before the continue response which causes it to not recognize
    // the stopped action
    do_continue: AtomicBool,

    stack_trace: Mutex<Vec<StackFrame>>,
}

impl Debugger {
    fn seq(&self) -> i64 {
        self.seq.fetch_add(1, Ordering::SeqCst) as i64
    }

    fn should_break(&self, source_name: &str, line: Line) -> bool {
        let sources = self.sources.lock().expect("sources mutex is poisoned");
        sources.get(source_name)
            .map_or(false, |source| source.breakpoints.contains(&line))
    }

    fn line(&self, line: i64) -> Line {
        Line::from((line as usize)
            .saturating_sub(self.lines_start_at_1.load(Ordering::Acquire) as usize))
    }

    fn send_event<T>(&self, value: T)
        where T: serde::Serialize,
    {
        write_message(&*self.stream, &value).unwrap();
    }

    // Stores the stackstack trace so it can be returned in later requests
    fn store_stack_trace(&self, info: &DebugInfo) {
        let mut vec = self.stack_trace.lock().unwrap();
        vec.clear();
        let mut i = 0;

        let sources = self.sources.lock().unwrap();
        while let Some(stack_info) = info.stack_info(i) {
            vec.push(StackFrame {
                column: 0,
                end_column: None,
                end_line: None,
                id: 0,
                line: stack_info.line().map_or(0, |line| line.to_usize() as i64 + 1),
                module_id: None,
                name: "main".to_string(),
                source: sources.get(stack_info.source_name())
                    .and_then(|source| source.source.clone()),
            });
            i += 1;
        }
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
            sources: Mutex::new(HashMap::new()),
            continue_barrier: Barrier::new(2),
            seq: seq.clone(),
            lines_start_at_1: AtomicBool::new(true),
            do_continue: AtomicBool::new(false),
            stack_trace: Mutex::new(Vec::new()),
        });

        let mut io = IoHandler::new();
        io.add_async_method("initialize",
                            ServerCommand::new(InitializeHandler { debugger: debugger.clone() }));

        {
            let debugger = debugger.clone();
            let configuration_done =
                move |_: ConfigurationDoneArguments| -> BoxFuture<(), ServerError<()>> {
                    // Notify the launched thread that it can start executing
                    debugger.continue_barrier.wait();
                    Ok(()).into_future().boxed()
                };
            io.add_async_method("configurationDone", ServerCommand::new(configuration_done));
        }

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
                let breakpoints = args.breakpoints
                    .iter()
                    .flat_map(|bs| bs.iter().map(|breakpoint| debugger.line(breakpoint.line)))
                    .collect();

                let opt = args.source
                    .path
                    .as_ref()
                    .map(|path| filename_to_module(path.trim_right_matches(".glu")));
                if let Some(path) = opt {
                    let mut sources = debugger.sources.lock().unwrap();
                    sources.insert(path,
                                   SourceData {
                                       source: Some(args.source.clone()),
                                       breakpoints: breakpoints,
                                   });
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

        {
            let debugger = debugger.clone();
            let stack_trace = move |_: StackTraceArguments| -> BoxFuture<_, ServerError<()>> {
                let stack_frames = (*debugger.stack_trace.lock().unwrap()).clone();
                Ok(StackTraceResponseBody {
                        total_frames: Some(stack_frames.len() as i64),
                        stack_frames: stack_frames,
                    })
                    .into_future()
                    .boxed()
            };
            io.add_async_method("stackTrace", ServerCommand::new(stack_trace));
        }

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
            let cont =
                move |_: ContinueArguments| -> BoxFuture<ContinueResponseBody, ServerError<()>> {
                    debugger.do_continue.store(true, Ordering::Release);
                    Ok(ContinueResponseBody { all_threads_continued: Some(true) })
                        .into_future()
                        .boxed()
                };
            io.add_async_method("continue", ServerCommand::new(cont));
        }
        spawn(move || {
            let read = BufReader::new(&*stream);
            let post_request_action = || {
                if debugger.do_continue.load(Ordering::Acquire) {
                    debugger.do_continue.store(false, Ordering::Release);
                    debugger.continue_barrier.wait();
                }
                Ok(())
            };
            main_loop(read,
                      &*stream,
                      &io,
                      exit_token,
                      post_request_action,
                      translate_request,
                      |body| translate_response(&debugger, body))
                .unwrap()
        });
    }
}
