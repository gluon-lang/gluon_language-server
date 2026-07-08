'use strict';

import * as vscode from 'vscode';
import { workspace, ExtensionContext } from 'vscode';
import { LanguageClient, LanguageClientOptions, ServerOptions } from 'vscode-languageclient/node';

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
    let serverPath = process.env["__GLUON_LANGUAGE_SERVER_DEBUG"] || config.get("language-server.path", "gluon_language-server");

    // If the extension is launched in debug mode then the debug server options are used
    // Otherwise the run options are used
    let serverOptions: ServerOptions = {
        command: serverPath,
        args: [],
        options: {
            env: process.env["__GLUON_LANGUAGE_SERVER_DEBUG"] ? {
                "RUST_BACKTRACE": "1",
                "RUST_LOG": "gluon_language_server=debug",
            } : undefined
        }
    }

    // Options to control the language client
    let clientOptions: LanguageClientOptions = {
        // Register the server for plain text documents
        documentSelector: [{
            scheme: 'file',
            language: 'gluon',
        }],
        synchronize: {
            // Synchronize the setting section 'languageServerExample' to the server
            configurationSection: 'gluon',
            // Notify the server about file changes to '.clientrc files contain in the workspace
            fileEvents: workspace.createFileSystemWatcher('**/.clientrc')
        }
    }

    // Create the language client and start the client.
    let client = new LanguageClient('gluon', 'Gluon Language Server', serverOptions, clientOptions);
    client.start();

    const restartLanguageServer = vscode.commands.registerCommand('gluon.restartLanguageServer', async () => {
        try {
            await client.restart();
            vscode.window.showInformationMessage('Gluon language server restarted.');
        } catch (error) {
            const message = error instanceof Error ? error.message : String(error);
            vscode.window.showErrorMessage(`Failed to restart Gluon language server: ${message}`);
        }
    });

    // Push the client to the context's subscriptions so that it
    // is stopped and disposed on extension deactivation.
    context.subscriptions.push(client);
    context.subscriptions.push(restartLanguageServer);
}
