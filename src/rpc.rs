use std::{
    collections::VecDeque,
    fmt,
    io::{self, Write},
    marker::PhantomData,
    marker::Unpin,
    pin::Pin,
    str,
    task::{self, Poll},
};

use anyhow::anyhow;

use combine::{
    error::ParseError,
    from_str,
    parser::{
        combinator::{any_send_partial_state, AnySendPartialState},
        range::{range, take, take_while1},
    },
    skip_many,
    stream::{easy, PartialStream, RangeStream},
    Parser,
};

use bytes::{
    buf::{ext::BufMutExt, Buf},
    BytesMut,
};

use tokio_util::codec::{Decoder, Encoder};

use futures::{channel::mpsc, prelude::*, Sink, Stream};

use jsonrpc_core::{Error, ErrorCode, Params, RpcMethodSimple, RpcNotificationSimple, Value};

use languageserver_types::{notification, LogMessageParams, MessageType};

use serde;
use serde_json::{self, from_value, to_string, to_value};

use crate::BoxFuture;

#[derive(Debug, PartialEq)]
pub struct ServerError<E> {
    pub message: String,
    pub data: Option<E>,
}

impl<E, D> From<E> for ServerError<D>
where
    E: fmt::Display,
{
    fn from(err: E) -> ServerError<D> {
        ServerError {
            message: err.to_string(),
            data: None,
        }
    }
}

pub trait LanguageServerCommand<P>: Send + Sync + 'static
where
    Self::Future: Send + 'static,
{
    type Future: Future<Output = Result<Self::Output, ServerError<Self::Error>>> + Send + 'static;
    type Output: serde::Serialize;
    type Error: serde::Serialize;
    fn execute(&self, param: P) -> Self::Future;

    fn invalid_params(&self) -> Option<Self::Error> {
        None
    }
}

impl<'de, F, R, P, O, E> LanguageServerCommand<P> for F
where
    F: Fn(P) -> R + Send + Sync + 'static,
    R: Future<Output = Result<O, ServerError<E>>> + Send + 'static,
    P: serde::Deserialize<'de>,
    O: serde::Serialize,
    E: serde::Serialize,
{
    type Future = F::Output;
    type Output = O;
    type Error = E;

    fn execute(&self, param: P) -> Self::Future {
        self(param)
    }
}

pub trait LanguageServerNotification<P>: Send + Sync + 'static {
    fn execute(&self, param: P);
}

impl<'de, F, P> LanguageServerNotification<P> for F
where
    F: Fn(P) + Send + Sync + 'static,
    P: serde::Deserialize<'de> + 'static,
{
    fn execute(&self, param: P) {
        self(param)
    }
}
pub struct ServerCommand<T, P>(pub T, PhantomData<fn(P)>);

impl<T, P> ServerCommand<T, P> {
    pub fn method(command: T) -> ServerCommand<T, P>
    where
        T: LanguageServerCommand<P>,
        P: for<'de> serde::Deserialize<'de> + 'static,
    {
        ServerCommand(command, PhantomData)
    }

    pub fn notification(command: T) -> ServerCommand<T, P>
    where
        T: LanguageServerNotification<P>,
        P: for<'de> serde::Deserialize<'de> + 'static,
    {
        ServerCommand(command, PhantomData)
    }
}

impl<P, T> RpcMethodSimple for ServerCommand<T, P>
where
    T: LanguageServerCommand<P>,
    P: for<'de> serde::Deserialize<'de> + 'static,
{
    type Out = futures::compat::Compat<BoxFuture<Value, Error>>;
    fn call(&self, param: Params) -> Self::Out {
        let value = match param {
            Params::Map(map) => Value::Object(map),
            Params::Array(arr) => Value::Array(arr),
            Params::None => Value::Null,
        };
        let err = match from_value(value) {
            Ok(value) => {
                return self
                    .0
                    .execute(value)
                    .map(|result| match result {
                        Ok(value) => {
                            Ok(to_value(&value).expect("result data could not be serialized"))
                        }
                        Err(error) => Err(Error {
                            code: ErrorCode::InternalError,
                            message: error.message,
                            data: error
                                .data
                                .as_ref()
                                .map(|v| to_value(v).expect("error data could not be serialized")),
                        }),
                    })
                    .boxed()
                    .compat()
            }
            Err(err) => err,
        };
        let data = self.0.invalid_params();
        futures::future::err(Error {
            code: ErrorCode::InvalidParams,
            message: format!("Invalid params: {}", err),
            data: data
                .as_ref()
                .map(|v| to_value(v).expect("error data could not be serialized")),
        })
        .boxed()
        .compat()
    }
}

impl<T, P> RpcNotificationSimple for ServerCommand<T, P>
where
    T: LanguageServerNotification<P>,
    P: for<'de> serde::Deserialize<'de> + 'static,
{
    fn execute(&self, param: Params) {
        match param {
            Params::Map(map) => match from_value(Value::Object(map)) {
                Ok(value) => {
                    self.0.execute(value);
                }
                Err(err) => error!("{}", err), // FIXME log_message!("Invalid parameters. Reason: {}", err),
            },
            _ => (), // FIXME log_message!("Invalid parameters: {:?}", param),
        }
    }
}

pub(crate) async fn log_message(sender: mpsc::Sender<String>, message: String) {
    debug!("{}", message);
    send_response(
        sender,
        notification!("window/logMessage"),
        LogMessageParams {
            typ: MessageType::Log,
            message,
        },
    )
    .await
}

macro_rules! log_message {
    ($sender: expr, $($ts: tt)+) => { async {
        if log_enabled!(::log::Level::Debug) {
            let msg = format!( $($ts)+ );
            crate::rpc::log_message($sender, msg).await
        }
    } }
}

pub async fn send_response<T>(mut sender: mpsc::Sender<String>, _: Option<T>, value: T::Params)
where
    T: notification::Notification,
    T::Params: serde::Serialize,
{
    let r = format!(
        r#"{{"jsonrpc": "2.0", "method": "{}", "params": {} }}"#,
        T::METHOD,
        serde_json::to_value(value).unwrap()
    );
    let _ = sender.send(r).await;
}

pub fn write_message<W, T>(output: W, value: &T) -> io::Result<()>
where
    W: Write,
    T: serde::Serialize,
{
    let response = to_string(&value).unwrap();
    write_message_str(output, &response)
}

pub fn write_message_str<W>(mut output: W, response: &str) -> io::Result<()>
where
    W: Write,
{
    debug!("Respond: {}", response);
    write!(
        output,
        "Content-Length: {}\r\n\r\n{}",
        response.len(),
        response
    )?;
    output.flush()?;
    Ok(())
}

pub struct LanguageServerDecoder {
    state: AnySendPartialState,
}

impl LanguageServerDecoder {
    pub fn new() -> LanguageServerDecoder {
        LanguageServerDecoder {
            state: Default::default(),
        }
    }
}

/// Parses blocks of data with length headers
///
/// ```ignore
/// Content-Length: 18
///
/// { "some": "data" }
/// ```
fn decode_parser<'a, I>(
) -> impl Parser<I, Output = Vec<u8>, PartialState = AnySendPartialState> + 'a
where
    I: RangeStream<Token = u8, Range = &'a [u8]> + 'a,
    // Necessary due to rust-lang/rust#24159
    I::Error: ParseError<I::Token, I::Range, I::Position>,
{
    let content_length =
        range(&b"Content-Length: "[..]).with(from_str(take_while1(|b: u8| b.is_ascii_digit())));

    any_send_partial_state(
        (
            skip_many(range(&b"\r\n"[..])),
            content_length,
            range(&b"\r\n\r\n"[..]).map(|_| ()),
        )
            .then_partial(|&mut (_, message_length, _)| {
                take(message_length).map(|bytes: &[u8]| bytes.to_owned())
            }),
    )
}

impl Decoder for LanguageServerDecoder {
    type Item = String;
    type Error = anyhow::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        let (opt, removed_len) = combine::stream::decode(
            decode_parser(),
            &mut easy::Stream(PartialStream(&src[..])),
            &mut self.state,
        )
        .map_err(|err| {
            let err = err
                .map_range(|r| {
                    str::from_utf8(r)
                        .ok()
                        .map_or_else(|| format!("{:?}", r), |s| s.to_string())
                })
                .map_position(|p| p.translate_position(&src[..]));
            anyhow!("{}\nIn input: `{}`", err, str::from_utf8(src).unwrap())
        })?;

        src.advance(removed_len);

        match opt {
            None => Ok(None),

            Some(output) => {
                let value = String::from_utf8(output)?;
                Ok(Some(value))
            }
        }
    }
}

#[derive(Debug)]
pub struct LanguageServerEncoder;

impl Encoder<String> for LanguageServerEncoder {
    type Error = anyhow::Error;
    fn encode(&mut self, item: String, dst: &mut BytesMut) -> Result<(), Self::Error> {
        dst.reserve(item.len() + 60); // Ensure Content-Length fits
        write_message_str(dst.writer(), &item)?;
        Ok(())
    }
}

pub struct Entry<K, V, W> {
    pub key: K,
    pub value: V,
    pub version: W,
}

/// Queue which only keeps the latest work item for each key
pub struct UniqueSink<K, V, W> {
    sender: mpsc::UnboundedSender<Entry<K, V, W>>,
}

impl<K, V, W> Clone for UniqueSink<K, V, W> {
    fn clone(&self) -> Self {
        UniqueSink {
            sender: self.sender.clone(),
        }
    }
}

pub struct UniqueStream<K, V, W> {
    queue: VecDeque<Entry<K, V, W>>,
    receiver: mpsc::UnboundedReceiver<Entry<K, V, W>>,
    exhausted: bool,
}

pub fn unique_queue<K, V, W>() -> (UniqueSink<K, V, W>, UniqueStream<K, V, W>)
where
    K: PartialEq,
    W: Ord,
{
    let (sender, receiver) = mpsc::unbounded();
    (
        UniqueSink { sender },
        UniqueStream {
            queue: VecDeque::new(),
            receiver,
            exhausted: false,
        },
    )
}

impl<K, V, W> Stream for UniqueStream<K, V, W>
where
    K: PartialEq,
    W: Ord,
    Self: Unpin,
{
    type Item = Entry<K, V, W>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Option<Self::Item>> {
        while !self.exhausted {
            match self.receiver.poll_next_unpin(cx) {
                Poll::Ready(Some(item)) => {
                    if let Some(entry) = self.queue.iter_mut().find(|entry| entry.key == item.key) {
                        if entry.version < item.version {
                            *entry = item;
                        }
                        continue;
                    }
                    self.queue.push_back(item);
                }
                Poll::Ready(None) => {
                    self.exhausted = true;
                }
                Poll::Pending => break,
            }
        }
        match self.queue.pop_front() {
            Some(item) => Poll::Ready(Some(item)),
            None => {
                if self.exhausted {
                    Poll::Ready(None)
                } else {
                    Poll::Pending
                }
            }
        }
    }
}

impl<K, V, W> Sink<Entry<K, V, W>> for UniqueSink<K, V, W> {
    type Error = mpsc::SendError;

    fn poll_ready(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.sender).poll_ready(cx)
    }

    fn start_send(mut self: Pin<&mut Self>, item: Entry<K, V, W>) -> Result<(), Self::Error> {
        Pin::new(&mut self.sender).start_send(item)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.sender).poll_flush(cx)
    }

    fn poll_close(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.sender).poll_close(cx)
    }
}
