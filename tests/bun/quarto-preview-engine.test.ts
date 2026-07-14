import { describe, expect, it } from 'bun:test';
import type { ChildProcess, spawn } from 'child_process';
import { EventEmitter } from 'events';
import { readFileSync } from 'fs';
import { PassThrough } from 'stream';
import {
    PrefixedStderrWriter,
    probeQuartoPreviewUrl,
    QuartoOutputLineBuffer,
    QuartoPreviewProbeError,
    QuartoPreviewProcess,
    QUARTO_PREVIEW_BROWSE_CORRELATION_DELAY_MS,
    QUARTO_PREVIEW_LINE_CARRY_LIMIT,
} from '../../editors/vscode/src/quarto/quarto-preview-engine';

class Deferred<T> {
    readonly promise: Promise<T>;
    resolve!: (value: T) => void;

    constructor() {
        this.promise = new Promise<T>((resolve) => {
            this.resolve = resolve;
        });
    }
}

class FakeChild extends EventEmitter {
    readonly stdout = new PassThrough();
    readonly stderr = new PassThrough();
    pid: number | undefined = 123;

    kill(): boolean {
        return true;
    }
}

class NeverClosingFakeChild extends FakeChild {
    readonly signals: string[] = [];
    pid = undefined;

    override kill(signal?: string): boolean {
        this.signals.push(signal ?? 'SIGTERM');
        return true;
    }
}

function processFor(
    child: FakeChild,
    overrides: Partial<ConstructorParameters<typeof QuartoPreviewProcess>[0]> = {},
): QuartoPreviewProcess {
    const spawnProcess = (() => child as unknown as ChildProcess) as typeof spawn;
    return new QuartoPreviewProcess({
        quartoPath: 'quarto',
        sourceFsPath: '/project/doc.qmd',
        cwd: '/project',
        output: { append() {}, appendLine() {} } as never,
        onUnexpectedExit() {},
        startupTimeoutMs: 1_000,
        spawnProcess,
        ...overrides,
    });
}

describe('probeQuartoPreviewUrl', () => {
    it('reports trailing connection errors instead of a stale earlier 404', async () => {
        const outcomes: Array<number | Error> = [
            404,
            new Error('connect ECONNREFUSED'),
            new Error('socket closed'),
        ];
        const request = async (): Promise<number> => {
            const outcome = outcomes.shift();
            if (outcome instanceof Error) throw outcome;
            return outcome ?? 500;
        };

        try {
            await probeQuartoPreviewUrl('http://127.0.0.1:1/', 3, 0, request);
            throw new Error('expected probe failure');
        } catch (err) {
            expect(err).toBeInstanceOf(QuartoPreviewProbeError);
            expect((err as QuartoPreviewProbeError).kind).toBe('connection');
            expect((err as Error).message).toContain('socket closed');
            expect((err as Error).message).not.toContain('not browser-previewable');
        }
    });

    it('reports not-browser-previewable only when 404 is the final outcome', async () => {
        const outcomes: Array<number | Error> = [new Error('not bound'), 404];
        const request = async (): Promise<number> => {
            const outcome = outcomes.shift();
            if (outcome instanceof Error) throw outcome;
            return outcome ?? 500;
        };

        try {
            await probeQuartoPreviewUrl('http://127.0.0.1:1/', 2, 0, request);
            throw new Error('expected probe failure');
        } catch (err) {
            expect(err).toBeInstanceOf(QuartoPreviewProbeError);
            expect((err as QuartoPreviewProbeError).kind).toBe(
                'not-browser-previewable',
            );
        }
    });
});

describe('Quarto preview output line handling', () => {
    it('preserves a new empty line after swallowing a split CRLF', () => {
        const lines: string[] = [];
        const buffer = new QuartoOutputLineBuffer((line) => lines.push(line));

        buffer.feed('A\r');
        buffer.feed('\n');
        buffer.feed('\nB\n');

        expect(lines).toEqual(['A', '', 'B']);
    });

    it('flushes CR progress records, preserves split CRLF, and caps carry', () => {
        const lines: string[] = [];
        const writer = new PrefixedStderrWriter({
            appendLine: (line: string) => lines.push(line),
        } as never);
        writer.feed('10%\r');
        writer.feed('\n20%\r30%\r');
        writer.finish();
        expect(lines).toEqual([
            '[stderr] 10%',
            '[stderr] 20%',
            '[stderr] 30%',
        ]);

        const capped: string[] = [];
        const cappedWriter = new PrefixedStderrWriter({
            appendLine: (line: string) => capped.push(line),
        } as never);
        cappedWriter.feed('x'.repeat(QUARTO_PREVIEW_LINE_CARRY_LIMIT + 1));
        cappedWriter.finish();
        expect(capped.map((line) => line.slice('[stderr] '.length).length)).toEqual([
            QUARTO_PREVIEW_LINE_CARRY_LIMIT,
            1,
        ]);
    });

    it('reassembles stdout and stderr independently before URL scanning', async () => {
        const child = new FakeChild();
        const probed: string[] = [];
        const process = processFor(child, {
            browseCorrelationDelayMs: 50,
            probe: async (url) => {
                probed.push(url);
                return 204;
            },
        });
        const starting = process.start();

        child.stdout.write('Browse at http://localhost:4444/cha');
        child.stderr.write('unrelated stderr fragment');
        child.stdout.write('pter/?preview=1\n');
        child.stderr.write(' completed\nListening on http://127.0.0.1:4444/\n');

        const ready = await starting;
        expect(probed).toEqual([
            'http://127.0.0.1:4444/chapter/?preview=1',
        ]);
        expect(ready.rawUrl).toBe(
            'http://127.0.0.1:4444/chapter/?preview=1',
        );
    });
});

describe('Quarto preview process stop ladder', () => {
    it('self-stops exactly once after a readiness probe failure', async () => {
        const child = new NeverClosingFakeChild();
        const output: string[] = [];
        let unexpectedExits = 0;
        const process = processFor(child, {
            output: {
                append: (value: string) => output.push(value),
                appendLine: (value: string) => output.push(`${value}\n`),
            } as never,
            onUnexpectedExit: () => { unexpectedExits++; },
            probe: async () => { throw new Error('probe rejected'); },
            signalGraceMs: 1,
            killWaitMs: 1,
        });
        const starting = process.start();
        child.stderr.write('Listening on http://127.0.0.1:4444/\n');

        await expect(starting).rejects.toThrow('probe rejected');
        expect(child.signals[0]).toBe('SIGINT');
        await process.stop();

        expect(child.signals).toEqual(['SIGINT', 'SIGTERM', 'SIGKILL']);
        expect(unexpectedExits).toBe(0);
        const afterFailure = output.join('');
        child.stdout.write('ignored after failure');
        child.stderr.write('ignored after failure\n');
        await Promise.resolve();
        expect(output.join('')).toBe(afterFailure);
    });

    it('bounds an unconfirmed SIGKILL and detaches output listeners', async () => {
        const child = new NeverClosingFakeChild();
        const output: string[] = [];
        const process = processFor(child, {
            output: {
                append: (value: string) => output.push(value),
                appendLine: (value: string) => output.push(`${value}\n`),
            } as never,
            signalGraceMs: 1,
            killWaitMs: 1,
        });
        const starting = process.start().catch((err) => err as Error);
        child.stdout.write('before-stop');

        await process.stop();

        expect(child.signals).toEqual(['SIGINT', 'SIGTERM', 'SIGKILL']);
        expect(output.join('')).toContain(
            'did not confirm exit after SIGKILL; abandoning',
        );
        const afterStop = output.join('');
        child.stdout.write('after-stop');
        child.stderr.write('after-stop-error\n');
        await Promise.resolve();
        expect(output.join('')).toBe(afterStop);

        child.emit('close', null);
        expect(await starting).toBeInstanceOf(Error);
    });

    it('shares one ladder between stop and shutdown before output disposal', async () => {
        const child = new NeverClosingFakeChild();
        let disposed = false;
        let writesAfterDispose = 0;
        const process = processFor(child, {
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
            signalGraceMs: 50,
            shutdownTermGraceMs: 1,
            killWaitMs: 1,
        });
        const starting = process.start().catch((err) => err as Error);

        const stopping = process.stop();
        const shuttingDown = process.shutdown();
        expect(shuttingDown).toBe(stopping);
        await shuttingDown;
        disposed = true;
        await stopping;

        expect(child.signals).toEqual(['SIGINT', 'SIGTERM', 'SIGKILL']);
        expect(writesAfterDispose).toBe(0);
        child.emit('close', null);
        await starting;
    });

    it('does not reject teardown when abandonment logging throws', async () => {
        const child = new NeverClosingFakeChild();
        const process = processFor(child, {
            output: {
                append() {},
                appendLine() {
                    throw new Error('disposed output channel');
                },
            } as never,
            signalGraceMs: 1,
            killWaitMs: 1,
        });
        const starting = process.start().catch((err) => err as Error);

        await expect(process.stop()).resolves.toBeUndefined();
        child.emit('close', null);
        await starting;
    });

    it('flushes partial stderr exactly once when stop detaches listeners', async () => {
        const child = new NeverClosingFakeChild();
        const output: string[] = [];
        const process = processFor(child, {
            output: {
                append: (value: string) => output.push(value),
                appendLine: (value: string) => output.push(`${value}\n`),
            } as never,
            signalGraceMs: 1,
            killWaitMs: 1,
        });
        const starting = process.start().catch((err) => err as Error);
        child.stderr.write('partial before stop');

        await process.stop();
        child.emit('close', null);
        await starting;

        expect(output.filter((line) => (
            line === '[stderr] partial before stop\n'
        ))).toHaveLength(1);
    });
});

describe('Quarto deactivation ordering', () => {
    it('claims runtime shutdown before disposing panels and output', () => {
        const source = readFileSync(new URL(
            '../../editors/vscode/src/quarto/index.ts',
            import.meta.url,
        ), 'utf8');
        const runtimeShutdown = source.indexOf(
            'const previews = lifecycle.runtime.shutdown();',
        );
        const panelDisposal = source.indexOf(
            'QuartoPreviewPanel.disposeAllForDeactivation();',
        );
        const boundedAggregate = source.indexOf(
            'Promise.allSettled([previews, renders])',
        );
        const outputDisposal = source.indexOf('lifecycle.output.dispose();');

        expect(runtimeShutdown).toBeGreaterThan(-1);
        expect(panelDisposal).toBeGreaterThan(runtimeShutdown);
        expect(boundedAggregate).toBeGreaterThan(panelDisposal);
        expect(outputDisposal).toBeGreaterThan(boundedAggregate);
    });
});

describe('Browse-only preview correlation', () => {
    it('uses a 1500ms default correlation window', () => {
        expect(QUARTO_PREVIEW_BROWSE_CORRELATION_DELAY_MS).toBe(1_500);
    });

    it('lets late Listening supersede a failed Browse-only probe', async () => {
        const child = new FakeChild();
        const firstProbe = new Deferred<void>();
        const probed: string[] = [];
        const process = processFor(child, {
            browseCorrelationDelayMs: 1,
            lateListeningGraceMs: 100,
            probe: async (url) => {
                probed.push(url);
                if (probed.length === 1) {
                    firstProbe.resolve();
                    throw new Error('connect ECONNREFUSED');
                }
                return 200;
            },
        });
        const starting = process.start();
        child.stderr.write('Browse at http://localhost:4555/chapter/\n');

        await firstProbe.promise;
        child.stderr.write('Listening on http://127.0.0.1:4666/\n');

        const ready = await starting;
        expect(probed).toEqual([
            'http://localhost:4555/chapter/',
            'http://127.0.0.1:4666/chapter/',
        ]);
        expect(ready.rawUrl).toBe('http://127.0.0.1:4666/chapter/');
    });
});
