extern crate gluon_language_server;
extern crate languageserver_types;

extern crate jsonrpc_core;
#[macro_use]
extern crate pretty_assertions;
extern crate serde;
extern crate serde_json;
extern crate url;

extern crate gluon;

mod support;

use std::io::Write;

use languageserver_types::{request::*, *};

use crate::support::*;

fn text_document_definition<W: ?Sized>(stdin: &mut W, id: u64, uri: &str, position: Position)
where
    W: Write,
{
    let hover = support::method_call(
        "textDocument/definition",
        id,
        TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: support::test_url(uri),
            },
            position,
        },
    );

    support::write_message(stdin, hover).unwrap();
}

#[test]
fn goto_definition() {
    support::send_rpc(|stdin, stdout| {
        let text = r#"
let test = 1
let test2 = 1
test
"#;
        support::did_open(stdin, "test", text);

        let _: PublishDiagnosticsParams = expect_notification(&mut *stdout);

        text_document_definition(
            stdin,
            1,
            "test",
            Position {
                line: 3,
                character: 2,
            },
        );

        let def: GotoDefinitionResponse = expect_response(stdout);
        assert_eq!(
            def,
            GotoDefinitionResponse::Scalar(Location {
                uri: test_url("test"),
                range: Range {
                    start: Position {
                        line: 1,
                        character: 4
                    },
                    end: Position {
                        line: 1,
                        character: 8
                    },
                }
            })
        );
    });
}

#[test]
fn goto_definition_of_type() {
    support::send_rpc(|stdin, stdout| {
        let text = r#"
type Test = Int
let test2 : Test = 1
test
"#;
        support::did_open(stdin, "test", text);

        let _: PublishDiagnosticsParams = expect_notification(&mut *stdout);

        text_document_definition(
            stdin,
            1,
            "test",
            Position {
                line: 2,
                character: 14,
            },
        );

        let def: GotoDefinitionResponse = expect_response(stdout);
        assert_eq!(
            def,
            GotoDefinitionResponse::Scalar(Location {
                uri: test_url("test"),
                range: Range {
                    start: Position {
                        line: 1,
                        character: 5
                    },
                    end: Position {
                        line: 1,
                        character: 9
                    },
                }
            })
        );
    });
}
