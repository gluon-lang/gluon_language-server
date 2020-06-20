#[macro_use]
extern crate serde_derive;

#[macro_use]
extern crate log;

#[macro_use]
extern crate languageserver_types;

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

use gluon::{either, new_vm};

use futures::prelude::*;

pub use crate::{command::completion::CompletionData, server::Server};

pub type BoxFuture<I, E> = std::pin::Pin<Box<dyn Future<Output = Result<I, E>> + Send + 'static>>;

pub async fn run() {
    ::env_logger::init();

    let _matches = clap::App::new("debugger")
        .version(env!("CARGO_PKG_VERSION"))
        .get_matches();

    let thread = new_vm();

    if let Err(err) = tokio::spawn(async {
        Server::start(thread, tokio::io::stdin(), tokio::io::stdout()).await
    })
    .await
    .unwrap()
    {
        panic!("{}", err)
    }
}

async fn cancelable<F, G>(f: F, g: G)
where
    F: Future<Output = ()>,
    G: Future<Output = ()>,
{
    futures::pin_mut!(f);
    futures::pin_mut!(g);
    futures::future::select(f, g).await;
}
