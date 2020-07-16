#[macro_use]
extern crate pretty_assertions;

use serde_json;

mod support;

use tokio::io::AsyncWrite;

use url::Url;

use lsp_types::*;

use gluon_language_server::CompletionData;

use crate::support::{did_change, expect_notification, expect_response};

async fn completion<W: ?Sized>(stdin: &mut W, id: u64, uri: &str, position: Position)
where
    W: AsyncWrite + std::marker::Unpin,
{
    let hover = support::method_call(
        "textDocument/completion",
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

async fn resolve<W: ?Sized>(stdin: &mut W, id: u64, item: &CompletionItem)
where
    W: AsyncWrite + std::marker::Unpin,
{
    let hover = support::method_call("completionItem/resolve", id, item);

    support::write_message(stdin, hover).await.unwrap();
}

async fn workspace_symbol<W: ?Sized>(stdin: &mut W, id: u64, query: &str)
where
    W: AsyncWrite + std::marker::Unpin,
{
    let msg = support::method_call(
        "workspace/symbol",
        id,
        WorkspaceSymbolParams {
            query: query.into(),
            ..Default::default()
        },
    );

    support::write_message(stdin, msg).await.unwrap();
}

fn remove_completion_data(mut completions: Vec<CompletionItem>) -> Vec<CompletionItem> {
    for item in &mut completions {
        item.data.take();
    }
    completions
}

#[test]
fn local_completion_simple() {
    support::send_rpc(move |stdin, stdout| {
        Box::pin(async move {
            let text = r#"
let test = 2
let test1 = ""
te
"#;
            support::did_open(stdin, "test", text).await;

            let _: PublishDiagnosticsParams = expect_notification(&mut *stdout).await;

            completion(
                stdin,
                1,
                "test",
                Position {
                    line: 3,
                    character: 2,
                },
            )
            .await;

            let completions: Vec<CompletionItem> = expect_response(stdout).await;
            let completions = remove_completion_data(completions);
            assert_eq!(
                completions,
                vec![
                    CompletionItem {
                        label: "test".into(),
                        kind: Some(CompletionItemKind::Variable),
                        detail: Some("Int".into()),
                        ..CompletionItem::default()
                    },
                    CompletionItem {
                        label: "test1".into(),
                        kind: Some(CompletionItemKind::Variable),
                        detail: Some("String".into()),
                        ..CompletionItem::default()
                    },
                ]
            );
        })
    });
}

#[test]
fn operator_completion_on_whitespace() {
    support::send_rpc(move |stdin, stdout| {
        Box::pin(async move {
            let text = r#"
let {  } = { (*>) = 1 }
()
"#;
            support::did_open(stdin, "test", text).await;

            let _: PublishDiagnosticsParams = expect_notification(&mut *stdout).await;

            let insert_pos = Position {
                line: 1,
                character: 7,
            };
            completion(stdin, 1, "test", insert_pos).await;

            let completions: Vec<CompletionItem> = expect_response(stdout).await;
            let completions = remove_completion_data(completions);
            assert_eq!(
                completions,
                vec![CompletionItem {
                    label: "*>".into(),
                    kind: Some(CompletionItemKind::Variable),
                    detail: Some("Int".into()),
                    insert_text: Some("(*>)".to_string()),
                    ..CompletionItem::default()
                }]
            );
        })
    });
}

#[test]
fn prelude_completion() {
    support::send_rpc(move |stdin, stdout| {
        Box::pin(async move {
            support::did_open(stdin, "test", "no").await;

            let _: PublishDiagnosticsParams = expect_notification(&mut *stdout).await;

            completion(
                stdin,
                1,
                "test",
                Position {
                    line: 0,
                    character: 1,
                },
            )
            .await;

            let completions: Vec<CompletionItem> = expect_response(stdout).await;
            let completions = remove_completion_data(completions);
            assert_eq!(
                completions,
                vec![CompletionItem {
                    label: "not".into(),
                    kind: Some(CompletionItemKind::Function),
                    detail: Some("std.types.Bool -> std.types.Bool".into()),
                    ..CompletionItem::default()
                }]
            );
        })
    });
}

#[test]
fn resolve_completion() {
    support::send_rpc(move |stdin, stdout| {
        Box::pin(async move {
            let completion = CompletionItem {
                label: "test".into(),
                kind: Some(CompletionItemKind::Variable),
                detail: Some("Int".into()),
                data: Some(
                    serde_json::to_value(CompletionData {
                        text_document_uri: support::test_url("test"),
                        position: Position {
                            character: 2,
                            line: 4,
                        },
                    })
                    .unwrap(),
                ),
                ..CompletionItem::default()
            };

            let text = r#"
/// doc
let test = 2
let test1 = ""
te
"#;
            support::did_open(stdin, "test", text).await;

            let _: PublishDiagnosticsParams = expect_notification(&mut *stdout).await;

            resolve(stdin, 1, &completion).await;

            let actual: CompletionItem = expect_response(&mut *stdout).await;

            assert_eq!(
                actual,
                CompletionItem {
                    documentation: Some(Documentation::MarkupContent(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: "doc".to_string()
                    })),
                    ..completion
                }
            );
        })
    });
}

#[test]
fn url_encoded_path() {
    support::send_rpc(move |stdin, stdout| {
        Box::pin(async move {
            let text = r#"
let r = { abc = 1 }
r.
"#;
            let uri: Url = "file:///C%3A/examples/test.glu".parse().unwrap();
            support::did_open_uri(stdin, uri.clone(), text).await;

            let _: PublishDiagnosticsParams = expect_notification(&mut *stdout).await;

            let hover = support::method_call(
                "textDocument/completion",
                1,
                TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri: uri },
                    position: Position {
                        line: 2,
                        character: 2,
                    },
                },
            );

            support::write_message(stdin, hover).await.unwrap();

            let completions = expect_response(stdout).await;
            let completions = remove_completion_data(completions);
            assert_eq!(
                completions,
                vec![CompletionItem {
                    label: "abc".into(),
                    kind: Some(CompletionItemKind::Variable),
                    detail: Some("Int".into()),
                    ..CompletionItem::default()
                }]
            );
        })
    });
}

#[test]
fn local_completion_with_update() {
    support::send_rpc(move |stdin, stdout| {
        Box::pin(async move {
            let text = r#"
let test = 2
let test1 = ""
test2
"#;

            support::did_open(stdin, "test", text).await;

            let _: PublishDiagnosticsParams = expect_notification(&mut *stdout).await;

            did_change(
                stdin,
                "test",
                2,
                Range {
                    start: Position {
                        line: 3,
                        character: 2,
                    },
                    end: Position {
                        line: 3,
                        character: 5,
                    },
                },
                "st1",
            )
            .await;

            let _: PublishDiagnosticsParams = expect_notification(&mut *stdout).await;

            completion(
                stdin,
                1,
                "test",
                Position {
                    line: 3,
                    character: 2,
                },
            )
            .await;

            let completions = expect_response(stdout).await;

            let completions = remove_completion_data(completions);
            assert_eq!(
                completions,
                vec![CompletionItem {
                    label: "test1".into(),
                    kind: Some(CompletionItemKind::Variable),
                    detail: Some("String".into()),
                    ..CompletionItem::default()
                }]
            );
        })
    });
}

#[test]
fn local_completion_out_of_order_update() {
    support::send_rpc(move |stdin, stdout| {
        Box::pin(async move {
            let text = r#"
let test = 2
let test1 = ""
test2
"#;

            support::did_open(stdin, "test", text).await;

            let _: PublishDiagnosticsParams = expect_notification(&mut *stdout).await;

            did_change(
                stdin,
                "test",
                3,
                Range {
                    start: Position {
                        line: 3,
                        character: 3,
                    },
                    end: Position {
                        line: 3,
                        character: 5,
                    },
                },
                "t1",
            )
            .await;

            did_change(
                stdin,
                "test",
                2,
                Range {
                    start: Position {
                        line: 3,
                        character: 2,
                    },
                    end: Position {
                        line: 3,
                        character: 3,
                    },
                },
                "s",
            )
            .await;

            let _: PublishDiagnosticsParams = expect_notification(&mut *stdout).await;

            completion(
                stdin,
                1,
                "test",
                Position {
                    line: 3,
                    character: 2,
                },
            )
            .await;

            let completions = expect_response(stdout).await;

            let completions = remove_completion_data(completions);
            assert_eq!(
                completions,
                vec![CompletionItem {
                    label: "test1".into(),
                    kind: Some(CompletionItemKind::Variable),
                    detail: Some("String".into()),
                    ..CompletionItem::default()
                }]
            );
        })
    });
}

#[test]
fn completion_unicode_characters() {
    support::send_rpc(move |stdin, stdout| {
        Box::pin(async move {
            let text = r#"
let test = 1
"åäö" t
"#;
            support::did_open(stdin, "test", text).await;

            let _: PublishDiagnosticsParams = expect_notification(&mut *stdout).await;

            completion(
                stdin,
                1,
                "test",
                Position {
                    line: 2,
                    character: 7,
                },
            )
            .await;

            let completions: Vec<CompletionItem> = expect_response(stdout).await;
            let completions = remove_completion_data(completions);
            assert_eq!(
                completions,
                vec![CompletionItem {
                    label: "test".into(),
                    kind: Some(CompletionItemKind::Variable),
                    detail: Some("Int".into()),
                    ..CompletionItem::default()
                }]
            );
        })
    });
}

#[test]
fn workspace_symbol_test() {
    support::send_rpc(move |stdin, stdout| {
        Box::pin(async move {
            let text = r#"
let myfunc x = x
{ myfunc }
"#;
            support::did_open(stdin, "test", text).await;

            let _: PublishDiagnosticsParams = expect_notification(&mut *stdout).await;

            workspace_symbol(stdin, 2, "myfunc").await;

            let symbols: Vec<SymbolInformation> = expect_response(stdout).await;
            assert_eq!(
                symbols.into_iter().map(|s| s.name).collect::<Vec<_>>(),
                vec!["myfunc".to_string()],
            );
        })
    });
}
