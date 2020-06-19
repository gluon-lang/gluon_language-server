use std::sync::{Mutex, RwLock};

use gluon::{import::Import, RootedThread};

use futures_01::{
    future::{self, Either},
    prelude::*,
    sync::{mpsc, oneshot},
};

use tokio_02::io::BufReader;

use tokio_util::codec::{FramedRead, FramedWrite};

use futures::{compat::*, prelude::*};

use jsonrpc_core::{IoHandler, MetaIoHandler};

use serde;

use failure;

use crate::{
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
    pub async fn start<R, W>(
        thread: RootedThread,
        input: R,
        output: W,
    ) -> Result<(), failure::Error>
    where
        R: tokio_02::io::AsyncRead + Send + 'static,
        W: tokio_02::io::AsyncWrite + Send + 'static,
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
        let handlers = &handlers;

        let input = BufReader::new(input);

        let request_handler_future = FramedRead::new(input, rpc::LanguageServerDecoder::new())
            .map_err(|err| panic!("{}", err))
            .try_for_each(move |json| {
                let message_sender = message_sender.clone();
                async move {
                    debug!("Handle: {}", json);
                    handlers
                        .handle_request(&json)
                        .then(move |result| {
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
                        .compat()
                        .await
                }
            });

        tokio_02::spawn(
            message_receiver
                .compat()
                .map_err(|_| failure::err_msg("Unable to log message"))
                .forward(FramedWrite::new(output, LanguageServerEncoder))
                .map(|result| {
                    if let Err(err) = result {
                        error!("{}", err);
                    }
                }),
        );

        cancelable(
            shutdown,
            request_handler_future
                .map_err(|t: failure::Error| panic!("{}", t))
                .boxed()
                .compat(),
        )
        .map(|t| {
            info!("Server shutdown");
            t
        })
        .compat()
        .await
    }

    fn initialize(thread: &RootedThread) -> Server {
        let (message_log, message_log_receiver) = mpsc::channel(1);

        let (exit_sender, exit_receiver) = oneshot::channel();
        let exit_receiver = exit_receiver.shared();

        let mut io = IoHandler::new();

        crate::diagnostics::register(&mut io, thread, &message_log, exit_receiver.clone());

        crate::command::initialize::register(&mut io, thread);
        crate::command::completion::register(&mut io, thread, &message_log);
        crate::command::hover::register(&mut io, thread);
        crate::command::signature_help::register(&mut io, thread);
        crate::command::symbol::register(&mut io, thread);
        crate::command::document_highlight::register(&mut io, thread);
        crate::command::document_symbols::register(&mut io, thread);
        crate::command::formatting::register(&mut io, thread);
        crate::command::definition::register(&mut io, thread);

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
