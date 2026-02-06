import * as path from 'path';
import * as vscode from 'vscode';
import {
    LanguageClient,
    LanguageClientOptions,
    ServerOptions,
} from 'vscode-languageclient/node';

/**
 * Severity level for diagnostic messages.
 * Maps to LSP DiagnosticSeverity values.
 */
type SeverityLevel = "error" | "warning" | "information" | "hint";

/**
 * Initialization options passed to the Raven LSP server.
 * This interface matches the JSON structure expected by the server's
 * `parse_cross_file_config()` and related parsing functions.
 */
interface RavenInitializationOptions {
    crossFile?: {
        maxBackwardDepth?: number;
        maxForwardDepth?: number;
        maxChainDepth?: number;
        assumeCallSite?: "start" | "end";
        indexWorkspace?: boolean;
        maxRevalidationsPerTrigger?: number;
        revalidationDebounceMs?: number;
        missingFileSeverity?: SeverityLevel;
        circularDependencySeverity?: SeverityLevel;
        outOfScopeSeverity?: SeverityLevel;
        ambiguousParentSeverity?: SeverityLevel;
        maxChainDepthSeverity?: SeverityLevel;
        onDemandIndexing?: {
            enabled?: boolean;
            maxTransitiveDepth?: number;
            maxQueueSize?: number;
        };
        cache?: {
            metadataMaxEntries?: number;
            fileContentMaxEntries?: number;
            existenceMaxEntries?: number;
            workspaceIndexMaxEntries?: number;
        };
    };
    diagnostics?: {
        enabled?: boolean;
        undefinedVariables?: boolean;
    };
    packages?: {
        enabled?: boolean;
        additionalLibraryPaths?: string[];
        rPath?: string;
        missingPackageSeverity?: SeverityLevel;
    };
}

/**
 * Check if a setting is explicitly configured (not using default).
 * Uses config.inspect() to determine if the setting has a value at any scope.
 */
function isExplicitlyConfigured<T>(config: vscode.WorkspaceConfiguration, key: string): boolean {
    const inspection = config.inspect<T>(key);
    if (!inspection) {
        return false;
    }
    // A setting is explicitly configured if it has a value at any scope
    return (
        inspection.globalValue !== undefined ||
        inspection.workspaceValue !== undefined ||
        inspection.workspaceFolderValue !== undefined ||
        inspection.globalLanguageValue !== undefined ||
        inspection.workspaceLanguageValue !== undefined ||
        inspection.workspaceFolderLanguageValue !== undefined
    );
}

/**
 * Get a setting value only if it's explicitly configured.
 * Returns undefined if the setting is using its default value.
 */
function getExplicitSetting<T>(config: vscode.WorkspaceConfiguration, key: string): T | undefined {
    if (isExplicitlyConfigured<T>(config, key)) {
        return config.get<T>(key);
    }
    return undefined;
}

/**
 * Read all raven.* settings from VS Code configuration and construct
 * the initializationOptions object for the LSP server.
 * Only includes settings that are explicitly configured (omits defaults).
 */
function getInitializationOptions(): RavenInitializationOptions {
    const config = vscode.workspace.getConfiguration('raven');
    const options: RavenInitializationOptions = {};

    // Cross-file depth settings
    const maxBackwardDepth = getExplicitSetting<number>(config, 'crossFile.maxBackwardDepth');
    const maxForwardDepth = getExplicitSetting<number>(config, 'crossFile.maxForwardDepth');
    const maxChainDepth = getExplicitSetting<number>(config, 'crossFile.maxChainDepth');
    const assumeCallSite = getExplicitSetting<"start" | "end">(config, 'crossFile.assumeCallSite');
    const indexWorkspace = getExplicitSetting<boolean>(config, 'crossFile.indexWorkspace');
    const maxRevalidationsPerTrigger = getExplicitSetting<number>(config, 'crossFile.maxRevalidationsPerTrigger');
    const revalidationDebounceMs = getExplicitSetting<number>(config, 'crossFile.revalidationDebounceMs');
    const missingFileSeverity = getExplicitSetting<SeverityLevel>(config, 'crossFile.missingFileSeverity');
    const circularDependencySeverity = getExplicitSetting<SeverityLevel>(config, 'crossFile.circularDependencySeverity');
    const outOfScopeSeverity = getExplicitSetting<SeverityLevel>(config, 'crossFile.outOfScopeSeverity');
    const ambiguousParentSeverity = getExplicitSetting<SeverityLevel>(config, 'crossFile.ambiguousParentSeverity');
    const maxChainDepthSeverity = getExplicitSetting<SeverityLevel>(config, 'crossFile.maxChainDepthSeverity');

    // On-demand indexing settings
    const onDemandEnabled = getExplicitSetting<boolean>(config, 'crossFile.onDemandIndexing.enabled');
    const onDemandMaxTransitiveDepth = getExplicitSetting<number>(config, 'crossFile.onDemandIndexing.maxTransitiveDepth');
    const onDemandMaxQueueSize = getExplicitSetting<number>(config, 'crossFile.onDemandIndexing.maxQueueSize');

    // Build onDemandIndexing object only if any setting is configured
    let onDemandIndexing: {
        enabled?: boolean;
        maxTransitiveDepth?: number;
        maxQueueSize?: number;
    } | undefined = undefined;
    if (onDemandEnabled !== undefined || onDemandMaxTransitiveDepth !== undefined || onDemandMaxQueueSize !== undefined) {
        onDemandIndexing = {};
        if (onDemandEnabled !== undefined) {
            onDemandIndexing.enabled = onDemandEnabled;
        }
        if (onDemandMaxTransitiveDepth !== undefined) {
            onDemandIndexing.maxTransitiveDepth = onDemandMaxTransitiveDepth;
        }
        if (onDemandMaxQueueSize !== undefined) {
            onDemandIndexing.maxQueueSize = onDemandMaxQueueSize;
        }
    }

    // Read cache settings
    const cacheMetadataMaxEntries = getExplicitSetting<number>(config, 'crossFile.cache.metadataMaxEntries');
    const cacheFileContentMaxEntries = getExplicitSetting<number>(config, 'crossFile.cache.fileContentMaxEntries');
    const cacheExistenceMaxEntries = getExplicitSetting<number>(config, 'crossFile.cache.existenceMaxEntries');
    const cacheWorkspaceIndexMaxEntries = getExplicitSetting<number>(config, 'crossFile.cache.workspaceIndexMaxEntries');

    // Build cache object only if any cache setting is configured
    let cache: {
        metadataMaxEntries?: number;
        fileContentMaxEntries?: number;
        existenceMaxEntries?: number;
        workspaceIndexMaxEntries?: number;
    } | undefined = undefined;
    if (cacheMetadataMaxEntries !== undefined || cacheFileContentMaxEntries !== undefined ||
        cacheExistenceMaxEntries !== undefined || cacheWorkspaceIndexMaxEntries !== undefined) {
        cache = {};
        if (cacheMetadataMaxEntries !== undefined) {
            cache.metadataMaxEntries = cacheMetadataMaxEntries;
        }
        if (cacheFileContentMaxEntries !== undefined) {
            cache.fileContentMaxEntries = cacheFileContentMaxEntries;
        }
        if (cacheExistenceMaxEntries !== undefined) {
            cache.existenceMaxEntries = cacheExistenceMaxEntries;
        }
        if (cacheWorkspaceIndexMaxEntries !== undefined) {
            cache.workspaceIndexMaxEntries = cacheWorkspaceIndexMaxEntries;
        }
    }

    // Build crossFile object only if any setting is configured
    if (
        maxBackwardDepth !== undefined ||
        maxForwardDepth !== undefined ||
        maxChainDepth !== undefined ||
        assumeCallSite !== undefined ||
        indexWorkspace !== undefined ||
        maxRevalidationsPerTrigger !== undefined ||
        revalidationDebounceMs !== undefined ||
        missingFileSeverity !== undefined ||
        circularDependencySeverity !== undefined ||
        outOfScopeSeverity !== undefined ||
        ambiguousParentSeverity !== undefined ||
        maxChainDepthSeverity !== undefined ||
        onDemandIndexing !== undefined ||
        cache !== undefined
    ) {
        options.crossFile = {};
        if (maxBackwardDepth !== undefined) {
            options.crossFile.maxBackwardDepth = maxBackwardDepth;
        }
        if (maxForwardDepth !== undefined) {
            options.crossFile.maxForwardDepth = maxForwardDepth;
        }
        if (maxChainDepth !== undefined) {
            options.crossFile.maxChainDepth = maxChainDepth;
        }
        if (assumeCallSite !== undefined) {
            options.crossFile.assumeCallSite = assumeCallSite;
        }
        if (indexWorkspace !== undefined) {
            options.crossFile.indexWorkspace = indexWorkspace;
        }
        if (maxRevalidationsPerTrigger !== undefined) {
            options.crossFile.maxRevalidationsPerTrigger = maxRevalidationsPerTrigger;
        }
        if (revalidationDebounceMs !== undefined) {
            options.crossFile.revalidationDebounceMs = revalidationDebounceMs;
        }
        if (missingFileSeverity !== undefined) {
            options.crossFile.missingFileSeverity = missingFileSeverity;
        }
        if (circularDependencySeverity !== undefined) {
            options.crossFile.circularDependencySeverity = circularDependencySeverity;
        }
        if (outOfScopeSeverity !== undefined) {
            options.crossFile.outOfScopeSeverity = outOfScopeSeverity;
        }
        if (ambiguousParentSeverity !== undefined) {
            options.crossFile.ambiguousParentSeverity = ambiguousParentSeverity;
        }
        if (maxChainDepthSeverity !== undefined) {
            options.crossFile.maxChainDepthSeverity = maxChainDepthSeverity;
        }
        if (onDemandIndexing !== undefined) {
            options.crossFile.onDemandIndexing = onDemandIndexing;
        }
        if (cache !== undefined) {
            options.crossFile.cache = cache;
        }
    }

    // Diagnostics settings
    // Always send diagnostics.enabled since it's a master switch that affects all diagnostics
    const diagnosticsEnabled = config.get<boolean>('diagnostics.enabled');
    const undefinedVariables = getExplicitSetting<boolean>(config, 'diagnostics.undefinedVariables');
    
    // Always include diagnostics section with enabled value
    options.diagnostics = {
        enabled: diagnosticsEnabled,
    };
    if (undefinedVariables !== undefined) {
        options.diagnostics.undefinedVariables = undefinedVariables;
    }

    // Package settings
    const packagesEnabled = getExplicitSetting<boolean>(config, 'packages.enabled');
    const additionalLibraryPaths = getExplicitSetting<string[]>(config, 'packages.additionalLibraryPaths');
    const rPath = getExplicitSetting<string>(config, 'packages.rPath');
    const missingPackageSeverity = getExplicitSetting<SeverityLevel>(config, 'packages.missingPackageSeverity');

    if (
        packagesEnabled !== undefined ||
        additionalLibraryPaths !== undefined ||
        rPath !== undefined ||
        missingPackageSeverity !== undefined
    ) {
        options.packages = {};
        if (packagesEnabled !== undefined) {
            options.packages.enabled = packagesEnabled;
        }
        if (additionalLibraryPaths !== undefined) {
            options.packages.additionalLibraryPaths = additionalLibraryPaths;
        }
        if (rPath !== undefined) {
            options.packages.rPath = rPath;
        }
        if (missingPackageSeverity !== undefined) {
            options.packages.missingPackageSeverity = missingPackageSeverity;
        }
    }

    return options;
}

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
        initializationOptions: getInitializationOptions(),
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

    // Register configuration change listener
    context.subscriptions.push(
        vscode.workspace.onDidChangeConfiguration((event) => {
            if (event.affectsConfiguration('raven')) {
                // Send updated configuration to LSP server
                const settings = getInitializationOptions();
                client.sendNotification('workspace/didChangeConfiguration', {
                    settings: settings
                });
            }
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
