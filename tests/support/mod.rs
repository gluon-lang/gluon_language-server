#![allow(unused)]

extern crate futures;
extern crate languageserver_types;
extern crate tokio;
extern crate tokio_io;

use std::{
    collections::VecDeque,
    env,
    io::{self, BufRead, BufReader, Write},
    str,
    sync::{
        mpsc::{sync_channel, Receiver, SyncSender, TryRecvError},
        Arc,
    },
};

use jsonrpc_core::{
    id::Id,
    params::Params,
    request::{Call, MethodCall, Notification},
    response::{Output, Response},
    version::Version,
};

use serde::{de::DeserializeOwned, Serialize};
use serde_json::{from_str, from_value, ser::Serializer, to_value, Value};

use self::{
    futures::{future, Future, Poll, Stream},
    tokio::{
        codec::{Decoder, FramedRead},
        prelude::task,
    },
    tokio_io::io::AllowStdIo,
};

use url::Url;

use languageserver_types::*;

use gluon_language_server::rpc::LanguageServerDecoder;

extern crate gluon;
use self::gluon::new_vm;

pub fn test_url(uri: &str) -> Url {
    Url::from_file_path(&env::current_dir().unwrap().join(uri)).unwrap()
}

pub fn write_message<W, V>(writer: &mut W, value: V) -> io::Result<()>
where
    W: ?Sized + Write,
    V: Serialize,
{
    let mut vec = Vec::new();
    value.serialize(&mut Serializer::new(&mut vec)).unwrap();
    write!(
        writer,
        "Content-Length: {}\r\n\r\n{}",
        vec.len(),
        str::from_utf8(&vec).unwrap()
    )
}

pub fn method_call<T>(method: &str, id: u64, value: T) -> Call
where
    T: Serialize,
{
    let value = to_value(value);
    let params = match value {
        Ok(Value::Object(map)) => Params::Map(map),
        Ok(Value::Null) => Params::None,
        _ => panic!("Expected map"),
    };
    Call::MethodCall(MethodCall {
        jsonrpc: Some(Version::V2),
        method: method.into(),
        id: Id::Num(id),
        params: Some(params),
    })
}

pub fn notification<T>(method: &str, value: T) -> Call
where
    T: Serialize,
{
    let value = to_value(value);
    let params = match value {
        Ok(Value::Object(map)) => Params::Map(map),
        Ok(Value::Null) => Params::None,
        _ => panic!("Expected map or null"),
    };
    Call::Notification(Notification {
        jsonrpc: Some(Version::V2),
        method: method.into(),
        params: Some(params),
    })
}

pub fn did_open_uri<W: ?Sized>(stdin: &mut W, uri: Url, text: &str)
where
    W: Write,
{
    let did_open = notification(
        "textDocument/didOpen",
        DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: uri,
                language_id: "gluon".into(),
                text: text.into(),
                version: 1,
            },
        },
    );

    write_message(stdin, did_open).unwrap();
}

pub fn did_open<W: ?Sized>(stdin: &mut W, uri: &str, text: &str)
where
    W: Write,
{
    did_open_uri(stdin, test_url(uri), text)
}

pub fn did_change<W: ?Sized>(stdin: &mut W, uri: &str, version: u64, range: Range, text: &str)
where
    W: Write,
{
    did_change_event(
        stdin,
        uri,
        version,
        vec![TextDocumentContentChangeEvent {
            range: Some(range),
            range_length: None,
            text: text.to_string(),
        }],
    )
}

pub fn did_change_event<W: ?Sized>(
    stdin: &mut W,
    uri: &str,
    version: u64,
    content_changes: Vec<TextDocumentContentChangeEvent>,
) where
    W: Write,
{
    let hover = notification(
        "textDocument/didChange",
        DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier {
                uri: test_url(uri),
                version: Some(version),
            },
            content_changes,
        },
    );

    write_message(stdin, hover).unwrap();
}

fn read_until<T>(output: impl BufRead, f: impl FnMut(String) -> Option<T>) -> T {
    FramedRead::new(AllowStdIo::new(output), LanguageServerDecoder::new())
        .filter_map(f)
        .into_future()
        .map_err(|(err, _)| err)
        .wait()
        .expect("Success")
        .0
        .expect("Expected a response")
}

pub fn expect_response<R, T>(output: R) -> T
where
    T: DeserializeOwned,
    R: BufRead,
{
    read_until(output, |json| {
        // Skip all notifications
        if let Ok(Notification { .. }) = from_str(&json) {
            None
        } else if let Ok(Response::Single(Output::Success(response))) = from_str(&json) {
            Some(from_value(response.result).unwrap_or_else(|err| panic!("{}\n{}", err, json)))
        } else {
            panic!("Expected response, got `{}`", json)
        }
    })
}

pub fn hover<W: ?Sized>(stdin: &mut W, id: u64, uri: &str, position: Position)
where
    W: Write,
{
    let hover = method_call(
        "textDocument/hover",
        id,
        TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: test_url(uri) },
            position: position,
        },
    );

    write_message(stdin, hover).unwrap();
}

pub fn expect_notification<R, T>(output: R) -> T
where
    T: DeserializeOwned,
    R: BufRead,
{
    read_until(output, |json| {
        Some(match from_str(&json) {
            Ok(Notification {
                params: Some(params),
                ..
            }) => {
                let json_value = match params {
                    Params::Map(map) => Value::Object(map),
                    Params::Array(array) => Value::Array(array),
                    Params::None => Value::Null,
                };
                return from_value(json_value).unwrap();
            }
            Ok(_) => panic!("Expected notification\n{}", json),
            Err(err) => panic!("Expected notification, got `{}`\n{}", err, json),
        })
    })
}

pub fn run_no_panic_catch<F>(fut: F)
where
    F: Future<Item = (), Error = ()> + Send + ::std::panic::UnwindSafe + 'static,
{
    use std::sync::{Arc, Mutex};

    let error_result = Arc::new(Mutex::new(None));
    {
        let error_result = error_result.clone();
        tokio::run(
            fut.catch_unwind()
                .map_err(move |err| {
                    *error_result.lock().unwrap() = Some(err);
                })
                .and_then(|result| result),
        );
    }

    if let Some(err) = Arc::try_unwrap(error_result).unwrap().into_inner().unwrap() {
        panic!(err)
    }
}

fn start_local() -> (Box<Write>, Box<BufRead>) {
    let (stdin_read, mut stdin_write) = pipe();
    let (stdout_read, stdout_write) = pipe();
    let stdout_read = BufReader::new(SyncReadPipe(stdout_read));

    let thread = new_vm();
    tokio::spawn(
        ::gluon_language_server::Server::start(thread, stdin_read, stdout_write)
            .map_err(|err| panic!("{}", err)),
    );
    (Box::new(stdin_write), Box::new(stdout_read))
}

fn start_remote() -> (Box<Write>, Box<BufRead>) {
    use std::process::{Command, Stdio};

    let mut child = Command::new("target/debug/gluon_language-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    (
        Box::new(child.stdin.expect("stdin")),
        Box::new(BufReader::new(child.stdout.expect("stdout"))),
    )
}

pub fn send_rpc<F>(f: F)
where
    F: FnOnce(&mut Write, &mut BufRead) + Send + ::std::panic::UnwindSafe + 'static,
{
    run_no_panic_catch(future::lazy(move || {
        let (mut stdin_write, mut stdout_read) = if env::var("GLUON_TEST_REMOTE_SERVER").is_ok() {
            start_remote()
        } else {
            start_local()
        };

        {
            f(&mut stdin_write, &mut stdout_read);

            write_message(&mut stdin_write, method_call("shutdown", 1_000_000, ())).unwrap();

            let () = expect_response(&mut stdout_read);

            let exit = Call::Notification(Notification {
                jsonrpc: Some(Version::V2),
                method: "exit".into(),
                params: None,
            });
            write_message(&mut stdin_write, exit).unwrap();
            drop(stdin_write);
        }
        Ok(())
    }))
}

pub struct ReadPipe {
    task: Arc<task::AtomicTask>,
    recv: Receiver<Vec<u8>>,
    buffer: VecDeque<u8>,
}

impl io::Read for ReadPipe {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let l = buffer.len().min(self.buffer.len());
        for (to, from) in buffer.iter_mut().zip(self.buffer.drain(..l)) {
            *to = from;
        }
        self.task.register();
        match self.recv.try_recv() {
            Ok(buf) => {
                self.buffer.extend(buf);
                let extra = self.read(&mut buffer[l..]).unwrap_or(0);
                Ok(l + extra)
            }
            Err(TryRecvError::Disconnected) => Ok(0),
            Err(TryRecvError::Empty) => {
                if l == 0 {
                    Err(io::ErrorKind::WouldBlock.into())
                } else {
                    Ok(l)
                }
            }
        }
    }

    fn read_to_end(&mut self, buffer: &mut Vec<u8>) -> io::Result<usize> {
        let len = buffer.len();
        buffer.extend(self.buffer.drain(..));
        while let Ok(buf) = self.recv.recv() {
            buffer.extend(buf);
        }
        Ok(buffer.len() - len)
    }
}

impl tokio::io::AsyncRead for ReadPipe {}

pub struct SyncReadPipe(pub ReadPipe);

impl io::Read for SyncReadPipe {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self.0.read(buffer) {
            Ok(x) => Ok(x),
            Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => match self.0.recv.recv() {
                Ok(buf) => {
                    let l = buffer.len().min(buf.len());
                    buffer[..l].copy_from_slice(&buf[..l]);
                    self.0.buffer.extend(buf[l..].iter().cloned());
                    Ok(l)
                }
                Err(_) => Ok(0),
            },
            Err(err) => Err(err),
        }
    }
}

#[derive(Clone)]
pub struct WritePipe {
    task: Arc<task::AtomicTask>,
    sender: SyncSender<Vec<u8>>,
}

impl<'a> io::Write for &'a WritePipe {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        self.sender
            .send(data.to_owned())
            .map_err(|_| io::Error::from(io::ErrorKind::NotConnected))?;
        self.task.notify();
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl io::Write for WritePipe {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        (&*self).write(data)
    }

    fn flush(&mut self) -> io::Result<()> {
        (&*self).flush()
    }
}

impl tokio::io::AsyncWrite for WritePipe {
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        Ok(().into())
    }
}

pub fn pipe() -> (ReadPipe, WritePipe) {
    let task = Arc::new(task::AtomicTask::new());
    let (sender, receiver) = sync_channel(10);
    (
        ReadPipe {
            task: task.clone(),
            recv: receiver,
            buffer: VecDeque::new(),
        },
        WritePipe { task, sender },
    )
}
