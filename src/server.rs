use std::{
    io::BufReader,
    sync::{Mutex, RwLock},
};

use gluon::{import::Import, RootedThread};

use futures::{
    future::{self, Either},
    prelude::*,
    sync::{mpsc, oneshot},
};

use tokio_codec::{Framed, FramedParts};

use jsonrpc_core::{IoHandler, MetaIoHandler};

use languageserver_types::*;

use {
    cancelable,
    check_importer::CheckImporter,
    rpc::{self, *},
};

pub trait Handler {
    fn add_async_method<T, U>(&mut self, _: Option<T>, method: U)
    where
        T: ::languageserver_types::request::Request,
        U: LanguageServerCommand<T::Params, Output = T::Result>,
        <U::Future as IntoFuture>::Future: Send + 'static,
        T::Params: serde::de::DeserializeOwned + 'static,
        T::Result: serde::Serialize;
    fn add_notification<T, U>(&mut self, _: Option<T>, notification: U)
    where
        T: ::languageserver_types::notification::Notification,
        T::Params: serde::de::DeserializeOwned + 'static,
        U: LanguageServerNotification<T::Params>;
}

impl Handler for IoHandler {
    fn add_async_method<T, U>(&mut self, _: Option<T>, method: U)
    where
        T: ::languageserver_types::request::Request,
        U: LanguageServerCommand<T::Params, Output = T::Result>,
        <U::Future as IntoFuture>::Future: Send + 'static,
        T::Params: serde::de::DeserializeOwned + 'static,
        T::Result: serde::Serialize,
    {
        MetaIoHandler::add_method(self, T::METHOD, ServerCommand::method(method))
    }
    fn add_notification<T, U>(&mut self, _: Option<T>, notification: U)
    where
        T: ::languageserver_types::notification::Notification,
        T::Params: serde::de::DeserializeOwned + 'static,
        U: LanguageServerNotification<T::Params>,
    {
        MetaIoHandler::add_notification(self, T::METHOD, ServerCommand::notification(notification))
    }
}

macro_rules! request {
    ($t:tt) => {
        ::std::option::Option::None::<lsp_request!($t)>
    };
}
macro_rules! notification {
    ($t:tt) => {
        ::std::option::Option::None::<lsp_notification!($t)>
    };
}

pub type ShutdownReceiver = future::Shared<oneshot::Receiver<()>>;

pub struct Server {
    handlers: IoHandler,
    shutdown: ShutdownReceiver,
    message_receiver: mpsc::Receiver<String>,
    message_sender: mpsc::Sender<String>,
}

impl Server {
    pub fn start<R, W>(
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

        let Server {
            handlers,
            shutdown,
            message_receiver,
            message_sender,
        } = Server::initialize(&thread);

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

    fn initialize(thread: &RootedThread) -> Server {
        let (message_log, message_log_receiver) = mpsc::channel(1);

        let (exit_sender, exit_receiver) = oneshot::channel();
        let exit_receiver = exit_receiver.shared();

        let mut io = IoHandler::new();

        ::diagnostics::register(&mut io, thread, &message_log, exit_receiver.clone());

        ::command::initialize::register(&mut io, thread);
        ::command::completion::register(&mut io, thread, &message_log);
        ::command::hover::register(&mut io, thread);
        ::command::signature_help::register(&mut io, thread);
        ::command::symbol::register(&mut io, thread);
        ::command::document_highlight::register(&mut io, thread);
        ::command::document_symbols::register(&mut io, thread);
        ::command::formatting::register(&mut io, thread);

        io.add_async_method(request!("shutdown"), |_| Ok::<(), ServerError<()>>(()));

        let exit_sender = Mutex::new(Some(exit_sender));
        io.add_notification(notification!("exit"), move |_| {
            if let Some(exit_sender) = exit_sender.lock().unwrap().take() {
                exit_sender.send(()).unwrap()
            }
        });

        Server {
            handlers: io,
            shutdown: exit_receiver,
            message_receiver: message_log_receiver,
            message_sender: message_log,
        }
    }
}
