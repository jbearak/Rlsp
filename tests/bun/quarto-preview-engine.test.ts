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
    QUARTO_PREVIEW_STARTUP_TIMEOUT_MS,
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
        browseCorrelationDelayMs: 5,
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

    it('aborts retry waits promptly without issuing later requests', async () => {
        const controller = new AbortController();
        const firstRequest = new Deferred<void>();
        let requests = 0;
        const probing = probeQuartoPreviewUrl(
            'http://127.0.0.1:1/',
            20,
            100,
            async () => {
                requests++;
                firstRequest.resolve();
                throw new Error('connect ECONNREFUSED');
            },
            controller.signal,
        );

        await firstRequest.promise;
        controller.abort();
        await expect(probing).rejects.toMatchObject({ name: 'AbortError' });
        await new Promise((resolve) => setTimeout(resolve, 120));
        expect(requests).toBe(1);
    });

    it('dials localhost readiness URLs through the IPv4 listener', async () => {
        const requested: string[] = [];
        const status = await probeQuartoPreviewUrl(
            'http://localhost:4777/chapter/?preview=1',
            1,
            0,
            async (url) => {
                requested.push(url);
                return 204;
            },
        );

        expect(status).toBe(204);
        expect(requested).toEqual([
            'http://127.0.0.1:4777/chapter/?preview=1',
        ]);
    });
});

describe('Quarto preview startup idle timeout', () => {
    it('uses a generous output-idle default', () => {
        expect(QUARTO_PREVIEW_STARTUP_TIMEOUT_MS).toBe(120_000);
    });

    it('keeps a long initial render alive while output remains active', async () => {
        const child = new FakeChild();
        const process = processFor(child, {
            startupTimeoutMs: 100,
            probe: async () => 200,
        });
        const starting = process.start();

        await new Promise((resolve) => setTimeout(resolve, 60));
        child.stderr.write('rendering chunk one\n');
        await new Promise((resolve) => setTimeout(resolve, 60));
        child.stderr.write('Listening on http://127.0.0.1:4888/\n');

        expect((await starting).rawUrl).toBe('http://127.0.0.1:4888/');
    });

    it('fails a child that stays silent for the full idle window', async () => {
        const child = new FakeChild();
        const process = processFor(child, {
            startupTimeoutMs: 10,
            signalGraceMs: 1,
            killWaitMs: 1,
        });

        await expect(process.start()).rejects.toThrow(
            'produced no startup output for 10ms',
        );
    });
});

describe('Quarto preview output line handling', () => {
    it('returns the IPv4 URL that is safe to frame for localhost advertisements', async () => {
        const child = new FakeChild();
        const probed: string[] = [];
        const process = processFor(child, {
            probe: async (url) => {
                probed.push(url);
                return 200;
            },
        });
        const starting = process.start();
        child.stderr.write('Listening on http://localhost:4888/chapter/\n');

        const ready = await starting;
        expect(probed).toEqual(['http://127.0.0.1:4888/chapter/']);
        expect(ready).toEqual({
            rawUrl: 'http://127.0.0.1:4888/chapter/',
            origin: 'http://127.0.0.1:4888',
            statusCode: 200,
        });
    });

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
    it('stop before correlation fires clears the timer and settles start promptly', async () => {
        const child = new NeverClosingFakeChild();
        let probes = 0;
        const process = processFor(child, {
            browseCorrelationDelayMs: 100,
            probe: async () => {
                probes++;
                return 200;
            },
            signalGraceMs: 1,
            killWaitMs: 1,
        });
        const starting = process.start();
        child.stderr.write('Browse at http://localhost:4555/chapter/\n');

        const stopping = process.stop();
        await expect(starting).rejects.toThrow('startup was stopped');
        await stopping;
        await new Promise((resolve) => setTimeout(resolve, 120));

        expect(probes).toBe(0);
    });

    it('aborts an active readiness probe when stop begins', async () => {
        const child = new FakeChild();
        const probeStarted = new Deferred<void>();
        const probeAborted = new Deferred<void>();
        const process = processFor(child, {
            probe: async (_url, signal) => new Promise<number>((_resolve, reject) => {
                probeStarted.resolve();
                signal.addEventListener('abort', () => {
                    probeAborted.resolve();
                    reject(new Error('probe aborted'));
                }, { once: true });
            }),
            signalGraceMs: 1,
            killWaitMs: 1,
        });
        const starting = process.start().catch((err) => err as Error);
        child.stderr.write('Listening on http://127.0.0.1:4444/\n');
        await probeStarted.promise;

        await process.stop();

        await probeAborted.promise;
        expect(await starting).toBeInstanceOf(Error);
    });

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
        const commandShutdown = source.indexOf(
            'const commands = lifecycle.commands.shutdown();',
        );
        const panelDisposal = source.indexOf(
            'QuartoPreviewPanel.disposeAllForDeactivation();',
        );
        const boundedAggregate = source.indexOf('Promise.allSettled([');
        const outputDisposal = source.indexOf('lifecycle.output.dispose();');

        expect(commandShutdown).toBeGreaterThan(-1);
        expect(runtimeShutdown).toBeGreaterThan(commandShutdown);
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

    it('ignores proxy Browse output and probes the Listening URL with its own path', async () => {
        const child = new FakeChild();
        const probed: string[] = [];
        const process = processFor(child, {
            browseCorrelationDelayMs: 5,
            probe: async (url) => {
                probed.push(url);
                return 200;
            },
        });
        const starting = process.start();
        child.stderr.write(
            'Browse at https://proxy.example.test/proxy/4999/document/\n',
        );

        await new Promise((resolve) => setTimeout(resolve, 15));
        expect(probed).toEqual([]);
        child.stderr.write(
            'Listening on http://127.0.0.1:4999/listener/path?ready=1\n',
        );

        const ready = await starting;
        expect(probed).toEqual([
            'http://127.0.0.1:4999/listener/path?ready=1',
        ]);
        expect(ready.rawUrl).toBe(
            'http://127.0.0.1:4999/listener/path?ready=1',
        );
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
            'http://127.0.0.1:4555/chapter/',
            'http://127.0.0.1:4666/chapter/',
        ]);
        expect(ready.rawUrl).toBe('http://127.0.0.1:4666/chapter/');
    });

    it('lets late Listening supersede a successful Browse-only probe', async () => {
        const child = new FakeChild();
        const firstProbe = new Deferred<void>();
        const probed: string[] = [];
        const process = processFor(child, {
            browseCorrelationDelayMs: 1,
            lateListeningGraceMs: 100,
            probe: async (url) => {
                probed.push(url);
                if (probed.length === 1) firstProbe.resolve();
                return 200;
            },
        });
        const starting = process.start();
        child.stderr.write('Browse at http://localhost:4777/chapter/\n');

        await firstProbe.promise;
        await Promise.resolve();
        child.stderr.write('Listening on http://127.0.0.1:4888/\n');

        const ready = await starting;
        expect(probed).toEqual([
            'http://127.0.0.1:4777/chapter/',
            'http://127.0.0.1:4888/chapter/',
        ]);
        expect(ready.rawUrl).toBe('http://127.0.0.1:4888/chapter/');
    });

    it('lets late Listening supersede a provisional Browse 404', async () => {
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
                    throw new QuartoPreviewProbeError(
                        'not-browser-previewable',
                        'provisional 404',
                    );
                }
                return 200;
            },
        });
        const starting = process.start();
        child.stderr.write('Browse at http://localhost:4991/chapter/\n');

        await firstProbe.promise;
        child.stderr.write('Listening on http://127.0.0.1:4992/\n');

        const ready = await starting;
        expect(probed).toEqual([
            'http://127.0.0.1:4991/chapter/',
            'http://127.0.0.1:4992/chapter/',
        ]);
        expect(ready.rawUrl).toBe('http://127.0.0.1:4992/chapter/');
    });
});
