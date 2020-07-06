use std::{env, io, path::Path, process::Command};

fn main() {
    let git_commit = env::var("GIT_COMMIT")
        .or_else(|_| -> io::Result<_> {
            let output = Command::new("git").args(&["rev-parse", "HEAD"]).output()?;
            Ok(String::from_utf8(output.stdout).unwrap())
        })
        .expect("Unable to retrieve git revision");

    if !git_commit.is_empty() {
        println!("cargo:rustc-env=GIT_COMMIT={}", git_commit);
    }

    const EDIT_MSG: &str = "../.git/COMMIT_EDITMSG";
    if Path::new(EDIT_MSG).exists() {
        // This is the closest thing to making sure we rebuild this every time a new commit is made
        println!("cargo:rerun-if-changed={}", EDIT_MSG);
    }
}
