import { describe, expect, it } from 'bun:test';
import type {
    QuartoPreviewProcessLike,
    QuartoPreviewReady,
} from '../../editors/vscode/src/quarto/quarto-preview-engine';
import type {
    QuartoPreviewProxyLike,
    QuartoPreviewProxyReady,
} from '../../editors/vscode/src/quarto/quarto-preview-proxy';
import { QuartoPreviewWithProxyProcess } from '../../editors/vscode/src/quarto/quarto-preview-with-proxy';

class Deferred<T> {
    readonly promise: Promise<T>;
    resolve!: (value: T) => void;
    reject!: (error: Error) => void;

    constructor() {
        this.promise = new Promise<T>((resolve, reject) => {
            this.resolve = resolve;
            this.reject = reject;
        });
    }
}

class FakeInner implements QuartoPreviewProcessLike {
    readonly ready: QuartoPreviewReady = {
        rawUrl: 'http://127.0.0.1:4800/chapter/?preview=1',
        origin: 'http://127.0.0.1:4800',
        statusCode: 200,
    };

    constructor(private readonly events: string[]) {}

    async start(): Promise<QuartoPreviewReady> {
        this.events.push('inner:start');
        return this.ready;
    }

    async stop(): Promise<void> {
        this.events.push('inner:stop');
    }

    async shutdown(): Promise<void> {
        this.events.push('inner:shutdown');
    }
}

class FakeProxy implements QuartoPreviewProxyLike {
    readonly bind = new Deferred<QuartoPreviewProxyReady>();

    constructor(private readonly events: string[]) {}

    start(): Promise<QuartoPreviewProxyReady> {
        this.events.push('proxy:start');
        return this.bind.promise;
    }

    async close(): Promise<void> {
        this.events.push('proxy:close');
    }
}

describe('QuartoPreviewWithProxyProcess', () => {
    it('frames Quarto directly and never creates a proxy when assets are unavailable', async () => {
        const events: string[] = [];
        let proxyCreations = 0;
        const process = new QuartoPreviewWithProxyProcess({
            output: { appendLine() {} },
            onUnexpectedExit: () => undefined,
            bridgeAssets: undefined,
            createInner: () => new FakeInner(events),
            proxyFactory: () => {
                proxyCreations++;
                return new FakeProxy(events);
            },
            probe: async () => { throw new Error('probe must not run'); },
        });

        expect(await process.start()).toEqual(new FakeInner([]).ready);
        expect(proxyCreations).toBe(0);
        expect(events).toEqual(['inner:start']);
    });

    it('returns and probes the proxy origin while preserving the Quarto page path', async () => {
        const events: string[] = [];
        const proxy = new FakeProxy(events);
        const probed: string[] = [];
        const process = createProcess(events, proxy, {
            probe: async (url) => {
                probed.push(url);
                return 204;
            },
        });
        const starting = process.start();
        proxy.bind.resolve({
            origin: 'http://127.0.0.1:4900',
            url: 'http://127.0.0.1:4900/',
        });

        expect(await starting).toEqual({
            rawUrl: 'http://127.0.0.1:4900/chapter/?preview=1',
            browserUrl: 'http://127.0.0.1:4800/chapter/?preview=1',
            origin: 'http://127.0.0.1:4900',
            statusCode: 204,
        });
        expect(probed).toEqual([
            'http://127.0.0.1:4900/chapter/?preview=1',
        ]);
    });

    it('shares concurrent stop/shutdown and closes proxy before tightened teardown', async () => {
        const events: string[] = [];
        const proxy = new FakeProxy(events);
        const process = createProcess(events, proxy);
        const starting = process.start();
        proxy.bind.resolve({
            origin: 'http://127.0.0.1:4900',
            url: 'http://127.0.0.1:4900/',
        });
        await starting;

        const first = process.stop();
        const second = process.shutdown();
        expect(second).toBe(first);
        await first;
        expect(events.slice(-2)).toEqual(['proxy:close', 'inner:shutdown']);
    });

    it('closes an installed proxy and stops Quarto when Stop lands during bind', async () => {
        const events: string[] = [];
        const proxy = new FakeProxy(events);
        const process = createProcess(events, proxy);
        const starting = process.start();
        await Promise.resolve();
        expect(events).toEqual(['inner:start', 'proxy:start']);

        await process.stop();
        await expect(starting).rejects.toThrow('proxy startup was stopped');
        expect(events).toEqual([
            'inner:start',
            'proxy:start',
            'proxy:close',
            'inner:stop',
        ]);
    });

    it('falls back to Quarto and logs when the proxy bind fails', async () => {
        const events: string[] = [];
        const proxy = new FakeProxy(events);
        const diagnostics: string[] = [];
        const process = createProcess(events, proxy, { diagnostics });
        const starting = process.start();
        proxy.bind.reject(new Error('listen EADDRNOTAVAIL'));

        expect(await starting).toEqual(new FakeInner([]).ready);
        expect(events).toEqual(['inner:start', 'proxy:start', 'proxy:close']);
        expect(diagnostics).toEqual([
            '[quarto] Preview proxy unavailable; using Quarto directly: ' +
            'listen EADDRNOTAVAIL',
        ]);
    });

    it('falls back to Quarto and logs when the proxy readiness probe fails', async () => {
        const events: string[] = [];
        const proxy = new FakeProxy(events);
        const diagnostics: string[] = [];
        const process = createProcess(events, proxy, {
            diagnostics,
            probe: async () => { throw new Error('proxy probe timed out'); },
        });
        const starting = process.start();
        proxy.bind.resolve({
            origin: 'http://127.0.0.1:4900',
            url: 'http://127.0.0.1:4900/',
        });

        expect(await starting).toEqual(new FakeInner([]).ready);
        expect(events).toEqual(['inner:start', 'proxy:start', 'proxy:close']);
        expect(diagnostics).toEqual([
            '[quarto] Preview proxy unavailable; using Quarto directly: ' +
            'proxy probe timed out',
        ]);
    });

    it('treats a proxy 502 as unavailable and falls back to the Quarto origin', async () => {
        const events: string[] = [];
        const proxy = new FakeProxy(events);
        const diagnostics: string[] = [];
        const process = createProcess(events, proxy, {
            diagnostics,
            probe: async () => 502,
        });
        const starting = process.start();
        proxy.bind.resolve({
            origin: 'http://127.0.0.1:4900',
            url: 'http://127.0.0.1:4900/',
        });

        expect(await starting).toEqual(new FakeInner([]).ready);
        expect(events).toEqual(['inner:start', 'proxy:start', 'proxy:close']);
        expect(diagnostics).toEqual([
            '[quarto] Preview proxy unavailable; using Quarto directly: ' +
            'preview proxy readiness returned HTTP 502',
        ]);
    });

    it('does not fall back when Stop aborts the proxy readiness probe', async () => {
        const events: string[] = [];
        const proxy = new FakeProxy(events);
        const diagnostics: string[] = [];
        const probeStarted = new Deferred<void>();
        const process = createProcess(events, proxy, {
            diagnostics,
            probe: async (_url, signal) => {
                probeStarted.resolve();
                return new Promise<number>((_resolve, reject) => {
                    signal.addEventListener(
                        'abort',
                        () => reject(new Error('probe aborted')),
                        { once: true },
                    );
                });
            },
        });
        const starting = process.start();
        proxy.bind.resolve({
            origin: 'http://127.0.0.1:4900',
            url: 'http://127.0.0.1:4900/',
        });
        await probeStarted.promise;

        const stopping = process.stop();
        await expect(starting).rejects.toThrow('probe aborted');
        await stopping;
        expect(diagnostics).toEqual([]);
        expect(events).toEqual([
            'inner:start',
            'proxy:start',
            'proxy:close',
            'inner:stop',
        ]);
    });

    it('closes the proxy before propagating an unexpected Quarto exit', async () => {
        const events: string[] = [];
        const proxy = new FakeProxy(events);
        let unexpectedExit: ((code: number | null) => void) | null = null;
        const process = new QuartoPreviewWithProxyProcess({
            output: { appendLine() {} },
            onUnexpectedExit: (code) => events.push(`outer:exit:${String(code)}`),
            bridgeAssets: { javascript: '', css: '' },
            createInner: (callback) => {
                unexpectedExit = callback;
                return new FakeInner(events);
            },
            proxyFactory: () => proxy,
            probe: async () => 200,
        });
        const starting = process.start();
        proxy.bind.resolve({
            origin: 'http://127.0.0.1:4900',
            url: 'http://127.0.0.1:4900/',
        });
        await starting;

        (unexpectedExit as ((code: number | null) => void) | null)?.(7);
        await Promise.resolve();
        await Promise.resolve();
        expect(events.slice(-2)).toEqual(['proxy:close', 'outer:exit:7']);
    });
});

function createProcess(
    events: string[],
    proxy: FakeProxy,
    options: {
        probe?: (url: string, signal: AbortSignal) => Promise<number>;
        diagnostics?: string[];
    } = {},
): QuartoPreviewWithProxyProcess {
    return new QuartoPreviewWithProxyProcess({
        output: {
            appendLine: (message) => options.diagnostics?.push(message),
        },
        onUnexpectedExit: () => undefined,
        bridgeAssets: { javascript: '', css: '' },
        createInner: () => new FakeInner(events),
        proxyFactory: () => proxy,
        probe: options.probe ?? (async () => 200),
    });
}
