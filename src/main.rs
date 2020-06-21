use gluon_language_server;

#[tokio::main]
async fn main() {
    if let Err(err) = gluon_language_server::run().await {
        eprintln!("{}", err);
        std::process::exit(1);
    }
}
