#![allow(unused)]

use std::{
    collections::VecDeque,
    env,
    io::{self, BufRead, Write},
    marker::Unpin,
    str,
    sync::{
        mpsc::{sync_channel, Receiver, SyncSender, TryRecvError},
        Arc,
    },
};

use {
    futures::{compat::*, prelude::*},
    jsonrpc_core::{
        id::Id,
        params::Params,
        request::{Call, MethodCall, Notification},
        response::{Output, Response},
        version::Version,
    },
    lsp_types::*,
    serde::{de::DeserializeOwned, Serialize},
    serde_json::{from_str, from_value, ser::Serializer, to_value, Value},
    tokio::io::{AsyncBufRead, AsyncWrite, AsyncWriteExt, BufReader},
    tokio_util::codec::{Decoder, FramedRead},
    url::Url,
};

use gluon_language_server::rpc::LanguageServerDecoder;

pub fn test_url(uri: &str) -> Url {
    Url::from_file_path(&env::current_dir().unwrap().join(uri)).unwrap()
}

pub async fn write_message<W, V>(writer: &mut W, value: V) -> io::Result<()>
where
    W: ?Sized + AsyncWrite + Unpin,
    V: Serialize,
{
    let mut vec = Vec::new();
    value.serialize(&mut Serializer::new(&mut vec)).unwrap();
    writer
        .write_all(
            format!(
                "Content-Length: {}\r\n\r\n{}",
                vec.len(),
                str::from_utf8(&vec).unwrap()
            )
            .as_bytes(),
        )
        .await
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
        params,
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
        params,
    })
}

pub async fn did_open_uri<W: ?Sized>(stdin: &mut W, uri: Url, text: &str)
where
    W: AsyncWrite + Unpin,
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

    write_message(stdin, did_open).await.unwrap();
}

pub async fn did_open<W: ?Sized>(stdin: &mut W, uri: &str, text: &str)
where
    W: AsyncWrite + Unpin,
{
    did_open_uri(stdin, test_url(uri), text).await
}

pub async fn did_change<W: ?Sized>(stdin: &mut W, uri: &str, version: i64, range: Range, text: &str)
where
    W: AsyncWrite + Unpin,
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
    .await
}

pub async fn did_change_event<W: ?Sized>(
    stdin: &mut W,
    uri: &str,
    version: i64,
    content_changes: Vec<TextDocumentContentChangeEvent>,
) where
    W: AsyncWrite + Unpin,
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

    write_message(stdin, hover).await.unwrap();
}

async fn read_until<T>(
    output: impl AsyncBufRead + Unpin,
    mut f: impl FnMut(String) -> Option<T>,
) -> T {
    let stream = FramedRead::new(output, LanguageServerDecoder::new()).try_filter_map(|s| {
        let x = f(s);
        async { Ok(x) }
    });
    futures::pin_mut!(stream);
    stream
        .next()
        .await
        .expect("Expected a response")
        .expect("Success")
}

pub async fn expect_response<R, T>(output: R) -> T
where
    T: DeserializeOwned,
    R: AsyncBufRead + Unpin,
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
    .await
}

pub async fn hover<W: ?Sized>(stdin: &mut W, id: u64, uri: &str, position: Position)
where
    W: AsyncWrite + Unpin,
{
    let hover = method_call(
        "textDocument/hover",
        id,
        TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: test_url(uri) },
            position: position,
        },
    );

    write_message(stdin, hover).await.unwrap();
}

pub async fn expect_notification<R, T>(output: R) -> T
where
    T: DeserializeOwned,
    R: AsyncBufRead + Unpin,
{
    read_until(output, |json| {
        Some(match from_str(&json) {
            Ok(Notification { params, .. }) => {
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
    .await
}

pub fn run_no_panic_catch<F>(fut: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    tokio::runtime::Runtime::new().unwrap().block_on(fut)
}

struct ServerHandle {
    stdin: Box<dyn AsyncWrite + Send + Unpin>,
    stdout: Box<dyn AsyncBufRead + Send + Unpin>,
}

fn start_local() -> ServerHandle {
    let (mut stdin_write, stdin_read) = async_pipe::pipe();
    let (stdout_write, stdout_read) = async_pipe::pipe();
    let stdout_read = BufReader::new(stdout_read);

    tokio::spawn(async move {
        let thread = gluon::new_vm_async().await;
        if let Err(err) =
            ::gluon_language_server::Server::start(thread, stdin_read, stdout_write).await
        {
            panic!("{}", err)
        }
    });
    ServerHandle {
        stdin: Box::new(stdin_write),
        stdout: Box::new(stdout_read),
    }
}

fn start_remote() -> ServerHandle {
    use std::process::Stdio;
    use tokio::process::Command;

    let mut child = Command::new("target/debug/gluon_language-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    ServerHandle {
        stdin: Box::new(child.stdin.expect("stdin")),
        stdout: Box::new(BufReader::new(child.stdout.expect("stdout"))),
    }
}

pub fn send_rpc<F>(f: F)
where
    F: for<'a> FnOnce(
            &'a mut (dyn AsyncWrite + Send + Unpin),
            &'a mut (dyn AsyncBufRead + Send + Unpin),
        ) -> futures::future::BoxFuture<'a, ()>
        + Send
        + ::std::panic::UnwindSafe
        + 'static,
{
    run_no_panic_catch(async move {
        let ServerHandle {
            mut stdin,
            mut stdout,
        } = if env::var("GLUON_TEST_LOCAL_SERVER").is_ok() {
            // FIXME Local  testing may deadlock atm
            start_local()
        } else {
            start_remote()
        };

        {
            f(&mut stdin, &mut stdout).await;

            write_message(&mut stdin, method_call("shutdown", 1_000_000, ()))
                .await
                .unwrap();

            let () = expect_response(&mut stdout).await;

            let exit = Call::Notification(Notification {
                jsonrpc: Some(Version::V2),
                method: "exit".into(),
                params: Params::None,
            });
            write_message(&mut stdin, exit).await.unwrap();
            drop(stdin);
        }
    })
}
