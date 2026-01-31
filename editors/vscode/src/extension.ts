import * as path from 'path';
import * as vscode from 'vscode';
import {
    LanguageClient,
    LanguageClientOptions,
    ServerOptions,
} from 'vscode-languageclient/node';

let client: LanguageClient;

function getServerPath(context: vscode.ExtensionContext): string {
    const config = vscode.workspace.getConfiguration('rlsp');
    const configPath = config.get<string>('server.path');
    
    if (configPath) {
        return configPath;
    }

    // Use bundled binary
    const platform = process.platform;
    const binaryName = platform === 'win32' ? 'rlsp.exe' : 'rlsp';
    return path.join(context.extensionPath, 'bin', binaryName);
}

/**
 * Send activity notification to the server for cross-file revalidation prioritization.
 */
function sendActivityNotification() {
    if (!client) {
        return;
    }

    const activeEditor = vscode.window.activeTextEditor;
    const visibleEditors = vscode.window.visibleTextEditors;

    // Only include R files
    const isRFile = (uri: vscode.Uri) => {
        const ext = path.extname(uri.fsPath).toLowerCase();
        return ['.r', '.rmd', '.qmd'].includes(ext);
    };

    const activeUri = activeEditor?.document.uri;
    const activeUriStr = activeUri && isRFile(activeUri) ? activeUri.toString() : null;

    const visibleUris = visibleEditors
        .map(e => e.document.uri)
        .filter(isRFile)
        .map(uri => uri.toString());

    client.sendNotification('rlsp/activeDocumentsChanged', {
        activeUri: activeUriStr,
        visibleUris: visibleUris,
        timestampMs: Date.now(),
    });
}

export function activate(context: vscode.ExtensionContext) {
    const serverPath = getServerPath(context);

    const serverOptions: ServerOptions = {
        command: serverPath,
        args: ['--stdio'],
    };

    // Create output channel for server logs
    const outputChannel = vscode.window.createOutputChannel('Rlsp');

    const clientOptions: LanguageClientOptions = {
        documentSelector: [
            { scheme: 'file', language: 'r' },
            { scheme: 'untitled', language: 'r' },
        ],
        synchronize: {
            fileEvents: vscode.workspace.createFileSystemWatcher('**/*.{r,R,rmd,Rmd,qmd}'),
        },
        outputChannel: outputChannel,
    };

    client = new LanguageClient(
        'rlsp',
        'Rlsp - Static R Language Server',
        serverOptions,
        clientOptions
    );

    client.start();

    // Register activity signal listeners for cross-file revalidation prioritization
    context.subscriptions.push(
        vscode.window.onDidChangeActiveTextEditor(() => {
            sendActivityNotification();
        })
    );

    context.subscriptions.push(
        vscode.window.onDidChangeVisibleTextEditors(() => {
            sendActivityNotification();
        })
    );
}

export function deactivate(): Thenable<void> | undefined {
    if (!client) {
        return undefined;
    }
    return client.stop();
}
