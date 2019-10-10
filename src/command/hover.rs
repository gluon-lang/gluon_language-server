use jsonrpc_core::IoHandler;

use crate::completion;

use languageserver_types::{Hover, HoverContents, MarkedString, TextDocumentPositionParams};

use crate::{rpc::LanguageServerCommand, BoxFuture};

use super::*;

struct HoverCommand(RootedThread);
impl LanguageServerCommand<TextDocumentPositionParams> for HoverCommand {
    type Future = BoxFuture<Self::Output, ServerError<()>>;
    type Output = Option<Hover>;
    type Error = ();
    fn execute(
        &self,
        change: TextDocumentPositionParams,
    ) -> BoxFuture<Option<Hover>, ServerError<()>> {
        Box::new(
            (|| -> Result<_, _> {
                let thread = &self.0;
                retrieve_expr(thread, &change.text_document.uri, |module| {
                    let expr = &module.expr;

                    let source = &module.source;
                    let byte_index = position_to_byte_index(&source, &change.position)?;

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
                })
            })()
            .into_future(),
        )
    }

    fn invalid_params(&self) -> Option<Self::Error> {
        None
    }
}

pub fn register(io: &mut IoHandler, thread: &RootedThread) {
    io.add_async_method(request!("textDocument/hover"), HoverCommand(thread.clone()));
}
