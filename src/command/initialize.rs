use jsonrpc_core::IoHandler;

use lsp_types::{
    CompletionOptions, InitializeError, InitializeParams, InitializeResult, ServerCapabilities,
    ServerInfo, SignatureHelpOptions, TextDocumentSyncCapability, TextDocumentSyncKind,
    WorkDoneProgressOptions,
};

use crate::{rpc::LanguageServerCommand, BoxFuture};

use super::*;

struct Initialize(RootedThread);
impl LanguageServerCommand<InitializeParams> for Initialize {
    type Future = BoxFuture<Self::Output, ServerError<Self::Error>>;
    type Output = InitializeResult;
    type Error = InitializeError;
    fn execute(
        &self,
        change: InitializeParams,
    ) -> BoxFuture<InitializeResult, ServerError<InitializeError>> {
        let thread = self.0.clone();
        async move {
            let import = thread.get_macros().get("import").expect("Import macro");
            let import = import
                .downcast_ref::<Import<CheckImporter>>()
                .expect("Check importer");
            if let Some(ref uri) = change.root_uri {
                import.add_path(
                    uri.to_file_path()
                        .map_err(|()| "Unable to convert root_uri to file path")?,
                );
            }

            Ok(InitializeResult {
                server_info: Some(ServerInfo {
                    name: "Gluon language server".into(),
                    version: Some(match option_env!("GIT_COMMIT") {
                        Some(git_commit) => format!("{}-{}", env!("CARGO_PKG_VERSION"), git_commit),
                        None => env!("CARGO_PKG_VERSION").into(),
                    }),
                }),
                capabilities: ServerCapabilities {
                    text_document_sync: Some(TextDocumentSyncCapability::Kind(
                        TextDocumentSyncKind::Incremental,
                    )),
                    completion_provider: Some(CompletionOptions {
                        resolve_provider: Some(true),
                        trigger_characters: Some(vec![".".into()]),
                        work_done_progress_options: WorkDoneProgressOptions {
                            work_done_progress: None,
                        },
                        all_commit_characters: None,
                    }),
                    signature_help_provider: Some(SignatureHelpOptions {
                        trigger_characters: None,
                        retrigger_characters: None,
                        work_done_progress_options: WorkDoneProgressOptions {
                            work_done_progress: None,
                        },
                    }),
                    hover_provider: Some(true.into()),
                    document_formatting_provider: Some(lsp_types::OneOf::Left(true)),
                    document_highlight_provider: Some(lsp_types::OneOf::Left(true)),
                    document_symbol_provider: Some(lsp_types::OneOf::Left(true)),
                    workspace_symbol_provider: Some(lsp_types::OneOf::Left(true)),
                    definition_provider: Some(lsp_types::OneOf::Left(true)),
                    ..ServerCapabilities::default()
                },
            })
        }
        .boxed()
    }

    fn invalid_params(&self) -> Option<Self::Error> {
        Some(InitializeError { retry: false })
    }
}

pub fn register(io: &mut IoHandler, thread: &RootedThread) {
    io.add_async_method(request!("initialize"), Initialize(thread.clone()));
}
