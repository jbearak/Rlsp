import { describe, expect, it } from 'bun:test';
import type { ChildProcess, spawn } from 'child_process';
import { EventEmitter } from 'events';
import { PassThrough } from 'stream';
import {
    defaultQuartoFallbacks,
    probeQuartoBinary,
    QuartoNotFoundError,
    QuartoResolver,
} from '../../editors/vscode/src/quarto/quarto-detect';

const accessOk = async (_candidate: string): Promise<void> => undefined;

describe('QuartoResolver', () => {
    it('uses a configured resource-scoped path', async () => {
        const resolver = new QuartoResolver({
            getConfigured: () => '/custom/quarto',
            access: accessOk,
            probe: async () => 'Quarto CLI help',
        });
        expect(await resolver.resolve('file:///a.qmd')).toBe('/custom/quarto');
    });

    it('uses quarto on PATH when no path is configured', async () => {
        const resolver = new QuartoResolver({
            getConfigured: () => '',
            access: accessOk,
            probe: async (candidate) => {
                expect(candidate).toBe('quarto');
                return 'Quarto CLI help';
            },
            fallbacks: () => [],
        });
        expect(await resolver.resolve('a')).toBe('quarto');
    });

    it('walks platform fallbacks after PATH fails', async () => {
        const probed: string[] = [];
        const resolver = new QuartoResolver({
            getConfigured: () => '',
            access: accessOk,
            probe: async (candidate) => {
                probed.push(candidate);
                if (candidate !== '/real/quarto') throw new Error('not Quarto');
                return 'Quarto CLI help';
            },
            fallbacks: () => ['/wrong/quarto', '/real/quarto'],
        });
        expect(await resolver.resolve('a')).toBe('/real/quarto');
        expect(probed).toEqual(['quarto', '/wrong/quarto', '/real/quarto']);
    });

    it('rejects an accessible configured non-Quarto binary', async () => {
        const resolver = new QuartoResolver({
            getConfigured: () => '/bin/echo',
            access: accessOk,
            probe: async () => { throw new Error('missing Quarto CLI marker'); },
        });
        await expect(resolver.resolve('a')).rejects.toThrow(QuartoNotFoundError);
    });

    it('caches by effective configured value across resources', async () => {
        const values = new Map([
            ['uri-a', '/folder-a/quarto'],
            ['uri-b', '/folder-b/quarto'],
            ['uri-c', '/folder-a/quarto'],
        ]);
        const probes: string[] = [];
        const resolver = new QuartoResolver<string>({
            getConfigured: (uri) => values.get(uri) ?? '',
            access: accessOk,
            probe: async (candidate) => {
                probes.push(candidate);
                return 'Quarto CLI help';
            },
        });
        expect(await resolver.resolve('uri-a')).toBe('/folder-a/quarto');
        expect(await resolver.resolve('uri-b')).toBe('/folder-b/quarto');
        expect(await resolver.resolve('uri-c')).toBe('/folder-a/quarto');
        expect(probes).toEqual(['/folder-a/quarto', '/folder-b/quarto']);
    });

    it('shares one in-flight probe across concurrent first-use calls', async () => {
        let probeCalls = 0;
        let releaseProbe: (value: string) => void = () => undefined;
        const probeResult = new Promise<string>((resolve) => {
            releaseProbe = resolve;
        });
        const resolver = new QuartoResolver({
            getConfigured: () => '/custom/quarto',
            access: accessOk,
            probe: async () => {
                probeCalls++;
                return probeResult;
            },
        });

        const first = resolver.resolve('a');
        const second = resolver.resolve('b');
        expect(first).toBe(second);
        releaseProbe('Quarto CLI help');

        expect(await Promise.all([first, second])).toEqual([
            '/custom/quarto',
            '/custom/quarto',
        ]);
        expect(probeCalls).toBe(1);
    });

    it('invalidate clears every configured-value cache entry', async () => {
        let probes = 0;
        const resolver = new QuartoResolver({
            getConfigured: () => '/custom/quarto',
            access: accessOk,
            probe: async () => {
                probes++;
                return 'Quarto CLI help';
            },
        });
        await resolver.resolve('a');
        await resolver.resolve('b');
        expect(probes).toBe(1);
        resolver.invalidate();
        await resolver.resolve('a');
        expect(probes).toBe(2);
    });

    it('throws when PATH and all fallbacks fail', async () => {
        const resolver = new QuartoResolver({
            getConfigured: () => '',
            access: async () => { throw new Error('ENOENT'); },
            probe: async () => { throw new Error('ENOENT'); },
            fallbacks: () => ['/a', '/b'],
        });
        await expect(resolver.resolve('a')).rejects.toThrow(QuartoNotFoundError);
    });
});

describe('defaultQuartoFallbacks', () => {
    it('contains the specified macOS paths', () => {
        expect(defaultQuartoFallbacks('darwin')).toContain('/opt/homebrew/bin/quarto');
        expect(defaultQuartoFallbacks('darwin')).toContain('/usr/local/bin/quarto');
        expect(defaultQuartoFallbacks('darwin').some((p) => p.includes('RStudio.app'))).toBe(true);
    });

    it('builds Windows paths from environment roots', () => {
        expect(defaultQuartoFallbacks('win32', {
            LOCALAPPDATA: 'C:\\Users\\me\\AppData\\Local',
            PROGRAMFILES: 'C:\\Program Files',
        })).toEqual([
            'C:\\Users\\me\\AppData\\Local\\Programs\\Quarto\\bin\\quarto.exe',
            'C:\\Program Files\\Quarto\\bin\\quarto.exe',
        ]);
    });

    it('contains the specified Linux paths', () => {
        expect(defaultQuartoFallbacks('linux')).toEqual([
            '/usr/local/bin/quarto',
            '/opt/quarto/bin/quarto',
        ]);
    });
});

describe('probeQuartoBinary teardown', () => {
    it('spawns a process-group leader and tree-kills it on timeout', async () => {
        class FakeChild extends EventEmitter {
            readonly stdout = new PassThrough();
            readonly stderr = new PassThrough();
            pid = 4321;
        }
        const child = new FakeChild();
        let detached: boolean | undefined;
        const signals: string[] = [];
        const spawnProcess = ((_bin, _args, options) => {
            detached = options.detached;
            return child as unknown as ChildProcess;
        }) as typeof spawn;

        await expect(probeQuartoBinary(
            'quarto',
            1,
            spawnProcess,
            (_child, signal) => { signals.push(signal); },
        )).rejects.toThrow('timed out');

        expect(detached).toBe(process.platform !== 'win32');
        expect(signals).toEqual(['SIGKILL']);
    });
});
