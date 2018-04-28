extern crate debugserver_types;

use std::cell::RefCell;
use std::cmp;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::error::Error as StdError;
use std::fs::File;
use std::io::{self, BufRead, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::thread::spawn;

use codespan;

use futures::{Async, Future, IntoFuture};

use jsonrpc_core::IoHandler;

use serde;
use serde_json::{self, Value};

use self::debugserver_types::*;

use gluon::base::filename_to_module;
use gluon::base::pos::Line;
use gluon::base::resolve::remove_aliases_cow;
use gluon::base::types::{arg_iter, ArcType, Type};
use gluon::import::Import;
use gluon::vm::api::ValueRef;
use gluon::vm::internal::{Value as VmValue, ValuePrinter};
use gluon::vm::thread::{HookFlags, RootedThread, Thread as GluonThread, ThreadInternal};
use gluon::vm::Variants;
use gluon::{self, Compiler, Error as GluonError};

use rpc::{read_message, write_message, write_message_str, LanguageServerCommand, ServerCommand,
          ServerError};
use BoxFuture;

pub struct InitializeHandler {
    debugger: Arc<Debugger>,
}

impl LanguageServerCommand<InitializeRequestArguments> for InitializeHandler {
    type Output = Option<Capabilities>;
    type Error = ();
    fn execute(
        &self,
        args: InitializeRequestArguments,
    ) -> BoxFuture<Option<Capabilities>, ServerError<Self::Error>> {
        self.debugger
            .lines_start_at_1
            .store(args.lines_start_at_1.unwrap_or(true), Ordering::Release);

        self.debugger.send_event(InitializedEvent {
            body: None,
            event: "initialized".into(),
            seq: self.debugger.seq(),
            type_: "event".into(),
        });

        Box::new(
            Ok(Some(Capabilities {
                supports_configuration_done_request: Some(true),
                ..Capabilities::default()
            })).into_future(),
        )
    }
}

pub struct LaunchHandler {
    debugger: Arc<Debugger>,
}

fn strip_file_prefix(thread: &GluonThread, program: &str) -> String {
    let import = thread.get_macros().get("import").expect("Import macro");
    let import = import.downcast_ref::<Import>().expect("Importer");
    let paths = import.paths.read().unwrap();
    ::strip_file_prefix(&paths, &program.parse().unwrap()).unwrap()
}

impl LanguageServerCommand<Value> for LaunchHandler {
    type Output = Option<Value>;
    type Error = ();
    fn execute(&self, args: Value) -> BoxFuture<Option<Value>, ServerError<()>> {
        Box::new(self.execute_launch(args).into_future())
    }
}

impl LaunchHandler {
    fn execute_launch(&self, args: Value) -> Result<Option<Value>, ServerError<()>> {
        let program = args.get("program")
            .and_then(|s| s.as_str())
            .ok_or_else(|| ServerError {
                message: "No program argument found".into(),
                data: None,
            })?;
        let program = strip_file_prefix(&self.debugger.thread, program);
        let module = format!("@{}", filename_to_module(&program));
        let expr = {
            let mut file = File::open(&*program).map_err(|_| ServerError {
                message: format!("Program does not exist: `{}`", program),
                data: None,
            })?;
            let mut expr = String::new();
            file.read_to_string(&mut expr)
                .expect("Could not read from file");
            expr
        };

        let debugger = self.debugger.clone();
        self.debugger
            .thread
            .context()
            .set_hook(Some(Box::new(move |_, debug_info| {
                let pause = debugger.pause.swap(NONE, Ordering::Acquire);
                let stack_info = debug_info.stack_info(0).unwrap();
                debug!(
                    "Debugger at {}:{}:{}. Reason {}",
                    stack_info.source_name(),
                    stack_info.function_name().unwrap(),
                    stack_info
                        .line()
                        .map(|l| l.number())
                        .as_ref()
                        .map_or(&"unknown" as &::std::fmt::Display, |s| s),
                    pause
                );
                let reason = match pause {
                    PAUSE => "pause",
                    STEP_IN => "step",
                    STEP_OUT => {
                        let step_data = debugger.step_data.lock().unwrap();
                        if debug_info.stack_info_len() >= step_data.stack_frames {
                            // Continue executing if we are in a deeper function call or on the same
                            // level
                            debugger.pause.store(STEP_OUT, Ordering::Release);
                            return Ok(Async::Ready(()));
                        }
                        "step"
                    }
                    NEXT => {
                        let step_data = debugger.step_data.lock().unwrap();
                        let stack_info = debug_info.stack_info(0).unwrap();
                        let different_function = step_data.function_name
                            != stack_info.function_name().unwrap_or("<Unknown function>");
                        let cmp = debug_info.stack_info_len().cmp(&step_data.stack_frames);
                        if cmp == cmp::Ordering::Greater
                            || (cmp == cmp::Ordering::Equal && different_function)
                        {
                            // Continue executing if we are in a deeper function call or on the same
                            // level but in a different function (tail call)
                            debugger.pause.store(NEXT, Ordering::Release);
                            return Ok(Async::Ready(()));
                        }
                        "step"
                    }
                    _ => {
                        let stack_info = debug_info.stack_info(0).unwrap();
                        let line = stack_info.line();
                        match line {
                            Some(line) if debugger.should_break(stack_info.source_name(), line) => {
                                debug!("Breaking on {}", line.number());
                                "breakpoint"
                            }
                            _ => return Ok(Async::Ready(())),
                        }
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
            let mut run_future = FutureValue::sync(expr.compile(
                &mut compiler,
                &debugger.thread,
                &module,
                &expr,
                None,
            )).and_then(|compile_value| {
                // Since we cannot yield while importing modules we don't enable pausing or
                // breakpoints until we start executing the main module
                debugger
                    .thread
                    .context()
                    .set_hook_mask(HookFlags::LINE_FLAG);
                compile_value.run_expr(&mut compiler, &*debugger.thread, &module, &expr, ())
            })
                .map(|_| ());
            let mut result = match run_future {
                FutureValue::Value(Ok(_)) => Ok(Async::Ready(())),
                FutureValue::Value(Err(err)) => {
                    run_future = FutureValue::Value(Ok(()));
                    Err(err)
                }
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
        Box::new(Ok(None).into_future())
    }
}

// Translate debug server messages into jsonrpc-2.0
fn translate_request(
    message: String,
    current_command: &RefCell<String>,
) -> Result<String, Box<StdError>> {
    use languageserver_types::NumberOrString;
    use serde_json::Value;

    #[derive(Debug, Deserialize)]
    struct In {
        command: String,
        seq: NumberOrString,
        #[serde(default)]
        arguments: Value,
    }

    #[derive(Serialize)]
    struct Out<'a> {
        method: &'a str,
        id: NumberOrString,
        params: Value,
        jsonrpc: &'a str,
    }

    let data: Option<In> = try!(serde_json::from_str(&message));
    if let Some(data) = data {
        *current_command.borrow_mut() = data.command.clone();

        let out = Out {
            method: &data.command,
            id: data.seq,
            params: data.arguments,
            jsonrpc: "2.0",
        };
        return Ok(try!(serde_json::to_string(&out)));
    }
    Ok(message)
}

fn translate_response(
    debugger: &Debugger,
    message: String,
    current_command: &str,
) -> Result<String, Box<StdError>> {
    use languageserver_types::NumberOrString;
    use serde_json::Value;

    #[derive(Debug, Deserialize)]
    struct Error {
        message: Value,
    }

    fn deserialize<'de, D>(deserializer: D) -> Result<Option<Value>, D::Error>
    where
        D: serde::de::Deserializer<'de>,
    {
        serde::Deserialize::deserialize(deserializer).map(Some)
    }

    #[derive(Debug, Deserialize)]
    struct Message {
        id: NumberOrString,
        // Distinguishes result: null (`Some(Value::Null)`) from result not existing (`None`)
        #[serde(default)]
        #[serde(deserialize_with = "deserialize")]
        result: Option<Value>,
        error: Option<Error>,
    }

    #[derive(Serialize)]
    struct Out<'a> {
        command: &'a str,
        success: bool,
        request_seq: NumberOrString,
        seq: i64,
        #[serde(rename = "type")]
        typ: &'a str,
        body: Option<Value>,
        message: Option<Value>,
    }

    let data: Option<Message> = try!(serde_json::from_str(&message));
    if let Some(data) = data {
        let out = Out {
            command: &current_command,
            success: data.result.is_some(),
            request_seq: data.id,
            seq: debugger.seq(),
            typ: "response",
            body: data.result,
            message: data.error.map(|error| error.message),
        };

        return Ok(try!(serde_json::to_string(&out)));
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
        match value.get_variants().as_ref() {
            ValueRef::Array(_) | ValueRef::Data(_) | ValueRef::Closure(_) => {
                self.reference -= 1;
                self.map.insert(self.reference, (value, typ.clone()));
                self.reference
            }
            _ => 0,
        }
    }
}

fn indexed_variables(value: Variants) -> Option<i64> {
    match value.as_ref() {
        ValueRef::Array(ref array) => Some(array.len() as i64),
        _ => None,
    }
}

fn named_variables(value: Variants) -> Option<i64> {
    match value.as_ref() {
        ValueRef::Data(ref data) => Some(data.len() as i64),
        ValueRef::Closure(ref closure) => Some(closure.upvars().count() as i64),
        _ => None,
    }
}

const NONE: usize = 0;
const PAUSE: usize = 1;
const STEP_IN: usize = 2;
const STEP_OUT: usize = 3;
const NEXT: usize = 4;

#[derive(Default)]
struct StepData {
    stack_frames: usize,
    function_name: String,
}

pub trait SharedWrite: Send + Sync {
    fn with_write(&self, f: &mut FnMut(&mut Write) -> io::Result<usize>) -> io::Result<usize>;
}

impl<T> SharedWrite for T
where
    for<'a> &'a T: Write,
    T: Send + Sync,
{
    fn with_write(&self, f: &mut FnMut(&mut Write) -> io::Result<usize>) -> io::Result<usize> {
        let mut this = self;
        f(&mut this)
    }
}

pub struct Stdout(pub io::Stdout);
impl SharedWrite for Stdout {
    fn with_write(&self, f: &mut FnMut(&mut Write) -> io::Result<usize>) -> io::Result<usize> {
        f(&mut self.0.lock())
    }
}

impl<'s> Write for SharedWrite + 's {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.with_write(&mut |write| write.write(buf))
    }
    fn flush(&mut self) -> io::Result<()> {
        self.with_write(&mut |write| write.flush().map(|_| 0))
            .map(|_| ())
    }
}

impl<'a, 'b> Write for &'a (SharedWrite + 'b) {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.with_write(&mut |write| write.write(buf))
    }
    fn flush(&mut self) -> io::Result<()> {
        self.with_write(&mut |write| write.flush().map(|_| 0))
            .map(|_| ())
    }
}

struct Debugger {
    thread: RootedThread,
    stream: Arc<SharedWrite>,
    sources: Mutex<HashMap<String, SourceData>>,
    variables: Mutex<Variables>,
    continue_barrier: Barrier,
    seq: Arc<AtomicUsize>,
    lines_start_at_1: AtomicBool,
    // When continuing the response must be sent before actually conituing execution otherwise the
    // `stopped` action could be received by visual code before the continue response which causes
    // it to not recognize the stopped action
    do_continue: AtomicBool,
    pause: AtomicUsize,
    // Information used to determine when the step has finished
    step_data: Mutex<StepData>,
}

impl Debugger {
    fn seq(&self) -> i64 {
        self.seq.fetch_add(1, Ordering::SeqCst) as i64
    }

    fn should_break(&self, source_name: &str, line: Line) -> bool {
        let sources = self.sources.lock().expect("sources mutex is poisoned");
        sources
            .get(source_name)
            .map_or(false, |source| source.breakpoints.contains(&line))
    }

    fn line(&self, line: i64) -> Line {
        Line::from((line as usize)
            .saturating_sub(self.lines_start_at_1.load(Ordering::Acquire) as usize)
            as codespan::RawIndex)
    }

    fn send_event<T>(&self, value: T)
    where
        T: serde::Serialize,
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

        let mut mk_variable = |name: &str, typ: &ArcType, value: Variants| Variable {
            evaluate_name: None,
            indexed_variables: indexed_variables(value),
            kind: None,
            name: String::from(name),
            named_variables: named_variables(value),
            type_: Some(typ.to_string()),
            value: format!(
                "{}",
                ValuePrinter::new(&*self.thread.get_env(), typ, value)
                    .max_level(2)
                    .width(10000000)
            ),
            variables_reference: variables.insert(value.get_value(), typ),
        };
        match stack_info {
            Some(stack_info) => {
                let frames = context.stack.get_frames();
                let frame_index = frames.len().overflowing_sub(variable_ref.stack_index + 1).0;
                let frame = frames.get(frame_index).expect("frame");
                match variable_ref.typ {
                    VariableType::Local => {
                        let values = &context.stack.get_values()[frame.offset as usize..];
                        stack_info
                            .locals()
                            .zip(values)
                            .map(|(local, value)| {
                                mk_variable(
                                    local.name.declared_name(),
                                    &local.typ,
                                    value.get_variants(),
                                )
                            })
                            .collect()
                    }
                    VariableType::Upvar => stack_info
                        .upvars()
                        .iter()
                        .zip(frame.upvars())
                        .map(|(info, value)| {
                            mk_variable(&info.name, &info.typ, value.get_variants())
                        })
                        .collect(),
                }
            }
            None => {
                match variable {
                    Some((value, typ)) => {
                        let typ = remove_aliases_cow(&*self.thread.get_env(), &typ);
                        match value.get_variants().as_ref() {
                            ValueRef::Data(ref data) => match **typ {
                                Type::Record(ref row) => data.iter()
                                    .zip(row.row_iter())
                                    .map(|(field, type_field)| {
                                        mk_variable(
                                            type_field.name.declared_name(),
                                            &type_field.typ,
                                            field,
                                        )
                                    })
                                    .collect(),
                                Type::Variant(ref row) => {
                                    let type_field = row.row_iter()
                                        .nth(data.tag() as usize)
                                        .expect("Variant tag is out of bounds");
                                    data.iter()
                                        .zip(arg_iter(&type_field.typ))
                                        .map(|(field, typ)| mk_variable("", typ, field))
                                        .collect()
                                }
                                _ => vec![],
                            },
                            ValueRef::Closure(ref closure) => closure
                                .upvars()
                                .zip(&closure.debug_info().upvars)
                                .map(|(value, ref upvar_info)| {
                                    mk_variable(&upvar_info.name, &upvar_info.typ, value)
                                })
                                .collect(),
                            ValueRef::Array(ref array) => {
                                let element_type = match **typ {
                                    // Unpack the array type
                                    Type::App(_, ref args) => args[0].clone(),
                                    _ => Type::hole(),
                                };
                                let mut index = String::new();
                                array
                                    .iter()
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

pub fn spawn_server<R>(mut input: R, output: Arc<SharedWrite>)
where
    R: BufRead,
{
    let exit_token = Arc::new(AtomicBool::new(false));
    let seq = Arc::new(AtomicUsize::new(1));

    let debugger = Arc::new(Debugger {
        thread: gluon::new_vm(),
        stream: output.clone(),
        sources: Mutex::new(HashMap::new()),
        variables: Mutex::new(Variables {
            map: HashMap::new(),
            reference: i32::max_value() as i64,
        }),
        continue_barrier: Barrier::new(2),
        seq: seq.clone(),
        lines_start_at_1: AtomicBool::new(true),
        do_continue: AtomicBool::new(false),
        pause: AtomicUsize::new(NONE),
        step_data: Mutex::new(StepData::default()),
    });

    let mut io = IoHandler::new();
    io.add_method(
        "initialize",
        ServerCommand::method(InitializeHandler {
            debugger: debugger.clone(),
        }),
    );

    {
        let debugger = debugger.clone();
        let handler = move |_: ConfigurationDoneArguments| -> BoxFuture<(), ServerError<()>> {
            // Notify the launched thread that it can start executing
            debugger.continue_barrier.wait();
            Box::new(Ok(()).into_future())
        };
        io.add_method("configurationDone", ServerCommand::method(handler));
    }

    io.add_method(
        "launch",
        ServerCommand::method(LaunchHandler {
            debugger: debugger.clone(),
        }),
    );
    io.add_method(
        "disconnect",
        ServerCommand::method(DisconnectHandler {
            exit_token: exit_token.clone(),
        }),
    );
    {
        let debugger = debugger.clone();
        let set_break = move |args: SetBreakpointsArguments| -> BoxFuture<_, ServerError<()>> {
            let breakpoints = args.breakpoints
                .iter()
                .flat_map(|bs| bs.iter().map(|breakpoint| debugger.line(breakpoint.line)))
                .collect();

            let opt = args.source.path.as_ref().map(|path| {
                format!(
                    "@{}",
                    filename_to_module(&strip_file_prefix(&debugger.thread, path))
                )
            });
            if let Some(path) = opt {
                let mut sources = debugger.sources.lock().unwrap();
                sources.insert(
                    path,
                    SourceData {
                        source: Some(args.source.clone()),
                        breakpoints: breakpoints,
                    },
                );
            }

            Box::new(
                Ok(SetBreakpointsResponseBody {
                    breakpoints: args.breakpoints
                        .into_iter()
                        .flat_map(|bs| bs)
                        .map(|breakpoint| Breakpoint {
                            column: None,
                            end_column: None,
                            end_line: None,
                            id: None,
                            line: Some(breakpoint.line),
                            message: None,
                            source: None,
                            verified: true,
                        })
                        .collect(),
                }).into_future(),
            )
        };

        io.add_method("setBreakpoints", ServerCommand::method(set_break));
    }

    let threads = move |_: Value| -> BoxFuture<ThreadsResponseBody, ServerError<()>> {
        Box::new(
            Ok(ThreadsResponseBody {
                threads: vec![
                    Thread {
                        id: 1,
                        name: "main".to_string(),
                    },
                ],
            }).into_future(),
        )
    };

    io.add_method("threads", ServerCommand::method(threads));

    {
        let debugger = debugger.clone();
        let stack_trace = move |_: StackTraceArguments| -> BoxFuture<_, ServerError<()>> {
            let mut frames = Vec::new();
            let mut i = 0;

            let mut sources = debugger.sources.lock().unwrap();
            let context = debugger.thread.context();
            let info = context.debug_info();

            while let Some(stack_info) = info.stack_info(i) {
                if info.stack_info(i + 1).is_none() {
                    // Skip the top level scope
                    break;
                }

                let source = match sources.entry(stack_info.source_name().to_string()) {
                    Entry::Occupied(entry) => entry.get().source.clone(),
                    Entry::Vacant(entry) => {
                        // If the source is not in the map, create it from the debug info
                        let name = entry.key().to_string();
                        let path = format!("{}.glu", name.replace(".", "/"));
                        entry
                            .insert(SourceData {
                                source: Some(Source {
                                    path: Some(path),
                                    name: Some(name),
                                    ..Source::default()
                                }),
                                breakpoints: HashSet::new(),
                            })
                            .source
                            .clone()
                    }
                };
                frames.push(StackFrame {
                    column: 0,
                    end_column: None,
                    end_line: None,
                    id: i as i64,
                    line: stack_info
                        .line()
                        .map_or(0, |line| line.to_usize() as i64 + 1),
                    module_id: None,
                    name: stack_info
                        .function_name()
                        .unwrap_or("<unknown>")
                        .to_string(),
                    source: source,
                });

                i += 1;
            }

            Box::new(
                Ok(StackTraceResponseBody {
                    total_frames: Some(frames.len() as i64),
                    stack_frames: frames,
                }).into_future(),
            )
        };
        io.add_method("stackTrace", ServerCommand::method(stack_trace));
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
                    source: sources
                        .get(stack_info.source_name())
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
                    source: sources
                        .get(stack_info.source_name())
                        .and_then(|source| source.source.clone()),
                    variables_reference: (args.frame_id + 1) * 2,
                });
            }
            Box::new(Ok(ScopesResponseBody { scopes: scopes }).into_future())
        };
        io.add_method("scopes", ServerCommand::method(scopes));
    }

    {
        let debugger = debugger.clone();
        let cont = move |_: ContinueArguments| -> BoxFuture<ContinueResponseBody, ServerError<()>> {
            debugger.do_continue.store(true, Ordering::Release);
            Box::new(
                Ok(ContinueResponseBody {
                    all_threads_continued: Some(true),
                }).into_future(),
            )
        };
        io.add_method("continue", ServerCommand::method(cont));
    }

    {
        let debugger = debugger.clone();
        let cont = move |_: NextArguments| -> BoxFuture<Option<Value>, ServerError<()>> {
            debugger.do_continue.store(true, Ordering::Release);
            let context = debugger.thread.context();
            let debug_info = context.debug_info();
            debugger.pause.store(NEXT, Ordering::Release);
            *debugger.step_data.lock().unwrap() = StepData {
                stack_frames: debug_info.stack_info_len(),
                function_name: debug_info
                    .stack_info(0)
                    .unwrap()
                    .function_name()
                    .unwrap()
                    .to_string(),
            };
            Box::new(Ok(None).into_future())
        };
        io.add_method("next", ServerCommand::method(cont));
    }

    {
        let debugger = debugger.clone();
        let cont = move |_: StepInArguments| -> BoxFuture<Option<Value>, ServerError<()>> {
            debugger.do_continue.store(true, Ordering::Release);
            debugger.pause.store(STEP_IN, Ordering::Release);
            Box::new(Ok(None).into_future())
        };
        io.add_method("stepIn", ServerCommand::method(cont));
    }

    {
        let debugger = debugger.clone();
        let cont = move |_: StepOutArguments| -> BoxFuture<Option<Value>, ServerError<()>> {
            debugger.do_continue.store(true, Ordering::Release);
            let context = debugger.thread.context();
            let debug_info = context.debug_info();
            debugger.pause.store(STEP_OUT, Ordering::Release);
            *debugger.step_data.lock().unwrap() = StepData {
                stack_frames: debug_info.stack_info_len(),
                function_name: debug_info
                    .stack_info(0)
                    .expect("Stopped on a location without a frame")
                    .function_name()
                    .unwrap_or("<Unknown function>")
                    .to_string(),
            };
            Box::new(Ok(None).into_future())
        };
        io.add_method("stepOut", ServerCommand::method(cont));
    }

    {
        let debugger = debugger.clone();
        let cont = move |_: PauseArguments| -> BoxFuture<Option<Value>, ServerError<()>> {
            debugger.pause.store(PAUSE, Ordering::Release);
            Box::new(Ok(None).into_future())
        };
        io.add_method("pause", ServerCommand::method(cont));
    }

    {
        let debugger = debugger.clone();
        let cont = move |args: VariablesArguments| -> BoxFuture<_, ServerError<()>> {
            let variables = debugger.variables(args.variables_reference);
            Box::new(
                Ok(VariablesResponseBody {
                    variables: variables,
                }).into_future(),
            )
        };
        io.add_method("variables", ServerCommand::method(cont));
    }

    // The response needs the command so we need extract it from the request and inject it
    // in the response
    let command = RefCell::new(String::new());
    (move || -> Result<(), Box<StdError>> {
        while !exit_token.load(Ordering::SeqCst) {
            match try!(read_message(&mut input)) {
                Some(json) => {
                    let json = try!(translate_request(json, &command));
                    debug!("Handle: {}", json);
                    if let Some(response) = io.handle_request_sync(&json) {
                        let response =
                            try!(translate_response(&debugger, response, &command.borrow()));
                        try!(write_message_str(&*output, &response));
                        try!((&mut &*output).flush());
                    }
                    if debugger.do_continue.load(Ordering::Acquire) {
                        debugger.do_continue.store(false, Ordering::Release);
                        debugger.continue_barrier.wait();
                    }
                }
                None => return Ok(()),
            }
        }
        Ok(())
    })()
        .unwrap();
}
