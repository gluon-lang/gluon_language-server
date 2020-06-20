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

use futures_01::prelude::*;

pub use crate::{command::completion::CompletionData, server::Server};

pub type BoxFuture<I, E> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<I, E>> + Send + 'static>>;

pub fn run() {
    ::env_logger::init();

    let _matches = clap::App::new("debugger")
        .version(env!("CARGO_PKG_VERSION"))
        .get_matches();

    let thread = new_vm();

    tokio_compat::run_std(async {
        if let Err(err) = tokio_02::spawn(async {
            Server::start(thread, tokio_02::io::stdin(), tokio_02::io::stdout()).await
        })
        .await
        .unwrap()
        {
            panic!("{}", err)
        }
    })
}

fn cancelable<F, G>(f: F, g: G) -> impl Future<Item = (), Error = G::Error>
where
    F: IntoFuture,
    G: IntoFuture<Item = ()>,
{
    f.into_future()
        .then(|_| Ok(()))
        .select(g)
        .map(|_| ())
        .map_err(|err| err.0)
}
