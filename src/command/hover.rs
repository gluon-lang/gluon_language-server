use {
    futures::prelude::*,
    jsonrpc_core::IoHandler,
    lsp_types::{Hover, HoverContents, HoverParams, MarkedString},
};

use crate::{completion, rpc::LanguageServerCommand, BoxFuture};

use super::*;

struct HoverCommand(RootedThread);
impl LanguageServerCommand<HoverParams> for HoverCommand {
    type Future = BoxFuture<Self::Output, ServerError<()>>;
    type Output = Option<Hover>;
    type Error = ();
    fn execute(&self, change: HoverParams) -> BoxFuture<Option<Hover>, ServerError<()>> {
        let thread = self.0.clone();
        async move {
            retrieve_expr(
                &thread,
                &change.text_document_position_params.text_document.uri,
                |module| {
                    let expr = module.expr.expr();

                    let source = &module.source;
                    let byte_index = position_to_byte_index(
                        &source,
                        &change.text_document_position_params.position,
                    )?;

                    let env = thread.get_env();
                    let (_, metadata_map) = gluon::check::metadata::metadata(&env, &expr);
                    let opt_metadata =
                        completion::get_metadata(&metadata_map, source.span(), expr, byte_index);
                    let extract = (completion::TypeAt { env: &env }, completion::SpanAt);
                    Ok(
                        completion::completion(extract, source.span(), expr, byte_index)
                            .map(|(typ, span)| {
                                let contents = match opt_metadata.and_then(|m| m.comment.as_ref()) {
                                    Some(comment) => format!("{}\n\n{}", typ, comment.content),
                                    None => format!("{}", typ),
                                };
                                Some(Hover {
                                    contents: HoverContents::Scalar(MarkedString::String(contents)),
                                    range: byte_span_to_range(&source, span).ok(),
                                })
                            })
                            .unwrap_or_else(|()| None),
                    )
                },
            )
            .await
        }
        .boxed()
    }

    fn invalid_params(&self) -> Option<Self::Error> {
        None
    }
}

pub fn register(io: &mut IoHandler, thread: &RootedThread) {
    io.add_async_method(request!("textDocument/hover"), HoverCommand(thread.clone()));
}
