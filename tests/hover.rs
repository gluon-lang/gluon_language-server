extern crate gluon_language_server;
extern crate languageserver_types;

extern crate jsonrpc_core;
extern crate serde;
extern crate serde_json;
extern crate url;

mod support;

use std::io::Write;

use languageserver_types::{Hover, HoverContents, MarkedString, Position, PublishDiagnosticsParams,
                           Range, TextDocumentIdentifier, TextDocumentPositionParams};

use support::{expect_notification, expect_response};

fn hover<W: ?Sized>(stdin: &mut W, id: u64, uri: &str, position: Position)
where
    W: Write,
{
    let hover = support::method_call(
        "textDocument/hover",
        id,
        TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: support::test_url(uri),
            },
            position: position,
        },
    );

    support::write_message(stdin, hover).unwrap();
}

const STREAM_SOURCE: &'static str = r#"
let prelude = import! "std/prelude.glu"
let { Num } = prelude
let { Option } = import! "std/option.glu"

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
    support::send_rpc(|stdin, stdout| {
        let uri = "file:///c%3A/test/test.glu";
        support::did_open(stdin, uri, "123");

        let _: PublishDiagnosticsParams = expect_notification(&mut *stdout);

        hover(
            stdin,
            2,
            uri,
            Position {
                line: 0,
                character: 2,
            },
        );
        let hover: Hover = expect_response(stdout);

        assert_eq!(
            hover,
            Hover {
                contents: HoverContents::Scalar(MarkedString::String("Int".into())),
                range: Some(Range {
                    start: Position {
                        line: 0,
                        character: 0,
                    },
                    end: Position {
                        line: 0,
                        character: 3,
                    },
                }),
            }
        );
    });
}

#[test]
fn identifier() {
    support::send_rpc(|stdin, stdout| {
        let src = r#"
let test = 1
test
"#;
        support::did_open(stdin, "test", src);

        let _: PublishDiagnosticsParams = expect_notification(&mut *stdout);

        hover(
            stdin,
            2,
            "test",
            Position {
                line: 2,
                character: 2,
            },
        );

        let hover: Hover = expect_response(stdout);

        assert_eq!(
            hover,
            Hover {
                contents: HoverContents::Scalar(MarkedString::String("Int".into())),
                range: Some(Range {
                    start: Position {
                        line: 2,
                        character: 0,
                    },
                    end: Position {
                        line: 2,
                        character: 4,
                    },
                }),
            }
        );
    });
}

#[test]
fn stream() {
    support::send_rpc(|stdin, stdout| {
        support::did_open(stdin, "stream", STREAM_SOURCE);

        let _: PublishDiagnosticsParams = expect_notification(&mut *stdout);

        hover(
            stdin,
            2,
            "stream",
            Position {
                line: 13,
                character: 29,
            },
        );

        let hover: Hover = expect_response(stdout);

        assert_eq!(
            hover,
            Hover {
                contents: HoverContents::Scalar(MarkedString::String("Int".into())),
                range: Some(Range {
                    start: Position {
                        line: 13,
                        character: 28,
                    },
                    end: Position {
                        line: 13,
                        character: 29,
                    },
                }),
            }
        );
    });
}
