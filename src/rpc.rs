use std::error::Error as StdError;
use std::io::{self, BufRead, Read, Write};
use std::marker::PhantomData;

use jsonrpc_core::{Error, ErrorCode, RpcMethodSimple, Params, Value};
use futures::{self, BoxFuture, Future, IntoFuture};

use serde;
use serde_json::{from_value, to_value, to_string};

pub struct ServerError<E> {
    pub message: String,
    pub data: Option<E>,
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
    where F: Fn(P) -> BoxFuture<O, ServerError<E>> + Send + Sync + 'static,
          P: serde::Deserialize<'de>,
          O: serde::Serialize,
          E: serde::Serialize
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
    where F: Fn(P) + Send + Sync + 'static,
          P: serde::Deserialize<'de> + 'static
{
    fn execute(&self, param: P) {
        self(param)
    }
}
pub struct ServerCommand<T, P>(pub T, PhantomData<fn(P)>);

impl<T, P> ServerCommand<T, P> {
    pub fn new(command: T) -> ServerCommand<T, P> {
        ServerCommand(command, PhantomData)
    }
}

impl<P, T> RpcMethodSimple for ServerCommand<T, P>
    where T: LanguageServerCommand<P>,
          P: for<'de> serde::Deserialize<'de> + 'static
{
    fn call(&self, param: Params) -> BoxFuture<Value, Error> {
        let value = match param {
            Params::Map(map) => Value::Object(map),
            Params::Array(arr) => Value::Array(arr),
            Params::None => Value::Null,
        };
        match from_value(value.clone()) {
            Ok(value) => {
                return self.0
                           .execute(value)
                           .then(|result| match result {
                                     Ok(value) => {
                            Ok(to_value(&value).expect("result data could not be serialized"))
                                .into_future()
                        }
                                     Err(error) => {
                            Err(Error {
                                    code: ErrorCode::InternalError,
                                    message: error.message,
                                    data: error.data
                                        .as_ref()
                                        .map(|v| {
                                            to_value(v).expect("error data could not be serialized")
                                        }),
                                })
                                .into_future()
                        }
                                 })
                           .boxed()
            }
            Err(_) => (),
        }
        let data = self.0.invalid_params();
        futures::failed(Error {
                            code: ErrorCode::InvalidParams,
                            message: format!("Invalid params: {:?}", value),
                            data: data.as_ref()
                                .map(|v| {
                                         to_value(v).expect("error data could not be serialized")
                                     }),
                        })
                .boxed()
    }
}

pub fn read_message<R>(mut reader: R) -> Result<Option<String>, Box<StdError>>
    where R: BufRead + Read
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
    where W: Write,
          T: serde::Serialize
{
    let response = to_string(&value).unwrap();
    write_message_str(output, &response)
}

pub fn write_message_str<W>(mut output: W, response: &str) -> io::Result<()>
    where W: Write
{
    debug!("Respond: {}", response);
    try!(write!(output,
                "Content-Length: {}\r\n\r\n{}",
                response.len(),
                response));
    try!(output.flush());
    Ok(())
}
