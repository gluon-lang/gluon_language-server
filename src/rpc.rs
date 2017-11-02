extern crate combine;

use std::collections::VecDeque;
use std::error::Error as StdError;
use std::fmt;
use std::io::{self, BufRead, Read, Write};
use std::marker::PhantomData;
use std::sync::{Arc, Mutex};

use jsonrpc_core::{Error, ErrorCode, Params, RpcMethodSimple, RpcNotificationSimple, Value};
use futures::{self, Async, AsyncSink, Future, IntoFuture, Poll, Sink, StartSend};

use serde;
use serde_json::{from_value, to_string, to_value};

use BoxFuture;

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

pub trait LanguageServerCommand<P>: Send + Sync + 'static {
    type Output: serde::Serialize;
    type Error: serde::Serialize;
    fn execute(&self, param: P) -> BoxFuture<Self::Output, ServerError<Self::Error>>;

    fn invalid_params(&self) -> Option<Self::Error> {
        None
    }
}

impl<'de, F, P, O, E> LanguageServerCommand<P> for F
where
    F: Fn(P) -> BoxFuture<O, ServerError<E>> + Send + Sync + 'static,
    P: serde::Deserialize<'de>,
    O: serde::Serialize,
    E: serde::Serialize,
{
    type Output = O;
    type Error = E;

    fn execute(&self, param: P) -> BoxFuture<Self::Output, ServerError<Self::Error>> {
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
    fn call(&self, param: Params) -> BoxFuture<Value, Error> {
        let value = match param {
            Params::Map(map) => Value::Object(map),
            Params::Array(arr) => Value::Array(arr),
            Params::None => Value::Null,
        };
        let err = match from_value(value.clone()) {
            Ok(value) => {
                return Box::new(self.0.execute(value).then(|result| {
                    match result {
                        Ok(value) => Ok(
                            to_value(&value).expect("result data could not be serialized"),
                        ).into_future(),
                        Err(error) => Err(Error {
                            code: ErrorCode::InternalError,
                            message: error.message,
                            data: error
                                .data
                                .as_ref()
                                .map(|v| to_value(v).expect("error data could not be serialized")),
                        }).into_future(),
                    }
                }))
            }
            Err(err) => err,
        };
        let data = self.0.invalid_params();
        Box::new(futures::failed(Error {
            code: ErrorCode::InvalidParams,
            message: format!("Invalid params: {}", err),
            data: data.as_ref()
                .map(|v| to_value(v).expect("error data could not be serialized")),
        }))
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
                Err(err) => log_message!("Invalid parameters. Reason: {}", err),
            },
            _ => log_message!("Invalid parameters: {:?}", param),
        }
    }
}


pub fn read_message<R>(mut reader: R) -> Result<Option<String>, Box<StdError>>
where
    R: BufRead + Read,
{
    let mut header = String::new();
    let n = try!(reader.read_line(&mut header));
    if n == 0 {
        return Ok(None);
    }

    if header.starts_with("Content-Length: ") {
        let content_length = {
            let len = header["Content-Length:".len()..].trim();
            debug!("{}", len);
            try!(len.parse::<usize>())
        };
        while header != "\r\n" {
            header.clear();
            try!(reader.read_line(&mut header));
        }
        let mut content = vec![0; content_length];
        try!(reader.read_exact(&mut content));
        Ok(Some(try!(String::from_utf8(content))))
    } else {
        Err(format!("Invalid message: `{}`", header).into())
    }
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
    try!(write!(
        output,
        "Content-Length: {}\r\n\r\n{}",
        response.len(),
        response
    ));
    try!(output.flush());
    Ok(())
}


extern crate bytes;

use std::str;

use tokio_io::codec::{Decoder, Encoder};
use self::bytes::{BufMut, BytesMut};

#[derive(Debug)]
enum State {
    Nothing,
    ContentLength(usize),
}

#[derive(Debug)]
pub struct LanguageServerDecoder {
    state: State,
}

impl LanguageServerDecoder {
    pub fn new() -> LanguageServerDecoder {
        LanguageServerDecoder {
            state: State::Nothing,
        }
    }
}

use self::combine::range::{range, take};
use self::combine::{skip_many, Parser, many1};
use self::combine::easy::{self, Error as CombineError, Errors};
use self::combine::byte::digit;

fn combine_decode<'a, P, R>(
    mut parser: P,
    src: &'a [u8],
) -> Result<Option<(R, usize)>, Errors<usize, u8, String>>
where
    P: Parser<Input = easy::Stream<&'a [u8]>, Output = R>,
{
    match parser.parse(easy::Stream(&src[..])) {
        Ok((message, rest)) => Ok(Some((message, src.len() - rest.0.len()))),
        Err(err) => {
            return if err.errors
                .iter()
                .any(|err| *err == CombineError::end_of_input())
            {
                Ok(None)
            } else {
                Err(
                    err.map_range(|r| {
                        str::from_utf8(r)
                            .ok()
                            .map_or_else(|| format!("{:?}", r), |s| s.to_string())
                    }).map_position(|p| p.translate_position(&src[..])),
                )
            }
        }
    }
}

macro_rules! decode {
    ($parser: expr, $src: expr) => {
        {
            let (output, removed_len) = {
                match combine_decode($parser, &$src[..])? {
                    None => return Ok(None),
                    Some(x) => x,
                }
            };
            $src.split_to(removed_len);
            output
        }
    };
}

impl Decoder for LanguageServerDecoder {
    type Item = String;
    type Error = Box<::std::error::Error>;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        loop {
            match self.state {
                State::Nothing => {
                    let value = decode!(
                        (
                            skip_many(range(&b"\r\n"[..])),
                            range(&b"Content-Length: "[..]),
                            many1(digit()),
                            range(&b"\r\n\r\n"[..]),
                        ).map(|t| t.2)
                            .and_then(|digits: Vec<u8>| unsafe {
                                String::from_utf8_unchecked(digits).parse::<usize>()
                            }),
                        src
                    );

                    self.state = State::ContentLength(value);
                }
                State::ContentLength(message_length) => {
                    let message = decode!(
                        take(message_length).map(|bytes: &[u8]| bytes.to_owned()),
                        src
                    );
                    self.state = State::Nothing;
                    return Ok(Some(String::from_utf8(message)?));
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct LanguageServerEncoder;

impl Encoder for LanguageServerEncoder {
    type Item = String;
    type Error = Box<::std::error::Error>;
    fn encode(&mut self, item: Self::Item, dst: &mut BytesMut) -> Result<(), Self::Error> {
        write_message_str(dst.writer(), &item)?;
        Ok(())
    }
}

pub struct Entry<K, V> {
    pub key: K,
    pub value: V,
}

#[derive(Debug)]
pub struct SharedSink<S>(Arc<Mutex<S>>);

impl<S> Clone for SharedSink<S> {
    fn clone(&self) -> Self {
        SharedSink(self.0.clone())
    }
}

impl<S> SharedSink<S> {
    pub fn new(sink: S) -> SharedSink<S> {
        SharedSink(Arc::new(Mutex::new(sink)))
    }
}

impl<S> Sink for SharedSink<S>
where
    S: Sink,
{
    type SinkItem = S::SinkItem;
    type SinkError = S::SinkError;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        self.0.lock().unwrap().start_send(item)
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        self.0.lock().unwrap().poll_complete()
    }

    fn close(&mut self) -> Poll<(), Self::SinkError> {
        self.0.lock().unwrap().close()
    }
}

/// Queue which only keeps the latest work item for each key
pub struct UniqueQueue<S, K, V> {
    sink: S,
    queue: VecDeque<Entry<K, V>>,
}

impl<S, K, V> UniqueQueue<S, K, V> {
    pub fn new(sink: S) -> Self {
        UniqueQueue {
            sink,
            queue: VecDeque::new(),
        }
    }
}

impl<S, K, V> Sink for UniqueQueue<S, K, V>
where
    S: Sink<SinkItem = Entry<K, V>>,
    K: PartialEq,
{
    type SinkItem = Entry<K, V>;
    type SinkError = S::SinkError;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        match self.sink.start_send(item)? {
            AsyncSink::Ready => Ok(AsyncSink::Ready),
            AsyncSink::NotReady(item) => {
                if let Some(entry) = self.queue.iter_mut().find(|entry| entry.key == item.key) {
                    entry.value = item.value;
                }
                Ok(AsyncSink::Ready)
            }
        }
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        while let Some(item) = self.queue.pop_front() {
            match self.sink.start_send(item)? {
                AsyncSink::Ready => (),
                AsyncSink::NotReady(item) => {
                    self.queue.push_front(item);
                    break;
                }
            }
        }
        if self.queue.is_empty() {
            self.sink.poll_complete()
        } else {
            Ok(Async::NotReady)
        }
    }

    fn close(&mut self) -> Poll<(), Self::SinkError> {
        try_ready!(self.poll_complete());
        self.sink.close()
    }
}

pub struct SinkFn<F, I> {
    f: F,
    _marker: PhantomData<fn(I) -> I>,
}

pub fn sink_fn<F, I, E>(f: F) -> SinkFn<F, I>
where
    F: FnMut(I) -> StartSend<I, E>,
{
    SinkFn {
        f,
        _marker: PhantomData,
    }
}

impl<F, I, E> Sink for SinkFn<F, I>
where
    F: FnMut(I) -> StartSend<I, E>,
{
    type SinkItem = I;
    type SinkError = E;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        (self.f)(item)
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        Ok(Async::Ready(()))
    }
}
