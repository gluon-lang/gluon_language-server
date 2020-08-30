mod support;

use tokio::io::{AsyncBufRead, AsyncWrite};

use lsp_types::{
    DocumentSymbolParams, DocumentSymbolResponse, PublishDiagnosticsParams, TextDocumentIdentifier,
};

async fn document_symbols(
    stdin: &mut (dyn AsyncWrite + std::marker::Unpin + Send),
    stdout: &mut (dyn AsyncBufRead + std::marker::Unpin + Send),
    id: u64,
    uri: &str,
) -> DocumentSymbolResponse {
    support::run_method_call::<lsp_types::lsp_request!("textDocument/documentSymbol")>(
        stdin,
        stdout,
        id,
        DocumentSymbolParams {
            text_document: TextDocumentIdentifier {
                uri: support::test_url(uri),
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        },
    )
    .await
    .unwrap()
}

#[test]
fn issue_40() {
    support::send_rpc(|stdin, stdout| {
        Box::pin(async move {
            let text = r#"
type U = (Int, Int)

()
"#;
            support::did_open(stdin, "test.glu", text).await;

            let diagnostic: PublishDiagnosticsParams =
                support::expect_notification(&mut *stdout).await;

            assert_eq!(diagnostic.uri, support::test_url("test.glu"));

            document_symbols(stdin, stdout, 1, "test.glu").await;
        })
    });
}
