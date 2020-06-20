use languageserver_types::{DocumentHighlight, TextDocumentPositionParams};

use super::*;

use crate::completion;

pub fn register(io: &mut IoHandler, thread: &RootedThread) {
    let thread = thread.clone();
    let f = move |params: TextDocumentPositionParams| {
        let thread = thread.clone();
        async move {
            retrieve_expr(&thread, &params.text_document.uri, |module| {
                let expr = module.expr.expr();

                let source = &module.source;

                let byte_index = position_to_byte_index(&source, &params.position)?;

                let symbol_spans = completion::find_all_symbols(source.span(), expr, byte_index)
                    .map(|t| t.1)
                    .unwrap_or(Vec::new());

                symbol_spans
                    .into_iter()
                    .map(|span| {
                        Ok(DocumentHighlight {
                            kind: None,
                            range: byte_span_to_range(&source, span)?,
                        })
                    })
                    .collect::<Result<_, _>>()
                    .map(Some)
            })
            .await
        }
    };
    io.add_async_method(request!("textDocument/documentHighlight"), f);
}
