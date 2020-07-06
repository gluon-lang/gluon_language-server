#[macro_use]
extern crate serde_derive;

#[macro_use]
extern crate log;

extern crate gluon_completion as completion;

#[macro_use]
mod server;
#[macro_use]
pub mod rpc;

mod check_importer;
mod command;
mod diagnostics;
mod name;
mod text_edit;

use gluon::either;

use futures::prelude::*;

pub use crate::{command::completion::CompletionData, server::Server};

pub type BoxFuture<I, E> = std::pin::Pin<Box<dyn Future<Output = Result<I, E>> + Send + 'static>>;

pub async fn run() -> Result<(), anyhow::Error> {
    ::env_logger::init();

    let _matches = clap::App::new("debugger")
        .version(env!("CARGO_PKG_VERSION"))
        .get_matches();

    let thread = gluon::new_vm_async().await;
    Server::start(thread, tokio::io::stdin(), tokio::io::stdout()).await?;
    Ok(())
}

async fn cancelable<T, F, G>(f: F, g: G) -> T
where
    F: Future<Output = T>,
    G: Future<Output = T>,
{
    futures::pin_mut!(f);
    futures::pin_mut!(g);
    futures::future::select(f, g).await.factor_first().0
}

use gluon::base::{
    pos::{ByteIndex, ByteOffset, Span},
    source::FileMap,
};

fn position_to_byte_index(
    files: &FileMap,
    position: &lsp_types::Position,
) -> Result<ByteIndex, codespan_lsp::Error> {
    let index = codespan_lsp::position_to_byte_index(files, (), position)?;

    Ok(files.span().start() + ByteOffset::from(index as i64))
}

fn byte_span_to_range(
    files: &FileMap,
    span: Span<ByteIndex>,
) -> Result<lsp_types::Range, codespan_lsp::Error> {
    let start = files.span().start().to_usize();
    let range = span.start().to_usize() - start..span.end().to_usize() - start;
    codespan_lsp::byte_span_to_range(files, (), range)
}
