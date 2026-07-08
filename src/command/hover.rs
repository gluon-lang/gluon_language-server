use std::fmt::Write;

use completion::Extract;

use {
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

                    let db = thread.get_database();
                    let env = db.as_env();
                    let (_, metadata_map) = gluon::check::metadata::metadata(&env, &expr);
                    let opt_metadata =
                        completion::get_metadata(&metadata_map, source.span(), expr, byte_index);
                    let extract = (completion::TypeAt { env: &env }, completion::SpanAt);

                    let found = completion::complete(source.span(), expr, byte_index).ok();
                    Ok(found.and_then(|found| {
                        let (typ, span) = extract.extract(&found).ok()?;
                        let ident = completion::IdentAt.extract(&found).ok();

                        let contents = match opt_metadata.and_then(|m| m.comment.as_ref()) {
                            Some(comment) => {
                                let mut contents = String::new();
                                write!(contents, "```gluon\n").unwrap();
                                if let Some(ident) = ident {
                                    write!(contents, "{}: ", ident.declared_name()).unwrap();
                                }
                                write!(contents, "{}\n```\n{}", typ, comment.content).unwrap();
                                HoverContents::Markup(MarkupContent {
                                    kind: MarkupKind::Markdown,
                                    value: contents,
                                })
                            }
                            None => HoverContents::Scalar(MarkedString::from_language_code(
                                "gluon".into(),
                                format!("{}", typ),
                            )),
                        };
                        Some(Hover {
                            contents,
                            range: byte_span_to_range(&source, span).ok(),
                        })
                    }))
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
