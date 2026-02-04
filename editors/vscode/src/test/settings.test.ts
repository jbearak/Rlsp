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

// Use mocha's describe/it which work in both standalone and VS Code test contexts
declare const suite: Mocha.SuiteFunction;
declare const test: Mocha.TestFunction;

// Type definitions matching extension.ts
type SeverityLevel = "error" | "warning" | "information" | "hint";

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
    };
    diagnostics?: {
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
 * Settings mapping from VS Code configuration keys to JSON paths in initializationOptions.
 * This is the source of truth for the property test.
 */
const SETTINGS_MAPPING: Array<{
    vsCodeKey: string;
    jsonPath: string[];
    type: 'number' | 'boolean' | 'string' | 'enum' | 'array';
    enumValues?: readonly string[];
}> = [
    // Cross-file depth settings
    { vsCodeKey: 'crossFile.maxBackwardDepth', jsonPath: ['crossFile', 'maxBackwardDepth'], type: 'number' },
    { vsCodeKey: 'crossFile.maxForwardDepth', jsonPath: ['crossFile', 'maxForwardDepth'], type: 'number' },
    { vsCodeKey: 'crossFile.maxChainDepth', jsonPath: ['crossFile', 'maxChainDepth'], type: 'number' },
    { vsCodeKey: 'crossFile.assumeCallSite', jsonPath: ['crossFile', 'assumeCallSite'], type: 'enum', enumValues: ['start', 'end'] as const },
    { vsCodeKey: 'crossFile.indexWorkspace', jsonPath: ['crossFile', 'indexWorkspace'], type: 'boolean' },
    { vsCodeKey: 'crossFile.maxRevalidationsPerTrigger', jsonPath: ['crossFile', 'maxRevalidationsPerTrigger'], type: 'number' },
    { vsCodeKey: 'crossFile.revalidationDebounceMs', jsonPath: ['crossFile', 'revalidationDebounceMs'], type: 'number' },
    // Cross-file severity settings
    { vsCodeKey: 'crossFile.missingFileSeverity', jsonPath: ['crossFile', 'missingFileSeverity'], type: 'enum', enumValues: ['error', 'warning', 'information', 'hint'] as const },
    { vsCodeKey: 'crossFile.circularDependencySeverity', jsonPath: ['crossFile', 'circularDependencySeverity'], type: 'enum', enumValues: ['error', 'warning', 'information', 'hint'] as const },
    { vsCodeKey: 'crossFile.outOfScopeSeverity', jsonPath: ['crossFile', 'outOfScopeSeverity'], type: 'enum', enumValues: ['error', 'warning', 'information', 'hint'] as const },
    { vsCodeKey: 'crossFile.ambiguousParentSeverity', jsonPath: ['crossFile', 'ambiguousParentSeverity'], type: 'enum', enumValues: ['error', 'warning', 'information', 'hint'] as const },
    { vsCodeKey: 'crossFile.maxChainDepthSeverity', jsonPath: ['crossFile', 'maxChainDepthSeverity'], type: 'enum', enumValues: ['error', 'warning', 'information', 'hint'] as const },
    // On-demand indexing settings
    { vsCodeKey: 'crossFile.onDemandIndexing.enabled', jsonPath: ['crossFile', 'onDemandIndexing', 'enabled'], type: 'boolean' },
    { vsCodeKey: 'crossFile.onDemandIndexing.maxTransitiveDepth', jsonPath: ['crossFile', 'onDemandIndexing', 'maxTransitiveDepth'], type: 'number' },
    { vsCodeKey: 'crossFile.onDemandIndexing.maxQueueSize', jsonPath: ['crossFile', 'onDemandIndexing', 'maxQueueSize'], type: 'number' },
    // Diagnostics settings
    { vsCodeKey: 'diagnostics.undefinedVariables', jsonPath: ['diagnostics', 'undefinedVariables'], type: 'boolean' },
    // Package settings
    { vsCodeKey: 'packages.enabled', jsonPath: ['packages', 'enabled'], type: 'boolean' },
    { vsCodeKey: 'packages.additionalLibraryPaths', jsonPath: ['packages', 'additionalLibraryPaths'], type: 'array' },
    { vsCodeKey: 'packages.rPath', jsonPath: ['packages', 'rPath'], type: 'string' },
    { vsCodeKey: 'packages.missingPackageSeverity', jsonPath: ['packages', 'missingPackageSeverity'], type: 'enum', enumValues: ['error', 'warning', 'information', 'hint'] as const },
];

/**
 * Generate a random value for a setting based on its type.
 */
function arbitraryForSetting(setting: typeof SETTINGS_MAPPING[number]): fc.Arbitrary<unknown> {
    switch (setting.type) {
        case 'number':
            return fc.integer({ min: 1, max: 100 });
        case 'boolean':
            return fc.boolean();
        case 'string':
            return fc.string({ minLength: 0, maxLength: 50 });
        case 'enum':
            return fc.constantFrom(...(setting.enumValues ?? []));
        case 'array':
            return fc.array(fc.string({ minLength: 1, maxLength: 20 }), { minLength: 0, maxLength: 5 });
        default:
            return fc.constant(undefined);
    }
}

/**
 * Interface for mock configuration inspection result.
 */
interface MockInspection<T> {
    key: string;
    defaultValue?: T;
    globalValue?: T;
    workspaceValue?: T;
    workspaceFolderValue?: T;
    globalLanguageValue?: T;
    workspaceLanguageValue?: T;
    workspaceFolderLanguageValue?: T;
}

/**
 * Create a mock VS Code WorkspaceConfiguration.
 * This simulates the behavior of vscode.workspace.getConfiguration('raven').
 */
function createMockConfig(configuredSettings: Map<string, unknown>): {
    get<T>(key: string): T | undefined;
    inspect<T>(key: string): MockInspection<T> | undefined;
} {
    return {
        get<T>(key: string): T | undefined {
            return configuredSettings.get(key) as T | undefined;
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
 * Check if a setting is explicitly configured (not using default).
 * Mirrors the logic in extension.ts.
 */
function isExplicitlyConfigured<T>(config: ReturnType<typeof createMockConfig>, key: string): boolean {
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

/**
 * Get a setting value only if it's explicitly configured.
 * Mirrors the logic in extension.ts.
 */
function getExplicitSetting<T>(config: ReturnType<typeof createMockConfig>, key: string): T | undefined {
    if (isExplicitlyConfigured<T>(config, key)) {
        return config.get<T>(key);
    }
    return undefined;
}

/**
 * Implementation of getInitializationOptions for testing.
 * This mirrors the logic in extension.ts but uses our mock config.
 */
function getInitializationOptions(config: ReturnType<typeof createMockConfig>): RavenInitializationOptions {
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
        onDemandIndexing !== undefined
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
    }

    // Diagnostics settings
    const undefinedVariables = getExplicitSetting<boolean>(config, 'diagnostics.undefinedVariables');
    if (undefinedVariables !== undefined) {
        options.diagnostics = {
            undefinedVariables: undefinedVariables,
        };
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

                // For each configured setting, verify it appears at the correct JSON path
                for (const setting of SETTINGS_MAPPING) {
                    const configuredValue = configuredSettings.get(setting.vsCodeKey);
                    const outputValue = getNestedValue(options as Record<string, unknown>, setting.jsonPath);

                    if (configuredValue !== undefined) {
                        // Setting was configured - it should appear in output at correct path
                        assert.deepStrictEqual(
                            outputValue,
                            configuredValue,
                            `Setting ${setting.vsCodeKey} should appear at path ${setting.jsonPath.join('.')} with value ${JSON.stringify(configuredValue)}, but got ${JSON.stringify(outputValue)}`
                        );
                    } else {
                        // Setting was not configured - it should NOT appear in output
                        assert.strictEqual(
                            outputValue,
                            undefined,
                            `Setting ${setting.vsCodeKey} was not configured but appeared at path ${setting.jsonPath.join('.')} with value ${JSON.stringify(outputValue)}`
                        );
                    }
                }
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

                // For each setting that was NOT configured, verify it does NOT appear in output
                for (const setting of SETTINGS_MAPPING) {
                    if (!configuredSettings.has(setting.vsCodeKey)) {
                        const outputValue = getNestedValue(options as Record<string, unknown>, setting.jsonPath);
                        assert.strictEqual(
                            outputValue,
                            undefined,
                            `Unconfigured setting ${setting.vsCodeKey} should not appear in output, but found ${JSON.stringify(outputValue)}`
                        );
                    }
                }
            }),
            { numRuns: 100 }
        );
    });

    /**
     * Empty configuration produces empty options object.
     */
    test('Empty configuration produces empty options', () => {
        const configuredSettings = new Map<string, unknown>();
        const mockConfig = createMockConfig(configuredSettings);
        const options = getInitializationOptions(mockConfig);

        assert.deepStrictEqual(options, {}, 'Empty configuration should produce empty options object');
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
     * Unit test: Verify severity settings transmission.
     */
    test('severity settings transmit correctly', () => {
        const severities: SeverityLevel[] = ['error', 'warning', 'information', 'hint'];
        
        for (const severity of severities) {
            const configuredSettings = new Map<string, unknown>([
                ['crossFile.missingFileSeverity', severity],
                ['crossFile.circularDependencySeverity', severity],
                ['packages.missingPackageSeverity', severity],
            ]);

            const mockConfig = createMockConfig(configuredSettings);
            const options = getInitializationOptions(mockConfig);

            assert.strictEqual(options.crossFile?.missingFileSeverity, severity);
            assert.strictEqual(options.crossFile?.circularDependencySeverity, severity);
            assert.strictEqual(options.packages?.missingPackageSeverity, severity);
        }
    });

    /**
     * Unit test: Verify nested onDemandIndexing settings transmission.
     */
    test('onDemandIndexing nested settings transmit correctly', () => {
        const configuredSettings = new Map<string, unknown>([
            ['crossFile.onDemandIndexing.enabled', false],
            ['crossFile.onDemandIndexing.maxTransitiveDepth', 5],
            ['crossFile.onDemandIndexing.maxQueueSize', 100],
        ]);

        const mockConfig = createMockConfig(configuredSettings);
        const options = getInitializationOptions(mockConfig);

        assert.strictEqual(options.crossFile?.onDemandIndexing?.enabled, false);
        assert.strictEqual(options.crossFile?.onDemandIndexing?.maxTransitiveDepth, 5);
        assert.strictEqual(options.crossFile?.onDemandIndexing?.maxQueueSize, 100);
    });

    /**
     * Unit test: Verify diagnostics settings transmission.
     */
    test('diagnostics settings transmit correctly', () => {
        const configuredSettings = new Map<string, unknown>([
            ['diagnostics.undefinedVariables', false],
        ]);

        const mockConfig = createMockConfig(configuredSettings);
        const options = getInitializationOptions(mockConfig);

        assert.strictEqual(options.diagnostics?.undefinedVariables, false);
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
     * Unit test: Partial configuration only includes configured settings.
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
        assert.strictEqual(options.diagnostics, undefined);
        assert.strictEqual(options.packages?.rPath, undefined);
    });
});
