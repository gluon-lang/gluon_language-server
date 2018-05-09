'use strict';

import * as path from 'path';


import * as vscode from 'vscode';
import { workspace, Disposable, ExtensionContext } from 'vscode';
import { LanguageClient, LanguageClientOptions, SettingMonitor, ServerOptions, TransportKind } from 'vscode-languageclient';
import { Trace } from 'vscode-jsonrpc';

function activateDebugger(context: ExtensionContext) {

    let disposable = vscode.commands.registerCommand('extension.getProgramName', () => {
        return vscode.window.showInputBox({
            placeHolder: "Please enter the name of a text file in the workspace folder",
            value: "main.glu"
        });
    });
    context.subscriptions.push(disposable);

    const initialConfigurations = [
        {
            name: 'Gluon-Debug',
            type: 'gluon',
            request: 'launch',
            program: '${workspaceRoot}/${command.AskForProgramName}',
            stopOnEntry: true
        }
    ]

    context.subscriptions.push(vscode.commands.registerCommand('extension.provideInitialDebugConfigurations', () => {
        return JSON.stringify(initialConfigurations);
    }));
}

export function activate(context: ExtensionContext) {

    activateDebugger(context);

    let config = workspace.getConfiguration("gluon");
    let serverPath = config.get("language-server.path", "gluon_language-server");

    // If the extension is launched in debug mode then the debug server options are used
    // Otherwise the run options are used
    let serverOptions: ServerOptions = {
        command: serverPath,
        args: [],
        options: {
            env: {
                "RUST_BACKTRACE": "1",
                "RUST_LOG": "gluon_language_server=debug",
            }
        }
    }

    // Options to control the language client
    let clientOptions: LanguageClientOptions = {
        // Register the server for plain text documents
        documentSelector: ['gluon'],
        synchronize: {
            // Synchronize the setting section 'languageServerExample' to the server
            configurationSection: 'gluon',
            // Notify the server about file changes to '.clientrc files contain in the workspace
            fileEvents: workspace.createFileSystemWatcher('**/.clientrc')
        }
    }

    // Create the language client and start the client.
    let client = new LanguageClient('gluon', serverOptions, clientOptions);
    let disposable = client.start();

    // Push the disposable to the context's subscriptions so that the 
    // client can be deactivated on extension deactivation
    context.subscriptions.push(disposable);
}
