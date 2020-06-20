use std::sync::Arc;

use gluon::{
    self,
    base::{ast::OwnedExpr, fnv::FnvMap, symbol::Symbol, types::ArcType},
    import::Importer,
    query::Compilation,
    Error as GluonError, ModuleCompiler, Thread, ThreadExt,
};

use {futures::prelude::*, tokio_02::sync::Mutex, url::Url};

use crate::{name::module_name_to_file_, text_edit::TextChanges};

pub(crate) struct Module {
    pub source: Arc<codespan::FileMap>,
    pub expr: Arc<OwnedExpr<Symbol>>,
    pub uri: Url,
}

impl Module {
    pub(crate) fn empty(uri: Url) -> Module {
        Module {
            source: Arc::new(codespan::FileMap::new("".into(), "".into())),
            expr: Default::default(),
            uri,
        }
    }
}

pub struct State {
    pub uri: Url,
    pub version: Option<u64>,
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

    pub(crate) async fn module(&self, thread: &Thread, module: &str) -> gluon::Result<Module> {
        let mut db = thread.get_database();
        let m = db
            .typechecked_source_module(module.into(), None)
            .await
            .or_else(|(partial_value, err)| partial_value.ok_or(err))?;
        Ok(Module {
            source: db.get_filemap(module).expect("Filemap"),
            expr: m.expr.clone(),
            ..Module::empty(self.uri.clone())
        })
    }
}

#[derive(Clone)]
pub(crate) struct CheckImporter(pub(crate) Arc<Mutex<FnvMap<String, State>>>);
impl CheckImporter {
    pub(crate) fn new() -> CheckImporter {
        CheckImporter(Arc::new(Mutex::new(FnvMap::default())))
    }

    pub(crate) async fn module(&self, thread: &Thread, module: &str) -> Option<Module> {
        let map = self.0.lock().await;
        let s = map.get(module)?;
        s.module(thread, module).await.ok()
    }

    pub(crate) async fn modules(&self, thread: &Thread) -> impl Iterator<Item = Module> {
        let map = self.0.lock().await;

        futures::stream::iter(map.iter())
            .filter_map(|(module, s)| async move { s.module(thread, module).await.ok() })
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
