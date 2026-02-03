import * as path from 'path';
import * as vscode from 'vscode';
import {
    LanguageClient,
    LanguageClientOptions,
    ServerOptions,
} from 'vscode-languageclient/node';

let client: LanguageClient;

function getServerPath(context: vscode.ExtensionContext): string {
    const config = vscode.workspace.getConfiguration('raven');
    const configPath = config.get<string>('server.path');
    
    if (configPath) {
        return configPath;
    }

    // Use bundled binary
    const platform = process.platform;
    const binaryName = platform === 'win32' ? 'raven.exe' : 'raven';
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

    client.sendNotification('raven/activeDocumentsChanged', {
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
    const outputChannel = vscode.window.createOutputChannel('Raven');

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
        'raven',
        'Raven - R Language Server',
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

    // Prompt for word separators configuration
    promptWordSeparators(context);
}

async function promptWordSeparators(context: vscode.ExtensionContext) {
    const WORD_SEPARATORS = "`~!@#$%^&*()-=+[{]}\\|;:'\",<>/?";
    
    const config = vscode.workspace.getConfiguration('raven');
    const setting = config.get<string>('editor.dotInWordSeparators', 'ask');

    // If set to 'yes', ensure the setting is applied
    if (setting === 'yes') {
        await ensureWordSeparators(WORD_SEPARATORS);
        return;
    }

    // If set to 'no', do nothing
    if (setting === 'no') {
        return;
    }

    // If set to 'ask', check if we should prompt
    const wsConfig = vscode.workspace.getConfiguration();
    const rConfig = wsConfig.inspect('[r]');
    const hasWordSeparators = 
        (rConfig?.globalValue as any)?.['editor.wordSeparators'] !== undefined ||
        (rConfig?.workspaceValue as any)?.['editor.wordSeparators'] !== undefined ||
        (rConfig?.workspaceFolderValue as any)?.['editor.wordSeparators'] !== undefined;

    if (hasWordSeparators) {
        return;
    }

    // Show prompt
    const choice = await vscode.window.showInformationMessage(
        'This extension can treat dots as part of words in R files by updating editor.wordSeparators for [r]. Enable this behavior?',
        'Enable',
        'No thanks'
    );

    if (choice === 'Enable') {
        await config.update('editor.dotInWordSeparators', 'yes', vscode.ConfigurationTarget.Global);
        await ensureWordSeparators(WORD_SEPARATORS);
        
        const reload = await vscode.window.showInformationMessage(
            'R word separators updated: dots will now be part of words in R files. Reload window to apply?',
            'Reload',
            'Later'
        );
        if (reload === 'Reload') {
            vscode.commands.executeCommand('workbench.action.reloadWindow');
        }
    } else if (choice === 'No thanks') {
        await config.update('editor.dotInWordSeparators', 'no', vscode.ConfigurationTarget.Global);
    }
}

async function ensureWordSeparators(wordSeparators: string) {
    const config = vscode.workspace.getConfiguration();
    const currentRConfig = config.get('[r]', {}) as Record<string, any>;
    
    // Only update if not already set correctly
    if (currentRConfig['editor.wordSeparators'] !== wordSeparators) {
        const updatedRConfig = {
            ...currentRConfig,
            'editor.wordSeparators': wordSeparators
        };
        await config.update('[r]', updatedRConfig, vscode.ConfigurationTarget.Global);
    }
}

export function deactivate(): Thenable<void> | undefined {
    if (!client) {
        return undefined;
    }
    return client.stop();
}
