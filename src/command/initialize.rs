use jsonrpc_core::IoHandler;

use languageserver_types::{
    CompletionOptions, InitializeError, InitializeParams, InitializeResult, ServerCapabilities,
    SignatureHelpOptions, TextDocumentSyncCapability, TextDocumentSyncKind,
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
        let import = self.0.get_macros().get("import").expect("Import macro");
        let import = import
            .downcast_ref::<Import<CheckImporter>>()
            .expect("Check importer");
        if let Some(ref path) = change.root_path {
            import.add_path(path);
        }
        async move {
            Ok(InitializeResult {
                capabilities: ServerCapabilities {
                    text_document_sync: Some(TextDocumentSyncCapability::Kind(
                        TextDocumentSyncKind::Incremental,
                    )),
                    completion_provider: Some(CompletionOptions {
                        resolve_provider: Some(true),
                        trigger_characters: Some(vec![".".into()]),
                    }),
                    signature_help_provider: Some(SignatureHelpOptions {
                        trigger_characters: None,
                    }),
                    hover_provider: Some(true),
                    document_formatting_provider: Some(true),
                    document_highlight_provider: Some(true),
                    document_symbol_provider: Some(true),
                    workspace_symbol_provider: Some(true),
                    definition_provider: Some(true),
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
