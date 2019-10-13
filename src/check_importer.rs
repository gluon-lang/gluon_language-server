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
    Error as GluonError, ModuleCompiler, Thread, ThreadExt,
};

use codespan;

use url::Url;

use crate::{name::module_name_to_file_, text_edit::TextChanges};

pub(crate) struct Module {
    pub source: Arc<codespan::FileMap>,
    pub expr: Arc<SpannedExpr<Symbol>>,
    pub uri: Url,
}

impl Module {
    pub(crate) fn empty(uri: Url) -> Module {
        Module {
            source: Arc::new(codespan::FileMap::new("".into(), "".into())),
            expr: Arc::new(pos::spanned2(0.into(), 0.into(), Expr::Error(None))),
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

    pub(crate) fn module(&self, thread: &Thread, module: &str) -> gluon::Result<Module> {
        let db = thread.get_database();
        let m = db
            .typechecked_module(module.into(), None)
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

    pub(crate) fn module(&self, thread: &Thread, module: &str) -> Option<Module> {
        self.0
            .lock()
            .unwrap()
            .get(module)
            .and_then(|s| s.module(thread, module).ok())
    }

    pub(crate) fn modules(&self, thread: &Thread) -> impl Iterator<Item = Module> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(module, s)| s.module(thread, module).ok())
            .collect::<Vec<_>>()
            .into_iter()
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

        self.0.lock().unwrap().insert(
            module_name.into(),
            State {
                uri: module_name_to_file_(module_name)
                    .map_err(|err| (None, GluonError::from(err.to_string())))?,
                version: None,
                text_changes: TextChanges::new(),
            },
        );
        // Insert a global to ensure the globals type can be looked up
        vm.global_env()
            .set_dummy_global(module_name, typ.clone(), (*metadata).clone())
            .map_err(|err| (None, GluonError::from(err).into()))?;

        Ok(typ)
    }
}
