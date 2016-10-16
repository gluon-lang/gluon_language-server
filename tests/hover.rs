
extern crate gluon_language_server;
extern crate vscode_languageserver_types;

extern crate jsonrpc_core;
extern crate serde_json;
extern crate serde;

mod support;

use vscode_languageserver_types::{DidOpenTextDocumentParams, Hover, MarkedString, Position,
                                  TextDocumentIdentifier, TextDocumentItem,
                                  TextDocumentPositionParams};

#[test]
fn hover() {
    let hover: Hover = support::send_rpc(|mut stdin| {

        let did_open = support::notification("textDocument/didOpen",
                                             DidOpenTextDocumentParams {
                                                 text_document: TextDocumentItem {
                                                     uri: "test".into(),
                                                     language_id: "gluon".into(),
                                                     text: "123".into(),
                                                     version: 1,
                                                 },
                                             });

        support::write_message(&mut stdin, did_open).unwrap();

        let hover =
            support::method_call("textDocument/hover",
                                 2,
                                 TextDocumentPositionParams {
                                     text_document: TextDocumentIdentifier { uri: "test".into() },
                                     position: Position {
                                         line: 0,
                                         character: 2,
                                     },
                                 });

        support::write_message(&mut stdin, hover).unwrap();
    });

    assert_eq!(hover,
               Hover {
                   contents: vec![MarkedString::String("Int".into())],
                   range: None,
               });
}
