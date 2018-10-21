use std::sync::{Arc, Mutex};

use gluon::{
    base::{
        ast::{Expr, SpannedExpr, Typed},
        fnv::FnvMap,
        pos,
        symbol::Symbol,
        types::{ArcType, Type},
    },
    compiler_pipeline::*,
    import::Importer,
    vm::macros::Error as MacroError,
    Compiler, Thread,
};

use futures::{future, sync::oneshot, Future};

use codespan;

use url::Url;

use {module_name_to_file_, text_edit::TextChanges};

pub(crate) struct Module {
    pub source: Arc<codespan::FileMap>,
    pub expr: SpannedExpr<Symbol>,
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
            expr: pos::spanned2(0.into(), 0.into(), Expr::Error(None)),
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
        compiler: &mut Compiler,
        vm: &Thread,
        _earlier_errors_exist: bool,
        module_name: &str,
        input: &str,
        mut expr: SpannedExpr<Symbol>,
    ) -> Result<(), (Option<ArcType>, MacroError)> {
        let result = MacroValue { expr: &mut expr }
            .typecheck(compiler, vm, module_name, input)
            .map(|res| res.typ);

        let typ = result.as_ref().ok().map_or_else(
            || {
                expr.try_type_of(&*vm.get_env())
                    .unwrap_or_else(|_| Type::hole())
            },
            |typ| typ.clone(),
        );

        let (metadata, _) = gluon::check::metadata::metadata(&*vm.global_env().get_env(), &expr);

        let previous = self.0.lock().unwrap().insert(
            module_name.into(),
            self::Module {
                expr: expr,
                source: compiler.get_filemap(&module_name).unwrap().clone(),
                uri: module_name_to_file_(module_name)
                    .map_err(|err| (None, err.compat().into()))?,
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
            .set_dummy_global(module_name, typ.clone(), metadata)
            .map_err(|err| (None, err.into()))?;

        result.map(|_| ()).map_err(|err| (Some(typ), err.into()))
    }
}
