use lsp_types::{DocumentSymbolParams, DocumentSymbolResponse};

use crate::completion;

use super::*;

pub fn register(io: &mut IoHandler, thread: &RootedThread) {
    let thread = thread.clone();
    let f = move |params: DocumentSymbolParams| {
        let thread = thread.clone();
        async move {
            retrieve_expr(&thread, &params.text_document.uri, |module| {
                let expr = module.expr.expr();

                let symbols = completion::all_symbols(module.source.span(), expr);

                let source = &module.source;

                let x = completion_symbols_to_document_symbols(source, &symbols)?;
                Ok(Some(DocumentSymbolResponse::Nested(x)))
            })
            .await
        }
    };
    io.add_async_method(request!("textDocument/documentSymbol"), f);
}
