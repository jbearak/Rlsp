/**
 * Quarto live-preview subprocess and host-side readiness verification.
 *
 * The process is always spawned with an argv array (never a shell), bound to
 * loopback, and placed in its own POSIX process group so signal escalation
 * reaches helpers. Both output streams are copied to the Quarto output channel,
 * but each owns its own bounded line carry before complete lines enter the
 * shared URL scanner; partial stdout and stderr fragments can therefore never
 * corrupt one another. Bare-CR progress records count as line boundaries. A
 * parsed URL is not readiness: Quarto can print it before binding, so the
 * engine retries bounded Node `http` GETs against the validated raw loopback
 * URL before resolving `start()`. Startup uses a generous output-idle timeout,
 * reset by every stdout/stderr chunk, so long initial renders remain alive
 * while a truly silent process is still bounded. Probe retries and correlation
 * timers are cancelled as soon as startup settles or teardown begins, and an
 * intentional stop promptly rejects a still-pending `start()`. Any startup
 * failure claims the same shared stop ladder itself after capturing the
 * startup tail, so the engine remains leak-safe even without its runtime owner.
 *
 * Browse-only fallback waits for a correlation window and remains provisional
 * through its readiness probe and one final grace window. A late
 * `Listening on` line can therefore supersede any failed or successful
 * Browse-only probe before `start()` resolves and probe the corrected,
 * trusted-origin URL. Exact `localhost` candidates are normalized to the IPv4
 * address Quarto was explicitly bound to before either probing or framing.
 * Probe diagnosis follows the last attempt: trailing connection errors cannot
 * be masked by an earlier 404.
 *
 * Intentional stop is marked before the first signal. One shared per-child
 * teardown promise owns the ladder: `stop()` escalates SIGINT -> SIGTERM ->
 * SIGKILL, while a concurrent `shutdown()` tightens that same ladder instead
 * of signaling twice. Detachment removes the exact stdout/stderr listeners and
 * flushes already-received partial stderr once before signaling. The final
 * SIGKILL wait is bounded; abandonment logging is non-throwing if the shared
 * output channel is already disposing.
 */

import { ChildProcess, spawn } from 'child_process';
import * as http from 'http';
import type * as vscode from 'vscode';
import {
    PreviewUrlResult,
    QuartoPreviewOutputScanner,
} from './preview-url-parser';
import { QuartoProcessTeardown } from './quarto-process-teardown';

/**
 * Maximum startup silence. Two minutes favors slow, quiet renders while still
 * bounding a genuinely hung process; any output activity resets the interval.
 */
export const QUARTO_PREVIEW_STARTUP_TIMEOUT_MS = 120_000;
export const QUARTO_PREVIEW_PROBE_ATTEMPTS = 20;
export const QUARTO_PREVIEW_PROBE_DELAY_MS = 250;
export const QUARTO_PREVIEW_BROWSE_CORRELATION_DELAY_MS = 1_500;
export const QUARTO_PREVIEW_LINE_CARRY_LIMIT = 8 * 1024;

const SIGNAL_GRACE_MS = 5_000;
const SHUTDOWN_TERM_GRACE_MS = 2_000;

export interface QuartoPreviewReady {
    rawUrl: string;
    origin: string;
    statusCode: number;
}

export class QuartoPreviewStartError extends Error {
    constructor(message: string, readonly startupTail: string) {
        super(message);
        this.name = 'QuartoPreviewStartError';
    }
}

export interface QuartoPreviewProcessOptions {
    quartoPath: string;
    sourceFsPath: string;
    cwd: string;
    output: vscode.OutputChannel;
    onUnexpectedExit(code: number | null): void;
    /** Dependency-injected for engine tests. */
    spawnProcess?: typeof spawn;
    startupTimeoutMs?: number;
    probe?: (rawUrl: string, signal: AbortSignal) => Promise<number>;
    /** Dependency-injected correlation window for engine tests. */
    browseCorrelationDelayMs?: number;
    /** Dependency-injected grace after a Browse-only connection failure. */
    lateListeningGraceMs?: number;
    /** Dependency-injected signal cadence for process-ladder tests. */
    signalGraceMs?: number;
    shutdownTermGraceMs?: number;
    killWaitMs?: number;
}

export interface QuartoPreviewProcessLike {
    start(): Promise<QuartoPreviewReady>;
    stop(): Promise<void>;
    shutdown(): Promise<void>;
}

export class QuartoPreviewProcess implements QuartoPreviewProcessLike {
    private scanner = new QuartoPreviewOutputScanner();
    private startPromise: Promise<QuartoPreviewReady> | null = null;
    private teardownPromise: Promise<void> | null = null;
    private teardown: QuartoProcessTeardown | null = null;
    private closePromise: Promise<void> = Promise.resolve();
    private intentionalStop = false;
    private closed = false;
    private ready = false;
    private correlationTimer: NodeJS.Timeout | null = null;
    private lateListeningTimer: NodeJS.Timeout | null = null;
    private outputDetached = false;
    private outputDetacher: (() => void) | null = null;
    private activeProbeAbortController: AbortController | null = null;
    private cancelPendingStart: (() => void) | null = null;

    constructor(private readonly opts: QuartoPreviewProcessOptions) {}

    start(): Promise<QuartoPreviewReady> {
        if (this.startPromise) return this.startPromise;
        this.startPromise = this.startInner();
        return this.startPromise;
    }

    private startInner(): Promise<QuartoPreviewReady> {
        return new Promise<QuartoPreviewReady>((resolve, reject) => {
            const spawnProcess = this.opts.spawnProcess ?? spawn;
            let child: ChildProcess;
            try {
                child = spawnProcess(
                    this.opts.quartoPath,
                    [
                        'preview',
                        this.opts.sourceFsPath,
                        '--no-browser',
                        '--host',
                        '127.0.0.1',
                    ],
                    {
                        cwd: this.opts.cwd,
                        stdio: ['ignore', 'pipe', 'pipe'],
                        detached: process.platform !== 'win32',
                        env: process.env,
                    },
                );
            } catch (err) {
                reject(this.startError(`Could not spawn Quarto: ${errorMessage(err)}`));
                return;
            }

            let settled = false;
            let probeSequence = 0;
            let correlationFallbackStarted = false;
            let startupTimer: NodeJS.Timeout;
            let closeResolve: () => void = () => undefined;
            this.closePromise = new Promise<void>((r) => { closeResolve = r; });
            const correlationDelayMs = this.opts.browseCorrelationDelayMs
                ?? QUARTO_PREVIEW_BROWSE_CORRELATION_DELAY_MS;
            const finishStart = (
                fn: () => void,
            ): boolean => {
                if (settled) return false;
                settled = true;
                clearTimeout(startupTimer);
                this.cancelPendingStart = null;
                this.activeProbeAbortController?.abort();
                this.activeProbeAbortController = null;
                if (this.correlationTimer) {
                    clearTimeout(this.correlationTimer);
                    this.correlationTimer = null;
                }
                if (this.lateListeningTimer) {
                    clearTimeout(this.lateListeningTimer);
                    this.lateListeningTimer = null;
                }
                fn();
                return true;
            };

            const fail = (message: string): void => {
                // Capture raw startup context before stop detaches the stream
                // listeners and flushes their line carries.
                const error = this.startError(message);
                if (!finishStart(() => reject(error))) return;
                void this.stop().catch(() => undefined);
            };
            this.cancelPendingStart = () => {
                const error = this.startError('Quarto preview startup was stopped.');
                finishStart(() => reject(error));
            };
            const startupTimeoutMs = this.opts.startupTimeoutMs
                ?? QUARTO_PREVIEW_STARTUP_TIMEOUT_MS;
            const resetStartupTimer = (): void => {
                if (settled) return;
                clearTimeout(startupTimer);
                startupTimer = setTimeout(() => {
                    fail(
                        `Quarto preview produced no startup output for ` +
                        `${startupTimeoutMs}ms.`,
                    );
                }, startupTimeoutMs);
            };
            const beginProbe = (
                candidate: PreviewUrlResult,
                browseOnly: boolean,
            ): void => {
                if (settled) return;
                const readyCandidate = normalizePreviewUrlForIpv4Listener(candidate);
                const probeId = ++probeSequence;
                this.activeProbeAbortController?.abort();
                const probeAbort = new AbortController();
                this.activeProbeAbortController = probeAbort;
                if (!browseOnly && this.lateListeningTimer) {
                    clearTimeout(this.lateListeningTimer);
                    this.lateListeningTimer = null;
                }
                const probe = this.opts.probe
                    ?? ((url: string, signal: AbortSignal) =>
                        probeQuartoPreviewUrl(
                            url,
                            QUARTO_PREVIEW_PROBE_ATTEMPTS,
                            QUARTO_PREVIEW_PROBE_DELAY_MS,
                            requestStatus,
                            signal,
                        ));
                void probe(readyCandidate.url, probeAbort.signal).then((statusCode) => {
                    if (settled || probeId !== probeSequence) return;
                    if (browseOnly) {
                        // A document can print a plausible Browse line before
                        // Quarto reports its actual listener. Even a successful
                        // provisional probe gets one last correlation window.
                        const graceMs = this.opts.lateListeningGraceMs
                            ?? correlationDelayMs;
                        this.lateListeningTimer = setTimeout(() => {
                            this.lateListeningTimer = null;
                            if (!settled && probeId === probeSequence) {
                                finishStart(() => {
                                    this.ready = true;
                                    resolve({
                                        rawUrl: readyCandidate.url,
                                        origin: readyCandidate.origin,
                                        statusCode,
                                    });
                                });
                            }
                        }, graceMs);
                        return;
                    }
                    finishStart(() => {
                        this.ready = true;
                        resolve({
                            rawUrl: readyCandidate.url,
                            origin: readyCandidate.origin,
                            statusCode,
                        });
                    });
                }, (err) => {
                    if (settled || probeId !== probeSequence) return;
                    if (browseOnly) {
                        // Browse can precede a differently-bound Listening
                        // line. Keep every provisional diagnosis, including a
                        // 404, pending briefly so the trusted listener wins.
                        const graceMs = this.opts.lateListeningGraceMs
                            ?? correlationDelayMs;
                        this.lateListeningTimer = setTimeout(() => {
                            this.lateListeningTimer = null;
                            if (!settled && probeId === probeSequence) {
                                fail(errorMessage(err));
                            }
                        }, graceMs);
                        return;
                    }
                    fail(errorMessage(err));
                });
            };
            const scanLine = (line: string): void => {
                const candidate = this.scanner.feedLine(line, false);
                const failure = this.scanner.failure();
                if (failure) {
                    fail(failure);
                    return;
                }
                if (candidate) {
                    if (this.correlationTimer) {
                        clearTimeout(this.correlationTimer);
                        this.correlationTimer = null;
                    }
                    beginProbe(candidate, false);
                    return;
                }
                if (
                    (
                        this.scanner.hasBrowseCandidate()
                        || this.scanner.hasListeningCandidate()
                    )
                    && this.correlationTimer === null
                    && !correlationFallbackStarted
                ) {
                    this.correlationTimer = setTimeout(() => {
                        this.correlationTimer = null;
                        correlationFallbackStarted = true;
                        const browseOnly = this.scanner.acceptBrowseCandidate();
                        if (browseOnly) {
                            beginProbe(browseOnly, true);
                            return;
                        }
                        const listeningOnly = this.scanner.acceptListeningCandidate();
                        if (listeningOnly) beginProbe(listeningOnly, false);
                    }, correlationDelayMs);
                }
            };
            const stdoutLines = new QuartoOutputLineBuffer(scanLine);
            const stderrWriter = new PrefixedStderrWriter(this.opts.output, scanLine);

            const onStdoutData = (chunk: string): void => {
                resetStartupTimer();
                this.opts.output.append(chunk);
                this.scanner.captureRaw(chunk);
                stdoutLines.feed(chunk);
            };
            const onStderrData = (chunk: string): void => {
                resetStartupTimer();
                this.scanner.captureRaw(chunk);
                stderrWriter.feed(chunk);
            };
            child.stdout?.setEncoding('utf8');
            child.stdout?.on('data', onStdoutData);
            child.stderr?.setEncoding('utf8');
            child.stderr?.on('data', onStderrData);
            this.outputDetacher = () => {
                child.stdout?.off('data', onStdoutData);
                child.stderr?.off('data', onStderrData);
                // Raw stdout was already appended as bytes arrived. Stderr is
                // line-prefixed, so surface its received partial record now.
                try {
                    stderrWriter.finish();
                } catch {
                    // The activation output may already be disposing.
                }
            };
            this.teardown = new QuartoProcessTeardown({
                child,
                output: this.opts.output,
                processKind: 'preview',
                closePromise: this.closePromise,
                isClosed: () => this.closed,
                detachOutput: () => this.detachProcessOutput(),
                stopGraceMs: this.opts.signalGraceMs ?? SIGNAL_GRACE_MS,
                shutdownTermGraceMs: this.opts.shutdownTermGraceMs
                    ?? SHUTDOWN_TERM_GRACE_MS,
                killWaitMs: this.opts.killWaitMs ?? 1_000,
            });
            if (this.outputDetached) this.detachProcessOutput();
            child.on('error', (err) => {
                fail(`Quarto preview process error: ${errorMessage(err)}`);
            });
            child.on('close', (code) => {
                this.closed = true;
                if (!this.outputDetached) {
                    stdoutLines.finish();
                    stderrWriter.finish();
                }
                closeResolve();
                if (!settled) {
                    this.scanner.finish();
                    fail(`Quarto preview exited before it became ready (code ${String(code)}).`);
                    return;
                }
                if (this.ready && !this.intentionalStop) {
                    this.opts.onUnexpectedExit(code);
                }
            });

            resetStartupTimer();
        });
    }

    stop(): Promise<void> {
        this.intentionalStop = true;
        this.cancelPendingStart?.();
        this.abortStartupWork();
        this.detachProcessOutput();
        if (this.teardownPromise) return this.teardownPromise;
        this.teardownPromise = this.teardown?.stop() ?? Promise.resolve();
        return this.teardownPromise;
    }

    shutdown(): Promise<void> {
        this.intentionalStop = true;
        this.cancelPendingStart?.();
        this.abortStartupWork();
        this.detachProcessOutput();
        if (!this.teardown) {
            if (this.teardownPromise) return this.teardownPromise;
            this.teardownPromise = Promise.resolve();
            return this.teardownPromise;
        }
        const teardown = this.teardown.shutdown();
        this.teardownPromise = teardown;
        return teardown;
    }

    /** Remove only Raven's stream callbacks, once, before process signaling. */
    private detachProcessOutput(): void {
        if (this.outputDetached && this.outputDetacher === null) return;
        this.outputDetached = true;
        this.outputDetacher?.();
        this.outputDetacher = null;
    }

    private abortStartupWork(): void {
        if (this.correlationTimer) {
            clearTimeout(this.correlationTimer);
            this.correlationTimer = null;
        }
        if (this.lateListeningTimer) {
            clearTimeout(this.lateListeningTimer);
            this.lateListeningTimer = null;
        }
        this.activeProbeAbortController?.abort();
        this.activeProbeAbortController = null;
    }

    private startError(message: string): QuartoPreviewStartError {
        return new QuartoPreviewStartError(message, this.scanner.startupTail());
    }
}

/**
 * Retry GET until any non-404 HTTP response proves the server is alive.
 * Only a final 404 identifies formats Quarto cannot browser-preview; any
 * later connection error becomes the final diagnosis instead of allowing a
 * stale earlier response to win.
 */
export async function probeQuartoPreviewUrl(
    rawUrl: string,
    attempts: number = QUARTO_PREVIEW_PROBE_ATTEMPTS,
    delayMs: number = QUARTO_PREVIEW_PROBE_DELAY_MS,
    request: (url: string, signal?: AbortSignal) => Promise<number> = requestStatus,
    signal?: AbortSignal,
): Promise<number> {
    const requestUrl = normalizePreviewUrlForIpv4Listener(
        validatePreviewUrlForNormalization(rawUrl),
    ).url;
    let lastOutcome: '404' | 'error' | null = null;
    let lastError: Error | null = null;
    for (let attempt = 0; attempt < attempts; attempt++) {
        throwIfAborted(signal);
        try {
            const status = await request(requestUrl, signal);
            if (status !== 404) return status;
            lastOutcome = '404';
            lastError = null;
        } catch (err) {
            if (signal?.aborted) throw abortError();
            lastOutcome = 'error';
            lastError = err instanceof Error ? err : new Error(String(err));
        }
        if (attempt + 1 < attempts) await delay(delayMs, signal);
    }
    if (lastOutcome === '404') {
        throw new QuartoPreviewProbeError(
            'not-browser-previewable',
            'Quarto returned HTTP 404 for the preview path. This output format is not ' +
            'browser-previewable; use Raven: Quarto Render instead.',
        );
    }
    throw new QuartoPreviewProbeError(
        'connection',
        `Could not connect to the Quarto preview server${lastError ? `: ${lastError.message}` : '.'}`,
    );
}

/** Match the explicitly IPv4-bound Quarto listener for probes and webviews. */
function normalizePreviewUrlForIpv4Listener(
    candidate: PreviewUrlResult,
): PreviewUrlResult {
    const parsed = new URL(candidate.url);
    if (parsed.hostname !== 'localhost') return candidate;
    parsed.hostname = '127.0.0.1';
    return { origin: parsed.origin, url: parsed.toString() };
}

function validatePreviewUrlForNormalization(rawUrl: string): PreviewUrlResult {
    const parsed = new URL(rawUrl);
    return { origin: parsed.origin, url: parsed.toString() };
}

export type QuartoPreviewProbeErrorKind = 'not-browser-previewable' | 'connection';

/** A readiness failure whose kind controls Browse-only late correlation. */
export class QuartoPreviewProbeError extends Error {
    constructor(readonly kind: QuartoPreviewProbeErrorKind, message: string) {
        super(message);
        this.name = 'QuartoPreviewProbeError';
    }
}

function requestStatus(rawUrl: string, signal?: AbortSignal): Promise<number> {
    return new Promise<number>((resolve, reject) => {
        if (signal?.aborted) {
            reject(abortError());
            return;
        }
        const req = http.request(rawUrl, { method: 'GET' }, (response) => {
            signal?.removeEventListener('abort', onAbort);
            const status = response.statusCode ?? 0;
            response.resume();
            resolve(status);
        });
        const onAbort = (): void => {
            req.destroy(abortError());
        };
        signal?.addEventListener('abort', onAbort, { once: true });
        req.setTimeout(750, () => {
            req.destroy(new Error('HTTP readiness probe timed out'));
        });
        req.on('error', (err) => {
            signal?.removeEventListener('abort', onAbort);
            reject(err);
        });
        req.end();
    });
}

function delay(ms: number, signal?: AbortSignal): Promise<void> {
    return new Promise((resolve, reject) => {
        if (signal?.aborted) {
            reject(abortError());
            return;
        }
        const timer = setTimeout(() => {
            signal?.removeEventListener('abort', onAbort);
            resolve();
        }, ms);
        const onAbort = (): void => {
            clearTimeout(timer);
            reject(abortError());
        };
        signal?.addEventListener('abort', onAbort, { once: true });
    });
}

function throwIfAborted(signal: AbortSignal | undefined): void {
    if (signal?.aborted) throw abortError();
}

function abortError(): Error {
    const error = new Error('Quarto preview readiness probe aborted');
    error.name = 'AbortError';
    return error;
}

function errorMessage(err: unknown): string {
    return err instanceof Error ? err.message : String(err);
}

/**
 * Reassemble one process stream into complete LF, CRLF, or bare-CR lines.
 *
 * Carries are independent per instance. An unterminated fragment is capped at
 * 8 KiB by emitting each full-size prefix as a line, preventing either stream
 * from retaining unbounded output. A CR emitted at a chunk boundary remembers
 * to swallow a leading LF from the next chunk so split CRLF remains one break.
 */
export class QuartoOutputLineBuffer {
    private carry = '';
    private swallowLeadingLf = false;

    constructor(
        private readonly onLine: (line: string) => void,
        private readonly carryLimit: number = QUARTO_PREVIEW_LINE_CARRY_LIMIT,
    ) {}

    feed(chunk: string): void {
        // An empty chunk carries no line data and must not clear a pending
        // split-CRLF decision; Node never emits empty `data` events, but the
        // class is reused and unit-tested directly.
        if (chunk === '') return;
        let input = chunk;
        if (this.swallowLeadingLf) {
            // The pending CRLF decision belongs only to this next chunk. Even
            // when the chunk is exactly the swallowed LF, a later leading LF
            // is a distinct empty record and must remain observable.
            this.swallowLeadingLf = false;
            if (input.startsWith('\n')) input = input.slice(1);
        }

        const combined = this.carry + input;
        const delimiter = /\r\n|\r|\n/g;
        let start = 0;
        let match: RegExpExecArray | null;
        while ((match = delimiter.exec(combined)) !== null) {
            this.onLine(combined.slice(start, match.index));
            start = delimiter.lastIndex;
            this.swallowLeadingLf = match[0] === '\r' && start === combined.length;
        }
        this.carry = combined.slice(start);

        const limit = Math.max(1, this.carryLimit);
        while (this.carry.length > limit) {
            this.onLine(this.carry.slice(0, limit));
            this.carry = this.carry.slice(limit);
        }
    }

    finish(): void {
        if (this.carry !== '') this.onLine(this.carry);
        this.carry = '';
        this.swallowLeadingLf = false;
    }
}

/** Prefix complete stderr records while sharing their lines with the scanner. */
export class PrefixedStderrWriter {
    private readonly lines: QuartoOutputLineBuffer;

    constructor(
        output: vscode.OutputChannel,
        onLine: (line: string) => void = () => undefined,
    ) {
        this.lines = new QuartoOutputLineBuffer((line) => {
            output.appendLine(`[stderr] ${line}`);
            onLine(line);
        });
    }

    feed(chunk: string): void {
        this.lines.feed(chunk);
    }

    finish(): void {
        this.lines.finish();
    }
}
