use super::*;

use languageserver_types::WorkspaceSymbolParams;

use completion;

pub fn register(io: &mut IoHandler, thread: &RootedThread) {
    let thread = thread.clone();
    let f = move |params: WorkspaceSymbolParams| -> _ {
        let import = thread.get_macros().get("import").expect("Import macro");
        let import = import
            .downcast_ref::<Import<CheckImporter>>()
            .expect("Check importer");
        let modules = import.importer.0.lock().unwrap();

        let mut symbols = Vec::<SymbolInformation>::new();

        for module in modules.values() {
            let source = &module.source;

            symbols.extend(
                completion::all_symbols(module.source.span(), &module.expr)
                    .into_iter()
                    .filter(|symbol| match symbol.value {
                        CompletionSymbol::Value { ref name, .. }
                        | CompletionSymbol::Type { ref name, .. } => {
                            name.declared_name().contains(&params.query)
                        }
                    })
                    .map(|symbol| {
                        completion_symbol_to_symbol_information(&source, symbol, module.uri.clone())
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            );
        }

        Ok(Some(symbols))
    };
    io.add_async_method(request!("workspace/symbol"), f);
}
