use clap;

#[macro_use]
extern crate serde_derive;

use futures;

use tokio;

#[macro_use]
extern crate log;

#[macro_use]
extern crate languageserver_types;

use gluon;
extern crate gluon_completion as completion;

macro_rules! box_future {
    ($e:expr) => {{
        let fut: $crate::BoxFuture<_, _> = Box::new($e.into_future());
        fut
    }};
}

macro_rules! try_future {
    ($e:expr) => {
        match $e {
            Ok(x) => x,
            Err(err) => return box_future!(Err(err.into())),
        }
    };
}

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

use futures::{
    future::{self, Either},
    prelude::*,
};

pub use crate::{command::completion::CompletionData, server::Server};

pub type BoxFuture<I, E> = Box<dyn Future<Item = I, Error = E> + Send + 'static>;

pub fn run() {
    ::env_logger::init();

    let _matches = clap::App::new("debugger")
        .version(env!("CARGO_PKG_VERSION"))
        .get_matches();

    let thread = new_vm();

    tokio::run(future::lazy(move || {
        Server::start(thread, tokio::io::stdin(), tokio::io::stdout())
            .map_err(|err| panic!("{}", err))
    }))
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
