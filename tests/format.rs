#[macro_use]
extern crate pretty_assertions;

#[allow(unused)]
mod support;

use tokio::io::AsyncWrite;

use lsp_types::*;

use crate::support::{did_change_event, expect_notification, expect_response, hover};

async fn format<W: ?Sized>(stdin: &mut W, id: u64, uri: &str)
where
    W: AsyncWrite + std::marker::Unpin,
{
    let hover = support::method_call(
        "textDocument/formatting",
        id,
        DocumentFormattingParams {
            text_document: TextDocumentIdentifier {
                uri: support::test_url(uri),
            },
            options: FormattingOptions {
                tab_size: 4,
                insert_spaces: true,
                ..Default::default()
            },
            work_done_progress_params: Default::default(),
        },
    );

    support::write_message(stdin, hover).await.unwrap();
}

#[test]
fn simple() {
    let text = r#"
let x =           1
x   +
   2
"#;
    let expected = r#"
let x = 1
x + 2
"#;
    support::send_rpc(move |stdin, stdout| {
        Box::pin(async move {
            support::did_open(stdin, "test", text).await;

            let _: PublishDiagnosticsParams = expect_notification(&mut *stdout).await;

            format(stdin, 2, "test").await;

            let edits: Vec<TextEdit> = expect_response(stdout).await;

            assert_eq!(
                edits,
                vec![TextEdit {
                    range: Range {
                        start: Position {
                            line: 0,
                            character: 0,
                        },
                        end: Position {
                            line: 4,
                            character: 0,
                        },
                    },
                    new_text: expected.to_string(),
                }]
            );
        })
    });
}

#[test]
fn empty_content_changes_do_not_lockup_server() {
    let text = r#"
let x = 1
x + "abc"
"#;
    support::send_rpc(move |stdin, stdout| {
        Box::pin(async move {
            // Insert a dummy file so that test is not the first file
            support::did_open(stdin, "dummy", r#""aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa""#).await;
            let _: PublishDiagnosticsParams = expect_notification(&mut *stdout).await;
            support::did_open(stdin, "test", text).await;

            let _: PublishDiagnosticsParams = expect_notification(&mut *stdout).await;

            // Since nothing changed we don't update the version
            did_change_event(stdin, "test", 1, vec![]).await;

            hover(
                stdin,
                4,
                "test",
                Position {
                    line: 2,
                    character: 7,
                },
            )
            .await;
            let hover: Hover = expect_response(&mut *stdout).await;

            assert_eq!(
                hover,
                Hover {
                    contents: HoverContents::Scalar(MarkedString::String("String".into())),
                    range: Some(Range {
                        start: Position {
                            line: 2,
                            character: 4,
                        },
                        end: Position {
                            line: 2,
                            character: 9,
                        },
                    }),
                }
            );
        })
    });
}
