# vscode-gluon

An extension for for [Visual Studio Code][] which provides syntax hightlighting and completion for the programming language [gluon][].

## Installing the language server

The language server is available at [crates.io][] and can be installed by running `cargo install gluon_language-server`.

```json
{
    // Specifies where the language server is found. By default this looks for "gluon_language-server" in $PATH
    "gluon.language-server.path": "gluon_language-server"
}
```

[Visual Studio Code]:https://code.visualstudio.com/
[gluon]:https://github.com/Marwes/gluon
[crates.io]:https://crates.io/