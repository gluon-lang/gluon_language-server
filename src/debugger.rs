extern crate debugserver_types;
extern crate env_logger;
extern crate futures;
extern crate jsonrpc_core;
extern crate serde;
extern crate serde_json;

extern crate gluon_language_server;

use std::error::Error as StdError;
use std::io::{BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::spawn;

use futures::{BoxFuture, Future, IntoFuture};

use jsonrpc_core::IoHandler;

use serde_json::Value;

use debugserver_types::*;

use gluon_language_server::rpc::{LanguageServerCommand, ServerCommand, ServerError, main_loop};

pub struct InitializeHandler {
    stream: Arc<TcpStream>,
    seq: Arc<AtomicUsize>,
}

impl LanguageServerCommand<InitializeRequestArguments> for InitializeHandler {
    type Output = Option<Capabilities>;
    type Error = ();
    fn execute(&self,
               _args: InitializeRequestArguments)
               -> BoxFuture<Option<Capabilities>, ServerError<Self::Error>> {

        let initialized = InitializedEvent {
            body: None,
            event: "initialized".into(),
            seq: self.seq.fetch_add(1, Ordering::SeqCst) as i64,
            type_: "event".into(),
        };
        let response = serde_json::to_string(&initialized).unwrap();
        write!(&*self.stream,
               "Content-Length: {}\r\n\r\n{}",
               response.len(),
               response)
            .map_err(|err| {
                ServerError {
                    message: format!("Unable to write result to initialize: {}", err),
                    data: None,
                }
            })
            .map(|_| None)
            .into_future()
            .boxed()
    }
}

pub struct LaunchHandler;

impl LanguageServerCommand<Value> for LaunchHandler {
    type Output = Option<Value>;
    type Error = ();
    fn execute(&self, _args: Value) -> BoxFuture<Option<Value>, ServerError<()>> {
        Ok(None).into_future().boxed()
    }
}

pub struct DisconnectHandler {
    exit_token: Arc<AtomicBool>,
}

impl LanguageServerCommand<DisconnectArguments> for DisconnectHandler {
    type Output = Option<Value>;
    type Error = ();
    fn execute(&self, _args: Value) -> BoxFuture<Option<Value>, ServerError<()>> {
        self.exit_token.store(true, Ordering::SeqCst);
        Ok(None).into_future().boxed()
    }
}


// Translate debug server messages into jsonrpc-2.0
fn translate_request(message: String) -> Result<String, Box<StdError>> {
    use serde_json::Value;
    let mut data: Value = try!(serde_json::from_str(&message));
    if data.is_object() {
        let data = data.as_object_mut().unwrap();
        let command = data.get("command").and_then(|c| c.as_str()).expect("command").to_string();
        let seq = data.get("seq")
            .and_then(|c| {
                c.as_str()
                    .map(|s| Value::String(s.to_string()))
                    .or_else(|| c.as_i64().map(Value::from))
            })
            .expect("seq");
        let arguments = data.remove("arguments").expect("arguments");
        let map = vec![("method".to_string(), Value::String(command)),
                       ("id".to_string(), seq),
                       ("params".to_string(), arguments),
                       ("jsonrpc".to_owned(), Value::String("2.0".to_string()))]
            .into_iter()
            .collect();
        return Ok(try!(serde_json::to_string(&Value::Object(map))));
    }
    Ok(message)
}

fn translate_response(message: String) -> Result<String, Box<StdError>> {
    use serde_json::Value;
    let mut data: Value = try!(serde_json::from_str(&message));
    if data.is_object() {
        let data = data.as_object_mut().unwrap();
        let seq = data.get("id")
            .and_then(|c| {
                c.as_str()
                    .map(|s| Value::String(s.to_string()))
                    .or_else(|| c.as_i64().map(Value::from))
            })
            .expect("id");
        let result = data.remove("result");
        let error = data.remove("error");

        let mut map = vec![("success".to_string(), Value::Bool(result.is_some())),
                           ("request_seq".to_string(), seq.clone()),
                           ("seq".to_string(), seq),
                           ("type".to_owned(), Value::String("response".to_string()))];

        if let Some(result) = result {
            map.push(("body".to_string(), result));
        }
        if let Some(mut error) = error {
            map.push(("message".to_string(),
                      error.as_object_mut().unwrap().remove("message").expect("error message")));
        }

        let map = map.into_iter().collect();
        return Ok(try!(serde_json::to_string(&Value::Object(map))));
    }
    Ok(message)
}

pub fn main() {
    env_logger::init().unwrap();

    let listener = TcpListener::bind("127.0.0.1:4711").unwrap();
    for stream in listener.incoming() {
        let stream = Arc::new(stream.unwrap());

        let exit_token = Arc::new(AtomicBool::new(false));
        let seq = Arc::new(AtomicUsize::new(0));

        let mut io = IoHandler::new();
        io.add_async_method("initialize",
                            ServerCommand::new(InitializeHandler {
                                stream: stream.clone(),
                                seq: seq,
                            }));
        io.add_async_method("launch", ServerCommand::new(LaunchHandler));
        io.add_async_method("disconnect",
                            ServerCommand::new(DisconnectHandler {
                                exit_token: exit_token.clone(),
                            }));

        spawn(move || {
            let read = BufReader::new(&*stream);
            main_loop(read,
                      &*stream,
                      &io,
                      exit_token,
                      translate_request,
                      translate_response)
                .unwrap()
        });
    }
}
