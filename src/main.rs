use gluon_language_server;

#[tokio::main]
async fn main() {
    gluon_language_server::run().await;
}
