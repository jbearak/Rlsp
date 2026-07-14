import { describe, expect, it } from 'bun:test';
import { QuartoCommandLifecycle } from '../../editors/vscode/src/quarto/quarto-command-lifecycle';
import { createSafeQuartoOutputChannel } from '../../editors/vscode/src/quarto/quarto-output';

class Deferred<T> {
    readonly promise: Promise<T>;
    resolve!: (value: T) => void;

    constructor() {
        this.promise = new Promise<T>((resolve) => { this.resolve = resolve; });
    }
}

describe('Quarto command deactivation lifecycle', () => {
    it('invokes command ownership synchronously and awaits its continuation', async () => {
        const release = new Deferred<void>();
        let entered = false;
        let cancelCalls = 0;
        const lifecycle = new QuartoCommandLifecycle({
            delay: () => ({
                promise: new Promise<void>(() => undefined),
                cancel: () => { cancelCalls++; },
            }),
        });
        const work = lifecycle.run(async () => {
            entered = true;
            await release.promise;
        });
        expect(entered).toBe(true);

        let shutdownSettled = false;
        const shutdown = lifecycle.shutdown().then(() => { shutdownSettled = true; });
        await Promise.resolve();
        expect(shutdownSettled).toBe(false);

        release.resolve();
        await work;
        await shutdown;
        expect(cancelCalls).toBe(1);
    });

    it('makes writes from a bounded-out continuation harmless after disposal', async () => {
        const bound = new Deferred<void>();
        const resume = new Deferred<void>();
        const writes: string[] = [];
        let rawDisposed = false;
        const output = createSafeQuartoOutputChannel({
            name: 'test',
            append: (value: string) => {
                if (rawDisposed) throw new Error('append after dispose');
                writes.push(value);
            },
            appendLine: (value: string) => {
                if (rawDisposed) throw new Error('appendLine after dispose');
                writes.push(`${value}\n`);
            },
            replace() {},
            clear() {},
            show() {},
            hide() {},
            dispose: () => { rawDisposed = true; },
        } as never);
        const lifecycle = new QuartoCommandLifecycle({
            delay: () => ({ promise: bound.promise, cancel() {} }),
        });
        const work = lifecycle.run(async () => {
            await resume.promise;
            output.appendLine('late');
        });

        const shutdown = lifecycle.shutdown();
        bound.resolve();
        await shutdown;
        output.dispose();
        resume.resolve();

        await expect(work).resolves.toBeUndefined();
        expect(writes).toEqual([]);
    });
});
