#[macro_use]
extern crate pretty_assertions;

mod support;

use tokio::io::AsyncWrite;

use lsp_types::*;

use crate::support::*;

async fn text_document_definition<W: ?Sized>(stdin: &mut W, id: u64, uri: &str, position: Position)
where
    W: AsyncWrite + std::marker::Unpin,
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

    support::write_message(stdin, hover).await.unwrap();
}

fn test_goto_definition(text: &str, position: Position, expected: GotoDefinitionResponse) {
    let text = text.to_string();
    support::send_rpc(move |stdin, stdout| {
        Box::pin(async move {
            support::did_open(stdin, "test", &text).await;

            let _: PublishDiagnosticsParams = expect_notification(&mut *stdout).await;

            text_document_definition(stdin, 1, "test", position).await;

            let def: GotoDefinitionResponse = expect_response(stdout).await;
            assert_eq!(def, expected);
        })
    });
}

#[test]
fn goto_definition() {
    let text = r#"
let test = 1
let test2 = 1
test
"#;
    test_goto_definition(
        text,
        Position {
            line: 3,
            character: 2,
        },
        GotoDefinitionResponse::Scalar(Location {
            uri: test_url("test"),
            range: Range {
                start: Position {
                    line: 1,
                    character: 4,
                },
                end: Position {
                    line: 1,
                    character: 8,
                },
            },
        }),
    );
}

#[test]
fn goto_definition_of_type() {
    let text = r#"
type Test = Int
let test2 : Test = 1
test
"#;
    test_goto_definition(
        text,
        Position {
            line: 2,
            character: 14,
        },
        GotoDefinitionResponse::Scalar(Location {
            uri: test_url("test"),
            range: Range {
                start: Position {
                    line: 1,
                    character: 5,
                },
                end: Position {
                    line: 1,
                    character: 9,
                },
            },
        }),
    );
}

#[test]
fn goto_definition_module() {
    let text = r#"
let test = import! tests.main
test
"#;
    test_goto_definition(
        text,
        Position {
            line: 1,
            character: 20,
        },
        GotoDefinitionResponse::Scalar(Location {
            uri: test_url("tests/main.glu"),
            range: Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 0,
                    character: 0,
                },
            },
        }),
    )
}

#[test]
fn goto_definition_argument() {
    let text = r#"
let test abc = abc
test
"#;
    test_goto_definition(
        text,
        Position {
            line: 1,
            character: 18,
        },
        GotoDefinitionResponse::Scalar(Location {
            uri: test_url("test"),
            range: Range {
                start: Position {
                    line: 1,
                    character: 9,
                },
                end: Position {
                    line: 1,
                    character: 12,
                },
            },
        }),
    )
}
