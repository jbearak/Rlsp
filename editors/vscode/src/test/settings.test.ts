/**
 * Property-based tests for settings transmission correctness.
 * 
 * **Feature: vscode-settings-exposure, Property 1: Settings Transmission Correctness**
 * 
 * *For any* configured Raven setting and its value, when `getInitializationOptions()` is called,
 * the returned object SHALL contain that value at the correct JSON path corresponding to the
 * LSP server's expected structure.
 * 
 * **Validates: Requirements 1.4, 2.3, 3.2, 4.3, 5.6, 6.4, 7.2, 8.5, 10.2**
 */

import * as assert from 'assert';
import * as fc from 'fast-check';
import {
    getInitializationOptions,
    RavenConfigurationInspection,
    RavenInitializationOptions,
    RavenWorkspaceConfiguration,
    SeverityLevel,
} from '../initializationOptions';

// Use mocha's describe/it which work in both standalone and VS Code test contexts
declare const suite: Mocha.SuiteFunction;
declare const test: Mocha.TestFunction;

/**
 * Assert that each setting in SETTINGS_MAPPING has the expected value in the
 * output options, given which settings were explicitly configured.
 *
 * - Configured settings must match their configured value.
 * - Unconfigured settings with a runtime default must match that default.
 * - Unconfigured settings without a default must be undefined.
 */
function assertSettingsValues(
    options: Record<string, unknown>,
    configuredSettings: Map<string, unknown>,
    /** When true, only check unconfigured settings (Property 3). */
    unconfiguredOnly = false,
): void {
    for (const setting of SETTINGS_MAPPING) {
        const isConfigured = configuredSettings.has(setting.vsCodeKey);
        if (unconfiguredOnly && isConfigured) continue;

        const outputValue = getNestedValue(options, setting.jsonPath);

        if (isConfigured) {
            const configuredValue = configuredSettings.get(setting.vsCodeKey);
            assert.deepStrictEqual(
                outputValue,
                configuredValue,
                `Setting ${setting.vsCodeKey} should appear at path ${setting.jsonPath.join('.')} with value ${JSON.stringify(configuredValue)}, but got ${JSON.stringify(outputValue)}`,
            );
        } else if (setting.defaultWhenUnconfigured !== undefined) {
            assert.deepStrictEqual(
                outputValue,
                setting.defaultWhenUnconfigured,
                `Setting ${setting.vsCodeKey} should use its runtime default at path ${setting.jsonPath.join('.')} when unconfigured`,
            );
        } else {
            assert.strictEqual(
                outputValue,
                undefined,
                `Unconfigured setting ${setting.vsCodeKey} should not appear in output, but found ${JSON.stringify(outputValue)}`,
            );
        }
    }
}

/**
 * Settings mapping from VS Code configuration keys to JSON paths in initializationOptions.
 * This is the source of truth for the property test.
 */
const SETTINGS_MAPPING: Array<{
    vsCodeKey: string;
    jsonPath: string[];
    type: 'number' | 'boolean' | 'string' | 'enum' | 'enumOrArray' | 'array';
    enumValues?: readonly (string | boolean)[];
    defaultWhenUnconfigured?: unknown;
}> = [
    // Cross-file depth settings
    { vsCodeKey: 'crossFile.backwardDependencies', jsonPath: ['crossFile', 'backwardDependencies'], type: 'enum', enumValues: ['auto', 'explicit'] as const },
    { vsCodeKey: 'crossFile.maxBackwardDepth', jsonPath: ['crossFile', 'maxBackwardDepth'], type: 'number' },
    { vsCodeKey: 'crossFile.maxForwardDepth', jsonPath: ['crossFile', 'maxForwardDepth'], type: 'number' },
    { vsCodeKey: 'crossFile.maxChainDepth', jsonPath: ['crossFile', 'maxChainDepth'], type: 'number' },
    { vsCodeKey: 'crossFile.maxTransitiveDependentsVisited', jsonPath: ['crossFile', 'maxTransitiveDependentsVisited'], type: 'number' },
    { vsCodeKey: 'crossFile.assumeCallSite', jsonPath: ['crossFile', 'assumeCallSite'], type: 'enum', enumValues: ['start', 'end'] as const },
    { vsCodeKey: 'crossFile.indexWorkspace', jsonPath: ['crossFile', 'indexWorkspace'], type: 'boolean' },
    { vsCodeKey: 'crossFile.maxRevalidationsPerTrigger', jsonPath: ['crossFile', 'maxRevalidationsPerTrigger'], type: 'number' },
    { vsCodeKey: 'crossFile.revalidationDebounceMs', jsonPath: ['crossFile', 'revalidationDebounceMs'], type: 'number' },
    { vsCodeKey: 'crossFile.editedFileDebounceMs', jsonPath: ['crossFile', 'editedFileDebounceMs'], type: 'number' },
    // Cross-file severity settings
    { vsCodeKey: 'crossFile.missingFileSeverity', jsonPath: ['crossFile', 'missingFileSeverity'], type: 'enum', enumValues: ['error', 'warning', 'information', 'info', 'hint', 'off'] as const },
    { vsCodeKey: 'crossFile.caseMismatchSeverity', jsonPath: ['crossFile', 'caseMismatchSeverity'], type: 'enum', enumValues: ['auto', 'error', 'warning', 'information', 'info', 'hint', 'off'] as const },
    { vsCodeKey: 'crossFile.circularDependencySeverity', jsonPath: ['crossFile', 'circularDependencySeverity'], type: 'enum', enumValues: ['error', 'warning', 'information', 'info', 'hint', 'off'] as const },
    { vsCodeKey: 'crossFile.outOfScopeSeverity', jsonPath: ['crossFile', 'outOfScopeSeverity'], type: 'enum', enumValues: ['error', 'warning', 'information', 'info', 'hint', 'off'] as const },
    { vsCodeKey: 'crossFile.maxChainDepthSeverity', jsonPath: ['crossFile', 'maxChainDepthSeverity'], type: 'enum', enumValues: ['error', 'warning', 'information', 'info', 'hint', 'off'] as const },
    { vsCodeKey: 'crossFile.redundantDirectiveSeverity', jsonPath: ['crossFile', 'redundantDirectiveSeverity'], type: 'enum', enumValues: ['error', 'warning', 'information', 'info', 'hint', 'off'] as const },
    // On-demand indexing settings
    { vsCodeKey: 'crossFile.onDemandIndexing.enabled', jsonPath: ['crossFile', 'onDemandIndexing', 'enabled'], type: 'boolean' },
    // Cache settings
    { vsCodeKey: 'crossFile.cache.metadataMaxEntries', jsonPath: ['crossFile', 'cache', 'metadataMaxEntries'], type: 'number' },
    { vsCodeKey: 'crossFile.cache.fileContentMaxEntries', jsonPath: ['crossFile', 'cache', 'fileContentMaxEntries'], type: 'number' },
    { vsCodeKey: 'crossFile.cache.existenceMaxEntries', jsonPath: ['crossFile', 'cache', 'existenceMaxEntries'], type: 'number' },
    { vsCodeKey: 'crossFile.cache.workspaceIndexMaxEntries', jsonPath: ['crossFile', 'cache', 'workspaceIndexMaxEntries'], type: 'number' },
    // Diagnostics settings
    { vsCodeKey: 'diagnostics.enabled', jsonPath: ['diagnostics', 'enabled'], type: 'boolean', defaultWhenUnconfigured: true },
    { vsCodeKey: 'diagnostics.jags', jsonPath: ['diagnostics', 'jags'], type: 'enum', enumValues: ['off', 'on'] as const },
    { vsCodeKey: 'diagnostics.stan', jsonPath: ['diagnostics', 'stan'], type: 'enum', enumValues: ['off', 'on'] as const },
    { vsCodeKey: 'diagnostics.maxSyntaxDiagnosticsPerFile', jsonPath: ['diagnostics', 'maxSyntaxDiagnosticsPerFile'], type: 'number', defaultWhenUnconfigured: 500 },
    { vsCodeKey: 'diagnostics.undefinedVariableSeverity', jsonPath: ['diagnostics', 'undefinedVariableSeverity'], type: 'enum', enumValues: ['error', 'warning', 'information', 'info', 'hint', 'off'] as const },
    { vsCodeKey: 'diagnostics.undefinedVariableInCallArguments', jsonPath: ['diagnostics', 'undefinedVariableInCallArguments'], type: 'boolean' },
    { vsCodeKey: 'diagnostics.undefinedVariableInBracketIndices', jsonPath: ['diagnostics', 'undefinedVariableInBracketIndices'], type: 'boolean' },
    // Package settings
    { vsCodeKey: 'packages.enabled', jsonPath: ['packages', 'enabled'], type: 'boolean' },
    { vsCodeKey: 'packages.additionalLibraryPaths', jsonPath: ['packages', 'additionalLibraryPaths'], type: 'array' },
    { vsCodeKey: 'packages.rPath', jsonPath: ['packages', 'rPath'], type: 'string' },
    { vsCodeKey: 'packages.missingPackageSeverity', jsonPath: ['packages', 'missingPackageSeverity'], type: 'enum', enumValues: ['error', 'warning', 'information', 'info', 'hint', 'off'] as const },
    { vsCodeKey: 'packages.namespaceMemberSeverity', jsonPath: ['packages', 'namespaceMemberSeverity'], type: 'enum', enumValues: ['error', 'warning', 'information', 'info', 'hint', 'off'] as const },
    { vsCodeKey: 'packages.watchLibraryPaths', jsonPath: ['packages', 'watchLibraryPaths'], type: 'boolean' },
    { vsCodeKey: 'packages.watchDebounceMs', jsonPath: ['packages', 'watchDebounceMs'], type: 'number' },
    { vsCodeKey: 'packages.packageMode', jsonPath: ['packages', 'packageMode'], type: 'enum', enumValues: ['auto', 'enabled', 'disabled'] as const },
    { vsCodeKey: 'packages.rprofilePrelude', jsonPath: ['packages', 'rprofilePrelude'], type: 'boolean' },
    // Symbol settings
    { vsCodeKey: 'symbols.workspaceMaxResults', jsonPath: ['symbols', 'workspaceMaxResults'], type: 'number' },
    // Completion settings
    { vsCodeKey: 'completion.triggerOnOpenParen', jsonPath: ['completion', 'triggerOnOpenParen'], type: 'boolean' },
    // Indentation settings
    { vsCodeKey: 'indentation.enabled', jsonPath: ['indentation', 'enabled'], type: 'boolean' },
    { vsCodeKey: 'indentation.argumentStyle', jsonPath: ['indentation', 'argumentStyle'], type: 'enum', enumValues: ['aligned', 'indented', 'off'] as const },
    { vsCodeKey: 'indentation.infixContinuationStyle', jsonPath: ['indentation', 'infixContinuationStyle'], type: 'enum', enumValues: ['aligned', 'indented', 'off'] as const },
    { vsCodeKey: 'indentation.style', jsonPath: ['indentation', 'style'], type: 'enum', enumValues: ['rstudio', 'rstudio-minus', 'off'] as const },
    // Linting settings — always emitted using each key's package.json default
    // so that resetting a key propagates to the server instead of leaving
    // stale state active. Defaults below mirror package.json.
    { vsCodeKey: 'linting.enabled', jsonPath: ['linting', 'enabled'], type: 'enum', enumValues: ['auto', 'on', 'off', true, false] as const, defaultWhenUnconfigured: 'auto' },
    { vsCodeKey: 'linting.readHomeLintr', jsonPath: ['linting', 'readHomeLintr'], type: 'boolean', defaultWhenUnconfigured: false },
    { vsCodeKey: 'linting.lineLength', jsonPath: ['linting', 'lineLength'], type: 'number', defaultWhenUnconfigured: 80 },
    { vsCodeKey: 'linting.objectLength', jsonPath: ['linting', 'objectLength'], type: 'number', defaultWhenUnconfigured: 30 },
    { vsCodeKey: 'linting.indentationUnit', jsonPath: ['linting', 'indentationUnit'], type: 'number', defaultWhenUnconfigured: 2 },
    { vsCodeKey: 'linting.infixContinuationStyle', jsonPath: ['linting', 'infixContinuationStyle'], type: 'enum', enumValues: ['indented', 'aligned', 'either'] as const, defaultWhenUnconfigured: 'either' },
    { vsCodeKey: 'linting.assignmentOperator', jsonPath: ['linting', 'assignmentOperator'], type: 'enum', enumValues: ['<-', '='] as const, defaultWhenUnconfigured: '<-' },
    { vsCodeKey: 'linting.stringDelimiter', jsonPath: ['linting', 'stringDelimiter'], type: 'enum', enumValues: ['"', "'"] as const, defaultWhenUnconfigured: '"' },
    { vsCodeKey: 'linting.lineLengthSeverity', jsonPath: ['linting', 'lineLengthSeverity'], type: 'enum', enumValues: ['error', 'warning', 'information', 'info', 'hint', 'off'] as const, defaultWhenUnconfigured: 'information' },
    { vsCodeKey: 'linting.trailingWhitespaceSeverity', jsonPath: ['linting', 'trailingWhitespaceSeverity'], type: 'enum', enumValues: ['error', 'warning', 'information', 'info', 'hint', 'off'] as const, defaultWhenUnconfigured: 'information' },
    { vsCodeKey: 'linting.noTabSeverity', jsonPath: ['linting', 'noTabSeverity'], type: 'enum', enumValues: ['error', 'warning', 'information', 'info', 'hint', 'off'] as const, defaultWhenUnconfigured: 'information' },
    { vsCodeKey: 'linting.trailingBlankLinesSeverity', jsonPath: ['linting', 'trailingBlankLinesSeverity'], type: 'enum', enumValues: ['error', 'warning', 'information', 'info', 'hint', 'off'] as const, defaultWhenUnconfigured: 'information' },
    { vsCodeKey: 'linting.assignmentOperatorSeverity', jsonPath: ['linting', 'assignmentOperatorSeverity'], type: 'enum', enumValues: ['error', 'warning', 'information', 'info', 'hint', 'off'] as const, defaultWhenUnconfigured: 'information' },
    { vsCodeKey: 'linting.objectNameStyleFunction', jsonPath: ['linting', 'objectNameStyleFunction'], type: 'enumOrArray', enumValues: ['snake_case', 'camelCase', 'dotted.case', 'UPPER_CASE', 'lowercase', 'symbols', 'any'] as const, defaultWhenUnconfigured: ['snake_case', 'symbols'] },
    { vsCodeKey: 'linting.objectNameStyleVariable', jsonPath: ['linting', 'objectNameStyleVariable'], type: 'enumOrArray', enumValues: ['snake_case', 'camelCase', 'dotted.case', 'UPPER_CASE', 'lowercase', 'symbols', 'any'] as const, defaultWhenUnconfigured: ['snake_case', 'symbols'] },
    { vsCodeKey: 'linting.objectNameStyleArgument', jsonPath: ['linting', 'objectNameStyleArgument'], type: 'enumOrArray', enumValues: ['snake_case', 'camelCase', 'dotted.case', 'UPPER_CASE', 'lowercase', 'symbols', 'any'] as const, defaultWhenUnconfigured: ['snake_case', 'symbols'] },
    { vsCodeKey: 'linting.objectNameRegexesFunction', jsonPath: ['linting', 'objectNameRegexesFunction'], type: 'array', defaultWhenUnconfigured: [] },
    { vsCodeKey: 'linting.objectNameRegexesVariable', jsonPath: ['linting', 'objectNameRegexesVariable'], type: 'array', defaultWhenUnconfigured: [] },
    { vsCodeKey: 'linting.objectNameRegexesArgument', jsonPath: ['linting', 'objectNameRegexesArgument'], type: 'array', defaultWhenUnconfigured: [] },
    { vsCodeKey: 'linting.objectNameSeverity', jsonPath: ['linting', 'objectNameSeverity'], type: 'enum', enumValues: ['error', 'warning', 'information', 'info', 'hint', 'off'] as const, defaultWhenUnconfigured: 'information' },
    { vsCodeKey: 'linting.infixSpacesSeverity', jsonPath: ['linting', 'infixSpacesSeverity'], type: 'enum', enumValues: ['error', 'warning', 'information', 'info', 'hint', 'off'] as const, defaultWhenUnconfigured: 'information' },
    { vsCodeKey: 'linting.commentedCodeSeverity', jsonPath: ['linting', 'commentedCodeSeverity'], type: 'enum', enumValues: ['error', 'warning', 'information', 'info', 'hint', 'off'] as const, defaultWhenUnconfigured: 'information' },
    { vsCodeKey: 'linting.quotesSeverity', jsonPath: ['linting', 'quotesSeverity'], type: 'enum', enumValues: ['error', 'warning', 'information', 'info', 'hint', 'off'] as const, defaultWhenUnconfigured: 'information' },
    { vsCodeKey: 'linting.commasSeverity', jsonPath: ['linting', 'commasSeverity'], type: 'enum', enumValues: ['error', 'warning', 'information', 'info', 'hint', 'off'] as const, defaultWhenUnconfigured: 'information' },
    { vsCodeKey: 'linting.tAndFSymbolSeverity', jsonPath: ['linting', 'tAndFSymbolSeverity'], type: 'enum', enumValues: ['error', 'warning', 'information', 'info', 'hint', 'off'] as const, defaultWhenUnconfigured: 'information' },
    { vsCodeKey: 'linting.semicolonSeverity', jsonPath: ['linting', 'semicolonSeverity'], type: 'enum', enumValues: ['error', 'warning', 'information', 'info', 'hint', 'off'] as const, defaultWhenUnconfigured: 'information' },
    { vsCodeKey: 'linting.equalsNaSeverity', jsonPath: ['linting', 'equalsNaSeverity'], type: 'enum', enumValues: ['error', 'warning', 'information', 'info', 'hint', 'off'] as const, defaultWhenUnconfigured: 'information' },
    { vsCodeKey: 'linting.objectLengthSeverity', jsonPath: ['linting', 'objectLengthSeverity'], type: 'enum', enumValues: ['error', 'warning', 'information', 'info', 'hint', 'off'] as const, defaultWhenUnconfigured: 'information' },
    { vsCodeKey: 'linting.vectorLogicSeverity', jsonPath: ['linting', 'vectorLogicSeverity'], type: 'enum', enumValues: ['error', 'warning', 'information', 'info', 'hint', 'off'] as const, defaultWhenUnconfigured: 'information' },
    { vsCodeKey: 'diagnostics.mixedLogicalSeverity', jsonPath: ['diagnostics', 'mixedLogicalSeverity'], type: 'enum', enumValues: ['error', 'warning', 'information', 'info', 'hint', 'off'] as const },
    { vsCodeKey: 'diagnostics.conditionAssignmentSeverity', jsonPath: ['diagnostics', 'conditionAssignmentSeverity'], type: 'enum', enumValues: ['error', 'warning', 'information', 'info', 'hint', 'off'] as const },
    { vsCodeKey: 'diagnostics.reportUnusedSuppressions', jsonPath: ['diagnostics', 'reportUnusedSuppressions'], type: 'boolean' },
    { vsCodeKey: 'linting.functionLeftParenthesesSeverity', jsonPath: ['linting', 'functionLeftParenthesesSeverity'], type: 'enum', enumValues: ['error', 'warning', 'information', 'info', 'hint', 'off'] as const, defaultWhenUnconfigured: 'information' },
    { vsCodeKey: 'linting.spacesInsideSeverity', jsonPath: ['linting', 'spacesInsideSeverity'], type: 'enum', enumValues: ['error', 'warning', 'information', 'info', 'hint', 'off'] as const, defaultWhenUnconfigured: 'information' },
    { vsCodeKey: 'linting.indentationSeverity', jsonPath: ['linting', 'indentationSeverity'], type: 'enum', enumValues: ['error', 'warning', 'information', 'info', 'hint', 'off'] as const, defaultWhenUnconfigured: 'information' },
    // Help viewer settings
    { vsCodeKey: 'help.viewerColumn', jsonPath: ['helpViewer', 'viewColumn'], type: 'enum', enumValues: ['active', 'beside'] as const },
];

/**
 * Generate a random value for a setting based on its type.
 */
function arbitraryForSetting(setting: typeof SETTINGS_MAPPING[number]): fc.Arbitrary<unknown> {
    switch (setting.type) {
        case 'number':
            // Include 0 to align with package.json schema (minimum: 0 for revalidationDebounceMs)
            return fc.integer({ min: 0, max: 100 });
        case 'boolean':
            return fc.boolean();
        case 'string':
            return fc.string({ minLength: 0, maxLength: 50 });
        case 'enum':
            return fc.constantFrom(...(setting.enumValues ?? []));
        case 'enumOrArray': {
            const enumValues = setting.enumValues ?? [];
            return fc.oneof(
                fc.constantFrom(...enumValues),
                fc.array(fc.constantFrom(...enumValues), { minLength: 0, maxLength: 5 }),
            );
        }
        case 'array':
            return fc.array(fc.string({ minLength: 1, maxLength: 20 }), { minLength: 0, maxLength: 5 });
        default:
            return fc.constant(undefined);
    }
}

/**
 * Interface for mock configuration inspection result.
 */
interface MockInspection<T> extends RavenConfigurationInspection<T> {
    key: string;
}

/**
 * Create a mock VS Code WorkspaceConfiguration.
 * This simulates the behavior of vscode.workspace.getConfiguration('raven').
 */
function createMockConfig(configuredSettings: Map<string, unknown>): RavenWorkspaceConfiguration {
    return {
        get<T>(key: string, defaultValue?: T): T | undefined {
            const value = configuredSettings.get(key) as T | undefined;
            return value !== undefined ? value : defaultValue;
        },
        inspect<T>(key: string): MockInspection<T> | undefined {
            const value = configuredSettings.get(key);
            if (value !== undefined) {
                // Simulate that the value is set at global scope
                return {
                    key: `raven.${key}`,
                    globalValue: value as T,
                };
            }
            // Setting not configured - return inspection with no values
            return {
                key: `raven.${key}`,
            };
        },
    };
}

/**
 * Get a value from a nested object using a path array.
 */
function getNestedValue(obj: Record<string, unknown>, path: string[]): unknown {
    let current: unknown = obj;
    for (const key of path) {
        if (current === null || current === undefined || typeof current !== 'object') {
            return undefined;
        }
        current = (current as Record<string, unknown>)[key];
    }
    return current;
}

/**
 * Generate an arbitrary for a subset of settings to configure.
 * Returns a Map of setting keys to their values.
 */
function arbitraryConfiguredSettings(): fc.Arbitrary<Map<string, unknown>> {
    // Generate a subset of settings to configure (each setting has 50% chance of being configured)
    const settingArbitraries = SETTINGS_MAPPING.map(setting => 
        fc.tuple(
            fc.constant(setting.vsCodeKey),
            fc.boolean(), // whether to configure this setting
            arbitraryForSetting(setting)
        )
    );

    return fc.tuple(...settingArbitraries).map(tuples => {
        const configuredSettings = new Map<string, unknown>();
        for (const [key, shouldConfigure, value] of tuples) {
            if (shouldConfigure) {
                configuredSettings.set(key, value);
            }
        }
        return configuredSettings;
    });
}

suite('Settings Transmission Property Tests', () => {
    /**
     * Property 1: Settings Transmission Correctness
     * 
     * *For any* configured Raven setting and its value, when `getInitializationOptions()` is called,
     * the returned object SHALL contain that value at the correct JSON path corresponding to the
     * LSP server's expected structure.
     * 
     * **Validates: Requirements 1.4, 2.3, 3.2, 4.3, 5.6, 6.4, 7.2, 8.5, 10.2**
     */
    test('Property 1: Settings Transmission Correctness - configured settings appear at correct JSON paths', () => {
        fc.assert(
            fc.property(arbitraryConfiguredSettings(), (configuredSettings) => {
                const mockConfig = createMockConfig(configuredSettings);
                const options = getInitializationOptions(mockConfig);

                assertSettingsValues(options as Record<string, unknown>, configuredSettings);
            }),
            { numRuns: 100 }
        );
    });

    /**
     * Property 1 (continued): Single setting transmission
     * 
     * For each individual setting type, verify that when only that setting is configured,
     * it appears at the correct JSON path.
     */
    test('Property 1: Single setting transmission - each setting type transmits correctly', () => {
        for (const setting of SETTINGS_MAPPING) {
            fc.assert(
                fc.property(arbitraryForSetting(setting), (value) => {
                    const configuredSettings = new Map<string, unknown>();
                    configuredSettings.set(setting.vsCodeKey, value);

                    const mockConfig = createMockConfig(configuredSettings);
                    const options = getInitializationOptions(mockConfig);

                    const outputValue = getNestedValue(options as Record<string, unknown>, setting.jsonPath);
                    assert.deepStrictEqual(
                        outputValue,
                        value,
                        `Setting ${setting.vsCodeKey} with value ${JSON.stringify(value)} should appear at path ${setting.jsonPath.join('.')}`
                    );
                }),
                { numRuns: 20 }
            );
        }
    });

    /**
     * Property 1 (continued): All settings configured simultaneously
     * 
     * When all settings are configured, they should all appear at their correct JSON paths.
     */
    test('Property 1: All settings configured - all appear at correct paths', () => {
        const allSettingsArbitrary = fc.tuple(
            ...SETTINGS_MAPPING.map(setting => 
                fc.tuple(fc.constant(setting.vsCodeKey), arbitraryForSetting(setting))
            )
        ).map(tuples => {
            const configuredSettings = new Map<string, unknown>();
            for (const [key, value] of tuples) {
                configuredSettings.set(key, value);
            }
            return configuredSettings;
        });

        fc.assert(
            fc.property(allSettingsArbitrary, (configuredSettings) => {
                const mockConfig = createMockConfig(configuredSettings);
                const options = getInitializationOptions(mockConfig);

                for (const setting of SETTINGS_MAPPING) {
                    const configuredValue = configuredSettings.get(setting.vsCodeKey);
                    const outputValue = getNestedValue(options as Record<string, unknown>, setting.jsonPath);

                    assert.deepStrictEqual(
                        outputValue,
                        configuredValue,
                        `Setting ${setting.vsCodeKey} should appear at path ${setting.jsonPath.join('.')}`
                    );
                }
            }),
            { numRuns: 50 }
        );
    });

    /**
     * Property 3: Unconfigured Settings Omission
     * 
     * *For any* Raven setting that is not explicitly configured (uses VS Code's undefined/default),
     * the `getInitializationOptions()` function SHALL NOT include that setting in the returned object,
     * allowing the LSP server to use its own defaults.
     * 
     * **Validates: Requirements 10.4**
     */
    test('Property 3: Unconfigured settings are omitted from output', () => {
        fc.assert(
            fc.property(arbitraryConfiguredSettings(), (configuredSettings) => {
                const mockConfig = createMockConfig(configuredSettings);
                const options = getInitializationOptions(mockConfig);

                assertSettingsValues(options as Record<string, unknown>, configuredSettings, true);
            }),
            { numRuns: 100 }
        );
    });

    /**
     * Empty configuration produces only runtime-default settings.
     */
    test('Empty configuration produces runtime defaults only', () => {
        const configuredSettings = new Map<string, unknown>();
        const mockConfig = createMockConfig(configuredSettings);
        const options = getInitializationOptions(mockConfig);
        assert.deepStrictEqual(options, {
            diagnostics: {
                enabled: true,
                maxSyntaxDiagnosticsPerFile: 500,
            },
            linting: {
                enabled: 'auto',
                lineLength: 80,
                objectLength: 30,
                indentationUnit: 2,
                infixContinuationStyle: 'either',
                assignmentOperator: '<-',
                stringDelimiter: '"',
                objectNameStyleFunction: ['snake_case', 'symbols'],
                objectNameStyleVariable: ['snake_case', 'symbols'],
                objectNameStyleArgument: ['snake_case', 'symbols'],
                objectNameRegexesFunction: [],
                objectNameRegexesVariable: [],
                objectNameRegexesArgument: [],
                lineLengthSeverity: 'information',
                trailingWhitespaceSeverity: 'information',
                noTabSeverity: 'information',
                trailingBlankLinesSeverity: 'information',
                assignmentOperatorSeverity: 'information',
                objectNameSeverity: 'information',
                infixSpacesSeverity: 'information',
                commentedCodeSeverity: 'information',
                quotesSeverity: 'information',
                readHomeLintr: false,
                commasSeverity: 'information',
                tAndFSymbolSeverity: 'information',
                semicolonSeverity: 'information',
                equalsNaSeverity: 'information',
                objectLengthSeverity: 'information',
                vectorLogicSeverity: 'information',
                functionLeftParenthesesSeverity: 'information',
                spacesInsideSeverity: 'information',
                indentationSeverity: 'information',
            },
        }, 'Empty configuration should produce only runtime defaults');

    });
});

suite('Settings Transmission Unit Tests', () => {
    /**
     * Unit test: Verify specific crossFile settings transmission.
     */
    test('crossFile depth settings transmit correctly', () => {
        const configuredSettings = new Map<string, unknown>([
            ['crossFile.maxBackwardDepth', 15],
            ['crossFile.maxForwardDepth', 20],
            ['crossFile.maxChainDepth', 30],
        ]);

        const mockConfig = createMockConfig(configuredSettings);
        const options = getInitializationOptions(mockConfig);

        assert.strictEqual(options.crossFile?.maxBackwardDepth, 15);
        assert.strictEqual(options.crossFile?.maxForwardDepth, 20);
        assert.strictEqual(options.crossFile?.maxChainDepth, 30);
    });

    /**
     * Unit test: Verify assumeCallSite enum transmission.
     */
    test('assumeCallSite enum transmits correctly', () => {
        for (const value of ['start', 'end'] as const) {
            const configuredSettings = new Map<string, unknown>([
                ['crossFile.assumeCallSite', value],
            ]);

            const mockConfig = createMockConfig(configuredSettings);
            const options = getInitializationOptions(mockConfig);

            assert.strictEqual(options.crossFile?.assumeCallSite, value);
        }
    });

    /**
     * Unit test: Verify backwardDependencies enum transmission.
     */
    test('backwardDependencies enum transmits correctly', () => {
        for (const value of ['auto', 'explicit'] as const) {
            const configuredSettings = new Map<string, unknown>([
                ['crossFile.backwardDependencies', value],
            ]);

            const mockConfig = createMockConfig(configuredSettings);
            const options = getInitializationOptions(mockConfig);

            assert.strictEqual(options.crossFile?.backwardDependencies, value);
        }
    });

    /**
     * Unit test: Verify the help.viewerColumn mapping accepts both allowed
     * literals ('active' / 'beside') and lands them at helpViewer.viewColumn.
     * Deterministic complement to the property-based coverage above so a
     * regression in either the mapping entry or the JSON path is obvious.
     */
    test('help.viewerColumn enum transmits correctly to helpViewer.viewColumn', () => {
        const mapping = SETTINGS_MAPPING.find(s => s.vsCodeKey === 'help.viewerColumn');
        assert.ok(mapping, 'help.viewerColumn must exist in SETTINGS_MAPPING');
        assert.strictEqual(mapping!.type, 'enum');
        assert.deepStrictEqual([...(mapping!.enumValues ?? [])], ['active', 'beside']);
        assert.deepStrictEqual(mapping!.jsonPath, ['helpViewer', 'viewColumn']);

        for (const value of ['active', 'beside'] as const) {
            const configuredSettings = new Map<string, unknown>([
                ['help.viewerColumn', value],
            ]);
            const mockConfig = createMockConfig(configuredSettings);
            const options = getInitializationOptions(mockConfig);
            assert.strictEqual(
                (options as Record<string, Record<string, unknown> | undefined>).helpViewer?.viewColumn,
                value,
                `help.viewerColumn=${value} must map to helpViewer.viewColumn`
            );
        }
    });

    /**
     * Unit test: Verify severity settings transmission.
     */
    test('severity settings transmit correctly', () => {
        const severities: SeverityLevel[] = ['error', 'warning', 'information', 'info', 'hint', 'off'];

        for (const severity of severities) {
            const configuredSettings = new Map<string, unknown>([
                ['crossFile.missingFileSeverity', severity],
                ['crossFile.circularDependencySeverity', severity],
                ['packages.missingPackageSeverity', severity],
                ['packages.namespaceMemberSeverity', severity],
                ['diagnostics.undefinedVariableSeverity', severity],
            ]);

            const mockConfig = createMockConfig(configuredSettings);
            const options = getInitializationOptions(mockConfig);

            assert.strictEqual(options.crossFile?.missingFileSeverity, severity);
            assert.strictEqual(options.crossFile?.circularDependencySeverity, severity);
            assert.strictEqual(options.packages?.missingPackageSeverity, severity);
            assert.strictEqual(options.packages?.namespaceMemberSeverity, severity);
            assert.strictEqual(options.diagnostics?.undefinedVariableSeverity, severity);
        }
    });

    /**
     * Unit test: caseMismatchSeverity accepts the standard severity levels AND
     * the host-derived `auto` sentinel (issue #530).
     */
    test('caseMismatchSeverity transmits including the auto sentinel', () => {
        const values = ['auto', 'error', 'warning', 'information', 'info', 'hint', 'off'] as const;
        for (const value of values) {
            const mockConfig = createMockConfig(
                new Map<string, unknown>([['crossFile.caseMismatchSeverity', value]]),
            );
            const options = getInitializationOptions(mockConfig);
            assert.strictEqual(options.crossFile?.caseMismatchSeverity, value);
        }
    });

    /**
     * Unit test: Verify the nested onDemandIndexing.enabled setting transmits.
     */
    test('onDemandIndexing nested setting transmits correctly', () => {
        const configuredSettings = new Map<string, unknown>([
            ['crossFile.onDemandIndexing.enabled', false],
        ]);

        const mockConfig = createMockConfig(configuredSettings);
        const options = getInitializationOptions(mockConfig);

        assert.strictEqual(options.crossFile?.onDemandIndexing?.enabled, false);
    });

    test('maxTransitiveDependentsVisited transmits correctly', () => {
        const configuredSettings = new Map<string, unknown>([
            ['crossFile.maxTransitiveDependentsVisited', 1234],
        ]);

        const mockConfig = createMockConfig(configuredSettings);
        const options = getInitializationOptions(mockConfig);

        assert.strictEqual(options.crossFile?.maxTransitiveDependentsVisited, 1234);
    });

    /**
     * Regression: resetting an explicitly-configured lint setting must
     * propagate to the server as the package.json default, not be silently
     * omitted. If a key were omitted, the server treats absence as "preserve
     * the previous value" — so a user who toggled `linting.enabled` on and
     * later reset it would still have linting active until restart.
     */
    test('lint settings always emit (reset propagates as default)', () => {
        const mockConfig = createMockConfig(new Map<string, unknown>());
        const options = getInitializationOptions(mockConfig);
        // Whole linting block populated with package.json defaults.
        assert.deepStrictEqual(options.linting, {
            enabled: 'auto',
            readHomeLintr: false,
            lineLength: 80,
            objectLength: 30,
            indentationUnit: 2,
            infixContinuationStyle: 'either',
            assignmentOperator: '<-',
            stringDelimiter: '"',
            objectNameStyleFunction: ['snake_case', 'symbols'],
            objectNameStyleVariable: ['snake_case', 'symbols'],
            objectNameStyleArgument: ['snake_case', 'symbols'],
            objectNameRegexesFunction: [],
            objectNameRegexesVariable: [],
            objectNameRegexesArgument: [],
            lineLengthSeverity: 'information',
            trailingWhitespaceSeverity: 'information',
            noTabSeverity: 'information',
            trailingBlankLinesSeverity: 'information',
            assignmentOperatorSeverity: 'information',
            objectNameSeverity: 'information',
            infixSpacesSeverity: 'information',
            commentedCodeSeverity: 'information',
            quotesSeverity: 'information',
            commasSeverity: 'information',
            tAndFSymbolSeverity: 'information',
            semicolonSeverity: 'information',
            equalsNaSeverity: 'information',
            objectLengthSeverity: 'information',
            vectorLogicSeverity: 'information',
            functionLeftParenthesesSeverity: 'information',
            spacesInsideSeverity: 'information',
            indentationSeverity: 'information',
        });
    });

    test('indentation settings forward new fields and compatibility alias together', () => {
        const configuredSettings = new Map<string, unknown>([
            ['indentation.enabled', true],
            ['indentation.argumentStyle', 'off'],
            ['indentation.infixContinuationStyle', 'indented'],
            ['indentation.style', 'rstudio-minus'],
        ]);
        const options = getInitializationOptions(createMockConfig(configuredSettings));
        assert.deepStrictEqual(options.indentation, {
            enabled: true,
            argumentStyle: 'off',
            infixContinuationStyle: 'indented',
            style: 'rstudio-minus',
        });
    });

    test('lint settings forward explicit values', () => {
        const configuredSettings = new Map<string, unknown>([
            ['linting.enabled', true],
            ['linting.readHomeLintr', true],
            ['linting.lineLength', 120],
            ['linting.assignmentOperator', '='],
            ['linting.infixContinuationStyle', 'either'],
            ['linting.lineLengthSeverity', 'warning'],
            ['linting.assignmentOperatorSeverity', 'off'],
            ['linting.objectNameStyleFunction', 'camelCase'],
            ['linting.objectNameStyleVariable', 'any'],
            ['linting.objectNameRegexesFunction', ['^\\.on[A-Z]', 'Factory$']],
            ['linting.objectNameSeverity', 'warning'],
            ['linting.infixSpacesSeverity', 'off'],
            ['linting.commentedCodeSeverity', 'warning'],
        ]);
        const mockConfig = createMockConfig(configuredSettings);
        const options = getInitializationOptions(mockConfig);
        assert.strictEqual(options.linting?.enabled, true);
        assert.strictEqual(options.linting?.readHomeLintr, true);
        assert.strictEqual(options.linting?.lineLength, 120);
        assert.strictEqual(options.linting?.assignmentOperator, '=');
        assert.strictEqual(options.linting?.infixContinuationStyle, 'either');
        assert.strictEqual(options.linting?.lineLengthSeverity, 'warning');
        assert.strictEqual(options.linting?.assignmentOperatorSeverity, 'off');
        assert.strictEqual(options.linting?.objectNameStyleFunction, 'camelCase');
        assert.strictEqual(options.linting?.objectNameStyleVariable, 'any');
        assert.deepStrictEqual(options.linting?.objectNameRegexesFunction, ['^\\.on[A-Z]', 'Factory$']);
        assert.strictEqual(options.linting?.objectNameSeverity, 'warning');
        assert.strictEqual(options.linting?.infixSpacesSeverity, 'off');
        assert.strictEqual(options.linting?.commentedCodeSeverity, 'warning');
        // Untouched keys still emit their defaults.
        assert.strictEqual(options.linting?.trailingWhitespaceSeverity, 'information');
        assert.deepStrictEqual(options.linting?.objectNameStyleArgument, ['snake_case', 'symbols']);
        assert.deepStrictEqual(options.linting?.objectNameRegexesVariable, []);
        assert.deepStrictEqual(options.linting?.objectNameRegexesArgument, []);
    });

    test('object name style scalar string transmits for backcompat', () => {
        const configuredSettings = new Map<string, unknown>([
            ['linting.objectNameStyleFunction', 'camelCase'],
        ]);

        const mockConfig = createMockConfig(configuredSettings);
        const options = getInitializationOptions(mockConfig);

        assert.strictEqual(options.linting?.objectNameStyleFunction, 'camelCase');
    });

    test('object name style arrays transmit correctly', () => {
        const configuredSettings = new Map<string, unknown>([
            ['linting.objectNameStyleFunction', ['snake_case', 'camelCase']],
            ['linting.objectNameStyleVariable', ['dotted.case', 'snake_case']],
            ['linting.objectNameStyleArgument', ['lowercase']],
        ]);

        const mockConfig = createMockConfig(configuredSettings);
        const options = getInitializationOptions(mockConfig);

        assert.deepStrictEqual(options.linting?.objectNameStyleFunction, ['snake_case', 'camelCase']);
        assert.deepStrictEqual(options.linting?.objectNameStyleVariable, ['dotted.case', 'snake_case']);
        assert.deepStrictEqual(options.linting?.objectNameStyleArgument, ['lowercase']);
    });

    test('empty array for objectNameStyle transmits correctly', () => {
        const configuredSettings = new Map<string, unknown>([
            ['linting.objectNameStyleFunction', []],
        ]);

        const mockConfig = createMockConfig(configuredSettings);
        const options = getInitializationOptions(mockConfig);

        assert.deepStrictEqual(options.linting?.objectNameStyleFunction, []);
    });

    test('object name regex arrays transmit correctly', () => {
        const configuredSettings = new Map<string, unknown>([
            ['linting.objectNameRegexesFunction', ['^\\.on[A-Z]', 'Factory$']],
            ['linting.objectNameRegexesVariable', ['^tmp_', 'Result$']],
            ['linting.objectNameRegexesArgument', ['^\\.x$', '^data$']],
        ]);

        const mockConfig = createMockConfig(configuredSettings);
        const options = getInitializationOptions(mockConfig);

        assert.deepStrictEqual(options.linting?.objectNameRegexesFunction, ['^\\.on[A-Z]', 'Factory$']);
        assert.deepStrictEqual(options.linting?.objectNameRegexesVariable, ['^tmp_', 'Result$']);
        assert.deepStrictEqual(options.linting?.objectNameRegexesArgument, ['^\\.x$', '^data$']);
    });

    /**
     * Unit test: Verify diagnostics settings transmission.
     */
    test('diagnostics settings transmit correctly', () => {
        const configuredSettings = new Map<string, unknown>([
            ['diagnostics.enabled', false],
            ['diagnostics.jags', 'on'],
            ['diagnostics.stan', 'off'],
            ['diagnostics.maxSyntaxDiagnosticsPerFile', 17],
            ['diagnostics.undefinedVariableSeverity', 'error'],
            ['diagnostics.undefinedVariableInCallArguments', false],
            ['diagnostics.undefinedVariableInBracketIndices', false],
        ]);

        const mockConfig = createMockConfig(configuredSettings);
        const options = getInitializationOptions(mockConfig);

        assert.strictEqual(options.diagnostics?.enabled, false);
        assert.strictEqual(options.diagnostics?.jags, 'on');
        assert.strictEqual(options.diagnostics?.stan, 'off');
        assert.strictEqual(options.diagnostics?.maxSyntaxDiagnosticsPerFile, 17);
        assert.strictEqual(options.diagnostics?.undefinedVariableSeverity, 'error');
        assert.strictEqual(options.diagnostics?.undefinedVariableInCallArguments, false);
        assert.strictEqual(options.diagnostics?.undefinedVariableInBracketIndices, false);
    });

    test('diagnostics syntax cap transmits zero as unlimited', () => {
        const mockConfig = createMockConfig(new Map<string, unknown>([
            ['diagnostics.maxSyntaxDiagnosticsPerFile', 0],
        ]));

        const options = getInitializationOptions(mockConfig);

        assert.strictEqual(options.diagnostics?.maxSyntaxDiagnosticsPerFile, 0);
    });

    /**
     * Unit test: the new issue #398 booleans are only emitted when explicitly
     * configured (no default leakage), mirroring the other opt-out booleans.
     */
    test('undefined-variable scope booleans are omitted when unset', () => {
        const mockConfig = createMockConfig(new Map<string, unknown>());
        const options = getInitializationOptions(mockConfig);

        assert.strictEqual(options.diagnostics?.undefinedVariableInCallArguments, undefined);
        assert.strictEqual(options.diagnostics?.undefinedVariableInBracketIndices, undefined);
        assert.strictEqual(options.diagnostics?.jags, undefined);
        assert.strictEqual(options.diagnostics?.stan, undefined);
    });

    /**
     * Unit test: Verify packages settings transmission.
     */
    test('packages settings transmit correctly', () => {
        const configuredSettings = new Map<string, unknown>([
            ['packages.enabled', true],
            ['packages.additionalLibraryPaths', ['/path/to/lib1', '/path/to/lib2']],
            ['packages.rPath', '/usr/bin/R'],
            ['packages.missingPackageSeverity', 'error'],
        ]);

        const mockConfig = createMockConfig(configuredSettings);
        const options = getInitializationOptions(mockConfig);

        assert.strictEqual(options.packages?.enabled, true);
        assert.deepStrictEqual(options.packages?.additionalLibraryPaths, ['/path/to/lib1', '/path/to/lib2']);
        assert.strictEqual(options.packages?.rPath, '/usr/bin/R');
        assert.strictEqual(options.packages?.missingPackageSeverity, 'error');
    });

    /**
     * Unit test: packages.watchLibraryPaths forwards when explicitly configured false.
     */
    test('forwards packages.watchLibraryPaths when explicitly configured', () => {
        const configuredSettings = new Map<string, unknown>([
            ['packages.watchLibraryPaths', false],
        ]);

        const mockConfig = createMockConfig(configuredSettings);
        const options = getInitializationOptions(mockConfig);

        assert.strictEqual(options.packages?.watchLibraryPaths, false);
    });

    /**
     * Unit test: packages.watchDebounceMs forwards when explicitly configured.
     */
    test('forwards packages.watchDebounceMs when explicitly configured', () => {
        const configuredSettings = new Map<string, unknown>([
            ['packages.watchDebounceMs', 300],
        ]);

        const mockConfig = createMockConfig(configuredSettings);
        const options = getInitializationOptions(mockConfig);

        assert.strictEqual(options.packages?.watchDebounceMs, 300);
    });

    /**
     * Unit test: watchLibraryPaths and watchDebounceMs are undefined when not configured.
     */
    test('watchLibraryPaths and watchDebounceMs are undefined when not configured', () => {
        const configuredSettings = new Map<string, unknown>();

        const mockConfig = createMockConfig(configuredSettings);
        const options = getInitializationOptions(mockConfig);

        assert.strictEqual(options.packages?.watchLibraryPaths, undefined);
        assert.strictEqual(options.packages?.watchDebounceMs, undefined);
    });

    /**
     * Unit test: Partial configuration only includes configured settings.
     * **Validates: Requirement 10.4**
     */
    test('partial configuration only includes configured settings', () => {
        const configuredSettings = new Map<string, unknown>([
            ['crossFile.maxBackwardDepth', 5],
            ['packages.enabled', false],
        ]);

        const mockConfig = createMockConfig(configuredSettings);
        const options = getInitializationOptions(mockConfig);

        // Configured settings should be present
        assert.strictEqual(options.crossFile?.maxBackwardDepth, 5);
        assert.strictEqual(options.packages?.enabled, false);

        // Unconfigured settings should be absent
        assert.strictEqual(options.crossFile?.maxForwardDepth, undefined);
        assert.strictEqual(options.crossFile?.assumeCallSite, undefined);
        assert.deepStrictEqual(options.diagnostics, { enabled: true, maxSyntaxDiagnosticsPerFile: 500 });
        assert.strictEqual(options.packages?.rPath, undefined);
    });

    /**
     * Unit test: Parent object not created when no child settings configured.
     * Verifies that parent objects (crossFile, diagnostics, packages) are not
     * created when none of their child settings are configured.
     * **Validates: Requirement 10.4**
     */
    test('parent objects not created when no child settings configured', () => {
        // Configure only packages settings - crossFile should be absent, diagnostics keep their master default
        const configuredSettings = new Map<string, unknown>([
            ['packages.enabled', true],
        ]);

        const mockConfig = createMockConfig(configuredSettings);
        const options = getInitializationOptions(mockConfig);

        assert.strictEqual(options.crossFile, undefined);
        assert.deepStrictEqual(options.diagnostics, { enabled: true, maxSyntaxDiagnosticsPerFile: 500 });
        assert.notStrictEqual(options.packages, undefined);
        assert.strictEqual(options.packages?.enabled, true);
    });

    /**
     * Unit test: Verify nested cache settings transmission.
     */
    test('cache nested settings transmit correctly', () => {
        const configuredSettings = new Map<string, unknown>([
            ['crossFile.cache.metadataMaxEntries', 1001],
            ['crossFile.cache.fileContentMaxEntries', 501],
            ['crossFile.cache.existenceMaxEntries', 2001],
            ['crossFile.cache.workspaceIndexMaxEntries', 5001],
        ]);

        const mockConfig = createMockConfig(configuredSettings);
        const options = getInitializationOptions(mockConfig);

        assert.strictEqual(options.crossFile?.cache?.metadataMaxEntries, 1001);
        assert.strictEqual(options.crossFile?.cache?.fileContentMaxEntries, 501);
        assert.strictEqual(options.crossFile?.cache?.existenceMaxEntries, 2001);
        assert.strictEqual(options.crossFile?.cache?.workspaceIndexMaxEntries, 5001);
    });

    /**
     * Unit test: All severity settings with different values.
     * Verifies that each severity setting can be configured independently
     * with different values.
     * **Validates: Requirement 10.2**
     */
    test('all severity settings can have different values', () => {
        const configuredSettings = new Map<string, unknown>([
            ['crossFile.missingFileSeverity', 'error'],
            ['crossFile.circularDependencySeverity', 'warning'],
            ['crossFile.outOfScopeSeverity', 'information'],
            ['crossFile.maxChainDepthSeverity', 'error'],
            ['packages.missingPackageSeverity', 'warning'],
            ['diagnostics.undefinedVariableSeverity', 'off'],
        ]);

        const mockConfig = createMockConfig(configuredSettings);
        const options = getInitializationOptions(mockConfig);

        assert.strictEqual(options.crossFile?.missingFileSeverity, 'error');
        assert.strictEqual(options.crossFile?.circularDependencySeverity, 'warning');
        assert.strictEqual(options.crossFile?.outOfScopeSeverity, 'information');
        assert.strictEqual(options.crossFile?.maxChainDepthSeverity, 'error');
        assert.strictEqual(options.packages?.missingPackageSeverity, 'warning');
        assert.strictEqual(options.diagnostics?.undefinedVariableSeverity, 'off');
    });

    /**
     * Unit test: Empty array for additionalLibraryPaths.
     * Verifies that an empty array is correctly transmitted.
     * **Validates: Requirement 10.2**
     */
    test('empty array for additionalLibraryPaths transmits correctly', () => {
        const configuredSettings = new Map<string, unknown>([
            ['packages.additionalLibraryPaths', []],
        ]);

        const mockConfig = createMockConfig(configuredSettings);
        const options = getInitializationOptions(mockConfig);

        assert.deepStrictEqual(options.packages?.additionalLibraryPaths, []);
    });

    /**
     * Unit test: Empty string for rPath.
     * Verifies that an empty string is correctly transmitted.
     * **Validates: Requirement 10.2**
     */
    test('empty string for rPath transmits correctly', () => {
        const configuredSettings = new Map<string, unknown>([
            ['packages.rPath', ''],
        ]);

        const mockConfig = createMockConfig(configuredSettings);
        const options = getInitializationOptions(mockConfig);

        assert.strictEqual(options.packages?.rPath, '');
    });

    /**
     * Unit test: Boolean false values are transmitted (not omitted).
     * Verifies that explicitly configured false values are included in output.
     * **Validates: Requirements 10.2, 10.4**
     */
    test('boolean false values are transmitted not omitted', () => {
        const configuredSettings = new Map<string, unknown>([
            ['crossFile.indexWorkspace', false],
            ['crossFile.onDemandIndexing.enabled', false],
            ['diagnostics.enabled', false],
            ['packages.enabled', false],
        ]);

        const mockConfig = createMockConfig(configuredSettings);
        const options = getInitializationOptions(mockConfig);

        // All false values should be present (not omitted)
        assert.strictEqual(options.crossFile?.indexWorkspace, false);
        assert.strictEqual(options.crossFile?.onDemandIndexing?.enabled, false);
        assert.strictEqual(options.diagnostics?.enabled, false);
        assert.strictEqual(options.packages?.enabled, false);
    });

    /**
     * Unit test: Zero values for number settings are transmitted.
     * Verifies that zero is a valid value and is transmitted correctly.
     * **Validates: Requirement 10.2**
     */
    test('zero values for number settings are transmitted', () => {
        const configuredSettings = new Map<string, unknown>([
            ['crossFile.maxBackwardDepth', 0],
            ['crossFile.revalidationDebounceMs', 0],
        ]);

        const mockConfig = createMockConfig(configuredSettings);
        const options = getInitializationOptions(mockConfig);

        assert.strictEqual(options.crossFile?.maxBackwardDepth, 0);
        assert.strictEqual(options.crossFile?.revalidationDebounceMs, 0);
    });

    /**
     * Unit test: Revalidation settings transmit correctly.
     * **Validates: Requirement 10.2**
     */
    test('revalidation settings transmit correctly', () => {
        const configuredSettings = new Map<string, unknown>([
            ['crossFile.maxRevalidationsPerTrigger', 25],
            ['crossFile.revalidationDebounceMs', 500],
            ['crossFile.editedFileDebounceMs', 150],
        ]);

        const mockConfig = createMockConfig(configuredSettings);
        const options = getInitializationOptions(mockConfig);

        assert.strictEqual(options.crossFile?.maxRevalidationsPerTrigger, 25);
        assert.strictEqual(options.crossFile?.revalidationDebounceMs, 500);
        assert.strictEqual(options.crossFile?.editedFileDebounceMs, 150);
    });

    /**
     * Unit test: indexWorkspace boolean setting transmits correctly.
     * **Validates: Requirement 10.2**
     */
    test('indexWorkspace boolean setting transmits correctly', () => {
        for (const value of [true, false]) {
            const configuredSettings = new Map<string, unknown>([
                ['crossFile.indexWorkspace', value],
            ]);

            const mockConfig = createMockConfig(configuredSettings);
            const options = getInitializationOptions(mockConfig);

            assert.strictEqual(options.crossFile?.indexWorkspace, value);
        }
    });
});
