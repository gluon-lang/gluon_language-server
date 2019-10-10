use languageserver_types::{DocumentFormattingParams, TextEdit};

use gluon_format;

use gluon::ThreadExt;

use super::{byte_span_to_range, retrieve_expr, Handler, IoHandler, RootedThread};

pub fn register(io: &mut IoHandler, thread: &RootedThread) {
    let thread = thread.clone();
    let format = move |params: DocumentFormattingParams| {
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
    };
    io.add_async_method(request!("textDocument/formatting"), format);
}
