use lsp_types::{DocumentFormattingParams, TextEdit};

use gluon::{base::source::Source, ThreadExt};

use super::{byte_span_to_range, retrieve_expr, Handler, IoHandler, RootedThread};

pub fn register(io: &mut IoHandler, thread: &RootedThread) {
    let thread = thread.clone();
    let format = move |params: DocumentFormattingParams| {
        let thread = thread.clone();
        async move {
            retrieve_expr(&thread, &params.text_document.uri, |module| {
                let formatted = thread.format_expr(
                    &mut gluon_format::Formatter::default(),
                    &module.source.name().to_string(),
                    module.source.src(),
                )?;
                let range = byte_span_to_range(&module.source, module.source.span())?;
                Ok(Some(vec![TextEdit {
                    range,
                    new_text: formatted,
                }]))
            })
            .await
        }
    };
    io.add_async_method(request!("textDocument/formatting"), format);
}
