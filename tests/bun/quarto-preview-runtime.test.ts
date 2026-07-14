import { describe, expect, it } from 'bun:test';
import {
    QuartoRuntime,
    type QuartoRuntimeDeps,
    type QuartoRuntimeProcessArgs,
    type QuartoRuntimeViewUpdate,
} from '../../editors/vscode/src/quarto/quarto-preview-runtime';
import type {
    QuartoPreviewProcessLike,
    QuartoPreviewReady,
} from '../../editors/vscode/src/quarto/quarto-preview-engine';

class Deferred<T> {
    promise: Promise<T>;
    resolve!: (value: T) => void;
    reject!: (error: Error) => void;

    constructor() {
        this.promise = new Promise<T>((resolve, reject) => {
            this.resolve = resolve;
            this.reject = reject;
        });
    }
}

class FakeProcess implements QuartoPreviewProcessLike {
    readonly ready = new Deferred<QuartoPreviewReady>();
    stopDeferred: Deferred<void> | null = null;
    stopCalls = 0;
    shutdownCalls = 0;
    live = true;
    rejectReadyOnStop = false;
    stopError: Error | null = null;
    shutdownError: Error | null = null;

    start(): Promise<QuartoPreviewReady> {
        return this.ready.promise;
    }

    async stop(): Promise<void> {
        this.stopCalls++;
        if (this.rejectReadyOnStop) {
            this.ready.reject(new Error('closed during preview startup'));
        }
        if (this.stopError) throw this.stopError;
        if (this.stopDeferred) await this.stopDeferred.promise;
        this.live = false;
    }

    async shutdown(): Promise<void> {
        this.shutdownCalls++;
        if (this.shutdownError) throw this.shutdownError;
        this.live = false;
    }
}

function harness(
    mapper?: (raw: string) => Promise<string>,
    overrides: Partial<QuartoRuntimeDeps> = {},
) {
    const processes: Array<{ args: QuartoRuntimeProcessArgs; process: FakeProcess }> = [];
    const liveCountsAtSpawn: number[] = [];
    const updates: QuartoRuntimeViewUpdate[] = [];
    const lifecycleErrors: string[] = [];
    const runtime = new QuartoRuntime({
        processFactory: (args) => {
            const process = new FakeProcess();
            processes.push({ args, process });
            liveCountsAtSpawn.push(processes.filter((entry) => entry.process.live).length);
            return process;
        },
        asExternalUri: mapper ?? (async (raw) => `external:${raw}`),
        onViewUpdate: (update) => updates.push(update),
        onLifecycleError: (message) => lifecycleErrors.push(message),
        shutdownGlobalTimeoutMs: 50,
        ...overrides,
    });
    return { runtime, processes, updates, liveCountsAtSpawn, lifecycleErrors };
}

async function waitForProcessCount(
    processes: readonly unknown[],
    expected: number,
): Promise<void> {
    for (let turn = 0; turn < 10 && processes.length < expected; turn++) {
        await Promise.resolve();
    }
    expect(processes.length).toBe(expected);
}

type RuntimeHarness = ReturnType<typeof harness>;

async function beginMixedKeyDrain(h: RuntimeHarness) {
    const sourceA = '/p/a.qmd';
    const sourceB = '/p/b.qmd';
    const first = h.runtime.startOrRestart({
        ...startArgs,
        key: sourceA,
        sourceFsPath: sourceA,
        cwd: '/p',
    });
    await waitForProcessCount(h.processes, 1);
    const old = h.processes[0].process;
    old.ready.resolve({
        rawUrl: 'http://127.0.0.1:1/',
        origin: 'http://127.0.0.1:1',
        statusCode: 200,
    });
    expect((await first).kind).toBe('ready');
    old.stopDeferred = new Deferred<void>();

    const second = h.runtime.startOrRestart({
        ...startArgs,
        key: '/p',
        sourceFsPath: sourceA,
        cwd: '/p',
    });
    const third = h.runtime.startOrRestart({
        ...startArgs,
        key: '/p',
        sourceFsPath: sourceB,
        cwd: '/p',
    });
    const generation = h.runtime.getSessionsForTesting().get('/p')?.generation;
    expect(typeof generation).toBe('number');
    return { old, second, third, sourceB, generation: generation as number };
}

const startArgs = {
    key: '/project',
    quartoPath: 'quarto',
    sourceFsPath: '/project/doc.qmd',
    cwd: '/project',
};

describe('QuartoRuntime generation discipline', () => {
    it('uses one monotonic generation counter across distinct keys', async () => {
        const h = harness();
        const first = h.runtime.startOrRestart({
            ...startArgs,
            key: '/project-a',
            sourceFsPath: '/project-a/a.qmd',
            cwd: '/project-a',
        });
        const second = h.runtime.startOrRestart({
            ...startArgs,
            key: '/project-b',
            sourceFsPath: '/project-b/b.qmd',
            cwd: '/project-b',
        });
        await waitForProcessCount(h.processes, 2);
        h.processes[0].process.ready.resolve({
            rawUrl: 'http://127.0.0.1:1/',
            origin: 'http://127.0.0.1:1',
            statusCode: 200,
        });
        h.processes[1].process.ready.resolve({
            rawUrl: 'http://127.0.0.1:2/',
            origin: 'http://127.0.0.1:2',
            statusCode: 200,
        });

        expect((await first).generation).toBe(1);
        expect((await second).generation).toBe(2);
    });

    it('Stop cancels pending Preview intent before a session exists', async () => {
        const h = harness();
        const source = '/project/pending.qmd';
        const pending = h.runtime.registerPendingStart(source);

        expect(await h.runtime.stopByLookup('/project', source)).toBe('stopped');
        expect(h.runtime.reconcilePendingStart(pending, '/project')).toBe(false);
        expect(h.runtime.consumePendingStart(pending)).toBe(false);
        expect(h.processes).toHaveLength(0);
        expect(await h.runtime.stopByLookup('/project', source)).toBe('none');
    });

    it('generation Stop cancels a reconciled pending Preview intent', async () => {
        const h = harness();
        const pending = h.runtime.registerPendingStart('/project/pending.qmd');
        expect(h.runtime.reconcilePendingStart(pending, '/project')).toBe(true);

        expect(await h.runtime.stopGeneration('/project', 99)).toBe('stopped');
        expect(h.runtime.consumePendingStart(pending)).toBe(false);
        expect(h.processes).toHaveLength(0);
    });

    it('simultaneous Start/Start claims generation 2 before generation 1 can finish', async () => {
        const h = harness();
        const first = h.runtime.startOrRestart(startArgs);
        await Promise.resolve();
        const p1 = h.processes[0].process;
        const second = h.runtime.startOrRestart(startArgs);
        await Promise.resolve();
        expect(p1.stopCalls).toBe(1);
        expect(h.runtime.getSessionsForTesting().get('/project')?.generation).toBe(2);
        await waitForProcessCount(h.processes, 2);
        const p2 = h.processes[1].process;
        p1.ready.resolve({ rawUrl: 'http://127.0.0.1:1/', origin: 'http://127.0.0.1:1', statusCode: 200 });
        p2.ready.resolve({ rawUrl: 'http://127.0.0.1:2/', origin: 'http://127.0.0.1:2', statusCode: 200 });
        expect((await first).kind).toBe('superseded');
        expect((await second).kind).toBe('ready');
        expect(h.updates.at(-1)?.generation).toBe(2);
    });

    it('Start/Stop during readiness cancels the pending generation', async () => {
        const h = harness();
        const start = h.runtime.startOrRestart(startArgs);
        await Promise.resolve();
        const proc = h.processes[0].process;
        const stop = h.runtime.stopByLookup('/project', '/project/doc.qmd');
        proc.ready.resolve({ rawUrl: 'http://127.0.0.1:1/', origin: 'http://127.0.0.1:1', statusCode: 200 });
        expect(await stop).toBe('stopped');
        expect((await start).kind).toBe('superseded');
        expect(h.runtime.getSessionsForTesting().size).toBe(0);
    });

    it('intentional Stop during startup ends stopped and never emits failed', async () => {
        const h = harness();
        const start = h.runtime.startOrRestart(startArgs);
        await waitForProcessCount(h.processes, 1);
        h.processes[0].process.rejectReadyOnStop = true;

        const stop = h.runtime.stopGeneration('/project', 1);

        expect(await stop).toBe('stopped');
        expect((await start).kind).toBe('superseded');
        const states = h.updates.map((update) => update.state.kind);
        expect(states).not.toContain('failed');
        expect(states.at(-1)).toBe('stopped');
        expect(h.runtime.getSessionsForTesting().size).toBe(0);
    });

    it('panel dispose during restart cancels the claimed generation before spawn', async () => {
        const h = harness();
        const first = h.runtime.startOrRestart(startArgs);
        await Promise.resolve();
        const old = h.processes[0].process;
        old.stopDeferred = new Deferred<void>();
        const restart = h.runtime.startOrRestart(startArgs);
        // Generation 2 owns the map synchronously but is waiting for the old
        // process to stop. This is the panel-dispose race from the spec.
        expect(h.runtime.getSessionsForTesting().get('/project')?.generation).toBe(2);
        const stop = h.runtime.stopGeneration('/project', 2);
        expect(await h.runtime.stopGeneration('/project', 2)).toBe('already-stopping');
        old.stopDeferred.resolve();
        expect(await stop).toBe('stopped');
        expect((await restart).kind).toBe('superseded');
        expect(h.processes.length).toBe(1);
        old.ready.resolve({ rawUrl: 'http://127.0.0.1:1/', origin: 'http://127.0.0.1:1', statusCode: 200 });
        await first;
    });

    it('drops a stale asExternalUri continuation', async () => {
        const mapping = new Deferred<string>();
        const h = harness(() => mapping.promise);
        const first = h.runtime.startOrRestart(startArgs);
        await Promise.resolve();
        h.processes[0].process.ready.resolve({
            rawUrl: 'http://127.0.0.1:1/',
            origin: 'http://127.0.0.1:1',
            statusCode: 200,
        });
        await Promise.resolve();
        const second = h.runtime.startOrRestart(startArgs);
        mapping.resolve('external:stale');
        expect((await first).kind).toBe('superseded');
        await waitForProcessCount(h.processes, 2);
        h.processes[1].process.ready.resolve({
            rawUrl: 'http://127.0.0.1:2/',
            origin: 'http://127.0.0.1:2',
            statusCode: 200,
        });
        expect((await second).kind).toBe('ready');
        expect(h.updates.some((u) =>
            u.generation === 1 && u.state.kind === 'serving')).toBe(false);
    });

    it('ignores stale close callbacks after replacement', async () => {
        const h = harness();
        const first = h.runtime.startOrRestart(startArgs);
        await Promise.resolve();
        const old = h.processes[0];
        const second = h.runtime.startOrRestart(startArgs);
        await waitForProcessCount(h.processes, 2);
        old.args.onUnexpectedExit(9);
        expect(h.updates.some((u) =>
            u.generation === 1 && u.state.kind === 'exited-unexpectedly')).toBe(false);
        old.process.ready.resolve({ rawUrl: 'http://127.0.0.1:1/', origin: 'http://127.0.0.1:1', statusCode: 200 });
        h.processes[1].process.ready.resolve({ rawUrl: 'http://127.0.0.1:2/', origin: 'http://127.0.0.1:2', statusCode: 200 });
        await first;
        await second;
    });

    it('stop is idempotent while a shared stop promise is active', async () => {
        const h = harness();
        const start = h.runtime.startOrRestart(startArgs);
        await Promise.resolve();
        const a = h.runtime.stopGeneration('/project', 1);
        const b = h.runtime.stopGeneration('/project', 1);
        expect(await b).toBe('already-stopping');
        expect(await a).toBe('stopped');
        expect(h.processes[0].process.stopCalls).toBe(1);
        h.processes[0].process.ready.resolve({ rawUrl: 'http://127.0.0.1:1/', origin: 'http://127.0.0.1:1', statusCode: 200 });
        await start;
    });

    it('settles rejected process stops across restart and explicit cleanup', async () => {
        const h = harness();
        const first = h.runtime.startOrRestart(startArgs);
        await waitForProcessCount(h.processes, 1);
        h.processes[0].process.ready.resolve({
            rawUrl: 'http://127.0.0.1:1/',
            origin: 'http://127.0.0.1:1',
            statusCode: 200,
        });
        expect((await first).kind).toBe('ready');
        h.processes[0].process.stopError = new Error('old stop rejected');

        const restart = h.runtime.startOrRestart(startArgs);
        await waitForProcessCount(h.processes, 2);
        h.processes[1].process.ready.resolve({
            rawUrl: 'http://127.0.0.1:2/',
            origin: 'http://127.0.0.1:2',
            statusCode: 200,
        });
        expect((await restart).kind).toBe('ready');
        h.processes[1].process.stopError = new Error('current stop rejected');

        expect(await h.runtime.stopGeneration('/project', 2)).toBe('stopped');
        expect(h.runtime.getSessionsForTesting().size).toBe(0);
        expect(h.updates.at(-1)?.state.kind).toBe('stopped');
        expect(h.lifecycleErrors).toEqual([
            expect.stringContaining('old stop rejected'),
            expect.stringContaining('current stop rejected'),
        ]);
    });

    it('source alias finds the session after the freshly computed key changes', async () => {
        const h = harness();
        const start = h.runtime.startOrRestart(startArgs);
        await Promise.resolve();
        const stop = h.runtime.stopByLookup('/project/new-key', '/project/doc.qmd');
        expect(await stop).toBe('stopped');
        h.processes[0].process.ready.resolve({ rawUrl: 'http://127.0.0.1:1/', origin: 'http://127.0.0.1:1', statusCode: 200 });
        await start;
    });

    it('stops both new-key and source-alias sessions when project keying changes', async () => {
        const h = harness();
        const standaloneA = {
            ...startArgs,
            key: '/project/a.qmd',
            sourceFsPath: '/project/a.qmd',
        };
        const first = h.runtime.startOrRestart(standaloneA);
        await Promise.resolve();
        h.processes[0].process.ready.resolve({
            rawUrl: 'http://127.0.0.1:1/',
            origin: 'http://127.0.0.1:1',
            statusCode: 200,
        });
        expect((await first).kind).toBe('ready');

        const projectB = {
            ...startArgs,
            sourceFsPath: '/project/b.qmd',
        };
        const second = h.runtime.startOrRestart(projectB);
        await Promise.resolve();
        h.processes[1].process.ready.resolve({
            rawUrl: 'http://127.0.0.1:2/',
            origin: 'http://127.0.0.1:2',
            statusCode: 200,
        });
        expect((await second).kind).toBe('ready');

        const oldStandalone = h.processes[0].process;
        const oldProject = h.processes[1].process;
        oldStandalone.stopDeferred = new Deferred<void>();
        oldProject.stopDeferred = new Deferred<void>();
        const restartA = h.runtime.startOrRestart({
            ...startArgs,
            sourceFsPath: '/project/a.qmd',
        });

        expect(oldStandalone.stopCalls).toBe(1);
        expect(oldProject.stopCalls).toBe(1);
        expect([...h.runtime.getSessionsForTesting().keys()]).toEqual(['/project']);
        expect(h.processes.length).toBe(2);

        oldStandalone.stopDeferred.resolve();
        oldProject.stopDeferred.resolve();
        await waitForProcessCount(h.processes, 3);
        h.processes[2].process.ready.resolve({
            rawUrl: 'http://127.0.0.1:3/',
            origin: 'http://127.0.0.1:3',
            statusCode: 200,
        });
        expect((await restartA).kind).toBe('ready');
    });

    it('shutdown includes a predecessor that is still retiring during restart', async () => {
        const h = harness();
        const first = h.runtime.startOrRestart(startArgs);
        await waitForProcessCount(h.processes, 1);
        const old = h.processes[0].process;
        old.ready.resolve({
            rawUrl: 'http://127.0.0.1:1/',
            origin: 'http://127.0.0.1:1',
            statusCode: 200,
        });
        expect((await first).kind).toBe('ready');

        old.stopDeferred = new Deferred<void>();
        const restart = h.runtime.startOrRestart(startArgs);
        expect(old.stopCalls).toBe(1);
        expect(h.processes.length).toBe(1);

        await h.runtime.shutdown();
        expect(old.shutdownCalls).toBe(1);
        expect(old.live).toBe(false);

        old.stopDeferred.resolve();
        expect((await restart).kind).toBe('superseded');
    });

    it('three rapid restarts drain a transitive slow retirement before spawning', async () => {
        const h = harness();
        const first = h.runtime.startOrRestart(startArgs);
        await waitForProcessCount(h.processes, 1);
        const old = h.processes[0].process;
        old.ready.resolve({
            rawUrl: 'http://127.0.0.1:1/',
            origin: 'http://127.0.0.1:1',
            statusCode: 200,
        });
        expect((await first).kind).toBe('ready');

        old.stopDeferred = new Deferred<void>();
        const second = h.runtime.startOrRestart(startArgs);
        const third = h.runtime.startOrRestart(startArgs);
        await Promise.resolve();

        expect(old.stopCalls).toBe(1);
        expect(old.live).toBe(true);
        expect(h.processes.length).toBe(1);

        old.stopDeferred.resolve();
        expect((await second).kind).toBe('superseded');
        await waitForProcessCount(h.processes, 2);
        expect(old.live).toBe(false);
        expect(h.liveCountsAtSpawn).toEqual([1, 1]);

        h.processes[1].process.ready.resolve({
            rawUrl: 'http://127.0.0.1:3/',
            origin: 'http://127.0.0.1:3',
            statusCode: 200,
        });
        expect((await third).kind).toBe('ready');
    });

    it('inherits a source-alias predecessor drain through a superseded project session', async () => {
        const h = harness();
        const sourceA = '/p/a.qmd';
        const sourceB = '/p/b.qmd';
        const first = h.runtime.startOrRestart({
            ...startArgs,
            key: sourceA,
            sourceFsPath: sourceA,
            cwd: '/p',
        });
        await waitForProcessCount(h.processes, 1);
        const old = h.processes[0].process;
        old.ready.resolve({
            rawUrl: 'http://127.0.0.1:1/',
            origin: 'http://127.0.0.1:1',
            statusCode: 200,
        });
        expect((await first).kind).toBe('ready');
        old.stopDeferred = new Deferred<void>();

        // The project marker appears: S2 changes key but retains source A's
        // alias, so it records S1 as its predecessor and begins the slow stop.
        const second = h.runtime.startOrRestart({
            ...startArgs,
            key: '/p',
            sourceFsPath: sourceA,
            cwd: '/p',
        });
        const s2 = h.runtime.getSessionsForTesting().get('/p');
        expect(s2?.getPredecessorCountForTesting()).toBe(1);

        // S3 shares only S2's project key. S1 matches neither S3's key nor its
        // source alias, so the predecessor graph is the only drain path.
        const third = h.runtime.startOrRestart({
            ...startArgs,
            key: '/p',
            sourceFsPath: sourceB,
            cwd: '/p',
        });
        const s3 = h.runtime.getSessionsForTesting().get('/p');
        expect(s3?.getPredecessorCountForTesting()).toBe(1);
        await Promise.resolve();

        expect(old.stopCalls).toBe(1);
        expect(old.live).toBe(true);
        expect(h.processes.length).toBe(1);

        old.stopDeferred.resolve();
        expect((await second).kind).toBe('superseded');
        await waitForProcessCount(h.processes, 2);
        expect(old.live).toBe(false);
        expect(h.liveCountsAtSpawn).toEqual([1, 1]);

        h.processes[1].process.ready.resolve({
            rawUrl: 'http://127.0.0.1:3/',
            origin: 'http://127.0.0.1:3',
            statusCode: 200,
        });
        expect((await third).kind).toBe('ready');
    });

    it('memoizes teardown across a four-session predecessor chain', async () => {
        const h = harness();
        const sourceA = '/p/a.qmd';
        const first = h.runtime.startOrRestart({
            ...startArgs,
            key: sourceA,
            sourceFsPath: sourceA,
            cwd: '/p',
        });
        await waitForProcessCount(h.processes, 1);
        const old = h.processes[0].process;
        old.ready.resolve({
            rawUrl: 'http://127.0.0.1:1/',
            origin: 'http://127.0.0.1:1',
            statusCode: 200,
        });
        expect((await first).kind).toBe('ready');
        old.stopDeferred = new Deferred<void>();

        const second = h.runtime.startOrRestart({
            ...startArgs,
            key: '/p',
            sourceFsPath: sourceA,
            cwd: '/p',
        });
        const s2 = h.runtime.getSessionsForTesting().get('/p');
        const third = h.runtime.startOrRestart({
            ...startArgs,
            key: '/p',
            sourceFsPath: '/p/b.qmd',
            cwd: '/p',
        });
        const s3 = h.runtime.getSessionsForTesting().get('/p');
        await Promise.resolve();
        await Promise.resolve();
        const fourth = h.runtime.startOrRestart({
            ...startArgs,
            key: '/p',
            sourceFsPath: '/p/c.qmd',
            cwd: '/p',
        });
        const s4 = h.runtime.getSessionsForTesting().get('/p');

        expect(s2?.getPredecessorCountForTesting()).toBeGreaterThanOrEqual(1);
        expect(s3?.getPredecessorCountForTesting()).toBeGreaterThanOrEqual(1);
        expect(s4?.getPredecessorCountForTesting()).toBeGreaterThanOrEqual(1);
        expect(old.stopCalls).toBe(1);
        expect(old.live).toBe(true);
        expect(h.processes.length).toBe(1);

        old.stopDeferred.resolve();
        expect((await second).kind).toBe('superseded');
        expect((await third).kind).toBe('superseded');
        await waitForProcessCount(h.processes, 2);
        expect(old.stopCalls).toBe(1);
        expect(old.live).toBe(false);
        expect(h.liveCountsAtSpawn).toEqual([1, 1]);

        h.processes[1].process.ready.resolve({
            rawUrl: 'http://127.0.0.1:4/',
            origin: 'http://127.0.0.1:4',
            statusCode: 200,
        });
        expect((await fourth).kind).toBe('ready');
    });

    it('explicit Stop stays discoverable through a mixed-key predecessor drain', async () => {
        const h = harness();
        const drain = await beginMixedKeyDrain(h);

        const stop = h.runtime.stopByLookup('/p', drain.sourceB);
        expect(await h.runtime.stopByLookup('/p', drain.sourceB)).toBe(
            'already-stopping',
        );
        const fourth = h.runtime.startOrRestart({
            ...startArgs,
            key: '/p',
            sourceFsPath: drain.sourceB,
            cwd: '/p',
        });
        await Promise.resolve();

        expect(drain.old.live).toBe(true);
        expect(h.processes.length).toBe(1);

        drain.old.stopDeferred?.resolve();
        expect(await stop).toBe('stopped');
        expect((await drain.second).kind).toBe('superseded');
        expect((await drain.third).kind).toBe('superseded');
        await waitForProcessCount(h.processes, 2);
        expect(drain.old.live).toBe(false);
        expect(h.liveCountsAtSpawn).toEqual([1, 1]);

        h.processes[1].process.ready.resolve({
            rawUrl: 'http://127.0.0.1:4/',
            origin: 'http://127.0.0.1:4',
            statusCode: 200,
        });
        expect((await fourth).kind).toBe('ready');
    });

    it('panel-dispose stop stays discoverable through a mixed-key predecessor drain', async () => {
        const h = harness();
        const drain = await beginMixedKeyDrain(h);

        const disposeStop = h.runtime.stopGeneration('/p', drain.generation);
        const fourth = h.runtime.startOrRestart({
            ...startArgs,
            key: '/p',
            sourceFsPath: drain.sourceB,
            cwd: '/p',
        });
        await Promise.resolve();

        expect(drain.old.live).toBe(true);
        expect(h.processes.length).toBe(1);

        drain.old.stopDeferred?.resolve();
        expect(await disposeStop).toBe('stopped');
        expect((await drain.second).kind).toBe('superseded');
        expect((await drain.third).kind).toBe('superseded');
        await waitForProcessCount(h.processes, 2);
        expect(drain.old.live).toBe(false);
        expect(h.liveCountsAtSpawn).toEqual([1, 1]);

        h.processes[1].process.ready.resolve({
            rawUrl: 'http://127.0.0.1:4/',
            origin: 'http://127.0.0.1:4',
            statusCode: 200,
        });
        expect((await fourth).kind).toBe('ready');
    });

    it('releases drained predecessor references across sequential restarts', async () => {
        const h = harness();
        const restartCount = 12;

        for (let index = 0; index < restartCount; index++) {
            const start = h.runtime.startOrRestart(startArgs);
            await waitForProcessCount(h.processes, index + 1);
            h.processes[index].process.ready.resolve({
                rawUrl: `http://127.0.0.1:${index + 1}/`,
                origin: `http://127.0.0.1:${index + 1}`,
                statusCode: 200,
            });
            expect((await start).kind).toBe('ready');
            expect(
                h.runtime.getSessionsForTesting()
                    .get('/project')
                    ?.getPredecessorCountForTesting(),
            ).toBeLessThanOrEqual(1);
        }

        expect(h.liveCountsAtSpawn.every((count) => count === 1)).toBe(true);
    });

    it('emits stopped for an explicitly stopped live generation', async () => {
        const h = harness();
        const start = h.runtime.startOrRestart(startArgs);
        await waitForProcessCount(h.processes, 1);
        h.processes[0].process.ready.resolve({
            rawUrl: 'http://127.0.0.1:1/',
            origin: 'http://127.0.0.1:1',
            statusCode: 200,
        });
        expect((await start).kind).toBe('ready');

        expect(await h.runtime.stopGeneration('/project', 1)).toBe('stopped');
        expect(h.updates.at(-1)?.state).toEqual({ kind: 'stopped' });
        expect(h.runtime.getSessionsForTesting().size).toBe(0);
    });

    it('startup failure leaves the failed panel state but removes the dead session', async () => {
        const h = harness();
        const start = h.runtime.startOrRestart(startArgs);
        await Promise.resolve();
        h.processes[0].process.ready.reject(new Error('preview failed'));
        expect((await start).kind).toBe('failed');
        expect(h.runtime.getSessionsForTesting().size).toBe(0);
        expect(h.updates.at(-1)?.state.kind).toBe('failed');
        expect(await h.runtime.stopByLookup('/project', '/project/doc.qmd')).toBe('none');
    });

    it('shutdown uses immediate process shutdown and rejects later starts', async () => {
        const h = harness();
        const start = h.runtime.startOrRestart(startArgs);
        await Promise.resolve();
        await h.runtime.shutdown();
        expect(h.processes[0].process.shutdownCalls).toBe(1);
        h.processes[0].process.ready.resolve({ rawUrl: 'http://127.0.0.1:1/', origin: 'http://127.0.0.1:1', statusCode: 200 });
        await start;
        await expect(h.runtime.startOrRestart(startArgs)).rejects.toThrow('deactivating');
    });

    it('settles and logs an injected shutdown rejection', async () => {
        const h = harness();
        const start = h.runtime.startOrRestart(startArgs);
        await waitForProcessCount(h.processes, 1);
        h.processes[0].process.shutdownError = new Error('shutdown rejected');

        await expect(h.runtime.shutdown()).resolves.toBeUndefined();
        expect(h.runtime.getSessionsForTesting().size).toBe(0);
        expect(h.lifecycleErrors).toEqual([
            expect.stringContaining('shutdown rejected'),
        ]);
        h.processes[0].process.ready.resolve({
            rawUrl: 'http://127.0.0.1:1/',
            origin: 'http://127.0.0.1:1',
            statusCode: 200,
        });
        await start;
    });

    it('cancels the global shutdown bound when child shutdown settles first', async () => {
        let scheduledMs: number | null = null;
        let cancelCalls = 0;
        const h = harness(undefined, {
            shutdownDelay: (ms) => {
                scheduledMs = ms;
                return {
                    promise: new Promise<void>(() => undefined),
                    cancel: () => { cancelCalls++; },
                };
            },
        });
        const start = h.runtime.startOrRestart(startArgs);
        await waitForProcessCount(h.processes, 1);
        h.processes[0].process.ready.resolve({
            rawUrl: 'http://127.0.0.1:1/',
            origin: 'http://127.0.0.1:1',
            statusCode: 200,
        });
        await start;

        await h.runtime.shutdown();

        expect(scheduledMs).toBe(50);
        expect(cancelCalls).toBe(1);
    });
});
