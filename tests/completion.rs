
extern crate gluon_language_server;
extern crate vscode_languageserver_types;

extern crate jsonrpc_core;
extern crate serde_json;
extern crate serde;

mod support;

use std::io::Write;

use vscode_languageserver_types::{CompletionItem, CompletionItemKind, Position,
                                  TextDocumentIdentifier, TextDocumentPositionParams};

fn completion<W: ?Sized>(stdin: &mut W, id: u64, uri: &str, position: Position)
    where W: Write,
{
    let hover = support::method_call("textDocument/completion",
                                     id,
                                     TextDocumentPositionParams {
                                         text_document: TextDocumentIdentifier { uri: uri.into() },
                                         position: position,
                                     });

    support::write_message(stdin, hover).unwrap();
}

#[test]
fn local_completion() {
    let completions: Vec<CompletionItem> = support::send_rpc(|mut stdin| {
        let text = r#"
let test = 2
let test1 = ""
te
"#;
        support::did_open(stdin, "test", text);

        completion(stdin,
                   1,
                   "test",
                   Position {
                       line: 3,
                       character: 2,
                   })
    });
    assert_eq!(completions,
               [CompletionItem {
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
                }]);
}

#[test]
fn prelude_completion() {
    let completions: Vec<CompletionItem> = support::send_rpc(|mut stdin| {
        support::did_open(stdin, "test", "no");

        completion(stdin,
                   1,
                   "test",
                   Position {
                       line: 0,
                       character: 1,
                   })
    });
    assert_eq!(completions,
               [CompletionItem {
                    label: "not".into(),
                    kind: Some(CompletionItemKind::Variable),
                    detail: Some("| False | True -> std.types.Bool".into()),
                    ..CompletionItem::default()
                }]);
}
