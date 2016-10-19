
extern crate gluon_language_server;
extern crate vscode_languageserver_types;

extern crate jsonrpc_core;
extern crate serde_json;
extern crate serde;

mod support;

use std::io::Write;

use vscode_languageserver_types::{Hover, MarkedString, Position, TextDocumentPositionParams,
                                  TextDocumentIdentifier};

fn hover<W: ?Sized>(stdin: &mut W, id: u64, uri: &str, position: Position)
    where W: Write,
{
    let hover = support::method_call("textDocument/hover",
                                     id,
                                     TextDocumentPositionParams {
                                         text_document: TextDocumentIdentifier { uri: uri.into() },
                                         position: position,
                                     });

    support::write_message(stdin, hover).unwrap();
}

const STREAM_SOURCE: &'static str = r#"
let prelude = import "std/prelude.glu"
and { Option, Num } = prelude
and { (+) } = prelude.num_Int

type Stream_ a =
    | Value a (Stream a)
    | Empty
and Stream a = Lazy (Stream_ a)

let from f : (Int -> Option a) -> Stream a =
        let from_ i =
                lazy (\_ ->
                    match f i with
                        | Some x -> Value x (from_ (i + 1))
                        | None -> Empty
                )
        in from_ 0

{ from }
"#;

#[test]
fn simple_hover() {
    let hover: Hover = support::send_rpc(|mut stdin| {

        support::did_open(stdin, "test", "123");

        hover(stdin,
              2,
              "test",
              Position {
                  line: 0,
                  character: 2,
              });
    });

    assert_eq!(hover,
               Hover {
                   contents: vec![MarkedString::String("Int".into())],
                   range: None,
               });
}

#[test]
fn identifier() {
    let hover: Hover = support::send_rpc(|mut stdin| {
        let src = r#"
let test = 1
test 
"#;
        support::did_open(stdin, "test", src);

        hover(stdin,
              2,
              "test",
              Position {
                  line: 2,
                  character: 2,
              });
    });

    assert_eq!(hover,
               Hover {
                   contents: vec![MarkedString::String("Int".into())],
                   range: None,
               });
}

#[test]
fn stream() {
    let hover: Hover = support::send_rpc(|mut stdin| {

        support::did_open(stdin, "stream", STREAM_SOURCE);

        hover(stdin,
              2,
              "stream",
              Position {
                  line: 13,
                  character: 29,
              });
    });

    assert_eq!(hover,
               Hover {
                   contents: vec![MarkedString::String("Int".into())],
                   range: None,
               });
}
