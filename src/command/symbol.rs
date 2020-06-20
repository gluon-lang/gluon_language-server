use super::*;

use languageserver_types::WorkspaceSymbolParams;

use crate::completion;

pub fn register(io: &mut IoHandler, thread: &RootedThread) {
    let thread = thread.clone();
    let f = move |params: WorkspaceSymbolParams| {
        let thread = thread.clone();
        async move {
            let import = thread.get_macros().get("import").expect("Import macro");
            let import = import
                .downcast_ref::<Import<CheckImporter>>()
                .expect("Check importer");

            let mut symbols = Vec::<SymbolInformation>::new();

            for module in import.importer.modules(&thread).await {
                let source = &module.source;

                let expr = module.expr.expr();

                symbols.extend(
                    completion::all_symbols(module.source.span(), expr)
                        .into_iter()
                        .filter(|symbol| symbol.value.name.declared_name().contains(&params.query))
                        .map(|symbol| {
                            completion_symbol_to_symbol_information(
                                &source,
                                symbol,
                                module.uri.clone(),
                            )
                        })
                        .collect::<Result<Vec<_>, _>>()?,
                );
            }

            Ok(Some(symbols))
        }
    };
    io.add_async_method(request!("workspace/symbol"), f);
}
