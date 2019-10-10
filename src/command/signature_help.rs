use gluon::RootedThread;

use jsonrpc_core::IoHandler;

use languageserver_types::{
    ParameterInformation, ParameterLabel, SignatureHelp, SignatureInformation,
    TextDocumentPositionParams,
};

use super::*;

use crate::completion;

pub fn register(io: &mut IoHandler, thread: &RootedThread) {
    let thread = thread.clone();

    io.add_async_method(
        request!("textDocument/signatureHelp"),
        move |params: TextDocumentPositionParams| -> Result<_, _> {
            retrieve_expr(&thread, &params.text_document.uri, |module| {
                let expr = &module.expr;

                let source = &module.source;
                let byte_pos = position_to_byte_index(&source, &params.position)?;

                let env = thread.get_env();

                Ok(
                    completion::signature_help(&env, module.source.span(), expr, byte_pos).map(
                        |help| {
                            let (_, metadata_map) = gluon::check::metadata::metadata(&env, expr);
                            let comment = if help.name.is_empty() {
                                None
                            } else {
                                completion::suggest_metadata(
                                    &metadata_map,
                                    &env,
                                    module.source.span(),
                                    expr,
                                    byte_pos,
                                    &help.name,
                                )
                                .and_then(|metadata| metadata.comment.clone())
                            };

                            SignatureHelp {
                                signatures: vec![SignatureInformation {
                                    label: help.name,
                                    documentation: Some(make_documentation(
                                        Some(&help.typ),
                                        &comment.as_ref().map_or("", |c| &c.content),
                                    )),
                                    parameters: Some(
                                        ::gluon::base::types::arg_iter(&help.typ)
                                            .map(|typ| ParameterInformation {
                                                label: ParameterLabel::Simple("".to_string()),
                                                documentation: Some(make_documentation(
                                                    Some(typ),
                                                    "",
                                                )),
                                            })
                                            .collect(),
                                    ),
                                }],
                                active_signature: None,
                                active_parameter: help.index.map(u64::from),
                            }
                        },
                    ),
                )
            })
        },
    );
}
