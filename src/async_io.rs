use std::io::{Error, ErrorKind, Read, Result};

use tokio;

use futures::{Async, AsyncSink, Future, Sink, Stream};
use futures::sync::mpsc::{channel, Receiver, Sender};

pub struct AsyncRead {
    _pool: ::futures_cpupool::CpuPool,
    size_sender: Sender<usize>,
    result_receiver: Receiver<Result<Vec<u8>>>,
    debt: Vec<u8>,
}

pub fn async_read<R>(mut read: R) -> AsyncRead
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
    tokio::spawn(pool.spawn(future));
    AsyncRead {
        _pool: pool,
        size_sender,
        result_receiver,
        debt: Vec::new(),
    }
}

impl AsyncRead {
    fn handle_input(&mut self, buf: &mut [u8], result: Result<Vec<u8>>) -> Result<usize> {
        let buffer = result?;
        let copy_len = buffer.len().min(buf.len());
        buf[..copy_len].copy_from_slice(&buffer[..copy_len]);
        self.debt.extend_from_slice(&buffer[copy_len..]);
        Ok(copy_len)
    }
}

impl Read for AsyncRead {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        if !self.debt.is_empty() {
            let copy_len = self.debt.len().min(buf.len());
            buf[..copy_len].copy_from_slice(&self.debt[..copy_len]);
            self.debt.drain(..copy_len);
            return Ok(copy_len);
        }
        match self.result_receiver.poll() {
            Ok(Async::Ready(Some(result))) => self.handle_input(buf, result),
            Ok(Async::Ready(None)) => Ok(0),

            Ok(Async::NotReady) => match self.size_sender.start_send(buf.len()) {
                Ok(AsyncSink::Ready) => match self.result_receiver.poll() {
                    Ok(Async::Ready(Some(result))) => self.handle_input(buf, result),
                    Ok(Async::Ready(None)) => Ok(0),
                    Ok(Async::NotReady) => Err(Error::new(
                        ErrorKind::WouldBlock,
                        "Waiting on input to be available",
                    )),
                    Err(_) => Err(ErrorKind::BrokenPipe.into()),
                },

                Ok(AsyncSink::NotReady(_)) => Err(Error::new(
                    ErrorKind::WouldBlock,
                    "Waiting on input to be available",
                )),
                Err(_) => match self.result_receiver.poll() {
                    Ok(Async::Ready(Some(result))) => self.handle_input(buf, result),
                    Ok(Async::Ready(None)) => Ok(0),
                    Ok(Async::NotReady) => Err(Error::new(
                        ErrorKind::WouldBlock,
                        "Waiting on input to be available",
                    )),
                    Err(_) => Err(Error::new(ErrorKind::Other, "Read thread has stopped")),
                },
            },

            Err(_) => Err(ErrorKind::BrokenPipe.into()),
        }
    }
}

impl ::tokio_io::AsyncRead for AsyncRead {}

#[cfg(test)]
mod tests {
    use super::*;

    use futures::future;

    use partial_io::{GenInterrupted, PartialAsyncRead, PartialOp, PartialWithErrors};
    use tokio_io::io::read_to_end;

    quickcheck! {
        fn partial_io(seq: PartialWithErrors<GenInterrupted>, input: Vec<u8>) -> () {
            tokio::run(future::lazy(move || {
                let reader = async_read(::std::io::Cursor::new(input.clone()));
                let partial_reader = PartialAsyncRead::new(reader, seq);

                read_to_end(partial_reader, vec![])
                   .map(move |t| assert_eq!(t.1, input))
                   .map_err(|err| panic!("{}", err))
            }));
        }
    }

    #[test]
    fn partial_2_1() {
        tokio::run(future::lazy(move || {
            let input = vec![0, 0];
            let reader = async_read(::std::io::Cursor::new(input.clone()));
            let seq = vec![PartialOp::Limited(2), PartialOp::Limited(1)];
            let partial_reader = PartialAsyncRead::new(reader, seq);

            read_to_end(partial_reader, vec![])
                .map(move |t| assert_eq!(t.1, input))
                .map_err(|err| panic!("{}", err))
        }));
    }
}
