#[allow(unused)]
mod support;

use languageserver_types::{DiagnosticSeverity, Position, PublishDiagnosticsParams, Range};

#[test]
fn type_error() {
    support::send_rpc(|stdin, stdout| {
        let text = r#"
not ""
"#;
        support::did_open(stdin, "test.glu", text);

        let diagnostic: PublishDiagnosticsParams = support::expect_notification(stdout);

        assert_eq!(diagnostic.uri, support::test_url("test.glu"));
        assert_eq!(
            diagnostic.diagnostics.len(),
            1,
            "{:?}",
            diagnostic.diagnostics
        );
        let error = &diagnostic.diagnostics[0];
        assert_eq!(error.severity, Some(DiagnosticSeverity::Error));
        assert_eq!(
            error.range,
            Range {
                start: Position {
                    line: 1,
                    character: 4,
                },
                end: Position {
                    line: 1,
                    character: 6,
                },
            }
        );
    });
}
