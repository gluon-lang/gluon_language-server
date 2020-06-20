use futures_01::sync::mpsc;

use languageserver_types::CompletionItem;

use crate::completion;

use languageserver_types::{CompletionParams, CompletionResponse};

use crate::{check_importer::Module, name::with_import, rpc::LanguageServerCommand, BoxFuture};

use url_serde;

use serde::Deserialize;
use serde_json;

use super::*;

#[derive(Serialize, Deserialize)]
pub struct CompletionData {
    #[serde(with = "url_serde")]
    pub text_document_uri: Url,
    pub position: Position,
}

#[derive(Clone)]
struct Completion(RootedThread);
impl LanguageServerCommand<CompletionParams> for Completion {
    type Future = BoxFuture<Self::Output, ServerError<()>>;
    type Output = Option<CompletionResponse>;
    type Error = ();
    fn execute(&self, change: CompletionParams) -> BoxFuture<Self::Output, ServerError<()>> {
        let thread = self.0.clone();
        let text_document_uri = change.text_document.uri.clone();
        async move {
            retrieve_expr(&thread.clone(), &text_document_uri, |module| {
                let Module {
                    ref expr,
                    ref source,
                    ..
                } = *module;

                let expr = expr.expr();

                let byte_index = position_to_byte_index(&source, &change.position)?;

                let query = completion::SuggestionQuery {
                    modules: with_import(&thread, |import| {
                        import.modules(&mut thread.module_compiler(&mut thread.get_database()))
                    }),
                    ..completion::SuggestionQuery::default()
                };

                let suggestions = query
                    .suggest(&thread.get_env(), source.span(), expr, byte_index)
                    .into_iter()
                    .filter(|suggestion| !suggestion.name.starts_with("__"))
                    .collect::<Vec<_>>();

                let mut items: Vec<_> = suggestions
                    .into_iter()
                    .map(|ident| {
                        // Remove the `:Line x, Row y suffix`
                        let name: &str = ident.name.as_ref();
                        let label =
                            String::from(name.split(':').next().unwrap_or(ident.name.as_ref()));
                        CompletionItem {
                            insert_text: if label.starts_with(char::is_alphabetic) {
                                None
                            } else {
                                Some(format!("({})", label))
                            },
                            kind: Some(ident_to_completion_item_kind(&label, ident.typ.as_ref())),
                            label,
                            detail: match ident.typ {
                                either::Either::Right(ref typ) => match **typ {
                                    Type::Hole => None,
                                    _ => Some(format!("{}", ident.typ)),
                                },
                                either::Either::Left(_) => Some(format!("{}", ident.typ)),
                            },
                            data: Some(
                                serde_json::to_value(CompletionData {
                                    text_document_uri: change.text_document.uri.clone(),
                                    position: change.position,
                                })
                                .expect("CompletionData"),
                            ),
                            ..CompletionItem::default()
                        }
                    })
                    .collect();

                items.sort_by(|l, r| l.label.cmp(&r.label));

                Ok(Some(CompletionResponse::Array(items)))
            })
            .await
        }
        .boxed()
    }

    fn invalid_params(&self) -> Option<Self::Error> {
        None
    }
}

pub fn register(io: &mut IoHandler, thread: &RootedThread, message_log: &mpsc::Sender<String>) {
    io.add_async_method(
        request!("textDocument/completion"),
        Completion(thread.clone()),
    );

    let thread = thread.clone();
    let message_log = message_log.clone();
    let resolve = move |mut item: CompletionItem| {
        let thread = thread.clone();
        let message_log = message_log.clone();
        async move {
            let data: CompletionData =
                CompletionData::deserialize(item.data.as_ref().unwrap()).expect("CompletionData");

            let message_log2 = message_log.clone();
            let thread = thread.clone();
            let label = item.label.clone();
            log_message!(message_log.clone(), "{:?}", data.text_document_uri).await;

            let comment = retrieve_expr_with_pos(
                &thread,
                &data.text_document_uri,
                &data.position,
                |module, byte_index| {
                    let mut db = thread.get_database();
                    let type_env = db.as_env();
                    let module_expr = module.expr.expr();
                    let (_, metadata_map) =
                        gluon::check::metadata::metadata(&type_env, module_expr);
                    Ok(completion::suggest_metadata(
                        &metadata_map,
                        &type_env,
                        module.source.span(),
                        module_expr,
                        byte_index,
                        &label,
                    )
                    .and_then(|metadata| metadata.comment.clone()))
                },
            )
            .await?;

            log_message!(message_log2, "{:?}", comment).await;

            item.documentation = Some(make_documentation(
                None::<&str>,
                comment.as_ref().map_or("", |comment| &comment.content),
            ));
            Ok(item)
        }
    };
    io.add_async_method(request!("completionItem/resolve"), resolve);
}
