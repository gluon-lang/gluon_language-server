#![cfg_attr(feature = "serde_macros", feature(custom_derive, plugin))]
#![cfg_attr(feature = "serde_macros", plugin(serde_macros))]

extern crate clap;

extern crate failure;

extern crate serde;
#[macro_use]
extern crate serde_derive;
extern crate serde_json;

extern crate futures;
extern crate jsonrpc_core;
extern crate tokio;
extern crate tokio_codec;
extern crate tokio_io;

extern crate env_logger;

#[macro_use]
extern crate log;
extern crate url;
extern crate url_serde;

extern crate bytes;

extern crate codespan;
extern crate codespan_lsp;
extern crate codespan_reporting;

extern crate languageserver_types;

extern crate gluon;
extern crate gluon_completion as completion;
extern crate gluon_format;

macro_rules! log_message {
    ($sender: expr, $($ts: tt)+) => {
        if log_enabled!(::log::Level::Debug) {
            Either::A(::log_message($sender, format!( $($ts)+ )))
        } else {
            Either::B(Ok(()).into_future())
        }
    }
}

macro_rules! box_future {
    ($e:expr) => {{
        let fut: BoxFuture<_, _> = Box::new($e.into_future());
        fut
    }};
}

macro_rules! try_future {
    ($e:expr) => {
        match $e {
            Ok(x) => x,
            Err(err) => return box_future!(Err(err.into())),
        }
    };
}

#[macro_use]
pub mod rpc;
mod check_importer;
mod diagnostics;
mod server;
mod text_edit;

use url::Url;

use gluon::base::filename_to_module;
use gluon::base::kind::ArcKind;
use gluon::base::pos::BytePos;
use gluon::base::types::{ArcType, Type};
use gluon::either;
use gluon::import::Import;
use gluon::vm::thread::Thread;
use gluon::{new_vm, RootedThread};

use std::env;
use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::str;
use std::sync::RwLock;

use languageserver_types::*;

use futures::future::Either;
use futures::sync::mpsc;
use futures::sync::oneshot;
use futures::{future, Future, IntoFuture, Sink, Stream};

use codespan_lsp::{byte_span_to_range, position_to_byte_index};

use tokio_codec::{Framed, FramedParts};

pub type BoxFuture<I, E> = Box<Future<Item = I, Error = E> + Send + 'static>;

use {
    check_importer::{CheckImporter, Module},
    rpc::*,
};

fn log_message(
    sender: mpsc::Sender<String>,
    message: String,
) -> impl Future<Item = (), Error = ()> {
    debug!("{}", message);
    let r = format!(
        r#"{{"jsonrpc": "2.0", "method": "window/logMessage", "params": {} }}"#,
        serde_json::to_value(&LogMessageParams {
            typ: MessageType::Log,
            message: message,
        })
        .unwrap()
    );
    sender.send(r).map(|_| ()).map_err(|_| ())
}

fn type_to_completion_item_kind(typ: &ArcType) -> CompletionItemKind {
    match **typ {
        _ if typ.as_function().is_some() => CompletionItemKind::Function,
        Type::Alias(ref alias) => type_to_completion_item_kind(alias.unresolved_type()),
        Type::App(ref f, _) => type_to_completion_item_kind(f),
        Type::Variant(_) => CompletionItemKind::Enum,
        Type::Record(_) => CompletionItemKind::Module,
        _ => CompletionItemKind::Variable,
    }
}

fn ident_to_completion_item_kind(
    id: &str,
    typ_or_kind: either::Either<&ArcKind, &ArcType>,
) -> CompletionItemKind {
    match typ_or_kind {
        either::Either::Left(_) => CompletionItemKind::Class,
        either::Either::Right(typ) => {
            if id.starts_with(char::is_uppercase) {
                CompletionItemKind::Constructor
            } else {
                type_to_completion_item_kind(typ)
            }
        }
    }
}

struct Initialize(RootedThread);
impl LanguageServerCommand<InitializeParams> for Initialize {
    type Future = BoxFuture<Self::Output, ServerError<Self::Error>>;
    type Output = InitializeResult;
    type Error = InitializeError;
    fn execute(
        &self,
        change: InitializeParams,
    ) -> BoxFuture<InitializeResult, ServerError<InitializeError>> {
        let import = self.0.get_macros().get("import").expect("Import macro");
        let import = import
            .downcast_ref::<Import<CheckImporter>>()
            .expect("Check importer");
        if let Some(ref path) = change.root_path {
            import.add_path(path);
        }
        Box::new(
            Ok(InitializeResult {
                capabilities: ServerCapabilities {
                    text_document_sync: Some(TextDocumentSyncCapability::Kind(
                        TextDocumentSyncKind::Incremental,
                    )),
                    completion_provider: Some(CompletionOptions {
                        resolve_provider: Some(true),
                        trigger_characters: Some(vec![".".into()]),
                    }),
                    signature_help_provider: Some(SignatureHelpOptions {
                        trigger_characters: None,
                    }),
                    hover_provider: Some(true),
                    document_formatting_provider: Some(true),
                    document_highlight_provider: Some(true),
                    document_symbol_provider: Some(true),
                    workspace_symbol_provider: Some(true),
                    ..ServerCapabilities::default()
                },
            })
            .into_future(),
        )
    }

    fn invalid_params(&self) -> Option<Self::Error> {
        Some(InitializeError { retry: false })
    }
}

fn retrieve_expr_future<'a, 'b, F, Q, R>(
    thread: &'a Thread,
    text_document_uri: &'b Url,
    f: F,
) -> impl Future<Item = R, Error = ServerError<()>> + 'static
where
    F: FnOnce(&mut Module) -> Q,
    Q: IntoFuture<Item = R, Error = ServerError<()>>,
    Q::Future: Send + 'static,
    R: Send + 'static,
{
    let filename = strip_file_prefix_with_thread(thread, text_document_uri);
    let module = filename_to_module(&filename);
    let import = thread.get_macros().get("import").expect("Import macro");
    let import = import
        .downcast_ref::<Import<CheckImporter>>()
        .expect("Check importer");
    let mut importer = import.importer.0.lock().unwrap();
    match importer.get_mut(&module) {
        Some(source_module) => return Either::A(f(source_module).into_future()),
        None => (),
    }
    Either::B(
        Err(ServerError {
            message: format!(
                "Module `{}` is not defined\n{:?}",
                module,
                importer.keys().collect::<Vec<_>>()
            ),
            data: None,
        })
        .into_future(),
    )
}

fn retrieve_expr<F, R>(thread: &Thread, text_document_uri: &Url, f: F) -> Result<R, ServerError<()>>
where
    F: FnOnce(&mut Module) -> Result<R, ServerError<()>>,
{
    let filename = strip_file_prefix_with_thread(thread, text_document_uri);
    let module = filename_to_module(&filename);
    let import = thread.get_macros().get("import").expect("Import macro");
    let import = import
        .downcast_ref::<Import<CheckImporter>>()
        .expect("Check importer");
    let mut importer = import.importer.0.lock().unwrap();
    match importer.get_mut(&module) {
        Some(source_module) => return f(source_module),
        None => (),
    }
    Err(ServerError {
        message: format!(
            "Module `{}` is not defined\n{:?}",
            module,
            importer.keys().collect::<Vec<_>>()
        ),
        data: None,
    })
}

fn retrieve_expr_with_pos<F, R>(
    thread: &Thread,
    text_document_uri: &Url,
    position: &Position,
    f: F,
) -> Result<R, ServerError<()>>
where
    F: FnOnce(&Module, BytePos) -> Result<R, ServerError<()>>,
{
    retrieve_expr(thread, text_document_uri, |module| {
        let byte_index = position_to_byte_index(&module.source, position)?;

        f(module, byte_index)
    })
}

#[derive(Serialize, Deserialize)]
pub struct CompletionData {
    #[serde(with = "url_serde")]
    pub text_document_uri: Url,
    pub position: Position,
}

#[derive(Clone)]
struct Completion(RootedThread);
impl LanguageServerCommand<CompletionParams> for Completion {
    type Future = BoxFuture<Self::Output, ServerError<()>>;
    type Output = Option<CompletionResponse>;
    type Error = ();
    fn execute(&self, change: CompletionParams) -> BoxFuture<Self::Output, ServerError<()>> {
        let thread = self.0.clone();
        let self_ = self.clone();
        let text_document_uri = change.text_document.uri.clone();
        let result = retrieve_expr_future(&self.0, &text_document_uri, move |module| {
            let Module {
                ref expr,
                ref source,
                dirty,
                ref mut waiters,
                ..
            } = *module;

            if dirty {
                let (sender, receiver) = oneshot::channel();
                waiters.push(sender);
                return box_future!(
                    receiver
                        .map_err(|_| {
                            let msg = "Completion sender was unexpectedly dropped";
                            error!("{}", msg);
                            ServerError::from(msg.to_string())
                        })
                        .and_then(move |_| self_.clone().execute(change))
                );
            }

            let byte_index = try_future!(position_to_byte_index(&source, &change.position));

            let query = completion::SuggestionQuery {
                modules: with_import(&thread, |import| import.modules()),
                ..completion::SuggestionQuery::default()
            };

            let suggestions = query
                .suggest(&*thread.get_env(), source.span(), expr, byte_index)
                .into_iter()
                .filter(|suggestion| !suggestion.name.starts_with("__"))
                .collect::<Vec<_>>();

            let mut items: Vec<_> = suggestions
                .into_iter()
                .map(|ident| {
                    // Remove the `:Line x, Row y suffix`
                    let name: &str = ident.name.as_ref();
                    let label = String::from(name.split(':').next().unwrap_or(ident.name.as_ref()));
                    CompletionItem {
                        insert_text: if label.starts_with(char::is_alphabetic) {
                            None
                        } else {
                            Some(format!("({})", label))
                        },
                        kind: Some(ident_to_completion_item_kind(&label, ident.typ.as_ref())),
                        label,
                        detail: match ident.typ {
                            either::Either::Right(ref typ) => match **typ {
                                Type::Hole => None,
                                _ => Some(format!("{}", ident.typ)),
                            },
                            either::Either::Left(_) => Some(format!("{}", ident.typ)),
                        },
                        data: Some(
                            serde_json::to_value(CompletionData {
                                text_document_uri: change.text_document.uri.clone(),
                                position: change.position,
                            })
                            .expect("CompletionData"),
                        ),
                        ..CompletionItem::default()
                    }
                })
                .collect();

            items.sort_by(|l, r| l.label.cmp(&r.label));

            Box::new(Ok(Some(CompletionResponse::Array(items))).into_future())
        });
        Box::new(result.into_future())
    }

    fn invalid_params(&self) -> Option<Self::Error> {
        None
    }
}

struct HoverCommand(RootedThread);
impl LanguageServerCommand<TextDocumentPositionParams> for HoverCommand {
    type Future = BoxFuture<Self::Output, ServerError<()>>;
    type Output = Option<Hover>;
    type Error = ();
    fn execute(
        &self,
        change: TextDocumentPositionParams,
    ) -> BoxFuture<Option<Hover>, ServerError<()>> {
        Box::new(
            (|| -> Result<_, _> {
                let thread = &self.0;
                retrieve_expr(thread, &change.text_document.uri, |module| {
                    let expr = &module.expr;

                    let source = &module.source;
                    let byte_index = position_to_byte_index(&source, &change.position)?;

                    let env = thread.get_env();
                    let (_, metadata_map) = gluon::check::metadata::metadata(&*env, &expr);
                    let opt_metadata =
                        completion::get_metadata(&metadata_map, source.span(), expr, byte_index);
                    let extract = (completion::TypeAt { env: &*env }, completion::SpanAt);
                    Ok(
                        completion::completion(extract, source.span(), expr, byte_index)
                            .map(|(typ, span)| {
                                let contents = match opt_metadata.and_then(|m| m.comment.as_ref()) {
                                    Some(comment) => format!("{}\n\n{}", typ, comment.content),
                                    None => format!("{}", typ),
                                };
                                Some(Hover {
                                    contents: HoverContents::Scalar(MarkedString::String(contents)),
                                    range: byte_span_to_range(&source, span).ok(),
                                })
                            })
                            .unwrap_or_else(|()| None),
                    )
                })
            })()
            .into_future(),
        )
    }

    fn invalid_params(&self) -> Option<Self::Error> {
        None
    }
}

fn codspan_name_to_module(name: &codespan::FileName) -> String {
    match *name {
        codespan::FileName::Virtual(ref s) => s.to_string(),
        codespan::FileName::Real(ref p) => filename_to_module(&p.display().to_string()),
    }
}

fn module_name_to_file_(s: &str) -> Result<Url, failure::Error> {
    let mut result = s.replace(".", "/");
    result.push_str(".glu");
    Ok(filename_to_url(Path::new(&result))
        .or_else(|_| url::Url::from_file_path(s))
        .map_err(|_| {
            failure::err_msg(format!("Unable to convert module name to a url: `{}`", s))
        })?)
}

fn filename_to_url(result: &Path) -> Result<Url, failure::Error> {
    let path = fs::canonicalize(&*result).or_else(|err| match env::current_dir() {
        Ok(path) => Ok(path.join(result)),
        Err(_) => Err(err),
    })?;
    Ok(url::Url::from_file_path(path).map_err(|_| {
        failure::err_msg(format!(
            "Unable to convert module name to a url: `{}`",
            result.display()
        ))
    })?)
}

fn module_name_to_file(importer: &CheckImporter, name: &codespan::FileName) -> Url {
    let s = codspan_name_to_module(name);
    importer
        .0
        .lock()
        .unwrap()
        .get(&s)
        .map(|source| source.uri.clone())
        .unwrap_or_else(|| module_name_to_file_(&s).unwrap())
}

fn with_import<F, R>(thread: &Thread, f: F) -> R
where
    F: FnOnce(&Import<CheckImporter>) -> R,
{
    let import = thread.get_macros().get("import").expect("Import macro");
    let import = import
        .downcast_ref::<Import<CheckImporter>>()
        .expect("Check importer");
    f(import)
}

fn strip_file_prefix_with_thread(thread: &Thread, url: &Url) -> String {
    with_import(thread, |import| {
        let paths = import.paths.read().unwrap();
        strip_file_prefix(&paths, url).unwrap_or_else(|err| panic!("{}", err))
    })
}

pub fn strip_file_prefix(paths: &[PathBuf], url: &Url) -> Result<String, failure::Error> {
    use std::env;

    let path = url
        .to_file_path()
        .map_err(|_| failure::err_msg("Expected a file uri"))?;
    let name = match fs::canonicalize(&*path) {
        Ok(name) => name,
        Err(_) => env::current_dir()?.join(&*path),
    };

    for path in paths {
        let canonicalized = fs::canonicalize(path).ok();

        let result = canonicalized
            .as_ref()
            .and_then(|path| name.strip_prefix(path).ok())
            .and_then(|path| path.to_str());
        if let Some(path) = result {
            return Ok(format!("{}", path));
        }
    }
    Ok(format!(
        "{}",
        name.strip_prefix(&env::current_dir()?)
            .unwrap_or_else(|_| &name)
            .display()
    ))
}

pub fn run() {
    ::env_logger::init();

    let _matches = clap::App::new("debugger")
        .version(env!("CARGO_PKG_VERSION"))
        .get_matches();

    let thread = new_vm();

    tokio::run(future::lazy(move || {
        start_server(thread, tokio::io::stdin(), tokio::io::stdout())
            .map_err(|err| panic!("{}", err))
    }))
}

fn cancelable<F, G>(f: F, g: G) -> impl Future<Item = (), Error = G::Error>
where
    F: IntoFuture,
    G: IntoFuture<Item = ()>,
{
    f.into_future()
        .then(|_| Ok(()))
        .select(g)
        .map(|_| ())
        .map_err(|err| err.0)
}

pub fn start_server<R, W>(
    thread: RootedThread,
    input: R,
    mut output: W,
) -> impl Future<Item = (), Error = failure::Error>
where
    R: tokio::io::AsyncRead + Send + 'static,
    W: tokio::io::AsyncWrite + Send + 'static,
{
    let _ = ::env_logger::try_init();

    {
        let macros = thread.get_macros();
        let mut check_import = Import::new(CheckImporter::new());
        {
            let import = macros.get("import").expect("Import macro");
            let import = import.downcast_ref::<Import>().expect("Importer");
            check_import.paths = RwLock::new((*import.paths.read().unwrap()).clone());

            let mut loaders = import.loaders.write().unwrap();
            check_import.loaders = RwLock::new(loaders.drain().collect());
        }
        macros.insert("import".into(), check_import);
    }

    let (io, exit_receiver, message_log_receiver, message_log) = server::initialize(&thread);

    let input = BufReader::new(input);

    let parts = FramedParts::new(input, rpc::LanguageServerDecoder::new());

    let request_handler_future = Framed::from_parts(parts)
        .map_err(|err| panic!("{}", err))
        .for_each(move |json| {
            debug!("Handle: {}", json);
            let message_log = message_log.clone();
            io.handle_request(&json).then(move |result| {
                if let Ok(Some(response)) = result {
                    debug!("Response: {}", response);
                    Either::A(
                        message_log
                            .clone()
                            .send(response)
                            .map(|_| ())
                            .map_err(|_| failure::err_msg("Unable to send")),
                    )
                } else {
                    Either::B(Ok(()).into_future())
                }
            })
        });

    tokio::spawn(
        message_log_receiver
            .map_err(|_| failure::err_msg("Unable to log message"))
            .for_each(move |message| -> Result<(), failure::Error> {
                Ok(write_message_str(&mut output, &message)?)
            })
            .map_err(|err| {
                error!("{}", err);
            }),
    );

    cancelable(
        exit_receiver,
        request_handler_future.map_err(|t: failure::Error| panic!("{}", t)),
    )
    .map(|t| {
        info!("Server shutdown");
        t
    })
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::path::PathBuf;

    use url::Url;

    use super::*;

    #[test]
    fn test_strip_file_prefix() {
        let renamed = strip_file_prefix(
            &[PathBuf::from(".")],
            &Url::from_file_path(env::current_dir().unwrap().join("test")).unwrap(),
        )
        .unwrap();
        assert_eq!(renamed, "test");
    }
}
