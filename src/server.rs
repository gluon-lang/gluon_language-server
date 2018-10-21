use std::sync::Mutex;

use gluon::RootedThread;

use futures::{
    future,
    prelude::*,
    sync::{mpsc, oneshot},
};

use jsonrpc_core::{IoHandler, MetaIoHandler};

use languageserver_types::*;

use rpc::*;

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

pub(crate) struct Server {
    pub handlers: IoHandler,
    pub shutdown: ShutdownReceiver,
    pub message_receiver: mpsc::Receiver<String>,
    pub message_sender: mpsc::Sender<String>,
}

impl Server {
    pub(crate) fn new(thread: &RootedThread) -> Server {
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
