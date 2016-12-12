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
use gluon::base::resolve::remove_aliases_cow;
use gluon::base::types::{ArcType, Type, arg_iter};
use gluon::vm::internal::{Value as VmValue, ValuePrinter};
use gluon::vm::thread::{RootedThread, ThreadInternal, LINE_FLAG};
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
            let reason = if debugger.pause.swap(false, Ordering::Relaxed) {
                "pause"
            } else {
                let stack_info = debug_info.stack_info(0).unwrap();
                match stack_info.line() {
                    Some(line) if debugger.should_break(stack_info.source_name(), line) => {
                        "breakpoint"
                    }
                    _ => return Ok(Async::Ready(())),
                }
            };
            debugger.send_event(StoppedEvent {
                body: StoppedEventBody {
                    all_threads_stopped: Some(true),
                    reason: reason.into(),
                    text: None,
                    thread_id: Some(1),
                },
                event: "stopped".to_string(),
                seq: debugger.seq(),
                type_: "event".to_string(),
            });
            debugger.variables.lock().unwrap().clear();
            Ok(Async::NotReady)
        })));

        let debugger = self.debugger.clone();
        spawn(move || {
            use gluon::compiler_pipeline::*;
            use gluon::vm::future::FutureValue;
            // Wait for the initialization to finish
            debugger.continue_barrier.wait();

            let mut compiler = Compiler::new();
            let mut run_future = FutureValue::sync(expr.compile(&mut compiler, &debugger.thread, &module, &expr, None))
                .and_then(|compile_value| {
                    // Since we cannot yield while importing modules we don't enable pausing or
                    // breakpoints until we start executing the main module
                    debugger.thread.context().set_hook_mask(LINE_FLAG);
                    compile_value.run_expr(&mut compiler, &debugger.thread, &module, &expr, ())
                })
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

struct Variables {
    map: HashMap<i64, (VmValue, ArcType)>,
    reference: i64,
}

impl Variables {
    fn clear(&mut self) {
        self.map.clear();
        self.reference = i32::max_value() as i64;
    }

    fn insert(&mut self, value: VmValue, typ: &ArcType) -> i64 {
        match value {
            VmValue::Array(_) |
            VmValue::Data(_) |
            VmValue::Closure(_) => {
                self.reference -= 1;
                self.map.insert(self.reference, (value, typ.clone()));
                self.reference
            }
            _ => 0,
        }
    }
}

fn indexed_variables(value: VmValue) -> Option<i64> {
    match value {
        VmValue::Array(ref array) => Some(array.len() as i64),
        _ => None,
    }
}

fn named_variables(value: VmValue) -> Option<i64> {
    match value {
        VmValue::Data(ref data) => Some(data.fields.len() as i64),
        VmValue::Closure(ref closure) => Some(closure.upvars.len() as i64),
        _ => None,
    }
}

struct Debugger {
    thread: RootedThread,
    stream: Arc<TcpStream>,
    sources: Mutex<HashMap<String, SourceData>>,
    variables: Mutex<Variables>,
    continue_barrier: Barrier,
    seq: Arc<AtomicUsize>,
    lines_start_at_1: AtomicBool,
    // When continuing the response must be sent before actually conituing execution otherwise the `stopped`
    // action could be received by visual code before the continue response which causes it to not recognize
    // the stopped action
    do_continue: AtomicBool,
    pause: AtomicBool,
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

    fn variables(&self, reference: i64) -> Vec<Variable> {
        let context = self.thread.context();
        let info = context.debug_info();
        let variable_ref = translate_reference(reference);
        let stack_info = info.stack_info(variable_ref.stack_index);

        let mut variables = self.variables.lock().unwrap();
        let variable = variables.map.get(&reference).cloned();

        let mut mk_variable = |name: &str, typ: &ArcType, value| {
            Variable {
                evaluate_name: None,
                indexed_variables: indexed_variables(value),
                kind: None,
                name: String::from(name),
                named_variables: named_variables(value),
                type_: Some(typ.to_string()),
                value: format!("{}",
                               ValuePrinter::new(&*self.thread.get_env(), typ, value)
                                   .max_level(2)
                                   .width(10000000)),
                variables_reference: variables.insert(value, typ),
            }
        };
        match stack_info {
            Some(stack_info) => {
                let frames = context.stack.get_frames();
                let frame_index = frames.len()
                    .overflowing_sub(variable_ref.stack_index + 1)
                    .0;
                let frame = frames.get(frame_index).expect("frame");
                match variable_ref.typ {
                    VariableType::Local => {
                        let values = &context.stack.get_values()[frame.offset as usize..];
                        stack_info.locals()
                            .zip(values)
                            .map(|(local, value)| {
                                mk_variable(local.name.declared_name(), &local.typ, *value)
                            })
                            .collect()
                    }
                    VariableType::Upvar => {
                        stack_info.upvars()
                            .iter()
                            .zip(frame.upvars())
                            .map(|(info, value)| mk_variable(&info.name, &info.typ, *value))
                            .collect()
                    }
                }
            }
            None => {
                match variable {
                    Some((value, typ)) => {
                        let typ = remove_aliases_cow(&*self.thread.get_env(), &typ);
                        match value {
                            VmValue::Data(ref data) => {
                                match **typ {
                                    Type::Record(ref row) => {
                                        data.fields
                                            .iter()
                                            .zip(row.row_iter())
                                            .map(|(field, type_field)| {
                                                mk_variable(type_field.name.declared_name(),
                                                            &type_field.typ,
                                                            *field)
                                            })
                                            .collect()
                                    }
                                    Type::Variant(ref row) => {
                                        let type_field = row.row_iter()
                                            .nth(data.tag as usize)
                                            .expect("Variant tag is out of bounds");
                                        data.fields
                                            .into_iter()
                                            .zip(arg_iter(&type_field.typ))
                                            .map(|(field, typ)| mk_variable("", typ, *field))
                                            .collect()
                                    }
                                    _ => vec![],
                                }
                            }
                            VmValue::Closure(ref closure) => {
                                closure.upvars
                                    .iter()
                                    .zip(&closure.function.debug_info.upvars)
                                    .map(|(value, ref upvar_info)| {
                                        mk_variable(&upvar_info.name, &upvar_info.typ, *value)
                                    })
                                    .collect()
                            }
                            VmValue::Array(ref array) => {
                                let element_type = match **typ {
                                    // Unpack the array type
                                    Type::App(_, ref args) => args[0].clone(),
                                    _ => Type::hole(),
                                };
                                let mut index = String::new();
                                array.iter()
                                    .enumerate()
                                    .map(|(i, value)| {
                                        use std::fmt::Write;
                                        index.clear();
                                        write!(index, "[{}]", i).unwrap();
                                        mk_variable(&index, &element_type, value)
                                    })
                                    .collect()
                            }
                            _ => vec![],
                        }
                    }
                    None => vec![],
                }
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
            variables: Mutex::new(Variables {
                map: HashMap::new(),
                reference: i32::max_value() as i64,
            }),
            continue_barrier: Barrier::new(2),
            seq: seq.clone(),
            lines_start_at_1: AtomicBool::new(true),
            do_continue: AtomicBool::new(false),
            pause: AtomicBool::new(false),
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
            let cont = move |_: PauseArguments| -> BoxFuture<Option<Value>, ServerError<()>> {
                debugger.pause.store(true, Ordering::Release);
                Ok(None).into_future().boxed()
            };
            io.add_async_method("pause", ServerCommand::new(cont));
        }

        {
            let debugger = debugger.clone();
            let cont =
                move |args: VariablesArguments| -> BoxFuture<VariablesResponseBody, ServerError<()>> {
                    let variables = debugger.variables(args.variables_reference);
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
