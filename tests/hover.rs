
extern crate gluon_language_server;
extern crate vscode_languageserver_types;

extern crate jsonrpc_core;
extern crate serde_json;
extern crate serde;

mod support;

use vscode_languageserver_types::{Hover, MarkedString, Position};

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

        support::hover(stdin,
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

        support::hover(stdin,
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

        support::hover(stdin,
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
