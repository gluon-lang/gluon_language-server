use futures::sync::{mpsc, oneshot};

use languageserver_types::CompletionItem;

use completion;

use languageserver_types::{CompletionParams, CompletionResponse};

use {
check_importer::Module,
    name::with_import, rpc::LanguageServerCommand, BoxFuture};

use url_serde;

use serde_json;
use serde::{ Deserialize};

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
        let self_ = self.clone();
        let text_document_uri = change.text_document.uri.clone();
        let result = retrieve_expr_future(&self.0, &text_document_uri, move |module| {
            let Module {
                ref expr,
                ref source,
                dirty,
                ref mut waiters,
                ..
            } = *module;

            if dirty {
                let (sender, receiver) = oneshot::channel();
                waiters.push(sender);
                return box_future!(
                    receiver
                        .map_err(|_| {
                            let msg = "Completion sender was unexpectedly dropped";
                            error!("{}", msg);
                            ServerError::from(msg.to_string())
                        })
                        .and_then(move |_| self_.clone().execute(change))
                );
            }

            let byte_index = try_future!(position_to_byte_index(&source, &change.position));

            let query = completion::SuggestionQuery {
                modules: with_import(&thread, |import| import.modules()),
                ..completion::SuggestionQuery::default()
            };

            let suggestions = query
                .suggest(&*thread.get_env(), source.span(), expr, byte_index)
                .into_iter()
                .filter(|suggestion| !suggestion.name.starts_with("__"))
                .collect::<Vec<_>>();

            let mut items: Vec<_> = suggestions
                .into_iter()
                .map(|ident| {
                    // Remove the `:Line x, Row y suffix`
                    let name: &str = ident.name.as_ref();
                    let label = String::from(name.split(':').next().unwrap_or(ident.name.as_ref()));
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

            Box::new(Ok(Some(CompletionResponse::Array(items))).into_future())
        });
        Box::new(result.into_future())
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
        let data: CompletionData =
            CompletionData::deserialize(item.data.as_ref().unwrap()).expect("CompletionData");

        let message_log2 = message_log.clone();
        let thread = thread.clone();
        let label = item.label.clone();
        log_message!(message_log.clone(), "{:?}", data.text_document_uri)
            .then(move |_| {
                retrieve_expr_with_pos(
                    &thread,
                    &data.text_document_uri,
                    &data.position,
                    |module, byte_index| {
                        let type_env = thread.global_env().get_env();
                        let (_, metadata_map) =
                            gluon::check::metadata::metadata(&*type_env, &module.expr);
                        Ok(completion::suggest_metadata(
                            &metadata_map,
                            &*type_env,
                            module.source.span(),
                            &module.expr,
                            byte_index,
                            &label,
                        )
                        .and_then(|metadata| metadata.comment.clone()))
                    },
                )
            })
            .and_then(move |comment| {
                log_message!(message_log2, "{:?}", comment)
                    .map(move |()| {
                        item.documentation = Some(make_documentation(
                            None::<&str>,
                            comment.as_ref().map_or("", |comment| &comment.content),
                        ));
                        item
                    })
                    .map_err(|_| panic!("Unable to send log message"))
            })
    };
    io.add_async_method(request!("completionItem/resolve"), resolve);
}
