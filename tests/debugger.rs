extern crate gluon_language_server;
extern crate debugserver_types;
extern crate languageserver_types;

extern crate jsonrpc_core;
extern crate serde_json;
extern crate serde;
extern crate url;

#[macro_use]
extern crate lazy_static;

#[allow(dead_code)]
mod support;

use std::net::TcpStream;
use std::path::PathBuf;
use std::process::Command;
use std::io::{BufRead, BufReader, Write};
use std::sync::Mutex;

use serde_json::{Value, from_str};

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

lazy_static! {
    static ref PORT: Mutex<i32> = Mutex::new(4711);
}

fn run_debugger<F>(f: F)
    where F: FnOnce(&mut i64, &TcpStream, &mut BufReader<&TcpStream>),
{
    let port = {
        let mut port = PORT.lock().unwrap();
        *port += 1;
        *port
    };
    let path = PathBuf::from(::std::env::args().next().unwrap());
    let debugger = path.parent()
        .and_then(|path| path.parent())
        .expect("debugger executable")
        .join("debugger");

    let mut child = Command::new(&debugger)
        .arg(port.to_string())
        .spawn()
        .unwrap_or_else(|_| panic!("Expected exe: {}", debugger.display()));

    let stream = TcpStream::connect(&format!("localhost:{}", port)[..]).unwrap();

    let mut seq = 0;
    let mut read = BufReader::new(&stream);

    request! {
        &stream,
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

    let _: InitializedEvent = expect_message(&mut read);

    let initialize_response: InitializeResponse = expect_message(&mut read);
    assert!(initialize_response.success);

    f(&mut seq, &stream, &mut read);

    request! {
        &stream,
        DisconnectRequest,
        "disconnect",
        seq,
        None
    };

    child.wait().unwrap();
}

fn launch<W>(stream: W, seq: &mut i64, program: &str)
    where W: Write,
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

fn expect_message<M, R>(read: R) -> M
    where M: serde::Deserialize,
          R: BufRead,
{
    let value = read_message(read).unwrap().unwrap();
    from_str(&value).unwrap()
}

#[test]
fn launch_program() {
    run_debugger(|seq, stream, mut read| {
        launch(stream, seq, "tests/main.glu");

        let launch_response: LaunchResponse = expect_message(&mut read);
        assert_eq!(launch_response.request_seq, *seq);
        assert!(launch_response.success);

        request! {
            stream,
            ConfigurationDoneRequest,
            "configurationDone",
            *seq,
            None
        };
        let _: ConfigurationDoneResponse = expect_message(&mut read);

        let _: TerminatedEvent = expect_message(&mut read);
    });
}

#[test]
fn infinite_loops_are_terminated() {
    run_debugger(|seq, stream, mut read| {
        launch(stream, seq, "tests/infinite_loop.glu");

        let launch_response: LaunchResponse = expect_message(&mut read);
        assert_eq!(launch_response.request_seq, *seq);
        assert!(launch_response.success);

        request! {
            stream,
            ConfigurationDoneRequest,
            "configurationDone",
            *seq,
            None
        };
        let _: ConfigurationDoneResponse = expect_message(&mut read);
    });
}
