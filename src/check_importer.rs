use std::sync::Arc;

use gluon::{
    self,
    base::{ast::OwnedExpr, fnv::FnvMap, symbol::Symbol, types::ArcType},
    import::Importer,
    query::Compilation,
    Error as GluonError, ModuleCompiler, Thread, ThreadExt,
};

use {futures::prelude::*, tokio::sync::Mutex, url::Url};

use crate::{name::module_name_to_file_, text_edit::TextChanges};

pub(crate) struct Module {
    pub source: Arc<gluon::base::source::FileMap>,
    pub expr: Arc<OwnedExpr<Symbol>>,
    pub uri: Url,
}

pub struct State {
    pub uri: Url,
    pub version: Option<i64>,
    pub text_changes: TextChanges,
}

impl State {
    pub(crate) fn empty(uri: Url) -> State {
        State {
            uri,
            version: None,
            text_changes: TextChanges::new(),
        }
    }
}

pub(crate) async fn get_module(
    thread: &Thread,
    module: &str,
) -> gluon::Result<(Arc<gluon::base::source::FileMap>, Arc<OwnedExpr<Symbol>>)> {
    let mut db = thread.get_database();
    let m = db
        .typechecked_source_module(module.into(), None)
        .await
        .or_else(|(opt, err)| opt.ok_or(err))?;
    Ok((db.get_filemap(module).expect("Filemap"), m.expr.clone()))
}

#[derive(Clone)]
pub(crate) struct CheckImporter(pub(crate) Arc<Mutex<FnvMap<String, State>>>);
impl CheckImporter {
    pub(crate) fn new() -> CheckImporter {
        CheckImporter(Arc::new(Mutex::new(FnvMap::default())))
    }

    pub(crate) async fn module(&self, thread: &Thread, module: &str) -> Option<Module> {
        let (source, expr) = get_module(thread, module).await.ok()?;

        let uri = {
            let map = self.0.lock().await;
            map.get(module)?.uri.clone()
        };

        Some(Module { source, expr, uri })
    }

    pub(crate) async fn modules(&self, thread: &Thread) -> impl Iterator<Item = Module> {
        let uris = self
            .0
            .lock()
            .await
            .iter()
            .map(|(module, s)| (module.clone(), s.uri.clone()))
            .collect::<Vec<_>>();

        futures::stream::iter(uris)
            .filter_map(|(module, uri)| async move {
                let (source, expr) = get_module(thread, &module).await.ok()?;
                Some(Module { source, expr, uri })
            })
            .collect::<Vec<_>>()
            .await
            .into_iter()
    }
}

#[async_trait::async_trait]
impl Importer for CheckImporter {
    async fn import(
        &self,
        compiler: &mut ModuleCompiler<'_>,
        _: &Thread,
        module_name: &str,
    ) -> Result<ArcType, (Option<ArcType>, GluonError)> {
        let typ = compiler
            .database
            .module_type(module_name.into(), None)
            .await
            .map_err(|err| (None, err))?;
        compiler
            .database
            .module_metadata(module_name.into(), None)
            .await
            .map_err(|err| (None, err))?;

        self.0.lock().await.insert(
            module_name.into(),
            State {
                uri: module_name_to_file_(module_name)
                    .map_err(|err| (None, GluonError::from(err.to_string())))?,
                version: None,
                text_changes: TextChanges::new(),
            },
        );

        Ok(typ)
    }
}
