#![cfg_attr(feature = "serde_macros", feature(custom_derive, plugin))]
#![cfg_attr(feature = "serde_macros", plugin(serde_macros))]

extern crate clap;

extern crate failure;

extern crate serde;
#[macro_use]
extern crate serde_derive;
extern crate serde_json;

extern crate futures;
extern crate jsonrpc_core;
extern crate tokio;
extern crate tokio_codec;
extern crate tokio_io;

extern crate env_logger;

#[macro_use]
extern crate log;
extern crate url;
extern crate url_serde;

extern crate bytes;

extern crate codespan;
extern crate codespan_lsp;
extern crate codespan_reporting;

#[macro_use]
extern crate languageserver_types;

extern crate gluon;
extern crate gluon_completion as completion;
extern crate gluon_format;

macro_rules! log_message {
    ($sender: expr, $($ts: tt)+) => {
        if log_enabled!(::log::Level::Debug) {
            $crate::Either::A(::log_message($sender, format!( $($ts)+ )))
        } else {
            $crate::Either::B(Ok(()).into_future())
        }
    }
}

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

use gluon::{either, import::Import, new_vm, RootedThread};

use std::io::BufReader;
use std::sync::RwLock;

use languageserver_types::*;

use futures::future::Either;
use futures::sync::mpsc;
use futures::{future, Future, IntoFuture, Sink, Stream};

use tokio_codec::{Framed, FramedParts};

use {check_importer::CheckImporter, rpc::*};

pub use command::completion::CompletionData;

pub type BoxFuture<I, E> = Box<Future<Item = I, Error = E> + Send + 'static>;

fn log_message(
    sender: mpsc::Sender<String>,
    message: String,
) -> impl Future<Item = (), Error = ()> {
    debug!("{}", message);
    let r = format!(
        r#"{{"jsonrpc": "2.0", "method": "window/logMessage", "params": {} }}"#,
        serde_json::to_value(&LogMessageParams {
            typ: MessageType::Log,
            message: message,
        })
        .unwrap()
    );
    sender.send(r).map(|_| ()).map_err(|_| ())
}

pub fn run() {
    ::env_logger::init();

    let _matches = clap::App::new("debugger")
        .version(env!("CARGO_PKG_VERSION"))
        .get_matches();

    let thread = new_vm();

    tokio::run(future::lazy(move || {
        start_server(thread, tokio::io::stdin(), tokio::io::stdout())
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

pub fn start_server<R, W>(
    thread: RootedThread,
    input: R,
    mut output: W,
) -> impl Future<Item = (), Error = failure::Error>
where
    R: tokio::io::AsyncRead + Send + 'static,
    W: tokio::io::AsyncWrite + Send + 'static,
{
    let _ = ::env_logger::try_init();

    {
        let macros = thread.get_macros();
        let mut check_import = Import::new(CheckImporter::new());
        {
            let import = macros.get("import").expect("Import macro");
            let import = import.downcast_ref::<Import>().expect("Importer");
            check_import.paths = RwLock::new((*import.paths.read().unwrap()).clone());

            let mut loaders = import.loaders.write().unwrap();
            check_import.loaders = RwLock::new(loaders.drain().collect());
        }
        macros.insert("import".into(), check_import);
    }

    let server::Server {
        handlers,
        shutdown,
        message_receiver,
        message_sender,
    } = server::Server::new(&thread);

    let input = BufReader::new(input);

    let parts = FramedParts::new(input, rpc::LanguageServerDecoder::new());

    let request_handler_future = Framed::from_parts(parts)
        .map_err(|err| panic!("{}", err))
        .for_each(move |json| {
            debug!("Handle: {}", json);
            let message_sender = message_sender.clone();
            handlers.handle_request(&json).then(move |result| {
                if let Ok(Some(response)) = result {
                    debug!("Response: {}", response);
                    Either::A(
                        message_sender
                            .send(response)
                            .map(|_| ())
                            .map_err(|_| failure::err_msg("Unable to send")),
                    )
                } else {
                    Either::B(Ok(()).into_future())
                }
            })
        });

    tokio::spawn(
        message_receiver
            .map_err(|_| failure::err_msg("Unable to log message"))
            .for_each(move |message| -> Result<(), failure::Error> {
                Ok(write_message_str(&mut output, &message)?)
            })
            .map_err(|err| {
                error!("{}", err);
            }),
    );

    cancelable(
        shutdown,
        request_handler_future.map_err(|t: failure::Error| panic!("{}", t)),
    )
    .map(|t| {
        info!("Server shutdown");
        t
    })
}
