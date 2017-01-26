use std::error::Error as StdError;
use std::io::{self, BufRead, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{self, AtomicBool};

use jsonrpc_core::{Error, ErrorCode, IoHandler, MethodCommand, MethodResult, Params, Value};

use serde;
use serde_json::{from_value, to_value};

pub struct ServerError<E> {
    pub message: String,
    pub data: Option<E>,
}

pub trait LanguageServerCommand: Send + Sync {
    type Param: serde::Deserialize;
    type Output: serde::Serialize;
    type Error: serde::Serialize;
    fn execute(&self, param: Self::Param) -> Result<Self::Output, ServerError<Self::Error>>;

    fn invalid_params(&self) -> Option<Self::Error>;
}

pub trait LanguageServerNotification: Send + Sync {
    type Param: serde::Deserialize;
    fn execute(&self, param: Self::Param);
}

pub struct ServerCommand<T>(pub T);

impl<T> MethodCommand for ServerCommand<T>
    where T: LanguageServerCommand,
{
    fn execute(&self, param: Params) -> MethodResult {
        match param {
            Params::Map(ref map) => {
                match from_value(Value::Object(map.clone())) {
                    Ok(value) => {
                        let result = match self.0.execute(value) {
                            Ok(value) => Ok(to_value(&value)),
                            Err(error) => {
                                Err(Error {
                                    code: ErrorCode::InternalError,
                                    message: error.message,
                                    data: error.data.as_ref().map(to_value),
                                })
                            }
                        };
                        return MethodResult::Sync(result);
                    }
                    Err(_) => (),
                }
            }
            _ => (),
        }
        let data = self.0.invalid_params();
        MethodResult::Sync(Err(Error {
            code: ErrorCode::InvalidParams,
            message: format!("Invalid params: {:?}", param),
            data: data.as_ref().map(to_value),
        }))
    }
}

pub fn read_message<R>(mut reader: R) -> Result<Option<String>, Box<StdError>>
    where R: BufRead + Read,
{
    let mut header = String::new();
    let n = try!(reader.read_line(&mut header));
    if n == 0 {
        // EOF
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

pub fn main_loop(io: &mut IoHandler, exit_token: Arc<AtomicBool>) -> Result<(), Box<StdError>> {
    let stdin = io::stdin();
    while !exit_token.load(atomic::Ordering::SeqCst) {
        match try!(read_message(stdin.lock())) {
            Some(json) => {
                debug!("Handle: {}", json);
                if let Some(response) = io.handle_request_sync(&json) {
                    print!("Content-Length: {}\r\n\r\n{}", response.len(), response);
                    try!(io::stdout().flush());
                }
            }
            None => return Ok(()),
        }
    }
    Ok(())
}
