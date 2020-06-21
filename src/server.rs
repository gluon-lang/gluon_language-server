use std::sync::{Mutex, RwLock};

use {
    anyhow::anyhow,
    futures::{
        channel::{mpsc, oneshot},
        compat::*,
        prelude::*,
    },
    jsonrpc_core::{IoHandler, MetaIoHandler},
    tokio_util::codec::{FramedRead, FramedWrite},
};

use gluon::{import::Import, RootedThread};

use crate::{
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
        R: tokio::io::AsyncRead,
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

        let message_receiver_task = tokio::spawn(
            message_receiver
                .map(Ok)
                .forward(FramedWrite::new(output, LanguageServerEncoder))
                .map(|result| {
                    if let Err(err) = result {
                        error!("{}", err);
                    }
                }),
        );

        let handlers = &handlers;
        FramedRead::new(input, rpc::LanguageServerDecoder::new())
            .take_until(shutdown)
            .try_for_each(move |json| {
                let mut message_sender = message_sender.clone();
                async move {
                    debug!("Handle: {}", json);
                    let result = handlers.handle_request(&json).compat().await;
                    match result {
                        Ok(Some(response)) => {
                            debug!("Response: {}", response);
                            message_sender
                                .send(response)
                                .await
                                .map_err(|_| anyhow!("Unable to send"))?;
                        }
                        Ok(None) => (),
                        Err(()) => (),
                    }
                    Ok(())
                }
            })
            .await?;

        message_receiver_task.await?;

        info!("Server shutdown");

        Ok(())
    }

    fn initialize(thread: &RootedThread) -> Server {
        use crate::command;

        let (message_log, message_log_receiver) = mpsc::channel(1);

        let (exit_sender, exit_receiver) = oneshot::channel();
        let exit_receiver = exit_receiver.map(|_| ()).boxed().shared();

        let mut io = IoHandler::new();

        crate::diagnostics::register(&mut io, thread, &message_log, exit_receiver.clone());

        command::initialize::register(&mut io, thread);
        command::completion::register(&mut io, thread, &message_log);
        command::hover::register(&mut io, thread);
        command::signature_help::register(&mut io, thread);
        command::symbol::register(&mut io, thread);
        command::document_highlight::register(&mut io, thread);
        command::document_symbols::register(&mut io, thread);
        command::formatting::register(&mut io, thread);
        command::definition::register(&mut io, thread);

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
