extern crate gluon_language_server;
extern crate debugserver_types;
extern crate languageserver_types;

extern crate jsonrpc_core;
extern crate serde_json;
extern crate serde;
extern crate url;

#[allow(dead_code)]
mod support;

use std::path::PathBuf;
use std::process::{ChildStdin, ChildStdout, Command, Stdio};
use std::io::{BufRead, BufReader, Write};
use std::fs::canonicalize;

use serde_json::{from_str, Value};

use debugserver_types::*;

use gluon_language_server::rpc::read_message;

macro_rules! request {
    ($stream: expr, $id: ident, $command: expr, $seq: expr, $expr: expr) => {
        let request = $id {
            arguments: $expr,
            command: $command.to_string(),
            seq: { $seq += 1; $seq },
            type_: "request".into(),
        };
        support::write_message($stream, request).unwrap();
    }
}

macro_rules! expect_response {
    ($read: expr, $typ: ty, $name: expr) => { {
        let msg: $typ = expect_message(&mut $read, $name);
        assert_eq!(msg.command, $name);
        msg
    } }
}

macro_rules! expect_event {
    ($read: expr, $typ: ty, $event: expr) => { {
        let event: $typ = expect_message(&mut $read, $event);
        assert_eq!(event.event, $event);
        event
    } }
}

fn run_debugger<F>(f: F)
where
    F: FnOnce(&mut i64, &mut ChildStdin, &mut BufReader<&mut ChildStdout>),
{
    let path = PathBuf::from(::std::env::args().next().unwrap());
    let debugger = path.parent()
        .and_then(|path| path.parent())
        .expect("debugger executable")
        .join("gluon_debugger");

    let mut child = Command::new(&debugger)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap_or_else(|_| panic!("Expected exe: {}", debugger.display()));

    {
        let mut stream = child.stdin.as_mut().unwrap();

        let mut seq = 0;
        let mut read = BufReader::new(child.stdout.as_mut().unwrap());

        request! {
            &mut stream,
            InitializeRequest,
            "initialize",
            seq,
            InitializeRequestArguments {
                adapter_id: "".into(),
                columns_start_at_1: None,
                lines_start_at_1: None,
                path_format: None,
                supports_run_in_terminal_request: None,
                supports_variable_paging: None,
                supports_variable_type: None,
            }
        };

        expect_event!(&mut read, InitializedEvent, "initialized");

        let initialize_response = expect_response!(&mut read, InitializeResponse, "initialize");
        assert!(initialize_response.success);

        f(&mut seq, &mut stream, &mut read);

        request! {
            &mut stream,
            DisconnectRequest,
            "disconnect",
            seq,
            None
        };
        expect_response!(read, DisconnectResponse, "disconnect");
    }

    child.wait().unwrap();
}

fn launch_relative<W>(stream: &mut W, seq: &mut i64, program: &str)
where
    W: Write,
{
    let path = url::Url::from_file_path(canonicalize(program).unwrap()).unwrap();
    launch(stream, seq, &path);
}

fn launch<W>(stream: &mut W, seq: &mut i64, program: &url::Url)
where
    W: Write,
{
    request! {
        stream,
        Request,
        "launch",
        *seq,
        Some(Value::Object(vec![("program".to_string(),
                                    Value::String(program.to_string()))]
                                .into_iter()
                                .collect()))
    };
}

fn request_debug_info<R, W>(
    seq: &mut i64,
    stream: &mut W,
    mut read: &mut R,
) -> (StackTraceResponse, ScopesResponse, VariablesResponse)
where
    R: BufRead,
    W: Write,
{
    request! {
        stream,
        ThreadsRequest,
        "threads",
        *seq,
        None
    };
    expect_response!(read, ThreadsResponse, "threads");

    request! {
        stream,
        StackTraceRequest,
        "stackTrace",
        *seq,
        StackTraceArguments {
            levels: Some(20),
            start_frame: None,
            thread_id: 1,
        }
    };
    let trace = expect_response!(read, StackTraceResponse, "stackTrace");

    request! {
        stream,
        ScopesRequest,
        "scopes",
        *seq,
        ScopesArguments { frame_id: 0 }
    };
    let scopes = expect_response!(read, ScopesResponse, "scopes");

    request! {
        stream,
        VariablesRequest,
        "variables",
        *seq,
        VariablesArguments {
            count: None,
            filter: None,
            start: None,
            variables_reference: 1
        }
    };
    let variables = expect_response!(read, VariablesResponse, "variables");
    (trace, scopes, variables)
}

fn expect_message<M, R>(read: R, expected: &str) -> M
where
    M: serde::de::DeserializeOwned,
    R: BufRead,
{
    let value = read_message(read)
        .unwrap()
        .unwrap_or_else(|| panic!("Rpc message not found: `{}`", expected));
    from_str(&value).unwrap_or_else(|err| {
        panic!("{} in message:\n{}", err, value);
    })
}

#[test]
fn launch_program() {
    run_debugger(|seq, stream, mut read| {
        launch_relative(stream, seq, "tests/main.glu");

        let launch_response = expect_response!(&mut read, LaunchResponse, "launch");
        assert_eq!(launch_response.request_seq, *seq);
        assert!(launch_response.success);

        request! {
            stream,
            ConfigurationDoneRequest,
            "configurationDone",
            *seq,
            None
        };
        expect_response!(&mut read, ConfigurationDoneResponse, "configurationDone");

        expect_event!(&mut read, TerminatedEvent, "terminated");
    });
}

#[test]
fn infinite_loops_are_terminated() {
    run_debugger(|seq, stream, mut read| {
        launch_relative(stream, seq, "tests/infinite_loop.glu");

        let launch_response = expect_response!(&mut read, LaunchResponse, "launch");
        assert_eq!(launch_response.request_seq, *seq);
        assert!(launch_response.success);

        request! {
            stream,
            ConfigurationDoneRequest,
            "configurationDone",
            *seq,
            None
        };

        expect_response!(&mut read, ConfigurationDoneResponse, "configurationDone");
    });
}

#[test]
fn pause() {
    run_debugger(|seq, stream, mut read| {
        launch_relative(stream, seq, "tests/infinite_loop.glu");

        let launch_response = expect_response!(&mut read, LaunchResponse, "launch");
        assert_eq!(launch_response.request_seq, *seq);
        assert!(launch_response.success);

        request! {
            stream,
            ConfigurationDoneRequest,
            "configurationDone",
            *seq,
            None
        };
        expect_response!(&mut read, ConfigurationDoneResponse, "configurationDone");

        request! {
            stream,
            PauseRequest,
            "pause",
            *seq,
            PauseArguments { thread_id: 0, }
        };
        expect_response!(&mut read, PauseResponse, "pause");

        expect_event!(&mut read, StoppedEvent, "stopped");

        request! {
            stream,
            ContinueRequest,
            "continue",
            *seq,
            ContinueArguments { thread_id: 0, }
        };
        expect_response!(&mut read, ContinueResponse, "continue");
    });
}

#[test]
fn breakpoints() {
    run_debugger(|seq, stream, mut read| {
        // Visual code actual sends the path to the file as an url encoded absolute path so mimick
        // that behaviour
        let main_path = url::Url::from_file_path(canonicalize("tests/main.glu").unwrap()).unwrap();
        launch(stream, seq, &main_path);

        let launch_response = expect_response!(read, LaunchResponse, "launch");
        assert_eq!(launch_response.request_seq, *seq);
        assert!(launch_response.success, "{:?}", launch_response);

        request! {
            stream,
            SetBreakpointsRequest,
            "setBreakpoints",
            *seq,
            SetBreakpointsArguments {
                breakpoints: Some(vec![
                    SourceBreakpoint {
                        column: None,
                        condition: None,
                        hit_condition: None,
                        line: 1,
                    },
                    SourceBreakpoint {
                        column: None,
                        condition: None,
                        hit_condition: None,
                        line: 14,
                    },
                ]),
                lines: None,
                source: Source {
                    path: Some(main_path.to_string()),
                    .. Source::default()
                },
                source_modified: None,
            }
        };
        expect_response!(read, SetBreakpointsResponse, "setBreakpoints");

        request! {
            stream,
            ConfigurationDoneRequest,
            "configurationDone",
            *seq,
            None
        };
        expect_response!(read, ConfigurationDoneResponse, "configurationDone");

        let stopped = expect_event!(read, StoppedEvent, "stopped");
        assert_eq!(stopped.body.reason, "breakpoint");

        request! {
            stream,
            ContinueRequest,
            "continue",
            *seq,
            ContinueArguments { thread_id: 0, }
        };
        expect_response!(read, ContinueResponse, "continue");

        let stopped = expect_event!(read, StoppedEvent, "stopped");
        assert_eq!(stopped.body.reason, "breakpoint");

        request! {
            stream,
            ContinueRequest,
            "continue",
            *seq,
            ContinueArguments { thread_id: 0, }
        };
        expect_response!(read, ContinueResponse, "continue");

        expect_event!(read, TerminatedEvent, "terminated");
    });
}

#[test]
fn step_in() {
    run_debugger(|seq, stream, mut read| {
        let main_path = url::Url::from_file_path(canonicalize("tests/main.glu").unwrap()).unwrap();
        launch(stream, seq, &main_path);

        let launch_response = expect_response!(read, LaunchResponse, "launch");
        assert_eq!(launch_response.request_seq, *seq);
        assert!(launch_response.success);

        request! {
            stream,
            SetBreakpointsRequest,
            "setBreakpoints",
            *seq,
            SetBreakpointsArguments {
                breakpoints: Some(vec![
                    SourceBreakpoint {
                        column: None,
                        condition: None,
                        hit_condition: None,
                        line: 14,
                    },
                ]),
                lines: None,
                source: Source {
                    path: Some(main_path.to_string()),
                    .. Source::default()
                },
                source_modified: None,
            }
        };
        expect_response!(read, SetBreakpointsResponse, "setBreakpoints");

        request! {
            stream,
            ConfigurationDoneRequest,
            "configurationDone",
            *seq,
            None
        };
        expect_response!(read, ConfigurationDoneResponse, "configurationDone");

        let stopped = expect_event!(read, StoppedEvent, "stopped");
        assert_eq!(stopped.body.reason, "breakpoint");

        request! {
            stream,
            StepInRequest,
            "stepIn",
            *seq,
            StepInArguments {
                target_id: None,
                thread_id: 0
            }
        };
        expect_response!(read, StepInResponse, "stepIn");
        let stopped = expect_event!(read, StoppedEvent, "stopped");
        assert_eq!(stopped.body.reason, "step");

        let (trace, _, _) = request_debug_info(seq, stream, read);
        let frames = &trace.body.stack_frames;
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].line, 6);
        assert_eq!(frames[0].name, "test");
        assert_eq!(frames[1].line, 14);

        request! {
            stream,
            ContinueRequest,
            "continue",
            *seq,
            ContinueArguments { thread_id: 0, }
        };
        expect_response!(read, ContinueResponse, "continue");

        expect_event!(read, TerminatedEvent, "terminated");
    });
}

#[test]
fn step_out() {
    run_debugger(|seq, stream, mut read| {
        let main_path = url::Url::from_file_path(canonicalize("tests/main.glu").unwrap()).unwrap();
        launch(stream, seq, &main_path);

        let launch_response = expect_response!(read, LaunchResponse, "launch");
        assert_eq!(launch_response.request_seq, *seq);
        assert!(launch_response.success);

        request! {
            stream,
            SetBreakpointsRequest,
            "setBreakpoints",
            *seq,
            SetBreakpointsArguments {
                breakpoints: Some(vec![
                    SourceBreakpoint {
                        column: None,
                        condition: None,
                        hit_condition: None,
                        line: 6,
                    },
                ]),
                lines: None,
                source: Source {
                    path: Some(main_path.to_string()),
                    .. Source::default()
                },
                source_modified: None,
            }
        };
        expect_response!(read, SetBreakpointsResponse, "setBreakpoints");

        request! {
            stream,
            ConfigurationDoneRequest,
            "configurationDone",
            *seq,
            None
        };
        expect_response!(read, ConfigurationDoneResponse, "configurationDone");

        let stopped = expect_event!(read, StoppedEvent, "stopped");
        assert_eq!(stopped.body.reason, "breakpoint");

        request! {
            stream,
            StepOutRequest,
            "stepOut",
            *seq,
            StepOutArguments {
                thread_id: 0
            }
        };
        expect_response!(read, StepOutResponse, "stepOut");
        let stopped = expect_event!(read, StoppedEvent, "stopped");
        assert_eq!(stopped.body.reason, "step");

        let (trace, _, _) = request_debug_info(seq, stream, read);
        let frames = &trace.body.stack_frames;
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].line, 15);

        request! {
            stream,
            ContinueRequest,
            "continue",
            *seq,
            ContinueArguments { thread_id: 0, }
        };
        expect_response!(read, ContinueResponse, "continue");

        expect_event!(read, TerminatedEvent, "terminated");
    });
}

#[test]
fn step_over() {
    run_debugger(|seq, stream, mut read| {
        let main_path = url::Url::from_file_path(canonicalize("tests/main.glu").unwrap()).unwrap();
        launch(stream, seq, &main_path);

        let launch_response = expect_response!(read, LaunchResponse, "launch");
        assert_eq!(launch_response.request_seq, *seq);
        assert!(launch_response.success);

        request! {
            stream,
            SetBreakpointsRequest,
            "setBreakpoints",
            *seq,
            SetBreakpointsArguments {
                breakpoints: Some(vec![
                    SourceBreakpoint {
                        column: None,
                        condition: None,
                        hit_condition: None,
                        line: 14,
                    },
                ]),
                lines: None,
                source: Source {
                    path: Some(main_path.to_string()),
                    .. Source::default()
                },
                source_modified: None,
            }
        };
        expect_response!(read, SetBreakpointsResponse, "setBreakpoints");

        request! {
            stream,
            ConfigurationDoneRequest,
            "configurationDone",
            *seq,
            None
        };
        expect_response!(read, ConfigurationDoneResponse, "configurationDone");

        let stopped = expect_event!(read, StoppedEvent, "stopped");
        assert_eq!(stopped.body.reason, "breakpoint");

        request! {
            stream,
            NextRequest,
            "next",
            *seq,
            NextArguments {
                thread_id: 0
            }
        };
        expect_response!(read, StepOutResponse, "next");
        let stopped = expect_event!(read, StoppedEvent, "stopped");
        assert_eq!(stopped.body.reason, "step");

        let (trace, _, _) = request_debug_info(seq, stream, read);
        let frames = &trace.body.stack_frames;
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].line, 15);

        request! {
            stream,
            ContinueRequest,
            "continue",
            *seq,
            ContinueArguments { thread_id: 0, }
        };
        expect_response!(read, ContinueResponse, "continue");

        expect_event!(read, TerminatedEvent, "terminated");
    });
}
