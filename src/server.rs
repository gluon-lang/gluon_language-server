use std::sync::{Mutex, RwLock};

use gluon::{import::Import, RootedThread};

use tokio::io::BufReader;

use tokio_util::codec::{FramedRead, FramedWrite};

use futures::{
    channel::{mpsc, oneshot},
    compat::*,
    prelude::*,
};

use jsonrpc_core::{IoHandler, MetaIoHandler};

use serde;

use anyhow::anyhow;

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

pub type ShutdownReceiver = future::Shared<futures::future::BoxFuture<'static, ()>>;

pub struct Server {
    handlers: IoHandler,
    shutdown: ShutdownReceiver,
    message_receiver: mpsc::Receiver<String>,
    message_sender: mpsc::Sender<String>,
}

impl Server {
    pub async fn start<R, W>(thread: RootedThread, input: R, output: W) -> Result<(), anyhow::Error>
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

                std::mem::swap(
                    check_import.compiler.get_mut().unwrap(),
                    &mut import.compiler.lock().unwrap(),
                );
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
                let mut message_sender = message_sender.clone();
                async move {
                    debug!("Handle: {}", json);
                    handlers
                        .handle_request(&json)
                        .compat()
                        .then(move |result| async move {
                            if let Ok(Some(response)) = result {
                                debug!("Response: {}", response);
                                message_sender
                                    .send(response)
                                    .await
                                    .map(|_| ())
                                    .map_err(|_| anyhow!("Unable to send"))
                            } else {
                                Ok(())
                            }
                        })
                        .await
                }
            });

        tokio::spawn(
            message_receiver
                .map(Ok)
                .map_err(|()| unreachable!())
                .forward(FramedWrite::new(output, LanguageServerEncoder))
                .map(|result| {
                    if let Err(err) = result {
                        error!("{}", err);
                    }
                }),
        );

        cancelable(
            shutdown,
            request_handler_future.unwrap_or_else(|t: anyhow::Error| panic!("{}", t)),
        )
        .await;
        info!("Server shutdown");

        Ok(())
    }

    fn initialize(thread: &RootedThread) -> Server {
        let (message_log, message_log_receiver) = mpsc::channel(1);

        let (exit_sender, exit_receiver) = oneshot::channel();
        let exit_receiver = exit_receiver.map(|_| ()).boxed().shared();

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

        io.add_async_method(request!("shutdown"), |_| async {
            Ok::<(), ServerError<()>>(())
        });

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
