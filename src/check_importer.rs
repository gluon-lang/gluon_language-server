use std::sync::{Arc, Mutex};

use gluon::{
    self,
    base::{
        ast::{Expr, SpannedExpr},
        fnv::FnvMap,
        pos,
        symbol::Symbol,
        types::ArcType,
    },
    import::Importer,
    query::Compilation,
    Error as GluonError, ModuleCompiler, Thread,
};

use futures::{future, sync::oneshot, Future};

use tokio;

use codespan;

use url::Url;

use crate::{name::module_name_to_file_, text_edit::TextChanges};

pub(crate) struct Module {
    pub source: Arc<codespan::FileMap>,
    pub expr: Arc<SpannedExpr<Symbol>>,
    pub uri: Url,
    pub dirty: bool,
    pub waiters: Vec<oneshot::Sender<()>>,
    pub version: Option<u64>,
    pub text_changes: TextChanges,
}

impl Module {
    pub(crate) fn empty(uri: Url) -> Module {
        Module {
            source: Arc::new(codespan::FileMap::new("".into(), "".into())),
            expr: Arc::new(pos::spanned2(0.into(), 0.into(), Expr::Error(None))),
            uri,
            dirty: false,
            waiters: Vec::new(),
            version: None,
            text_changes: TextChanges::new(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct CheckImporter(pub(crate) Arc<Mutex<FnvMap<String, Module>>>);
impl CheckImporter {
    pub(crate) fn new() -> CheckImporter {
        CheckImporter(Arc::new(Mutex::new(FnvMap::default())))
    }
}
impl Importer for CheckImporter {
    fn import(
        &self,
        compiler: &mut ModuleCompiler,
        vm: &Thread,
        module_name: &str,
    ) -> Result<ArcType, (Option<ArcType>, GluonError)> {
        let result = compiler
            .database
            .typechecked_module(module_name.into(), None);

        let value = result
            .or_else(|(opt, err)| opt.ok_or_else(|| err))
            .map_err(|err| (None, err))?;
        let expr = value.expr;
        let typ = value.typ;

        let (metadata, _) = gluon::check::metadata::metadata(compiler.database, &expr);

        let previous = self.0.lock().unwrap().insert(
            module_name.into(),
            self::Module {
                expr: expr.clone(),
                source: compiler.get_filemap(&module_name).unwrap().clone(),
                uri: module_name_to_file_(module_name)
                    .map_err(|err| (None, GluonError::from(err.to_string())))?,
                dirty: false,
                waiters: Vec::new(),
                version: None,
                text_changes: TextChanges::new(),
            },
        );
        if let Some(previous_module) = previous {
            tokio::spawn({
                future::join_all(
                    previous_module
                        .waiters
                        .into_iter()
                        .map(|sender| sender.send(())),
                )
                .map(|_| ())
                .map_err(|_| ())
            });
        }
        // Insert a global to ensure the globals type can be looked up
        vm.global_env()
            .set_dummy_global(module_name, typ.clone(), (*metadata).clone())
            .map_err(|err| (None, GluonError::from(err).into()))?;

        Ok(typ)
    }
}
