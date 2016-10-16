
extern crate gluon_language_server;
extern crate vscode_languageserver_types;

extern crate jsonrpc_core;
extern crate serde_json;
extern crate serde;

use std::env;
use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::str;

use jsonrpc_core::request::{Call, MethodCall, Notification};
use jsonrpc_core::version::Version;
use jsonrpc_core::params::Params;
use jsonrpc_core::response::{SyncOutput, SyncResponse};
use jsonrpc_core::id::Id;

use serde::Serialize;
use serde_json::ser::Serializer;
use serde_json::{Value, to_value, from_str, from_value};

use gluon_language_server::read_message;
use vscode_languageserver_types::{DidOpenTextDocumentParams, Hover, MarkedString, Position,
                                  TextDocumentIdentifier, TextDocumentItem,
                                  TextDocumentPositionParams};


fn write_message<W, V>(mut writer: W, value: V) -> io::Result<()>
    where W: Write,
          V: Serialize,
{
    let mut vec = Vec::new();
    value.serialize(&mut Serializer::new(&mut vec)).unwrap();
    write!(writer,
           "Content-Length: {}\r\n\r\n{}",
           vec.len(),
           str::from_utf8(&vec).unwrap())
}

fn method_call<T>(method: &str, id: u64, value: T) -> Call
    where T: Serialize,
{
    let value = to_value(value);
    let params = match value {
        Value::Object(map) => Params::Map(map),
        _ => panic!("Expected map"),
    };
    Call::MethodCall(MethodCall {
        jsonrpc: Version::V2,
        method: method.into(),
        id: Id::Num(id),
        params: Some(params),
    })
}

fn notification<T>(method: &str, value: T) -> Call
    where T: Serialize,
{
    let value = to_value(value);
    let params = match value {
        Value::Object(map) => Params::Map(map),
        _ => panic!("Expected map"),
    };
    Call::Notification(Notification {
        jsonrpc: Version::V2,
        method: method.into(),
        params: Some(params),
    })
}

#[test]
fn hover() {
    let args: Vec<_> = env::args().collect();
    let server_path =
        Path::new(&args[0][..]).parent().expect("folder").join("gluon_language-server");

    let mut child = Command::new(server_path)
        .arg("--quiet")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    {
        let mut stdin = child.stdin.as_mut().expect("stdin");

        let did_open = notification("textDocument/didOpen",
                                    DidOpenTextDocumentParams {
                                        text_document: TextDocumentItem {
                                            uri: "test".into(),
                                            language_id: "gluon".into(),
                                            text: "123".into(),
                                            version: 1,
                                        },
                                    });

        write_message(&mut stdin, did_open).unwrap();

        let hover = method_call("textDocument/hover",
                                2,
                                TextDocumentPositionParams {
                                    text_document: TextDocumentIdentifier { uri: "test".into() },
                                    position: Position {
                                        line: 0,
                                        character: 2,
                                    },
                                });

        write_message(&mut stdin, hover).unwrap();

        let exit = Call::Notification(Notification {
            jsonrpc: Version::V2,
            method: "exit".into(),
            params: None,
        });
        write_message(&mut stdin, exit).unwrap();
    }

    let result = child.wait_with_output().unwrap();
    assert!(result.status.success());

    let mut hover = None;
    let mut output = &result.stdout[..];
    while let Some(json) = read_message(&mut output).unwrap() {
        if let Ok(SyncResponse::Single(SyncOutput::Success(response))) = from_str(&json) {
            hover = from_value(response.result).ok();
        }
    }
    assert_eq!(hover,
               Some(Hover {
                   contents: vec![MarkedString::String("Int".into())],
                   range: None,
               }),
               "{}",
               str::from_utf8(&result.stdout).expect("UTF8"));
}
