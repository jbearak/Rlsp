/**
 * Severity level for diagnostic messages.
 * Maps to LSP DiagnosticSeverity values.
 */
export type SeverityLevel = "error" | "warning" | "information" | "info" | "hint" | "off";

/**
 * Naming scheme for the object-name lint. `any` disables the check for the
 * corresponding symbol kind without disabling the rule entirely.
 */
export type ObjectNameStyle =
    | "snake_case"
    | "camelCase"
    | "dotted.case"
    | "UPPER_CASE"
    | "lowercase"
    | "symbols"
    | "any";

export type ObjectNameStyleSetting = ObjectNameStyle | ObjectNameStyle[];

/**
 * Subset of VS Code's configuration inspection result used when building
 * initialization options.
 */
export interface RavenConfigurationInspection<T> {
    globalValue?: T;
    workspaceValue?: T;
    workspaceFolderValue?: T;
    globalLanguageValue?: T;
    workspaceLanguageValue?: T;
    workspaceFolderLanguageValue?: T;
}

/**
 * Minimal configuration interface compatible with vscode.WorkspaceConfiguration
 * and the test double used in the settings tests.
 */
export interface RavenWorkspaceConfiguration {
    get<T>(section: string): T | undefined;
    get<T>(section: string, defaultValue: T): T;
    inspect<T>(section: string): RavenConfigurationInspection<T> | undefined;
}

/**
 * Initialization options passed to the Raven LSP server.
 * This interface matches the JSON structure expected by the server's
 * `parse_cross_file_config()` and related parsing functions.
 */
export interface RavenInitializationOptions {
    /**
     * VS Code resources represented by a tab or visible peek editor at LSP
     * startup. This is client state, not a user-configurable Raven setting.
     */
    diagnosticUris?: string[];
    crossFile?: {
        backwardDependencies?: "auto" | "explicit";
        maxBackwardDepth?: number;
        maxForwardDepth?: number;
        maxChainDepth?: number;
        maxTransitiveDependentsVisited?: number;
        assumeCallSite?: "start" | "end";
        indexWorkspace?: boolean;
        maxRevalidationsPerTrigger?: number;
        revalidationDebounceMs?: number;
        editedFileDebounceMs?: number;
        missingFileSeverity?: SeverityLevel;
        caseMismatchSeverity?: SeverityLevel | "auto";
        circularDependencySeverity?: SeverityLevel;
        outOfScopeSeverity?: SeverityLevel;
        maxChainDepthSeverity?: SeverityLevel;
        redundantDirectiveSeverity?: SeverityLevel;
        onDemandIndexing?: {
            enabled?: boolean;
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
        jags?: "on" | "off";
        stan?: "on" | "off";
        maxSyntaxDiagnosticsPerFile?: number;
        undefinedVariableSeverity?: SeverityLevel;
        undefinedVariableInCallArguments?: boolean;
        undefinedVariableInBracketIndices?: boolean;
        mixedLogicalSeverity?: SeverityLevel;
        conditionAssignmentSeverity?: SeverityLevel;
        reportUnusedSuppressions?: boolean;
    };
    packages?: {
        enabled?: boolean;
        additionalLibraryPaths?: string[];
        rPath?: string;
        missingPackageSeverity?: SeverityLevel;
        namespaceMemberSeverity?: SeverityLevel;
        watchLibraryPaths?: boolean;
        watchDebounceMs?: number;
        packageMode?: 'auto' | 'enabled' | 'disabled';
        rprofilePrelude?: boolean;
    };
    symbols?: {
        workspaceMaxResults?: number;
    };
    completion?: {
        triggerOnOpenParen?: boolean;
    };
    indentation?: {
        enabled?: boolean;
        argumentStyle?: "aligned" | "indented" | "off";
        infixContinuationStyle?: "aligned" | "indented" | "off";
        style?: "rstudio" | "rstudio-minus" | "off";
    };
    linting?: {
        enabled?: boolean | "auto" | "on" | "off";
        /**
         * Client-only environment signal: whether a discovered `.lintr` may
         * auto-enable native linting under `enabled: "auto"`. Omitted by
         * non-VS-Code clients (the server then defaults to allowing it). See
         * `lintr-auto-enable.ts` and #337.
         */
        autoEnableFromDotLintr?: boolean;
        readHomeLintr?: boolean;
        lineLength?: number;
        objectLength?: number;
        indentationUnit?: number | "auto";
        infixContinuationStyle?: "indented" | "aligned" | "either";
        assignmentOperator?: "<-" | "=";
        stringDelimiter?: "\"" | "'";
        objectNameStyleFunction?: ObjectNameStyleSetting;
        objectNameStyleVariable?: ObjectNameStyleSetting;
        objectNameStyleArgument?: ObjectNameStyleSetting;
        objectNameRegexesFunction?: string[];
        objectNameRegexesVariable?: string[];
        objectNameRegexesArgument?: string[];
        lineLengthSeverity?: SeverityLevel;
        trailingWhitespaceSeverity?: SeverityLevel;
        noTabSeverity?: SeverityLevel;
        trailingBlankLinesSeverity?: SeverityLevel;
        assignmentOperatorSeverity?: SeverityLevel;
        objectNameSeverity?: SeverityLevel;
        infixSpacesSeverity?: SeverityLevel;
        commentedCodeSeverity?: SeverityLevel;
        quotesSeverity?: SeverityLevel;
        commasSeverity?: SeverityLevel;
        tAndFSymbolSeverity?: SeverityLevel;
        semicolonSeverity?: SeverityLevel;
        equalsNaSeverity?: SeverityLevel;
        objectLengthSeverity?: SeverityLevel;
        vectorLogicSeverity?: SeverityLevel;
        functionLeftParenthesesSeverity?: SeverityLevel;
        spacesInsideSeverity?: SeverityLevel;
        indentationSeverity?: SeverityLevel;
    };
    helpViewer?: { viewColumn?: 'active' | 'beside' };
}

function isExplicitlyConfigured<T>(config: RavenWorkspaceConfiguration, key: string): boolean {
    const inspection = config.inspect<T>(key);
    if (!inspection) {
        return false;
    }
    return (
        inspection.globalValue !== undefined ||
        inspection.workspaceValue !== undefined ||
        inspection.workspaceFolderValue !== undefined ||
        inspection.globalLanguageValue !== undefined ||
        inspection.workspaceLanguageValue !== undefined ||
        inspection.workspaceFolderLanguageValue !== undefined
    );
}

function getExplicitSetting<T>(config: RavenWorkspaceConfiguration, key: string): T | undefined {
    if (isExplicitlyConfigured<T>(config, key)) {
        return config.get<T>(key);
    }
    return undefined;
}

/**
 * Read Raven settings from the provided configuration object and construct the
 * LSP initialization options payload.
 *
 * `autoEnableFromDotLintr` is a client-only environment signal (not a `raven.*`
 * setting): when provided it is forwarded under `linting` so the server can
 * gate `.lintr` auto-enable. Omitting it (e.g. the CLI, older callers) leaves
 * the field out, and the server defaults to allowing auto-enable. See #337.
 */
export function getInitializationOptions(
    config: RavenWorkspaceConfiguration,
    autoEnableFromDotLintr?: boolean,
): RavenInitializationOptions {
    const options: RavenInitializationOptions = {};

    const backwardDependencies = getExplicitSetting<"auto" | "explicit">(config, 'crossFile.backwardDependencies');
    const maxBackwardDepth = getExplicitSetting<number>(config, 'crossFile.maxBackwardDepth');
    const maxForwardDepth = getExplicitSetting<number>(config, 'crossFile.maxForwardDepth');
    const maxChainDepth = getExplicitSetting<number>(config, 'crossFile.maxChainDepth');
    const maxTransitiveDependentsVisited = getExplicitSetting<number>(config, 'crossFile.maxTransitiveDependentsVisited');
    const assumeCallSite = getExplicitSetting<"start" | "end">(config, 'crossFile.assumeCallSite');
    const indexWorkspace = getExplicitSetting<boolean>(config, 'crossFile.indexWorkspace');
    const maxRevalidationsPerTrigger = getExplicitSetting<number>(config, 'crossFile.maxRevalidationsPerTrigger');
    const revalidationDebounceMs = getExplicitSetting<number>(config, 'crossFile.revalidationDebounceMs');
    const editedFileDebounceMs = getExplicitSetting<number>(config, 'crossFile.editedFileDebounceMs');
    const missingFileSeverity = getExplicitSetting<SeverityLevel>(config, 'crossFile.missingFileSeverity');
    const caseMismatchSeverity = getExplicitSetting<SeverityLevel | "auto">(config, 'crossFile.caseMismatchSeverity');
    const circularDependencySeverity = getExplicitSetting<SeverityLevel>(config, 'crossFile.circularDependencySeverity');
    const outOfScopeSeverity = getExplicitSetting<SeverityLevel>(config, 'crossFile.outOfScopeSeverity');
    const maxChainDepthSeverity = getExplicitSetting<SeverityLevel>(config, 'crossFile.maxChainDepthSeverity');
    const redundantDirectiveSeverity = getExplicitSetting<SeverityLevel>(config, 'crossFile.redundantDirectiveSeverity');

    const onDemandEnabled = getExplicitSetting<boolean>(config, 'crossFile.onDemandIndexing.enabled');
    const onDemandIndexing = onDemandEnabled !== undefined ? { enabled: onDemandEnabled } : undefined;

    const cacheMetadataMaxEntries = getExplicitSetting<number>(config, 'crossFile.cache.metadataMaxEntries');
    const cacheFileContentMaxEntries = getExplicitSetting<number>(config, 'crossFile.cache.fileContentMaxEntries');
    const cacheExistenceMaxEntries = getExplicitSetting<number>(config, 'crossFile.cache.existenceMaxEntries');
    const cacheWorkspaceIndexMaxEntries = getExplicitSetting<number>(config, 'crossFile.cache.workspaceIndexMaxEntries');

    let cache: {
        metadataMaxEntries?: number;
        fileContentMaxEntries?: number;
        existenceMaxEntries?: number;
        workspaceIndexMaxEntries?: number;
    } | undefined = undefined;
    if (
        cacheMetadataMaxEntries !== undefined ||
        cacheFileContentMaxEntries !== undefined ||
        cacheExistenceMaxEntries !== undefined ||
        cacheWorkspaceIndexMaxEntries !== undefined
    ) {
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

    if (
        backwardDependencies !== undefined ||
        maxBackwardDepth !== undefined ||
        maxForwardDepth !== undefined ||
        maxChainDepth !== undefined ||
        maxTransitiveDependentsVisited !== undefined ||
        assumeCallSite !== undefined ||
        indexWorkspace !== undefined ||
        maxRevalidationsPerTrigger !== undefined ||
        revalidationDebounceMs !== undefined ||
        editedFileDebounceMs !== undefined ||
        missingFileSeverity !== undefined ||
        caseMismatchSeverity !== undefined ||
        circularDependencySeverity !== undefined ||
        outOfScopeSeverity !== undefined ||
        maxChainDepthSeverity !== undefined ||
        redundantDirectiveSeverity !== undefined ||
        onDemandIndexing !== undefined ||
        cache !== undefined
    ) {
        options.crossFile = {};
        if (backwardDependencies !== undefined) {
            options.crossFile.backwardDependencies = backwardDependencies;
        }
        if (maxBackwardDepth !== undefined) {
            options.crossFile.maxBackwardDepth = maxBackwardDepth;
        }
        if (maxForwardDepth !== undefined) {
            options.crossFile.maxForwardDepth = maxForwardDepth;
        }
        if (maxChainDepth !== undefined) {
            options.crossFile.maxChainDepth = maxChainDepth;
        }
        if (maxTransitiveDependentsVisited !== undefined) {
            options.crossFile.maxTransitiveDependentsVisited = maxTransitiveDependentsVisited;
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
        if (editedFileDebounceMs !== undefined) {
            options.crossFile.editedFileDebounceMs = editedFileDebounceMs;
        }
        if (missingFileSeverity !== undefined) {
            options.crossFile.missingFileSeverity = missingFileSeverity;
        }
        if (caseMismatchSeverity !== undefined) {
            options.crossFile.caseMismatchSeverity = caseMismatchSeverity;
        }
        if (circularDependencySeverity !== undefined) {
            options.crossFile.circularDependencySeverity = circularDependencySeverity;
        }
        if (outOfScopeSeverity !== undefined) {
            options.crossFile.outOfScopeSeverity = outOfScopeSeverity;
        }
        if (maxChainDepthSeverity !== undefined) {
            options.crossFile.maxChainDepthSeverity = maxChainDepthSeverity;
        }
        if (redundantDirectiveSeverity !== undefined) {
            options.crossFile.redundantDirectiveSeverity = redundantDirectiveSeverity;
        }
        if (onDemandIndexing !== undefined) {
            options.crossFile.onDemandIndexing = onDemandIndexing;
        }
        if (cache !== undefined) {
            options.crossFile.cache = cache;
        }
    }

    const diagnosticsEnabled = config.get<boolean>('diagnostics.enabled', true);
    const jags = getExplicitSetting<"on" | "off">(config, 'diagnostics.jags');
    const stan = getExplicitSetting<"on" | "off">(config, 'diagnostics.stan');
    const maxSyntaxDiagnosticsPerFile = config.get<number>('diagnostics.maxSyntaxDiagnosticsPerFile', 500);
    const undefinedVariableSeverity = getExplicitSetting<SeverityLevel>(config, 'diagnostics.undefinedVariableSeverity');
    const undefinedVariableInCallArguments = getExplicitSetting<boolean>(config, 'diagnostics.undefinedVariableInCallArguments');
    const undefinedVariableInBracketIndices = getExplicitSetting<boolean>(config, 'diagnostics.undefinedVariableInBracketIndices');
    const mixedLogicalSeverity = getExplicitSetting<SeverityLevel>(config, 'diagnostics.mixedLogicalSeverity');
    const conditionAssignmentSeverity = getExplicitSetting<SeverityLevel>(config, 'diagnostics.conditionAssignmentSeverity');
    const reportUnusedSuppressions = getExplicitSetting<boolean>(config, 'diagnostics.reportUnusedSuppressions');

    options.diagnostics = {
        enabled: diagnosticsEnabled,
        maxSyntaxDiagnosticsPerFile,
    };
    if (jags !== undefined) {
        options.diagnostics.jags = jags;
    }
    if (stan !== undefined) {
        options.diagnostics.stan = stan;
    }
    if (undefinedVariableSeverity !== undefined) {
        options.diagnostics.undefinedVariableSeverity = undefinedVariableSeverity;
    }
    if (undefinedVariableInCallArguments !== undefined) {
        options.diagnostics.undefinedVariableInCallArguments = undefinedVariableInCallArguments;
    }
    if (undefinedVariableInBracketIndices !== undefined) {
        options.diagnostics.undefinedVariableInBracketIndices = undefinedVariableInBracketIndices;
    }
    if (mixedLogicalSeverity !== undefined) {
        options.diagnostics.mixedLogicalSeverity = mixedLogicalSeverity;
    }
    if (conditionAssignmentSeverity !== undefined) {
        options.diagnostics.conditionAssignmentSeverity = conditionAssignmentSeverity;
    }
    if (reportUnusedSuppressions !== undefined) {
        options.diagnostics.reportUnusedSuppressions = reportUnusedSuppressions;
    }

    const packagesEnabled = getExplicitSetting<boolean>(config, 'packages.enabled');
    const additionalLibraryPaths = getExplicitSetting<string[]>(config, 'packages.additionalLibraryPaths');
    const rPath = getExplicitSetting<string>(config, 'packages.rPath');
    const missingPackageSeverity = getExplicitSetting<SeverityLevel>(config, 'packages.missingPackageSeverity');
    const namespaceMemberSeverity = getExplicitSetting<SeverityLevel>(config, 'packages.namespaceMemberSeverity');
    const watchLibraryPaths = getExplicitSetting<boolean>(config, 'packages.watchLibraryPaths');
    const watchDebounceMs = getExplicitSetting<number>(config, 'packages.watchDebounceMs');
    const packageMode = getExplicitSetting<'auto' | 'enabled' | 'disabled'>(config, 'packages.packageMode');
    const rprofilePrelude = getExplicitSetting<boolean>(config, 'packages.rprofilePrelude');

    if (
        packagesEnabled !== undefined ||
        additionalLibraryPaths !== undefined ||
        rPath !== undefined ||
        missingPackageSeverity !== undefined ||
        namespaceMemberSeverity !== undefined ||
        watchLibraryPaths !== undefined ||
        watchDebounceMs !== undefined ||
        packageMode !== undefined ||
        rprofilePrelude !== undefined
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
        if (namespaceMemberSeverity !== undefined) {
            options.packages.namespaceMemberSeverity = namespaceMemberSeverity;
        }
        if (watchLibraryPaths !== undefined) {
            options.packages.watchLibraryPaths = watchLibraryPaths;
        }
        if (watchDebounceMs !== undefined) {
            options.packages.watchDebounceMs = watchDebounceMs;
        }
        if (packageMode !== undefined) {
            options.packages.packageMode = packageMode;
        }
        if (rprofilePrelude !== undefined) {
            options.packages.rprofilePrelude = rprofilePrelude;
        }
    }

    const workspaceMaxResults = getExplicitSetting<number>(config, 'symbols.workspaceMaxResults');
    if (workspaceMaxResults !== undefined) {
        options.symbols = { workspaceMaxResults };
    }

    const triggerOnOpenParen = getExplicitSetting<boolean>(config, 'completion.triggerOnOpenParen');
    if (triggerOnOpenParen !== undefined) {
        options.completion = { triggerOnOpenParen };
    }

    const indentationEnabled = getExplicitSetting<boolean>(config, 'indentation.enabled');
    const indentationArgumentStyle = getExplicitSetting<"aligned" | "indented" | "off">(config, 'indentation.argumentStyle');
    const indentationInfixStyle = getExplicitSetting<"aligned" | "indented" | "off">(config, 'indentation.infixContinuationStyle');
    const indentationStyle = getExplicitSetting<"rstudio" | "rstudio-minus" | "off">(config, 'indentation.style');
    if (
        indentationEnabled !== undefined ||
        indentationArgumentStyle !== undefined ||
        indentationInfixStyle !== undefined ||
        indentationStyle !== undefined
    ) {
        options.indentation = {};
        if (indentationEnabled !== undefined) options.indentation.enabled = indentationEnabled;
        if (indentationArgumentStyle !== undefined) options.indentation.argumentStyle = indentationArgumentStyle;
        if (indentationInfixStyle !== undefined) options.indentation.infixContinuationStyle = indentationInfixStyle;
        if (indentationStyle !== undefined) options.indentation.style = indentationStyle;
    }

    // Linting settings: always emit the full section using each setting's
    // package.json default. Otherwise, resetting an explicitly-configured key
    // (e.g. via VS Code's "Reset Setting") would omit it from the payload,
    // and the server treats absent keys as "preserve current value" — so the
    // previous state would persist until restart.
    options.linting = {
        enabled: config.get<boolean | 'auto' | 'on' | 'off'>('linting.enabled', 'auto'),
        readHomeLintr: config.get<boolean>('linting.readHomeLintr', false),
        lineLength: config.get<number>('linting.lineLength', 80),
        objectLength: config.get<number>('linting.objectLength', 30),
        indentationUnit: (() => {
            const v = config.get<number | 'auto'>('linting.indentationUnit', 'auto');
            // "auto" is resolved per-document via raven/documentIndentUnitsChanged.
            // Send 2 as a placeholder so the server has a sane default until the
            // first notification arrives.
            return v === 'auto' ? 2 : v;
        })(),
        infixContinuationStyle: config.get<"indented" | "aligned" | "either">('linting.infixContinuationStyle', 'either'),
        assignmentOperator: config.get<"<-" | "=">('linting.assignmentOperator', '<-'),
        stringDelimiter: config.get<"\"" | "'">('linting.stringDelimiter', '"'),
        objectNameStyleFunction: config.get<ObjectNameStyleSetting>('linting.objectNameStyleFunction', ['snake_case', 'symbols']),
        objectNameStyleVariable: config.get<ObjectNameStyleSetting>('linting.objectNameStyleVariable', ['snake_case', 'symbols']),
        objectNameStyleArgument: config.get<ObjectNameStyleSetting>('linting.objectNameStyleArgument', ['snake_case', 'symbols']),
        objectNameRegexesFunction: config.get<string[]>('linting.objectNameRegexesFunction', []),
        objectNameRegexesVariable: config.get<string[]>('linting.objectNameRegexesVariable', []),
        objectNameRegexesArgument: config.get<string[]>('linting.objectNameRegexesArgument', []),
        lineLengthSeverity: config.get<SeverityLevel>('linting.lineLengthSeverity', 'information'),
        trailingWhitespaceSeverity: config.get<SeverityLevel>('linting.trailingWhitespaceSeverity', 'information'),
        noTabSeverity: config.get<SeverityLevel>('linting.noTabSeverity', 'information'),
        trailingBlankLinesSeverity: config.get<SeverityLevel>('linting.trailingBlankLinesSeverity', 'information'),
        assignmentOperatorSeverity: config.get<SeverityLevel>('linting.assignmentOperatorSeverity', 'information'),
        objectNameSeverity: config.get<SeverityLevel>('linting.objectNameSeverity', 'information'),
        infixSpacesSeverity: config.get<SeverityLevel>('linting.infixSpacesSeverity', 'information'),
        commentedCodeSeverity: config.get<SeverityLevel>('linting.commentedCodeSeverity', 'information'),
        quotesSeverity: config.get<SeverityLevel>('linting.quotesSeverity', 'information'),
        commasSeverity: config.get<SeverityLevel>('linting.commasSeverity', 'information'),
        tAndFSymbolSeverity: config.get<SeverityLevel>('linting.tAndFSymbolSeverity', 'information'),
        semicolonSeverity: config.get<SeverityLevel>('linting.semicolonSeverity', 'information'),
        equalsNaSeverity: config.get<SeverityLevel>('linting.equalsNaSeverity', 'information'),
        objectLengthSeverity: config.get<SeverityLevel>('linting.objectLengthSeverity', 'information'),
        vectorLogicSeverity: config.get<SeverityLevel>('linting.vectorLogicSeverity', 'information'),
        functionLeftParenthesesSeverity: config.get<SeverityLevel>('linting.functionLeftParenthesesSeverity', 'information'),
        spacesInsideSeverity: config.get<SeverityLevel>('linting.spacesInsideSeverity', 'information'),
        indentationSeverity: config.get<SeverityLevel>('linting.indentationSeverity', 'information'),
    };

    // Computed environment signal, not a user setting — include it only when
    // the caller supplied one. Absent leaves the server on its back-compat
    // default (allow `.lintr` auto-enable). See #337.
    if (autoEnableFromDotLintr !== undefined) {
        options.linting.autoEnableFromDotLintr = autoEnableFromDotLintr;
    }

    const helpViewerColumn = getExplicitSetting<'active' | 'beside'>(config, 'help.viewerColumn');
    if (helpViewerColumn !== undefined) {
        options.helpViewer = { viewColumn: helpViewerColumn };
    }

    return options;
}
