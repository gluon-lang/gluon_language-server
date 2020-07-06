use lsp_types::{GotoDefinitionParams, GotoDefinitionResponse, Location};

use crate::{byte_span_to_range, completion, position_to_byte_index};

use super::*;

pub fn register(io: &mut IoHandler, thread: &RootedThread) {
    let thread = thread.clone();
    let f = move |params: GotoDefinitionParams| {
        let thread = thread.clone();
        async move {
            retrieve_expr(
                &thread,
                &params.text_document_position_params.text_document.uri,
                |module| {
                    let pos = position_to_byte_index(
                        &*module.source,
                        &params.text_document_position_params.position,
                    )?;
                    let module_expr = module.expr.expr();
                    let search_symbol =
                        match completion::symbol(module.source.span(), module_expr, pos) {
                            Ok(search_symbol) => search_symbol,
                            Err(_) => {
                                return Ok(None);
                            }
                        };

                    let source = &module.source;

                    if let Some(symbol) = completion::all_symbols(module.source.span(), module_expr)
                        .into_iter()
                        .find(|symbol| **symbol.value.name == *search_symbol)
                    {
                        return Ok(Some(GotoDefinitionResponse::Scalar(Location {
                            uri: module.uri.clone(),
                            range: byte_span_to_range(source, symbol.span)?,
                        })));
                    }

                    Ok(None)
                },
            )
            .await
        }
    };
    io.add_async_method(request!("textDocument/definition"), f);
}
