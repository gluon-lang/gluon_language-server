use codespan_lsp::position_to_byte_index;
use languageserver_types::{request::GotoDefinitionResponse, Location, TextDocumentPositionParams};

use completion;

use super::*;

pub fn register(io: &mut IoHandler, thread: &RootedThread) {
    let thread = thread.clone();
    let f = move |params: TextDocumentPositionParams| {
        retrieve_expr(&thread, &params.text_document.uri, |module| {
            let pos = position_to_byte_index(&module.source, &params.position)?;
            let search_symbol = match completion::symbol(module.source.span(), &module.expr, pos) {
                Ok(search_symbol) => search_symbol,
                Err(_) => {
                    return Ok(None);
                }
            };

            let source = &module.source;

            if let Some(symbol) = completion::all_symbols(module.source.span(), &module.expr)
                .into_iter()
                .find(|symbol| **symbol.value.name == *search_symbol)
            {
                return Ok(Some(GotoDefinitionResponse::Scalar(Location {
                    uri: module.uri.clone(),
                    range: byte_span_to_range(source, symbol.span)?,
                })));
            }

            Ok(None)
        })
    };
    io.add_async_method(request!("textDocument/definition"), f);
}
