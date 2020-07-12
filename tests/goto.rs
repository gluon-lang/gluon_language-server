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

#[test]
fn goto_definition() {
    support::send_rpc(|stdin, stdout| {
        Box::pin(async move {
            let text = r#"
let test = 1
let test2 = 1
test
"#;
            support::did_open(stdin, "test", text).await;

            let _: PublishDiagnosticsParams = expect_notification(&mut *stdout).await;

            text_document_definition(
                stdin,
                1,
                "test",
                Position {
                    line: 3,
                    character: 2,
                },
            )
            .await;

            let def: GotoDefinitionResponse = expect_response(stdout).await;
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
        })
    });
}

#[test]
fn goto_definition_of_type() {
    support::send_rpc(|stdin, stdout| {
        Box::pin(async move {
            let text = r#"
type Test = Int
let test2 : Test = 1
test
"#;
            support::did_open(stdin, "test", text).await;

            let _: PublishDiagnosticsParams = expect_notification(&mut *stdout).await;

            text_document_definition(
                stdin,
                1,
                "test",
                Position {
                    line: 2,
                    character: 14,
                },
            )
            .await;

            let def: GotoDefinitionResponse = expect_response(stdout).await;
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
        })
    });
}

#[test]
#[ignore]
fn goto_definition_module() {
    support::send_rpc(|stdin, stdout| {
        Box::pin(async move {
            let text = r#"
let test = import! main
test
"#;
            support::did_open(stdin, "test", text).await;

            let _: PublishDiagnosticsParams = expect_notification(&mut *stdout).await;

            text_document_definition(
                stdin,
                1,
                "test",
                Position {
                    line: 2,
                    character: 20,
                },
            )
            .await;

            let def: GotoDefinitionResponse = expect_response(stdout).await;
            assert_eq!(
                def,
                GotoDefinitionResponse::Scalar(Location {
                    uri: test_url("main"),
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
        })
    });
}
