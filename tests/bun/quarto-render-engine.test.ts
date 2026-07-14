import { describe, expect, it } from 'bun:test';
import { EventEmitter } from 'events';
import { readFileSync } from 'fs';
import { PassThrough } from 'stream';
import type { ChildProcess, spawn } from 'child_process';
import type { KnitEngineResult } from '../../editors/vscode/src/knit/knit-engine';
import { parseRenderedOutputPath } from '../../editors/vscode/src/knit/output-path';
import {
    appendQuartoRenderTail,
    clampQuartoRenderTimeoutMs,
    classifyQuartoRenderResult,
    DEFAULT_QUARTO_RENDER_TIMEOUT_MS,
    MAX_NODE_TIMER_MS,
    normalizeQuartoRenderTimeoutMs,
    QuartoRenderEngine,
    QUARTO_RENDER_RETAINED_OUTPUT_CHARS,
} from '../../editors/vscode/src/quarto/quarto-render-engine';

function result(overrides: Partial<KnitEngineResult>): KnitEngineResult {
    return {
        exitCode: 0,
        stdout: '',
        stderr: '',
        cancelled: false,
        timedOut: false,
        spawnError: null,
        ...overrides,
    };
}

describe('Quarto render result handling', () => {
    it('mirrors knit precedence when cancellation races the timeout', () => {
        expect(classifyQuartoRenderResult(result({
            cancelled: true,
            timedOut: true,
            exitCode: null,
        }))).toBe('cancelled');
    });

    it('keeps spawn errors ahead of cancellation', () => {
        expect(classifyQuartoRenderResult(result({
            spawnError: new Error('spawn failed'),
            cancelled: true,
            timedOut: true,
            exitCode: null,
        }))).toBe('spawnError');
    });

    it('caps retained output while preserving a trailing output path', () => {
        let retained = appendQuartoRenderTail(
            '',
            'x'.repeat(QUARTO_RENDER_RETAINED_OUTPUT_CHARS + 100),
        );
        retained = appendQuartoRenderTail(
            retained,
            '\nOutput created: rendered/report.html\n',
        );

        expect(retained.length).toBe(QUARTO_RENDER_RETAINED_OUTPUT_CHARS);
        expect(parseRenderedOutputPath(retained).paths).toEqual([
            'rendered/report.html',
        ]);
    });

    it('caps both the settings schema and armed Node timer delay', () => {
        expect(clampQuartoRenderTimeoutMs(Number.MAX_SAFE_INTEGER)).toBe(MAX_NODE_TIMER_MS);
        expect(clampQuartoRenderTimeoutMs(600_000)).toBe(600_000);
        expect(clampQuartoRenderTimeoutMs(0)).toBe(DEFAULT_QUARTO_RENDER_TIMEOUT_MS);

        const manifest = JSON.parse(readFileSync(
            new URL('../../editors/vscode/package.json', import.meta.url),
            'utf8',
        ));
        const schema = manifest.contributes.configuration.properties[
            'raven.quarto.render.timeoutMs'
        ];
        expect(schema.maximum).toBe(MAX_NODE_TIMER_MS);
    });

    it('normalizes invalid configured timeouts to the default', () => {
        for (const configured of [0, -1, Number.NaN, Infinity, undefined, '1']) {
            expect(normalizeQuartoRenderTimeoutMs(configured)).toBe(
                DEFAULT_QUARTO_RENDER_TIMEOUT_MS,
            );
        }
        expect(normalizeQuartoRenderTimeoutMs(1)).toBe(1);
        expect(normalizeQuartoRenderTimeoutMs(45_000)).toBe(45_000);
    });
});

class FakeChild extends EventEmitter {
    readonly stdout = new PassThrough();
    readonly stderr = new PassThrough();
    readonly signals: string[] = [];
    pid: number | undefined;

    kill(signal?: string): boolean {
        this.signals.push(signal ?? 'SIGTERM');
        queueMicrotask(() => this.emit('close', null));
        return true;
    }
}

class NeverClosingFakeChild extends FakeChild {
    override kill(signal?: string): boolean {
        this.signals.push(signal ?? 'SIGTERM');
        return true;
    }
}

class FakeCancellation {
    isCancellationRequested = false;
    private listener: (() => void) | null = null;

    onCancellationRequested(listener: () => void): { dispose(): void } {
        this.listener = listener;
        return { dispose: () => { this.listener = null; } };
    }

    cancel(): void {
        this.isCancellationRequested = true;
        this.listener?.();
    }
}

describe('QuartoRenderEngine lifecycle', () => {
    it('does not spawn when cancellation is already requested', async () => {
        let spawnCalls = 0;
        const spawnProcess = (() => {
            spawnCalls++;
            return new FakeChild() as unknown as ChildProcess;
        }) as typeof spawn;
        const engine = new QuartoRenderEngine();
        const result = await engine.run({
            quartoPath: 'quarto',
            sourceFsPath: '/project/doc.qmd',
            cwd: '/project',
            timeoutMs: 60_000,
            output: { append() {}, appendLine() {} } as never,
            cancellation: {
                isCancellationRequested: true,
                onCancellationRequested: () => ({ dispose() {} }),
            } as never,
            spawnProcess,
        });

        expect(result).toEqual({
            exitCode: null,
            stdout: '',
            stderr: '',
            cancelled: true,
            timedOut: false,
            spawnError: null,
        });
        expect(spawnCalls).toBe(0);
    });

    it('terminates live children on shutdown and rejects later spawns', async () => {
        const child = new FakeChild();
        let spawnCalls = 0;
        const spawnProcess = (() => {
            spawnCalls++;
            return child as unknown as ChildProcess;
        }) as typeof spawn;
        const engine = new QuartoRenderEngine();
        const cancellation = {
            isCancellationRequested: false,
            onCancellationRequested: () => ({ dispose() {} }),
        };
        const running = engine.run({
            quartoPath: 'quarto',
            sourceFsPath: '/project/doc.qmd',
            cwd: '/project',
            timeoutMs: 60_000,
            output: { append() {}, appendLine() {} } as never,
            cancellation: cancellation as never,
            spawnProcess,
        });

        expect(engine.getLiveChildCountForTesting()).toBe(1);
        await engine.shutdown();
        const shutdownResult = await running;
        expect(child.signals).toEqual(['SIGTERM']);
        expect(shutdownResult.cancelled).toBe(true);
        expect(classifyQuartoRenderResult(shutdownResult)).toBe('cancelled');
        expect(engine.getLiveChildCountForTesting()).toBe(0);

        const afterShutdown = await engine.run({
            quartoPath: 'quarto',
            sourceFsPath: '/project/later.qmd',
            cwd: '/project',
            timeoutMs: 60_000,
            output: { append() {}, appendLine() {} } as never,
            cancellation: cancellation as never,
            spawnProcess,
        });
        expect(afterShutdown.cancelled).toBe(true);
        expect(spawnCalls).toBe(1);
    });

    it('bounds unconfirmed shutdown and detaches output listeners', async () => {
        const child = new NeverClosingFakeChild();
        const spawnProcess = (() => child as unknown as ChildProcess) as typeof spawn;
        const engine = new QuartoRenderEngine();
        const output: string[] = [];
        const cancellation = {
            isCancellationRequested: false,
            onCancellationRequested: () => ({ dispose() {} }),
        };
        const running = engine.run({
            quartoPath: 'quarto',
            sourceFsPath: '/project/doc.qmd',
            cwd: '/project',
            timeoutMs: 60_000,
            output: {
                append: (value: string) => output.push(value),
                appendLine: (value: string) => output.push(`${value}\n`),
            } as never,
            cancellation: cancellation as never,
            spawnProcess,
            shutdownTermGraceMs: 1,
            shutdownKillWaitMs: 1,
        });
        child.stdout.write('before-shutdown');

        await engine.shutdown();

        expect(child.signals).toEqual(['SIGTERM', 'SIGKILL']);
        expect(output.join('')).toContain(
            'did not confirm exit after SIGKILL; abandoning',
        );
        const afterShutdown = output.join('');
        child.stdout.write('after-shutdown');
        child.stderr.write('after-shutdown-error\n');
        await Promise.resolve();
        expect(output.join('')).toBe(afterShutdown);

        child.emit('close', null);
        await running;
    });

    it('shares one ladder when cancellation and shutdown overlap', async () => {
        const child = new NeverClosingFakeChild();
        const spawnProcess = (() => child as unknown as ChildProcess) as typeof spawn;
        const engine = new QuartoRenderEngine();
        const cancellation = new FakeCancellation();
        let disposed = false;
        let writesAfterDispose = 0;
        const running = engine.run({
            quartoPath: 'quarto',
            sourceFsPath: '/project/doc.qmd',
            cwd: '/project',
            timeoutMs: 60_000,
            output: {
                append() {
                    if (disposed) writesAfterDispose++;
                },
                appendLine() {
                    if (disposed) {
                        writesAfterDispose++;
                        throw new Error('appendLine after dispose');
                    }
                },
            } as never,
            cancellation: cancellation as never,
            spawnProcess,
            signalGraceMs: 50,
            shutdownTermGraceMs: 1,
            shutdownKillWaitMs: 1,
        });

        cancellation.cancel();
        await engine.shutdown();
        disposed = true;
        const result = await running;

        expect(child.signals).toEqual(['SIGINT', 'SIGTERM', 'SIGKILL']);
        expect(result.cancelled).toBe(true);
        expect(result.timedOut).toBe(false);
        expect(writesAfterDispose).toBe(0);
    });

    it('bounds cancellation completion when the child never closes', async () => {
        const child = new NeverClosingFakeChild();
        const spawnProcess = (() => child as unknown as ChildProcess) as typeof spawn;
        const engine = new QuartoRenderEngine();
        const cancellation = new FakeCancellation();
        const running = engine.run({
            quartoPath: 'quarto',
            sourceFsPath: '/project/doc.qmd',
            cwd: '/project',
            timeoutMs: 60_000,
            output: { append() {}, appendLine() {} } as never,
            cancellation: cancellation as never,
            spawnProcess,
            signalGraceMs: 1,
            shutdownKillWaitMs: 1,
        });

        cancellation.cancel();
        const result = await running;

        expect(child.signals).toEqual(['SIGINT', 'SIGTERM', 'SIGKILL']);
        expect(result.cancelled).toBe(true);
        expect(result.timedOut).toBe(false);
        expect(result.exitCode).toBeNull();
    });

    it('flushes partial stderr exactly once before the abandon warning', async () => {
        const child = new NeverClosingFakeChild();
        const spawnProcess = (() => child as unknown as ChildProcess) as typeof spawn;
        const engine = new QuartoRenderEngine();
        const cancellation = new FakeCancellation();
        const output: string[] = [];
        const running = engine.run({
            quartoPath: 'quarto',
            sourceFsPath: '/project/doc.qmd',
            cwd: '/project',
            timeoutMs: 60_000,
            output: {
                append: (value: string) => output.push(value),
                appendLine: (value: string) => output.push(`${value}\n`),
            } as never,
            cancellation: cancellation as never,
            spawnProcess,
            signalGraceMs: 1,
            shutdownKillWaitMs: 1,
        });
        child.stderr.write('partial diagnostic');

        cancellation.cancel();
        await running;

        const rendered = output.join('');
        expect(rendered).toContain(
            '[stderr] partial diagnostic\n[quarto] render process',
        );
        expect(rendered.match(/\[stderr\] partial diagnostic\n/g)).toHaveLength(1);
        expect(rendered).not.toContain(
            '[stderr] partial diagnostic[quarto] render process',
        );
    });

    it('bounds timeout completion and tolerates a disposed warning sink', async () => {
        const child = new NeverClosingFakeChild();
        const spawnProcess = (() => child as unknown as ChildProcess) as typeof spawn;
        const engine = new QuartoRenderEngine();
        const cancellation = new FakeCancellation();
        const result = await engine.run({
            quartoPath: 'quarto',
            sourceFsPath: '/project/doc.qmd',
            cwd: '/project',
            timeoutMs: 1,
            output: {
                append() {},
                appendLine() {
                    throw new Error('disposed output channel');
                },
            } as never,
            cancellation: cancellation as never,
            spawnProcess,
            signalGraceMs: 1,
            shutdownKillWaitMs: 1,
        });

        expect(child.signals).toEqual(['SIGINT', 'SIGTERM', 'SIGKILL']);
        expect(result.cancelled).toBe(false);
        expect(result.timedOut).toBe(true);
        expect(result.exitCode).toBeNull();
    });
});
