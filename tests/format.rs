extern crate gluon_language_server;
extern crate languageserver_types;

extern crate jsonrpc_core;
extern crate serde_json;
extern crate serde;
extern crate url;
#[macro_use]
extern crate pretty_assertions;

#[allow(unused)]
mod support;

use std::io::Write;

use languageserver_types::*;

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
let x =
         1
x   +
   2
"#;
    let expected = r#"
let x = 1
x + 2
"#;
    let edits: Vec<TextEdit> = support::send_rpc(|stdin| {
        support::did_open(stdin, "test", text);

        format(stdin, 2, "test")
    });

    assert_eq!(
        edits,
        vec![
            TextEdit {
                range: Range {
                    start: Position {
                        line: 0,
                        character: 0,
                    },
                    end: Position {
                        line: 5,
                        character: 0,
                    },
                },
                new_text: expected.to_string(),
            },
        ]
    );
}
