extern crate clap;
extern crate env_logger;

extern crate failure;

extern crate gluon_language_server;

use std::io::{self, BufReader};
use std::net::TcpListener;
use std::sync::Arc;

use clap::{App, Arg};

use gluon_language_server::debugger::spawn_server;

pub fn main() {
    env_logger::init();

    let matches = App::new("debugger")
        .version(env!("CARGO_PKG_VERSION"))
        .arg(Arg::with_name("port").value_name("PORT").takes_value(true))
        .get_matches();

    match matches.value_of("port") {
        Some(port) => {
            let listener = TcpListener::bind(&format!("127.0.0.1:{}", port)[..]).unwrap();

            eprintln!("Listening on port {}", port);

            let stream = listener.incoming().next().unwrap();
            let tcp_stream = Arc::new(stream.unwrap());
            let stream2 = tcp_stream.clone();
            let input = BufReader::new(&*tcp_stream);
            spawn_server(input, stream2);
        }
        None => {
            let input = io::stdin();
            spawn_server(
                input.lock(),
                Arc::new(gluon_language_server::debugger::Stdout(io::stdout())),
            );
        }
    }
}
