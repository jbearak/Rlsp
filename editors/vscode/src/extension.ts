import * as path from 'path';
import * as vscode from 'vscode';
import {
    ExecuteCommandRequest,
    LanguageClient,
    LanguageClientOptions,
    ServerOptions,
} from 'vscode-languageclient/node';
import { activateHelpViewer, wrapHoverWithHelpTrust } from './help';
import { registerAutoCloseFix } from './autoCloseFix';
import { registerScaffoldCommands, renderRavenToml } from './scaffold';
import {
    getInitializationOptions as buildInitializationOptions,
    RavenInitializationOptions,
} from './initializationOptions';
import {
    buildDocumentIndentUnitsPayload,
    clearIneligibleDiagnostics,
    diagnosticResourceUris,
    forgetResolvedEditorOptions,
    getUpdatedGlobalLanguageConfig,
    invalidateResolvedEditorOptions,
    isIndentUnitDocument,
    isRDocument,
    planDotInWordMigration,
    resolveFormatOnTypeForDocument,
    resolveInsertSpacesForDocument,
    resolveTabSizeForDocument,
} from './extensionHelpers';
import { registerRunningStateReconciliation } from './client-state';
import {
    shouldTriggerDirectivePathSuggest,
    shouldTriggerNestedPathSuggest,
} from './pathCompletionTriggers';
import {
    register_r_terminal,
    register_send_to_r_commands,
    register_inspection_commands,
    get_or_create_r_terminal,
    _dispose_cached_r_terminal_for_test,
} from './send-to-r';
import { register_build_commands } from './build-commands';
import {
    register_chunks_navigation_and_highlight,
    register_chunks_with_terminal,
} from './chunks';
import { registerRSnippetCompletionsForRmdAndQuarto } from './r-snippet-provider';
import { register_r_package_detection } from './r-package-detection';
import { PlotServices } from './plot';
import { registerDataViewer, dataViewerDirOf } from './data-viewer';
import type { DataViewerManager } from './data-viewer/manager';
import {
    detectAutoDisableReason,
    notifyAutoDisable,
    readRConsoleActivation,
    registerActivationReactivity,
    resolveRConsoleActivation,
} from './r-console-activation';
import { registerKnit, disposeKnitGrammarRegistryForDeactivation } from './knit';
import {
    registerQuarto,
    stopAllQuartoForDeactivation,
} from './quarto';
import { validateServerBinary } from './server-binary-check';
import { dotLintrAutoEnableAllowed } from './lintr-auto-enable';

/**
 * Read all raven.* settings from VS Code configuration and construct
 * the initializationOptions object for the LSP server.
 * Explicit settings are forwarded, and master defaults like diagnostics.enabled
 * are included when the server contract requires them.
 */
function getInitializationOptions(): RavenInitializationOptions {
    const options = buildInitializationOptions(
        vscode.workspace.getConfiguration('raven'),
        // Client-only environment signal gating `.lintr` auto-enable (#337).
        // Recomputed on every call so it tracks REditorSupport / Positron and
        // `r.lsp.*` state when settings change (see the config listener below).
        dotLintrAutoEnableAllowed(),
    );
    return {
        ...options,
        // Seed the server before vscode-languageclient synchronizes didOpen
        // documents, preventing even a startup-only hidden-diagnostic pass.
        diagnosticUris: diagnosticResourceUris(),
    };
}

let client: LanguageClient;
const R_CONSOLE_ENABLED_CONTEXT = 'raven.rConsoleEnabled';
const WORD_SEPARATORS = "`~!@#$%^&*()-=+[{]}\\|;:'\",<>/?";

/**
 * Languages whose `editor.wordSeparators` we override so that dots are part of
 * a word — affecting double-click selection and word-wise cursor motion over
 * dotted names like `my.variable`.
 *
 * `rmd` and `quarto` are deliberately EXCLUDED, even though their
 * `rmd-language-configuration.json` shares R's `wordPattern` and therefore
 * already gets the *other* half of this feature (the pattern drives
 * Cmd+click / `getWordRangeAtPosition` across dotted names — see the
 * wordPattern tests in `lsp.test.ts`).
 *
 * The asymmetry is intentional: `editor.wordSeparators` resolves per *document*
 * language, not per embedded code chunk. An `.Rmd` / `.Rmarkdown` / `.qmd`
 * file is a single `rmd`/`quarto` document mixing Markdown prose with R
 * chunks, so overriding `[rmd]`/`[quarto]` would change word selection in the
 * prose too, not just the R code. We scope the separator override to pure
 * R/JAGS documents and let the shared `wordPattern` cover dotted-name
 * navigation inside `.Rmd` / `.Rmarkdown` / `.qmd`.
 */
const DOT_IN_WORD_LANGUAGE_IDS = ['r', 'jags'] as const;

function hasWordSeparatorsOverride(configValue: unknown): boolean {
    return typeof configValue === 'object'
        && configValue !== null
        && 'editor.wordSeparators' in configValue;
}

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
 * Send raven/documentIndentUnitsChanged for the open indent-unit documents.
 * The payload carries producer-relevant `editor.insertSpaces` and
 * `editor.formatOnType` state separately from the v0.14-compatible unit list.
 * See `buildDocumentIndentUnitsPayload` for the wire-compatibility invariant.
 */
function sendDocumentIndentUnitsNotification() {
    // Fires from document/editor listeners that can run before the async
    // client.start() resolves, or when it never started (unusable binary).
    // sendNotification throws "Client is not running" in those states.
    if (!client || !client.isRunning()) {
        return;
    }

    const ravenCfg = vscode.workspace.getConfiguration('raven');
    const setting = ravenCfg.get<number | 'auto'>('linting.indentationUnit', 'auto');

    const payload = buildDocumentIndentUnitsPayload(
        setting,
        vscode.workspace.textDocuments,
        resolveTabSizeForDocument,
        resolveInsertSpacesForDocument,
        resolveFormatOnTypeForDocument,
    );
    client.sendNotification('raven/documentIndentUnitsChanged', payload);
}

/**
 * Send editor activity and diagnostic-resource ownership to the server.
 */
function sendActivityNotification() {
    const diagnosticUris = diagnosticResourceUris();

    // vscode-languageclient keeps this collection across automatic crash
    // restarts. Prune it even while the server is down so a tab closed during
    // downtime cannot leave stale Problems behind.
    clearIneligibleDiagnostics(client?.diagnostics, diagnosticUris);

    // Same guard as sendDocumentIndentUnitsNotification: editor-activity
    // listeners can fire before the client starts (or when it never did),
    // and sendNotification throws unless the client is running.
    if (!client || !client.isRunning()) {
        return;
    }

    const activeEditor = vscode.window.activeTextEditor;
    const visibleEditors = vscode.window.visibleTextEditors;

    const activeDocument = activeEditor?.document;
    const activeUriStr = activeDocument && isRDocument(activeDocument)
        ? activeDocument.uri.toString()
        : null;

    const visibleUris = visibleEditors
        .map(editor => editor.document)
        .filter(isRDocument)
        .map(document => document.uri.toString());

    client.sendNotification('raven/activeDocumentsChanged', {
        activeUri: activeUriStr,
        visibleUris: visibleUris,
        diagnosticUris,
        timestampMs: Date.now(),
    });
}

/** Resend all editor-owned document state after a language-client start. */
function synchronizeClientDocumentState() {
    sendDocumentIndentUnitsNotification();
    sendActivityNotification();
}

/**
 * Public extension API surface, returned from `activate()` and reachable
 * from other extensions and the test harness via
 * `vscode.extensions.getExtension('jbearak.raven-r').exports`.
 *
 * The only consumer today is the Mocha test suite, which uses the live
 * LanguageClient to round-trip `workspace/executeCommand` calls (e.g.
 * `raven.getHelpHtml`) that are intentionally NOT registered as VS Code
 * commands per the executeCommandProvider rule in CLAUDE.md.
 */
export interface RavenExtensionApi {
    /** Returns the live LSP client once activation has installed it. */
    getLanguageClient(): LanguageClient | undefined;
    /**
     * Creates (or reuses) a Raven-managed R terminal with the bootstrap
     * profile injected, then sends `code` to it. Used by integration tests.
     */
    sendToRTerminal(code: string): Promise<void>;
    /** Names of currently-open data viewer panels. Used by integration tests. */
    getDataViewerPanelNames(): string[];
    /** Column names for a named data viewer panel. Used by integration tests. */
    getDataViewerPanelColumnNames(panelName: string): string[] | undefined;
    /** Latest visible-row range for a data viewer panel, or undefined if
     *  none has arrived yet. Used by integration tests to verify scroll
     *  position. */
    getDataViewerPanelVisibleRange(panelName: string):
        { start: number; end: number } | undefined;
    /** Latest on-screen row range for a data viewer panel, excluding
     *  fetched overscan rows. Used by integration tests. */
    getDataViewerPanelViewportRange(panelName: string):
        { start: number; end: number } | undefined;
    /** Latest selected focus cell for a data viewer panel. Used by
     *  integration tests. */
    getDataViewerPanelFocusCell(panelName: string):
        { row: number; col: number } | undefined;
    /** Test-only: dispatch a synthetic key event in a data viewer panel.
     *  Used by integration tests to drive End / Home / PageDown / PageUp.
     *  Awaiting waits for the message to be queued; poll
     *  getDataViewerPanelVisibleRange to observe the result. */
    pressDataViewerKey(panelName: string, key: string): Promise<void>;
    /** Test-only: scroll a data viewer panel to a fractional vertical position.
     *  fraction=0 jumps to top, fraction=1 jumps to bottom. Used by
     *  integration tests to exercise the grid scroll pipeline.
     *  Awaiting waits for the message to be queued; poll
     *  getDataViewerPanelVisibleRange to observe the result. */
    dragDataViewerScrollbar(panelName: string, fraction: number): Promise<void>;
    /**
     * Test-only: forget the bundled extension's cached R terminal so the next
     * `sendToRTerminal` recreates it through the real `createTerminal` path.
     * Needed by integration suites that follow another suite which stubbed
     * `vscode.window.createTerminal` — that stub's fake terminal is invisible
     * to `onDidCloseTerminal` and would otherwise be reused indefinitely.
     */
    _disposeCachedRTerminalForTest(): void;
}

export function activate(context: vscode.ExtensionContext): RavenExtensionApi {
    const serverPath = getServerPath(context);

    function buildRustLogEnv(): Record<string, string> | undefined {
        const traceLevel = vscode.workspace.getConfiguration('raven').get<string>('trace.server', 'off');
        const rustLog = traceLevel === 'verbose' ? 'raven=trace' :
                        traceLevel === 'messages' ? 'raven=debug' : undefined;
        return rustLog ? { ...process.env as Record<string, string>, RUST_LOG: rustLog } : undefined;
    }

    const serverOptions: ServerOptions = {
        command: serverPath,
        args: ['--stdio'],
        options: { env: buildRustLogEnv() },
    };

    // Create output channel for server logs
    const outputChannel = vscode.window.createOutputChannel('Raven');

    const clientOptions: LanguageClientOptions = {
        // `rmd` and `quarto` are included so the document outline can surface
        // chunk entries (issue #227). The server side gates diagnostics for
        // these documents because the R tree-sitter parser would otherwise
        // emit syntax errors on prose. Chunk-level LSP features inside R
        // chunks (hover/completion/go-to-def) are tracked as a follow-up to
        // #230 and will need a more targeted flow (e.g. virtual-document
        // injection per fenced R block).
        documentSelector: [
            { scheme: 'file', language: 'r' },
            { scheme: 'untitled', language: 'r' },
            { scheme: 'file', language: 'rmd' },
            { scheme: 'untitled', language: 'rmd' },
            { scheme: 'file', language: 'quarto' },
            { scheme: 'untitled', language: 'quarto' },
            { scheme: 'file', language: 'jags' },
            { scheme: 'untitled', language: 'jags' },
            { scheme: 'file', language: 'stan' },
            { scheme: 'untitled', language: 'stan' },
        ],
        synchronize: {
            // Matches the LSP `documentSelector` above. `.Rmd`,
            // `.Rmarkdown`, and `.qmd` are included so workspace file events
            // for those documents reach the server too. `raven.toml` and
            // `.lintr` are watched so portable project-config edits reach the
            // server for live reconfiguration.
            fileEvents: [
                vscode.workspace.createFileSystemWatcher(
                    '**/*.{r,R,rmd,Rmd,RMD,rmarkdown,Rmarkdown,RMARKDOWN,qmd,Qmd,QMD,jags,Jags,JAGS,bugs,Bugs,BUGS,stan,Stan,STAN}',
                ),
                vscode.workspace.createFileSystemWatcher('**/raven.toml'),
                vscode.workspace.createFileSystemWatcher('**/.lintr'),
                vscode.workspace.createFileSystemWatcher('**/.Rprofile'),
            ],
        },
        outputChannel: outputChannel,
        initializationOptions: getInitializationOptions,
        middleware: {
            provideHover: (document, position, token, next) =>
                wrapHoverWithHelpTrust(async (doc, pos, tok) => {
                    const result = await next(doc, pos, tok);
                    return result as vscode.Hover | null | undefined;
                })(document, position, token),
        },
    };

    client = new LanguageClient(
        'raven',
        'Raven - R Language Server',
        serverOptions,
        clientOptions
    );

    // The listener survives vscode-languageclient's internal crash cleanup.
    // `Running` fires before start() resolves, so the helper waits for that
    // same in-flight promise before resending current ownership.
    context.subscriptions.push(
        registerRunningStateReconciliation(
            client,
            synchronizeClientDocumentState,
            (error) => outputChannel.appendLine(
                `Raven: failed to reconcile document state after restart: ${String(error)}`,
            ),
        ),
    );

    // Pre-check the binary before starting the LSP. vscode-languageclient's
    // generic "couldn't create connection to server" toast hides the real
    // cause (missing binary, no exec bit, wrong target). Surface the actual
    // reason here and skip the start so the user gets one clear message.
    const binaryCheck = validateServerBinary(serverPath);
    const configuredPath = vscode.workspace.getConfiguration('raven').get<string>('server.path');

    // The server emits `raven/projectConfigLoaded` whenever it picks up (or
    // re-picks up) a portable `raven.toml` / `.lintr` — and now also when
    // the file is removed. `path: null` + `source: null` is the cleared
    // form; both fields must be present and consistent for a "config in
    // effect" notification. Surface the source so users can confirm
    // what's authoritative at a glance.
    client.onNotification(
        'raven/projectConfigLoaded',
        (params: unknown) => {
            // Runtime type guard so a future server-side schema change fails
            // loudly rather than silently rendering "undefined" in the UI.
            // Enforce pair-shape consistency: both fields are null (cleared)
            // OR both fields are non-empty strings with `source` matching
            // the known discriminator set. Half-null / empty-string / unknown
            // source values are treated as malformed and logged.
            const isValidSource = (v: unknown): v is 'raven.toml' | '.lintr' =>
                v === 'raven.toml' || v === '.lintr';
            if (typeof params !== 'object' || params === null) {
                outputChannel.appendLine(
                    `Raven: ignoring malformed projectConfigLoaded payload: ${JSON.stringify(params)}`,
                );
                return;
            }
            const rawPath = (params as { path?: unknown }).path;
            const rawSource = (params as { source?: unknown }).source;
            const cleared = rawPath === null && rawSource === null;
            const inEffect =
                typeof rawPath === 'string' && rawPath.length > 0 && isValidSource(rawSource);
            if (!cleared && !inEffect) {
                outputChannel.appendLine(
                    `Raven: ignoring malformed projectConfigLoaded payload: ${JSON.stringify(params)}`,
                );
                return;
            }
            if (cleared) {
                outputChannel.appendLine('Raven: project config cleared (no raven.toml / .lintr in effect)');
                vscode.window.setStatusBarMessage('$(circle-slash) Raven: no project config', 5000);
                return;
            }
            const path = rawPath as string;
            const source = rawSource as 'raven.toml' | '.lintr';
            outputChannel.appendLine(`Raven: using config at ${path} (${source})`);
            vscode.window.setStatusBarMessage(`$(check) Raven: using ${source}`, 5000);
        },
    );

    if (binaryCheck.ok) {
        void client.start();
    } else {
        const detail = configuredPath
            ? `Raven LSP cannot start: configured raven.server.path "${serverPath}" is not a usable binary (${binaryCheck.reason}). Update raven.server.path or clear it to use the bundled binary.`
            : `Raven LSP cannot start: bundled binary at "${serverPath}" is unusable (${binaryCheck.reason}). Build it with "cargo build --release -p raven" and re-bundle the extension, or set raven.server.path to a built binary.`;
        outputChannel.appendLine(detail);
        void vscode.window.showErrorMessage(detail);
    }

    // Activate help viewer (registers raven.openHelpPanel, raven.help.back, raven.help.forward).
    activateHelpViewer(context, client);

    // R-console activation gating. The R console, plot viewer, and data viewer
    // share one umbrella — `raven.rConsole.activation`. Default `auto` steps
    // aside when REditorSupport (R) is enabled or VS Code is running as
    // Positron, so Raven supplements rather than fights existing R-session
    // setups. The help viewer activates regardless and is wired above.
    const r_console_resolved = resolveRConsoleActivation();
    void vscode.commands.executeCommand(
        'setContext',
        R_CONSOLE_ENABLED_CONTEXT,
        r_console_resolved === 'enabled',
    );
    let data_viewer_manager: DataViewerManager | undefined;
    if (r_console_resolved === 'enabled') {
        // Plot services (session server + viewer panel) for managed R terminals.
        // Constructed before raven.restart registration so the closure has a live
        // reference, not just a temporal-dead-zone forward binding.
        const plot_services = new PlotServices(context, dataViewerDirOf(context));
        active_plot_services = plot_services;
        data_viewer_manager = registerDataViewer(context, plot_services.server, dataViewerDirOf(context));

        // Internal command that PlotViewerPanel dispatches from its
        // `set-theme-applied` handler so it can fan out to every open
        // panel without holding a PlotServices reference. Mirrors the
        // raven.knit.cancelExport pattern. Not user-invocable (no entry
        // in package.json's commandPalette menu).
        context.subscriptions.push(
            vscode.commands.registerCommand(
                'raven.plot.broadcastStateUpdate',
                () => plot_services.broadcastStateUpdate(),
            ),
        );

        // Register R terminal and send-to-R commands
        register_r_terminal(context, plot_services);
        register_send_to_r_commands(context);
        register_inspection_commands(context);
        register_build_commands(context);
        register_chunks_with_terminal(context);

        // Chunk navigation and highlighting overlap with REditorSupport's
        // chunk surfaces, so they're gated behind R-console activation. With
        // REditorSupport / Positron handling chunks, Raven steps aside.
        register_chunks_navigation_and_highlight(context);

        // R snippets for `rmd` / `quarto` are registered programmatically here
        // (rather than statically in package.json) so they only appear when
        // Raven's R-console is active. The static `language: "r"` registration
        // in package.json continues to provide them in `.R` files. See
        // docs/coexistence.md.
        registerRSnippetCompletionsForRmdAndQuarto(context);
    }

    // Package-mode context key. The `raven.isRPackage` key gates the
    // Build commands' palette entries and editor-title submenu — every
    // `when` clause that uses it is also gated on `raven.rConsoleEnabled`,
    // so the key has no visible effect when R-console is disabled. We
    // still register the detection unconditionally so the key is
    // populated for whichever surfaces (current or future) consult it.
    register_r_package_detection(context);

    // `Raven: Knit Preview` registers unconditionally so its command-link
    // works even when the resolved gate is closed. The handler itself
    // re-checks `resolveRConsoleActivation()` at invocation and surfaces
    // a clear info message if the gate is closed. Setting
    // `raven.rmdKnit.enabled` to match the resolved gate gates the
    // command-palette entry.
    registerKnit(context, r_console_resolved === 'enabled', () => client);

    // Quarto Preview / Render is an independent CLI-backed workflow. It is
    // registered unconditionally and performs its own trust + CLI preflight;
    // unlike chunk execution and Knit Preview it is not part of the R-console
    // activation gate.
    registerQuarto(context);

    // Register restart command — re-reads trace config so changed settings take effect.
    //
    // Intentionally does NOT restart plot_services: existing Raven-managed R
    // terminals already hold the current RAVEN_SESSION_PORT/RAVEN_SESSION_TOKEN
    // in their environment, so tearing the session server down and bringing it
    // back up on a different port would leave those terminals POSTing to a
    // dead/unauthorized server until the user manually closes them.
    context.subscriptions.push(
        vscode.commands.registerCommand('raven.restart', async () => {
            (serverOptions as { options: { env: Record<string, string> | undefined } }).options.env = buildRustLogEnv();
            await client.restart();
            // Preserve the command's completion contract. The Running-state
            // listener also covers this transition, but its continuation need
            // not complete before executeCommand's caller resumes.
            synchronizeClientDocumentState();
        })
    );

    context.subscriptions.push(
        vscode.commands.registerCommand('raven.refreshPackages', async () => {
            try {
                const response = await client.sendRequest(ExecuteCommandRequest.type, {
                    command: 'raven.refreshPackages',
                    arguments: []
                });
                // Server returns `{ cleared: N }` — surface it so users get
                // feedback that the command actually did something.
                const cleared =
                    response && typeof (response as { cleared?: unknown }).cleared === 'number'
                        ? (response as { cleared: number }).cleared
                        : undefined;
                if (cleared !== undefined) {
                    vscode.window.setStatusBarMessage(
                        `Raven: refreshed ${cleared} package cache ${cleared === 1 ? 'entry' : 'entries'}`,
                        3000,
                    );
                }
            } catch (err) {
                vscode.window.showErrorMessage(`Raven refreshPackages failed: ${err}`);
            }
        })
    );

    // Generate a CI package-exports database by running the bundled binary's
    // `packages freeze` against the first workspace folder. Registered manually
    // (not via executeCommandProvider.commands, which must stay vec![] — see
    // CLAUDE.md) so vscode-languageclient doesn't double-register it.
    context.subscriptions.push(
        vscode.commands.registerCommand('raven.packages.freeze', async () => {
            const folder = vscode.workspace.workspaceFolders?.[0];
            if (!folder) {
                vscode.window.showErrorMessage(
                    'Raven: open a workspace folder to generate its package database.',
                );
                return;
            }
            const freezeServerPath = getServerPath(context);
            const binaryCheck = validateServerBinary(freezeServerPath);
            if (!binaryCheck.ok) {
                vscode.window.showErrorMessage(
                    `Raven: cannot generate package database — server binary is unusable (${binaryCheck.reason}).`,
                );
                return;
            }
            const cp = await import('node:child_process');
            await vscode.window.withProgress(
                {
                    location: vscode.ProgressLocation.Notification,
                    title: 'Raven: generating package database…',
                },
                () =>
                    new Promise<void>((resolve) => {
                        const proc = cp.spawn(
                            freezeServerPath,
                            ['packages', 'freeze', '--workspace', folder.uri.fsPath],
                            { cwd: folder.uri.fsPath },
                        );
                        let stderr = '';
                        proc.stderr.on('data', (d) => (stderr += d.toString()));
                        proc.on('error', (err) => {
                            vscode.window.showErrorMessage(
                                `Raven: package database generation failed: ${err.message}`,
                            );
                            resolve();
                        });
                        proc.on('close', (code) => {
                            if (code === 0) {
                                vscode.window.showInformationMessage(
                                    'Raven: wrote .raven/packages.json',
                                );
                            } else {
                                vscode.window.showErrorMessage(
                                    `Raven: package database generation failed: ${stderr.trim()}`,
                                );
                            }
                            resolve();
                        });
                    }),
            );
        }),
    );

    // Register auto-close pair overtype fix
    context.subscriptions.push(registerAutoCloseFix());

    // Register .gitignore / .lintr scaffold commands
    registerScaffoldCommands(context);

    // Scaffold a portable `raven.toml` from current VS Code linting settings.
    // Lives here (not in `scaffold.ts`) because it pulls the nested LSP-shape
    // linting payload via `buildInitializationOptions`, which the rest of
    // extension.ts already imports.
    context.subscriptions.push(
        vscode.commands.registerCommand('raven.createProjectConfig', async () => {
            await scaffoldProjectConfig();
        }),
    );

    // If `auto` chose to disable, surface a one-time popover so the user knows
    // why their R console / plot viewer / data viewer didn't activate.
    if (r_console_resolved === 'disabled' && readRConsoleActivation() === 'auto') {
        void notifyAutoDisable(context, detectAutoDisableReason());
    }

    // Listen for setting changes and REditorSupport extension toggles, and
    // prompt the user to reload when the resolved activation flips.
    registerActivationReactivity(context, r_console_resolved);

    // Register activity and tab-ownership listeners. A hidden text model can
    // be opened by another extension without appearing in visible editors, so
    // tab changes are the authoritative diagnostic-ownership signal.
    context.subscriptions.push(
        vscode.window.onDidChangeActiveTextEditor(() => {
            sendActivityNotification();
        })
    );

    context.subscriptions.push(
        vscode.window.onDidChangeVisibleTextEditors((editors) => {
            sendActivityNotification();
            // A document can be opened first as a hidden text model. Initial
            // TextEditor creation does not guarantee an options-change event,
            // so visibility is the first reliable point at which
            // detectIndentation/status-bar-resolved values can be observed.
            if (editors.some((editor) => isIndentUnitDocument(editor.document))) {
                sendDocumentIndentUnitsNotification();
            }
        })
    );

    context.subscriptions.push(
        vscode.window.tabGroups.onDidChangeTabs(() => {
            sendActivityNotification();
        })
    );

    // Register configuration change listener
    context.subscriptions.push(
        vscode.workspace.onDidChangeConfiguration((event) => {
            // `raven.*` carries the user-facing settings. `r.lsp.enabled` /
            // `r.lsp.diagnostics` feed the client-only `autoEnableFromDotLintr`
            // signal (#337), so a change there must also re-push init options
            // — the resolved value lives only inside getInitializationOptions().
            if (
                event.affectsConfiguration('raven') ||
                event.affectsConfiguration('r.lsp.enabled') ||
                event.affectsConfiguration('r.lsp.diagnostics')
            ) {
                // Send updated configuration to LSP server. Guard on
                // isRunning(): client.start() is gated on the binary check
                // above and is async, so the client may never have started
                // (unusable binary) or still be starting — sendNotification
                // throws "Client is not running" otherwise.
                if (client && client.isRunning()) {
                    client.sendNotification('workspace/didChangeConfiguration', {
                        settings: getInitializationOptions(),
                    });
                }
            }
            // Editor-setting changes must invalidate last-visible memos first:
            // hidden documents otherwise keep stale resolved values forever.
            // Visible editors repopulate from their live TextEditorOptions.
            const affectedEditorUris = (section: string): Set<string> => {
                if (!event.affectsConfiguration(section)) {
                    return new Set();
                }
                return new Set(
                    vscode.workspace.textDocuments
                        .filter(isIndentUnitDocument)
                        .filter((doc) => event.affectsConfiguration(section, {
                            uri: doc.uri,
                            languageId: doc.languageId,
                        }))
                        .map((doc) => doc.uri.toString())
                );
            };
            const tabSizeUris = affectedEditorUris('editor.tabSize');
            const insertSpacesUris = affectedEditorUris('editor.insertSpaces');
            const detectIndentationUris = affectedEditorUris('editor.detectIndentation');
            const formatOnTypeUris = affectedEditorUris('editor.formatOnType');
            const tabSizeInvalidationUris = new Set([
                ...tabSizeUris,
                ...detectIndentationUris,
            ]);
            const insertSpacesInvalidationUris = new Set([
                ...insertSpacesUris,
                ...detectIndentationUris,
            ]);
            if (tabSizeInvalidationUris.size > 0 || insertSpacesInvalidationUris.size > 0) {
                invalidateResolvedEditorOptions({
                    tabSize: tabSizeInvalidationUris,
                    insertSpaces: insertSpacesInvalidationUris,
                });
            }

            // tabSize/detectIndentation affect per-document units in "auto"
            // mode; insertSpaces/detectIndentation and formatOnType determine
            // whether the Enter producer can run.
            if (
                event.affectsConfiguration('raven.linting.indentationUnit') ||
                tabSizeUris.size > 0 ||
                insertSpacesUris.size > 0 ||
                detectIndentationUris.size > 0 ||
                formatOnTypeUris.size > 0
            ) {
                sendDocumentIndentUnitsNotification();
            }
        })
    );

    // Installing/uninstalling/enabling/disabling REditorSupport flips the
    // `.lintr` auto-enable signal (#337) without any settings change, so
    // re-push init options when the extension set changes. The server
    // hot-applies the new value; unlike the R-console features, linting needs
    // no window reload to retrack it.
    //
    // `onDidChange` can fire during the activation window (before the
    // fire-and-forget `client.start()` above resolves) and after shutdown, so
    // guard on `client.isRunning()` — `sendNotification` throws otherwise. Only
    // re-push when the resolved signal actually flips: extension-set changes
    // are noisy and the value rarely moves.
    let lastDotLintrAutoEnable = dotLintrAutoEnableAllowed();
    context.subscriptions.push(
        vscode.extensions.onDidChange(() => {
            const next = dotLintrAutoEnableAllowed();
            if (next === lastDotLintrAutoEnable) {
                return;
            }
            lastDotLintrAutoEnable = next;
            if (!client || !client.isRunning()) {
                return;
            }
            client.sendNotification('workspace/didChangeConfiguration', {
                settings: getInitializationOptions(),
            });
        })
    );

    context.subscriptions.push(
        vscode.workspace.onDidOpenTextDocument((doc) => {
            if (isIndentUnitDocument(doc)) {
                sendDocumentIndentUnitsNotification();
            }
        })
    );

    context.subscriptions.push(
        vscode.workspace.onDidCloseTextDocument((doc) => {
            if (isIndentUnitDocument(doc)) {
                forgetResolvedEditorOptions(doc.uri.toString());
                sendDocumentIndentUnitsNotification();
            }
        })
    );

    context.subscriptions.push(
        vscode.window.onDidChangeTextEditorOptions((event) => {
            if (isIndentUnitDocument(event.textEditor.document)) {
                sendDocumentIndentUnitsNotification();
            }
        })
    );

    context.subscriptions.push(
        vscode.workspace.onDidChangeTextDocument((event) => {
            const activeEditor = vscode.window.activeTextEditor;
            if (!activeEditor || activeEditor.document.uri.toString() !== event.document.uri.toString()) {
                return;
            }
            if (!isRDocument(event.document) || event.contentChanges.length !== 1) {
                return;
            }

            const change = event.contentChanges[0];
            const lineText = event.document.lineAt(change.range.start.line).text;
            const linePrefix = lineText.slice(0, change.range.start.character + change.text.length);
            const shouldTriggerSuggest =
                (change.rangeLength === 0 &&
                    shouldTriggerDirectivePathSuggest(change.text, linePrefix)) ||
                shouldTriggerNestedPathSuggest(change.text, linePrefix);
            if (shouldTriggerSuggest) {
                void vscode.commands.executeCommand('editor.action.triggerSuggest');
            }
        }),
    );

    // Migrate the deprecated `dotInWordSeparators` to `dotInWord`, then apply.
    // The prompt reads the new key, so it must run only after migration settles.
    // A `config.update` failure must not swallow the prompt, so log and proceed.
    void migrateDotInWordSetting()
        .catch((err) => {
            outputChannel.appendLine(
                `Raven: failed to migrate dotInWordSeparators -> dotInWord: ${err}`,
            );
        })
        .then(() => promptWordSeparators());

    return {
        getLanguageClient: () => client,
        sendToRTerminal: async (code: string) => {
            const terminal = await get_or_create_r_terminal();
            terminal.sendText(code, true);
        },
        getDataViewerPanelNames: () => data_viewer_manager?.getPanelNames() ?? [],
        getDataViewerPanelColumnNames: (panelName: string) =>
            data_viewer_manager?.getPanelColumnNames(panelName),
        getDataViewerPanelVisibleRange: (panelName: string) =>
            data_viewer_manager?.getPanelVisibleRange(panelName),
        getDataViewerPanelViewportRange: (panelName: string) =>
            data_viewer_manager?.getPanelViewportRange(panelName),
        getDataViewerPanelFocusCell: (panelName: string) =>
            data_viewer_manager?.getPanelFocusCell(panelName),
        pressDataViewerKey: async (panelName: string, key: string) => {
            await data_viewer_manager?.pressKeyOnPanel(panelName, key);
        },
        dragDataViewerScrollbar: async (panelName: string, fraction: number) => {
            await data_viewer_manager?.dragScrollbarOnPanel(panelName, fraction);
        },
        _disposeCachedRTerminalForTest: () => _dispose_cached_r_terminal_for_test(),
    };
}

async function applyDotInWordActions(
    config: vscode.WorkspaceConfiguration,
    actions: ReturnType<typeof planDotInWordMigration>,
) {
    for (const action of actions) {
        if (action.newValue !== undefined) {
            await config.update('editor.dotInWord', action.newValue, action.target);
        }
        await config.update('editor.dotInWordSeparators', undefined, action.target);
    }
}

/**
 * One-time, idempotent migration from the deprecated
 * `raven.editor.dotInWordSeparators` to `raven.editor.dotInWord`. Copies any
 * explicitly-set old value to the new key at the same scope and clears the old
 * key, so a user's `settings.json` ends up using the new name rather than
 * relying on a silent fallback. Safe to run on every activation: it's a no-op
 * once the old key is gone, and it re-runs if Settings Sync reintroduces it.
 *
 * Global and Workspace values are workspace-wide, so they're read and written
 * through an unscoped configuration. `workspaceFolderValue` only resolves on a
 * resource-scoped configuration, so each workspace folder is migrated through a
 * configuration scoped to that folder's URI — otherwise folder-specific
 * overrides in a multi-root workspace would be missed (and a `WorkspaceFolder`
 * update without a resource would throw).
 */
export async function migrateDotInWordSetting() {
    const wideConfig = vscode.workspace.getConfiguration('raven');
    await applyDotInWordActions(
        wideConfig,
        planDotInWordMigration(
            wideConfig.inspect('editor.dotInWordSeparators'),
            wideConfig.inspect('editor.dotInWord'),
            [vscode.ConfigurationTarget.Global, vscode.ConfigurationTarget.Workspace],
        ),
    );

    for (const folder of vscode.workspace.workspaceFolders ?? []) {
        const folderConfig = vscode.workspace.getConfiguration('raven', folder.uri);
        await applyDotInWordActions(
            folderConfig,
            planDotInWordMigration(
                folderConfig.inspect('editor.dotInWordSeparators'),
                folderConfig.inspect('editor.dotInWord'),
                [vscode.ConfigurationTarget.WorkspaceFolder],
            ),
        );
    }
}

async function promptWordSeparators() {
    const config = vscode.workspace.getConfiguration('raven');
    // Keep this fallback in sync with the manifest default in package.json. VS
    // Code returns the manifest default for an unset key, so this only fires if
    // the schema entry is ever removed — but a divergence here would be a silent
    // behavior change, so they must match. `migrateDotInWordSetting()` has
    // already run, so any pre-existing old value now lives under `dotInWord`.
    const setting = config.get<string>('editor.dotInWord', 'yes');

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
    const missingWordSeparatorsLanguage = DOT_IN_WORD_LANGUAGE_IDS.find((languageId) => {
        const languageConfig = wsConfig.inspect(`[${languageId}]`);
        return !hasWordSeparatorsOverride(languageConfig?.globalValue)
            && !hasWordSeparatorsOverride(languageConfig?.workspaceValue)
            && !hasWordSeparatorsOverride(languageConfig?.workspaceFolderValue);
    });

    if (missingWordSeparatorsLanguage === undefined) {
        return;
    }

    // Show prompt
    const choice = await vscode.window.showInformationMessage(
        'This extension can treat dots as part of words in R and JAGS files by updating editor.wordSeparators for [r] and [jags]. Enable this behavior?',
        'Enable',
        'No thanks'
    );

    if (choice === 'Enable') {
        await config.update('editor.dotInWord', 'yes', vscode.ConfigurationTarget.Global);
        await ensureWordSeparators(WORD_SEPARATORS);
        
        const reload = await vscode.window.showInformationMessage(
            'R and JAGS word separators updated: dots will now be part of words in R and JAGS files. Reload window to apply?',
            'Reload',
            'Later'
        );
        if (reload === 'Reload') {
            vscode.commands.executeCommand('workbench.action.reloadWindow');
        }
    } else if (choice === 'No thanks') {
        await config.update('editor.dotInWord', 'no', vscode.ConfigurationTarget.Global);
    }
}

async function ensureWordSeparators(wordSeparators: string) {
    const config = vscode.workspace.getConfiguration();
    
    for (const languageId of DOT_IN_WORD_LANGUAGE_IDS) {
        const updatedLanguageConfig = getUpdatedGlobalLanguageConfig(
            config.inspect<Record<string, unknown>>(`[${languageId}]`),
            wordSeparators,
        );

        // Only update if not already set correctly
        if (updatedLanguageConfig !== null) {
            await config.update(`[${languageId}]`, updatedLanguageConfig, vscode.ConfigurationTarget.Global);
        }
    }
}

async function scaffoldProjectConfig(): Promise<void> {
    const folders = vscode.workspace.workspaceFolders;
    if (!folders || folders.length === 0) {
        vscode.window.showErrorMessage('Raven: open a workspace folder first.');
        return;
    }
    const target = vscode.Uri.joinPath(folders[0].uri, 'raven.toml');
    try {
        await vscode.workspace.fs.stat(target);
        const choice = await vscode.window.showWarningMessage(
            'raven.toml already exists. Overwrite?',
            { modal: true },
            'Overwrite',
            'Cancel',
        );
        if (choice !== 'Overwrite') return;
    } catch {
        // not present — fall through
    }

    // Reuse the existing factory that converts VS Code's flat
    // `raven.linting.*` settings into the nested LSP init-options shape.
    // The TOML we render is the same shape Raven's server consumes.
    const config = vscode.workspace.getConfiguration('raven');
    const initOptions = buildInitializationOptions(config);
    const body = renderRavenToml(initOptions.linting as Record<string, unknown> | undefined);
    const encoder = new TextEncoder();
    await vscode.workspace.fs.writeFile(target, encoder.encode(body));
    const doc = await vscode.workspace.openTextDocument(target);
    await vscode.window.showTextDocument(doc);
}

let active_plot_services: PlotServices | null = null;

export function deactivate(): Thenable<void> | undefined {
    // Drop the knit grammar registry's cached `vscode.extensions.onDidChange`
    // listener. VS Code does NOT unload the JS module on deactivate, so
    // the module-scoped reference would otherwise survive into a
    // subsequent activation (disable→enable in the Extensions view, dev
    // reload) as a non-null but disposed Disposable — leaving the new
    // context's `subscriptions` array without a fresh listener and the
    // cached registry permanently stale across install/uninstall events.
    disposeKnitGrammarRegistryForDeactivation();
    const stops: Thenable<void>[] = [];
    // One awaited Quarto lifecycle thenable stops preview and render process
    // trees, disposes preview panels that retain activation-scoped callbacks,
    // and releases the shared output channel last.
    stops.push(stopAllQuartoForDeactivation());
    if (active_plot_services) stops.push(active_plot_services.dispose());
    if (client) stops.push(client.stop());
    if (stops.length === 0) return undefined;
    return Promise.all(stops).then(() => undefined);
}
