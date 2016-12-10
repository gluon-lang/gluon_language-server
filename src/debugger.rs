extern crate clap;
extern crate debugserver_types;
extern crate env_logger;
extern crate futures;
extern crate jsonrpc_core;
extern crate serde;
extern crate serde_json;
extern crate log;

extern crate gluon_language_server;
extern crate gluon;

use std::cell::RefCell;
use std::error::Error as StdError;
use std::fs::File;
use std::io::{BufReader, Read};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Barrier, Mutex};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::spawn;
use std::collections::{HashMap, HashSet};

use clap::{App, Arg};
use futures::{Async, BoxFuture, Future, IntoFuture};

use jsonrpc_core::IoHandler;

use serde_json::Value;

use debugserver_types::*;

use gluon::base::pos::Line;
use gluon::vm::api::{OpaqueValue, Hole};
use gluon::vm::thread::{DebugInfo, RootedThread, ThreadInternal, LINE_FLAG};
use gluon::{Compiler, Error as GluonError, filename_to_module};

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
        self.debugger.thread.context().set_hook(Some(Box::new(move |_, debug_info| {
            let stack_info = debug_info.stack_info(0).unwrap();
            if let Some(line) = stack_info.line() {
                if debugger.should_break(stack_info.source_name(), line) {

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
                    return Ok(Async::NotReady);
                }
            }
            Ok(Async::Ready(()))
        })));
        self.debugger.thread.context().set_hook_mask(LINE_FLAG);

        let debugger = self.debugger.clone();
        spawn(move || {
            use gluon::vm::future::FutureValue;
            // Wait for the initialization to finish
            debugger.continue_barrier.wait();

            let mut run_future = Compiler::new()
                .run_expr_async::<OpaqueValue<RootedThread, Hole>>(&debugger.thread,
                                                                   &module,
                                                                   &expr)
                .map(|_| ());
            let mut result = match run_future {
                FutureValue::Value(_) => Ok(Async::Ready(())),
                _ => Ok(Async::NotReady),
            };
            loop {
                match result {
                    Ok(Async::NotReady) => {
                        debugger.continue_barrier.wait();
                    }
                    Ok(Async::Ready(())) => break,
                    Err(err) => {
                        let output = OutputEvent {
                            body: OutputEventBody {
                                category: Some("telemetry".into()),
                                output: format!("{}", err),
                                data: None,
                            },
                            event: "output".into(),
                            seq: debugger.seq(),
                            type_: "event".into(),
                        };
                        write_message(&*debugger.stream, &output).unwrap();
                        break;
                    }
                }
                result = run_future.poll().map_err(GluonError::from);
            }
            let terminated = TerminatedEvent {
                body: None,
                event: "terminated".into(),
                seq: debugger.seq(),
                type_: "event".into(),
            };
            write_message(&*debugger.stream, &terminated).unwrap();
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
fn translate_request(message: String,
                     current_command: &RefCell<String>)
                     -> Result<String, Box<StdError>> {
    use serde_json::Value;
    let mut data: Value = try!(serde_json::from_str(&message));
    if data.is_object() {
        let data = data.as_object_mut().unwrap();
        let command = data.get("command").and_then(|c| c.as_str()).expect("command").to_string();
        *current_command.borrow_mut() = command.clone();
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

fn translate_response(debugger: &Debugger,
                      message: String,
                      current_command: &str)
                      -> Result<String, Box<StdError>> {
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

        let mut map = vec![("command".to_string(), Value::String(current_command.to_string())),
                           ("success".to_string(), Value::Bool(result.is_some())),
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
    thread: RootedThread,
    stream: Arc<TcpStream>,
    sources: Mutex<HashMap<String, SourceData>>,
    continue_barrier: Barrier,
    seq: Arc<AtomicUsize>,
    lines_start_at_1: AtomicBool,
    // When continuing the response must be sent before actually conituing execution otherwise the `stopped`
    // action could be received by visual code before the continue response which causes it to not recognize
    // the stopped action
    do_continue: AtomicBool,
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

    fn variables(&self, info: &DebugInfo, reference: i64) -> Vec<Variable> {
        let mk_variable = |name: &str| {
            Variable {
                evaluate_name: None,
                indexed_variables: None,
                kind: None,
                name: String::from(name),
                named_variables: None,
                type_: None,
                value: "".to_string(),
                variables_reference: 0,
            }
        };
        let variable_ref = translate_reference(reference);
        let stack_info = info.stack_info(variable_ref.stack_index);
        match variable_ref.typ {
            VariableType::Local => {
                stack_info.iter()
                    .flat_map(|stack_info| {
                        stack_info.locals().map(|local| mk_variable(local.name.declared_name()))
                    })
                    .collect()
            }
            VariableType::Upvar => {
                stack_info.iter()
                    .flat_map(|stack_info| {
                        stack_info.upvars().iter().map(|upvar| mk_variable(&upvar.name))
                    })
                    .collect()
            }
        }
    }
}

enum VariableType {
    Local,
    Upvar,
}

struct VariableReference {
    stack_index: usize,
    typ: VariableType,
}

fn translate_reference(reference: i64) -> VariableReference {
    VariableReference {
        stack_index: ((reference - 1) / 2) as usize,
        typ: if reference % 2 == 1 {
            VariableType::Local
        } else {
            VariableType::Upvar
        },
    }
}

pub fn main() {
    env_logger::init().unwrap();

    let matches = App::new("debugger")
        .version(env!("CARGO_PKG_VERSION"))
        .arg(Arg::with_name("port").value_name("PORT").takes_value(true))
        .get_matches();

    let port = matches.value_of("port").unwrap_or("4711");

    let listener = TcpListener::bind(&format!("127.0.0.1:{}", port)[..]).unwrap();
    if let Some(stream) = listener.incoming().next() {
        let stream = Arc::new(stream.unwrap());

        let exit_token = Arc::new(AtomicBool::new(false));
        let seq = Arc::new(AtomicUsize::new(1));

        let debugger = Arc::new(Debugger {
            thread: gluon::new_vm(),
            stream: stream.clone(),
            sources: Mutex::new(HashMap::new()),
            continue_barrier: Barrier::new(2),
            seq: seq.clone(),
            lines_start_at_1: AtomicBool::new(true),
            do_continue: AtomicBool::new(false),
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
                      ServerCommand::new(LaunchHandler { debugger: debugger.clone() }));
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
                let mut frames = Vec::new();
                let mut i = 0;

                let sources = debugger.sources.lock().unwrap();
                let context = debugger.thread.context();
                let info = context.debug_info();

                while let Some(stack_info) = info.stack_info(i) {
                    if info.stack_info(i + 1).is_none() {
                        // Skip the top level scope
                        break;
                    }

                    frames.push(StackFrame {
                        column: 0,
                        end_column: None,
                        end_line: None,
                        id: i as i64,
                        line: stack_info.line().map_or(0, |line| line.to_usize() as i64 + 1),
                        module_id: None,
                        name: stack_info.function_name().unwrap_or("<unknown>").to_string(),
                        source: sources.get(stack_info.source_name())
                            .and_then(|source| source.source.clone()),
                    });

                    i += 1;
                }

                Ok(StackTraceResponseBody {
                    total_frames: Some(frames.len() as i64),
                    stack_frames: frames,
                })
                    .into_future()
                    .boxed()
            };
            io.add_async_method("stackTrace", ServerCommand::new(stack_trace));
        }

        {
            let debugger = debugger.clone();
            let scopes = move |args: ScopesArguments| -> BoxFuture<_, ServerError<()>> {
                let mut scopes = Vec::new();

                let sources = debugger.sources.lock().unwrap();
                let context = debugger.thread.context();
                let info = context.debug_info();

                if let Some(stack_info) = info.stack_info(args.frame_id as usize) {
                    scopes.push(Scope {
                        column: None,
                        end_column: None,
                        end_line: None,
                        expensive: false,
                        indexed_variables: None,
                        line: stack_info.line().map(|line| line.to_usize() as i64 + 1),
                        name: "Locals".to_string(),
                        named_variables: Some(stack_info.locals().count() as i64),
                        source: sources.get(stack_info.source_name())
                            .and_then(|source| source.source.clone()),
                        variables_reference: (args.frame_id + 1) * 2 - 1,
                    });
                    scopes.push(Scope {
                        column: None,
                        end_column: None,
                        end_line: None,
                        expensive: false,
                        indexed_variables: None,
                        line: stack_info.line().map(|line| line.to_usize() as i64 + 1),
                        name: "Upvars".to_string(),
                        named_variables: Some(stack_info.locals().count() as i64),
                        source: sources.get(stack_info.source_name())
                            .and_then(|source| source.source.clone()),
                        variables_reference: (args.frame_id + 1) * 2,
                    });
                }
                Ok(ScopesResponseBody { scopes: scopes })
                    .into_future()
                    .boxed()
            };
            io.add_async_method("scopes", ServerCommand::new(scopes));
        }

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

        {
            let debugger = debugger.clone();
            let cont =
                move |args: VariablesArguments| -> BoxFuture<VariablesResponseBody, ServerError<()>> {
                    let context = debugger.thread.context();
                    let info = context.debug_info();
                    let mut variables: Vec<_> = debugger.variables(&info, args.variables_reference);
                    let frames = context.stack.get_frames();
                    let variable = translate_reference(args.variables_reference);
                    let frame_index = frames.len()
                        .overflowing_sub(variable.stack_index + 1)
                        .0;
                    match variable.typ {
                        VariableType::Local => {
                            if let Some(frame) = frames.get(frame_index) {
                                for (var, value) in variables.iter_mut()
                                    .zip(&context.stack.get_values()[frame.offset as usize..]) {
                                    var.value = format!("{:?}", value);
                                }
                            }
                        }
                        VariableType::Upvar => (),
                    }
                    Ok(VariablesResponseBody { variables: variables }).into_future().boxed()
                };
            io.add_async_method("variables", ServerCommand::new(cont));
        }

        let handle = spawn(move || {
            let read = BufReader::new(&*stream);
            let post_request_action = || {
                if debugger.do_continue.load(Ordering::Acquire) {
                    debugger.do_continue.store(false, Ordering::Release);
                    debugger.continue_barrier.wait();
                }
                Ok(())
            };

            // The response needs the command so we need extract it from the request and inject it in the response
            let command = RefCell::new(String::new());
            main_loop(read,
                      &*stream,
                      &io,
                      exit_token,
                      post_request_action,
                      |body| translate_request(body, &command),
                      |body| translate_response(&debugger, body, &command.borrow()))
                .unwrap()
        });

        handle.join().unwrap();
    }
}
