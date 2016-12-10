

extern crate gluon_language_server;
extern crate debugserver_types;
extern crate languageserver_types;

extern crate jsonrpc_core;
extern crate serde_json;
extern crate serde;

#[allow(dead_code)]
mod support;

use std::net::TcpStream;
use std::path::PathBuf;
use std::process::Command;
use std::io::BufReader;

use serde_json::{Value, from_str};

use debugserver_types::*;

use gluon_language_server::rpc::read_message;

macro_rules! request {
    ($stream: expr, $id: ident, $command: expr, $seq: ident, $expr: expr) => {
        let request = $id {
            arguments: $expr,
            command: $command.to_string(),
            seq: { $seq += 1; $seq },
            type_: "request".into(),
        };
        support::write_message(&$stream, request).unwrap();
    }
}

#[test]
fn launch() {
    let path = PathBuf::from(::std::env::var("OUT_DIR").unwrap());
    let debugger = path.parent()
        .and_then(|path| path.parent())
        .and_then(|path| path.parent())
        .expect("debugger executable")
        .join("debugger");

    let mut child = Command::new(&debugger)
        .spawn()
        .unwrap_or_else(|_| panic!("Expected exe: {}", debugger.display()));

    let stream = TcpStream::connect("localhost:4711").unwrap();

    let mut seq = 0;
    let mut read = BufReader::new(&stream);

    request! {
        stream,
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

    let value = read_message(&mut read).unwrap().unwrap();
    let _: InitializedEvent = from_str(&value).unwrap();

    let value = read_message(&mut read).unwrap().unwrap();
    let initialize_response: InitializeResponse = from_str(&value).unwrap();
    assert!(initialize_response.success);

    let launch = Value::Object(vec![("arguments".to_string(),
                                     Value::Object(vec![("program".to_string(),
                                                         Value::String("tests/main.glu"
                                                             .to_string()))]
                                         .into_iter()
                                         .collect())),
                                    ("seq".to_string(),
                                     Value::I64({
                                         seq += 1;
                                         seq
                                     })),
                                    ("command".to_string(), Value::String("launch".to_string())),
                                    ("type".to_string(), Value::String("request".to_string()))]
        .into_iter()
        .collect());
    support::write_message(&stream, launch).unwrap();

    let value = read_message(&mut read).unwrap().unwrap();
    let launch_response: LaunchResponse = from_str(&value).unwrap();
    assert_eq!(launch_response.request_seq, seq);
    assert!(launch_response.success);

    request! {
        stream,
        ConfigurationDoneRequest,
        "configurationDone",
        seq,
        None
    };
    let value = read_message(&mut read).unwrap().unwrap();
    let _: ConfigurationDoneResponse = from_str(&value).unwrap();

    let value = read_message(&mut read).unwrap().unwrap();
    let _: TerminatedEvent = from_str(&value).unwrap();

    request! {
        stream,
        DisconnectRequest,
        "disconnect",
        seq,
        None
    };

    child.wait().unwrap();
}
