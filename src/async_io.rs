use std::io::{Error, ErrorKind, Read, Result};

use futures::{Async, AsyncSink, Future, Sink, Stream};
use tokio_core::reactor::Handle;
use futures::sync::mpsc::{channel, Receiver, Sender};

pub struct AsyncRead {
    _pool: ::futures_cpupool::CpuPool,
    size_sender: Sender<usize>,
    result_receiver: Receiver<Result<Vec<u8>>>,
}


pub fn async_read<R>(handle: &Handle, mut read: R) -> AsyncRead
where
    R: Read + Send + 'static,
{
    let (size_sender, size_receiver) = channel(1);
    let (result_sender, result_receiver) = channel(1);

    let future = size_receiver
        .map_err(|_| {
            panic!("Size sender shut down");
        })
        .map(move |size| {
            let mut buffer = vec![0; size];
            match read.read(&mut buffer) {
                Ok(len) => {
                    buffer.truncate(len);
                    Ok(buffer)
                }
                Err(err) => Err(err),
            }
        })
        .forward(result_sender.sink_map_err(|_| {
            panic!("Result receiver shut down");
        }))
        .map(|_| ());

    let pool = ::futures_cpupool::Builder::new()
        .pool_size(1)
        .name_prefix("input-reader-")
        .create();
    handle.spawn(pool.spawn(future));
    AsyncRead {
        _pool: pool,
        size_sender,
        result_receiver,
    }
}

impl Read for AsyncRead {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        match self.result_receiver.poll() {
            Ok(Async::Ready(Some(result))) => {
                let buffer = result?;
                assert!(
                    buffer.len() <= buf.len(),
                    "{} <= {} is false",
                    buffer.len(),
                    buf.len()
                );
                buf[..buffer.len()].copy_from_slice(&buffer);
                Ok(buffer.len())
            }
            Ok(Async::Ready(None)) => Ok(0),
            Ok(Async::NotReady) => {
                let err = match self.size_sender.start_send(buf.len()) {
                    Ok(AsyncSink::Ready) => match self.result_receiver.poll() {
                        Ok(Async::Ready(Some(result))) => {
                            let buffer = result?;
                            assert!(
                                buffer.len() <= buf.len(),
                                "{} <= {} is false",
                                buffer.len(),
                                buf.len()
                            );
                            buf[..buffer.len()].copy_from_slice(&buffer);
                            return Ok(buffer.len());
                        }
                        Ok(Async::Ready(None)) => return Ok(0),
                        Ok(Async::NotReady) => {
                            Error::new(ErrorKind::WouldBlock, "Waiting on input to be available")
                        }
                        Err(_) => ErrorKind::BrokenPipe.into(),
                    },
                    Ok(AsyncSink::NotReady(_)) => {
                        Error::new(ErrorKind::Other, "Read thread2 has stopped")
                    }
                    Err(_) => match self.result_receiver.poll() {
                        Ok(Async::Ready(Some(Err(err)))) => err,
                        Ok(Async::Ready(None)) => return Ok(0),
                        Ok(Async::NotReady) => {
                            Error::new(ErrorKind::WouldBlock, "Waiting on input to be available")
                        }
                        _ => Error::new(ErrorKind::Other, "Read thread has stopped"),
                    },
                };
                Err(err)
            }
            Err(_) => return Err(ErrorKind::BrokenPipe.into()),
        }
    }
}

impl ::tokio_io::AsyncRead for AsyncRead {}
