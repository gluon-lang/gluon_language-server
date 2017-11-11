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

use url::Url;

use languageserver_types::{CompletionItem, CompletionItemKind, DidChangeTextDocumentParams,
                           Position, PublishDiagnosticsParams, Range,
                           TextDocumentContentChangeEvent, TextDocumentIdentifier,
                           TextDocumentPositionParams, VersionedTextDocumentIdentifier};

use gluon_language_server::CompletionData;

use support::{expect_notification, expect_response};


fn completion<W: ?Sized>(stdin: &mut W, id: u64, uri: &str, position: Position)
where
    W: Write,
{
    let hover = support::method_call(
        "textDocument/completion",
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

fn did_change<W: ?Sized>(stdin: &mut W, uri: &str, range: Range, text: &str)
where
    W: Write,
{
    let hover = support::notification(
        "textDocument/didChange",
        DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier {
                uri: support::test_url(uri),
                version: 1,
            },
            content_changes: vec![
                TextDocumentContentChangeEvent {
                    range: Some(range),
                    range_length: None,
                    text: text.to_string(),
                },
            ],
        },
    );

    support::write_message(stdin, hover).unwrap();
}


fn resolve<W: ?Sized>(stdin: &mut W, id: u64, item: &CompletionItem)
where
    W: Write,
{
    let hover = support::method_call("completionItem/resolve", id, item);

    support::write_message(stdin, hover).unwrap();
}

fn remove_completion_data(mut completions: Vec<CompletionItem>) -> Vec<CompletionItem> {
    for item in &mut completions {
        item.data.take();
    }
    completions
}

#[test]
fn local_completion() {
    support::send_rpc(|stdin, stdout| {
        let text = r#"
let test = 2
let test1 = ""
te
"#;
        support::did_open(stdin, "test", text);

        let _: PublishDiagnosticsParams = expect_notification(&mut *stdout);

        completion(
            stdin,
            1,
            "test",
            Position {
                line: 3,
                character: 2,
            },
        );

        let completions: Vec<CompletionItem> = expect_response(stdout);
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
    });
}

#[test]
fn prelude_completion() {
    support::send_rpc(|stdin, stdout| {
        support::did_open(stdin, "test", "no");

        let _: PublishDiagnosticsParams = expect_notification(&mut *stdout);

        completion(
            stdin,
            1,
            "test",
            Position {
                line: 0,
                character: 1,
            },
        );

        let completions: Vec<CompletionItem> = expect_response(stdout);
        let completions = remove_completion_data(completions);
        assert_eq!(
            completions,
            vec![
                CompletionItem {
                    label: "not".into(),
                    kind: Some(CompletionItemKind::Variable),
                    detail: Some("std.types.Bool -> std.types.Bool".into()),
                    ..CompletionItem::default()
                },
            ]
        );
    });
}

#[test]
fn resolve_completion() {
    support::send_rpc(|stdin, stdout| {
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
                }).unwrap(),
            ),
            ..CompletionItem::default()
        };

        let text = r#"
/// doc
let test = 2
let test1 = ""
te
"#;
        support::did_open(stdin, "test", text);

        let _: PublishDiagnosticsParams = expect_notification(&mut *stdout);

        resolve(stdin, 1, &completion);

        let actual: CompletionItem = expect_response(&mut *stdout);

        assert_eq!(
            actual,
            CompletionItem {
                documentation: Some("doc".to_string()),
                ..completion
            }
        );
    });
}

#[test]
fn url_encoded_path() {
    support::send_rpc(|stdin, stdout| {
        let text = r#"
let r = { abc = 1 }
r.
"#;
        let uri: Url = "file:///C%3A/examples/test.glu".parse().unwrap();
        support::did_open_uri(stdin, uri.clone(), text);

        let _: PublishDiagnosticsParams = expect_notification(&mut *stdout);

        let hover = support::method_call(
            "textDocument/completion",
            1,
            TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri },
                position: Position {
                    line: 3,
                    character: 2,
                },
            },
        );

        support::write_message(stdin, hover).unwrap();

        let completions = expect_response(stdout);
        let completions = remove_completion_data(completions);
        assert_eq!(
            completions,
            vec![
                CompletionItem {
                    label: "abc".into(),
                    kind: Some(CompletionItemKind::Variable),
                    detail: Some("Int".into()),
                    ..CompletionItem::default()
                },
            ]
        );
    });
}

#[test]
fn local_completion_with_update() {
    support::send_rpc(|stdin, stdout| {
        let text = r#"
let test = 2
let test1 = ""
test2
"#;

        support::did_open(stdin, "test", text);

        let _: PublishDiagnosticsParams = expect_notification(&mut *stdout);

        did_change(
            stdin,
            "test",
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
        );

        let _: PublishDiagnosticsParams = expect_notification(&mut *stdout);

        completion(
            stdin,
            1,
            "test",
            Position {
                line: 3,
                character: 2,
            },
        );

        let completions = expect_response(stdout);

        let completions = remove_completion_data(completions);
        assert_eq!(
            completions,
            vec![
                CompletionItem {
                    label: "test1".into(),
                    kind: Some(CompletionItemKind::Variable),
                    detail: Some("String".into()),
                    ..CompletionItem::default()
                },
            ]
        );
    });
}
