use lsp_types::{GotoDefinitionParams, GotoDefinitionResponse, Location};

use gluon::base::symbol::SymbolRef;

use crate::{byte_span_to_range, completion, position_to_byte_index};

use super::*;

pub fn register(io: &mut IoHandler, thread: &RootedThread) {
    let thread = thread.clone();
    let f = move |params: GotoDefinitionParams| {
        let thread = thread.clone();
        async move {
            let module = retrieve_module_from_url(
                &thread,
                &params.text_document_position_params.text_document.uri,
            )
            .await?;

            let pos = position_to_byte_index(
                &*module.source,
                &params.text_document_position_params.position,
            )?;
            let module_expr = module.expr.expr();
            let search_symbol = match completion::symbol(module.source.span(), module_expr, pos) {
                Ok(search_symbol) => search_symbol,
                Err(_) => {
                    return Ok(None);
                }
            };

            debug!("Found symbol {}", search_symbol);

            if search_symbol.is_global() {
                let module = retrieve_module(&thread, search_symbol.as_pretty_str()).await?;

                Ok(Some(GotoDefinitionResponse::Scalar(Location {
                    uri: module.uri.clone(),
                    range: Default::default(),
                })))
            } else {
                let source = &module.source;

                let all_symbols = completion::all_symbols(module.source.span(), module_expr);

                fn find_symbol<'a, 'ast>(
                    all_symbols: Vec<Spanned<CompletionSymbol<'a, 'ast>, BytePos>>,
                    search_symbol: &SymbolRef,
                ) -> Option<Spanned<CompletionSymbol<'a, 'ast>, BytePos>> {
                    all_symbols.into_iter().find_map(|symbol| {
                        if **symbol.value.name == *search_symbol {
                            Some(symbol)
                        } else {
                            find_symbol(symbol.value.children, search_symbol)
                        }
                    })
                }
                if let Some(symbol) = find_symbol(all_symbols, search_symbol) {
                    return Ok(Some(GotoDefinitionResponse::Scalar(Location {
                        uri: module.uri.clone(),
                        range: byte_span_to_range(source, symbol.span)?,
                    })));
                }

                Ok(None)
            }
        }
    };
    io.add_async_method(request!("textDocument/definition"), f);
}
