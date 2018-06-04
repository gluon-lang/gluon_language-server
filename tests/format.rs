extern crate gluon_language_server;
extern crate languageserver_types;

extern crate jsonrpc_core;
#[macro_use]
extern crate pretty_assertions;
extern crate serde;
extern crate serde_json;
extern crate url;

#[allow(unused)]
mod support;

use std::io::Write;

use languageserver_types::*;

use support::{did_change_event, expect_notification, expect_response, hover};

fn format<W: ?Sized>(stdin: &mut W, id: u64, uri: &str)
where
    W: Write,
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
                properties: Default::default(),
            },
        },
    );

    support::write_message(stdin, hover).unwrap();
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
        support::did_open(stdin, "test", text);

        let _: PublishDiagnosticsParams = expect_notification(&mut *stdout);

        format(stdin, 2, "test");

        let edits: Vec<TextEdit> = expect_response(stdout);

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
    });
}

#[test]
fn empty_content_changes_do_not_lockup_server() {
    let text = r#"
let x = 1
x + "abc"
"#;
    support::send_rpc(move |stdin, stdout| {
        support::did_open(stdin, "test", text);

        let _: PublishDiagnosticsParams = expect_notification(&mut *stdout);

        did_change_event(stdin, "test", 2, vec![]);
        let _: PublishDiagnosticsParams = expect_notification(&mut *stdout);

        hover(
            stdin,
            4,
            "test",
            Position {
                line: 2,
                character: 7,
            },
        );
        let hover: Hover = expect_response(&mut *stdout);

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
    });
}
