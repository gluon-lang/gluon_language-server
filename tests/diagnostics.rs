extern crate gluon_language_server;
extern crate languageserver_types;

extern crate jsonrpc_core;
extern crate serde;
extern crate serde_json;
extern crate url;

#[allow(unused)]
mod support;

use languageserver_types::{DiagnosticSeverity, Position, PublishDiagnosticsParams, Range};

#[test]
fn type_error() {
    support::send_rpc(|stdin, stdout| {
        let text = r#"
"" + 1
"#;
        support::did_open(stdin, "test.glu", text);

        let diagnostic: PublishDiagnosticsParams = support::expect_notification(stdout);

        assert_eq!(diagnostic.uri, support::test_url("test.glu"));
        assert_eq!(diagnostic.diagnostics.len(), 1);
        let error = &diagnostic.diagnostics[0];
        assert_eq!(error.severity, Some(DiagnosticSeverity::Error));
        assert_eq!(
            error.range,
            Range {
                start: Position {
                    line: 1,
                    character: 0,
                },
                end: Position {
                    line: 1,
                    character: 2,
                },
            }
        );
    });
}
