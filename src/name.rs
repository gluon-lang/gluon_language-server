//! Name translation between LSP and gluon

use std::{
    env, fs,
    path::{Path, PathBuf},
};

use gluon::{base::filename_to_module, import::Import, Thread};

use check_importer::CheckImporter;

use codespan;
use failure;
use url::{self, Url};

fn codspan_name_to_module(name: &codespan::FileName) -> String {
    match *name {
        codespan::FileName::Virtual(ref s) => s.to_string(),
        codespan::FileName::Real(ref p) => filename_to_module(&p.display().to_string()),
    }
}

pub(crate) fn module_name_to_file_(s: &str) -> Result<Url, failure::Error> {
    let mut result = s.replace(".", "/");
    result.push_str(".glu");
    Ok(filename_to_url(Path::new(&result))
        .or_else(|_| url::Url::from_file_path(s))
        .map_err(|_| {
            failure::err_msg(format!("Unable to convert module name to a url: `{}`", s))
        })?)
}

pub(crate) fn filename_to_url(result: &Path) -> Result<Url, failure::Error> {
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

pub(crate) fn module_name_to_file(importer: &CheckImporter, name: &codespan::FileName) -> Url {
    let s = codspan_name_to_module(name);
    importer
        .0
        .lock()
        .unwrap()
        .get(&s)
        .map(|source| source.uri.clone())
        .unwrap_or_else(|| module_name_to_file_(&s).unwrap())
}

pub(crate) fn with_import<F, R>(thread: &Thread, f: F) -> R
where
    F: FnOnce(&Import<CheckImporter>) -> R,
{
    let import = thread.get_macros().get("import").expect("Import macro");
    let import = import
        .downcast_ref::<Import<CheckImporter>>()
        .expect("Check importer");
    f(import)
}

pub(crate) fn strip_file_prefix_with_thread(thread: &Thread, url: &Url) -> String {
    with_import(thread, |import| {
        let paths = import.paths.read().unwrap();
        strip_file_prefix(&paths, url).unwrap_or_else(|err| panic!("{}", err))
    })
}

pub(crate) fn strip_file_prefix(paths: &[PathBuf], url: &Url) -> Result<String, failure::Error> {
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
