/**
 * Activation-scoped one-shot `quarto render` subprocess engine.
 *
 * This mirrors the knit engine's result contract and cancellation semantics:
 * argv-only spawning, inherited environment, POSIX process-group ownership,
 * streamed output, and SIGINT -> SIGTERM -> SIGKILL escalation for both user
 * cancellation and timeout. Every spawned child is registered before control
 * returns to the event loop. A token already cancelled at `run()` entry returns
 * without spawning, so document code cannot execute before the normal
 * post-spawn cancellation hook is installed. `shutdown()` first rejects new
 * work, then sends every live child immediate SIGTERM followed by bounded
 * SIGKILL, so extension deactivation cannot leave workspace code running.
 * Command policy and notifications live outside the engine so progress UI can
 * close before any outcome toast is shown.
 *
 * The render timeout is capped at Node's signed 32-bit timer maximum at the
 * arm site as well as in the settings schema. This prevents stale or synced
 * out-of-range values from overflowing to an approximately 1ms cancellation.
 *
 * Stdout and stderr continue streaming in full to the output channel, while
 * the result retains only a 256-Ki-character tail of each stream. Quarto emits
 * `Output created:` near the end, so the tail is sufficient for output-path
 * parsing and useful failure context without allowing unbounded accumulation.
 * Cancellation, timeout, and shutdown share one per-child teardown promise and
 * detach their exact data listeners and flush already-received partial stderr
 * once before signaling. Deactivation can tighten an in-flight graceful ladder
 * but never starts a second one. A bounded post-SIGKILL wait completes the
 * render result even without `close`; its abandonment warning cannot throw
 * through a disposed output channel.
 */

import { ChildProcess, spawn } from 'child_process';
import type * as vscode from 'vscode';
import type { KnitEngineResult } from '../knit/knit-engine';
import { QuartoProcessTeardown } from './quarto-process-teardown';

export interface QuartoRenderOptions {
    quartoPath: string;
    sourceFsPath: string;
    cwd: string;
    timeoutMs: number;
    output: vscode.OutputChannel;
    cancellation: vscode.CancellationToken;
    spawnProcess?: typeof spawn;
    /** Dependency-injected process-ladder cadence for tests. */
    signalGraceMs?: number;
    shutdownTermGraceMs?: number;
    shutdownKillWaitMs?: number;
}

const SIGNAL_GRACE_MS = 5_000;
const SHUTDOWN_TERM_GRACE_MS = 2_000;
const SHUTDOWN_KILL_WAIT_MS = 1_000;

export const QUARTO_RENDER_RETAINED_OUTPUT_CHARS = 256 * 1024;
export const MAX_NODE_TIMER_MS = 2_147_483_647;

/** Clamp a configured timeout to the largest delay Node timers represent. */
export function clampQuartoRenderTimeoutMs(timeoutMs: number): number {
    return Math.min(timeoutMs, MAX_NODE_TIMER_MS);
}

export type QuartoRenderResultKind =
    | 'spawnError'
    | 'cancelled'
    | 'timedOut'
    | 'failed'
    | 'ok';

/**
 * Classify with the same precedence as knit:
 * spawn error > user cancellation > timeout > process failure > success.
 */
export function classifyQuartoRenderResult(
    result: KnitEngineResult,
): QuartoRenderResultKind {
    if (result.spawnError) return 'spawnError';
    if (result.cancelled) return 'cancelled';
    if (result.timedOut) return 'timedOut';
    if (result.exitCode !== 0) return 'failed';
    return 'ok';
}

/** Retain only the suffix needed for Quarto's end-of-run result lines. */
export function appendQuartoRenderTail(
    current: string,
    chunk: string,
    limit: number = QUARTO_RENDER_RETAINED_OUTPUT_CHARS,
): string {
    if (limit <= 0) return '';
    const combined = current + chunk;
    return combined.length <= limit ? combined : combined.slice(-limit);
}

export class QuartoRenderEngine {
    private readonly liveChildren = new Set<LiveRenderChild>();
    private deactivating = false;
    private shutdownPromise: Promise<void> | null = null;

    async run(opts: QuartoRenderOptions): Promise<KnitEngineResult> {
        if (this.deactivating || opts.cancellation.isCancellationRequested) {
            return emptyCancelledResult();
        }

        let child: ChildProcess;
        try {
            child = (opts.spawnProcess ?? spawn)(
                opts.quartoPath,
                ['render', opts.sourceFsPath],
                {
                    cwd: opts.cwd,
                    stdio: ['ignore', 'pipe', 'pipe'],
                    detached: process.platform !== 'win32',
                    env: process.env,
                },
            );
        } catch (err) {
            return emptySpawnError(err);
        }

        const live = new LiveRenderChild(
            child,
            opts.output,
            opts.signalGraceMs ?? SIGNAL_GRACE_MS,
            opts.shutdownTermGraceMs ?? SHUTDOWN_TERM_GRACE_MS,
            opts.shutdownKillWaitMs ?? SHUTDOWN_KILL_WAIT_MS,
        );
        this.liveChildren.add(live);
        try {
            return await this.runSpawned(opts, child, live);
        } finally {
            this.liveChildren.delete(live);
        }
    }

    /**
     * Begin the activation's bounded shutdown. The deactivating flag is set
     * synchronously so a later command continuation cannot spawn a new child.
     */
    shutdown(): Promise<void> {
        if (this.shutdownPromise) return this.shutdownPromise;
        this.deactivating = true;
        const snapshot = [...this.liveChildren];
        this.shutdownPromise = Promise.allSettled(
            snapshot.map((child) => child.shutdown()),
        ).then(() => undefined);
        return this.shutdownPromise;
    }

    /** Test-only count of registered render children. */
    getLiveChildCountForTesting(): number {
        return this.liveChildren.size;
    }

    private async runSpawned(
        opts: QuartoRenderOptions,
        child: ChildProcess,
        live: LiveRenderChild,
    ): Promise<KnitEngineResult> {
        let stdout = '';
        let stderr = '';
        let cancelled = false;
        let timedOut = false;
        let spawnError: NodeJS.ErrnoException | null = null;
        const timers: NodeJS.Timeout[] = [];
        const stderrWriter = new StderrWriter(opts.output);

        let outputDetached = false;
        const onStdoutData = (chunk: string): void => {
            stdout = appendQuartoRenderTail(stdout, chunk);
            opts.output.append(chunk);
        };
        const onStderrData = (chunk: string): void => {
            stderr = appendQuartoRenderTail(stderr, chunk);
            stderrWriter.feed(chunk);
        };
        child.stdout?.setEncoding('utf8');
        child.stdout?.on('data', onStdoutData);
        child.stderr?.setEncoding('utf8');
        child.stderr?.on('data', onStderrData);
        live.setOutputDetacher(() => {
            if (outputDetached) return;
            outputDetached = true;
            child.stdout?.off('data', onStdoutData);
            child.stderr?.off('data', onStderrData);
            try {
                stderrWriter.finish();
            } catch {
                // The activation output may already be disposing.
            }
        });
        child.on('error', (err) => {
            spawnError = err as NodeJS.ErrnoException;
        });

        child.on('close', (code) => {
            live.markClosed(code);
            if (!outputDetached) stderrWriter.finish();
        });

        const escalate = (): void => {
            if (live.closed) return;
            void live.stop();
        };
        const cancel = (): void => {
            if (cancelled) return;
            cancelled = true;
            escalate();
        };
        const cancelHook = opts.cancellation.onCancellationRequested(cancel);
        if (opts.cancellation.isCancellationRequested) cancel();
        timers.push(setTimeout(() => {
            if (timedOut || live.closed || live.shuttingDown) return;
            timedOut = true;
            escalate();
        }, clampQuartoRenderTimeoutMs(opts.timeoutMs)));

        const exitCode = await live.waitForCompletion();
        for (const timer of timers) clearTimeout(timer);
        cancelHook.dispose();
        return { exitCode, stdout, stderr, cancelled, timedOut, spawnError };
    }
}

function emptySpawnError(err: unknown): KnitEngineResult {
    return {
        exitCode: null,
        stdout: '',
        stderr: '',
        cancelled: false,
        timedOut: false,
        spawnError: err as NodeJS.ErrnoException,
    };
}

function emptyCancelledResult(): KnitEngineResult {
    return {
        exitCode: null,
        stdout: '',
        stderr: '',
        cancelled: true,
        timedOut: false,
        spawnError: null,
    };
}

class LiveRenderChild {
    closed = false;
    shuttingDown = false;
    private readonly closePromise: Promise<void>;
    private closeResolve: () => void = () => undefined;
    private readonly completionPromise: Promise<number | null>;
    private completionResolve: (code: number | null) => void = () => undefined;
    private completionSettled = false;
    private completionLinkedToTeardown = false;
    private readonly teardown: QuartoProcessTeardown;
    private outputDetached = false;
    private outputDetacher: (() => void) | null = null;

    constructor(
        child: ChildProcess,
        output: vscode.OutputChannel,
        signalGraceMs: number,
        shutdownTermGraceMs: number,
        shutdownKillWaitMs: number,
    ) {
        this.closePromise = new Promise((resolve) => {
            this.closeResolve = resolve;
        });
        this.completionPromise = new Promise((resolve) => {
            this.completionResolve = resolve;
        });
        this.teardown = new QuartoProcessTeardown({
            child,
            output,
            processKind: 'render',
            closePromise: this.closePromise,
            isClosed: () => this.closed,
            detachOutput: () => this.detachOutput(),
            stopGraceMs: signalGraceMs,
            shutdownTermGraceMs,
            killWaitMs: shutdownKillWaitMs,
        });
    }

    markClosed(code: number | null): void {
        if (this.closed) return;
        this.closed = true;
        this.closeResolve();
        this.complete(code);
    }

    setOutputDetacher(detacher: () => void): void {
        if (this.outputDetached) {
            detacher();
            return;
        }
        this.outputDetacher = detacher;
    }

    detachOutput(): void {
        if (this.outputDetached) return;
        this.outputDetached = true;
        this.outputDetacher?.();
        this.outputDetacher = null;
    }

    stop(): Promise<void> {
        return this.linkCompletion(this.teardown.stop());
    }

    shutdown(): Promise<void> {
        this.shuttingDown = true;
        return this.linkCompletion(this.teardown.shutdown());
    }

    waitForCompletion(): Promise<number | null> {
        return this.completionPromise;
    }

    private linkCompletion(teardown: Promise<void>): Promise<void> {
        if (!this.completionLinkedToTeardown) {
            this.completionLinkedToTeardown = true;
            void teardown.then(
                () => this.complete(null),
                () => this.complete(null),
            );
        }
        return teardown;
    }

    private complete(code: number | null): void {
        if (this.completionSettled) return;
        this.completionSettled = true;
        this.completionResolve(code);
    }
}

class StderrWriter {
    private atLineStart = true;

    constructor(private readonly output: vscode.OutputChannel) {}

    feed(chunk: string): void {
        let start = 0;
        while (start < chunk.length) {
            if (this.atLineStart) {
                this.output.append('[stderr] ');
                this.atLineStart = false;
            }
            const newline = chunk.indexOf('\n', start);
            if (newline === -1) {
                this.output.append(chunk.slice(start));
                return;
            }
            this.output.append(chunk.slice(start, newline + 1));
            this.atLineStart = true;
            start = newline + 1;
        }
    }

    finish(): void {
        if (!this.atLineStart) this.output.appendLine('');
        this.atLineStart = true;
    }
}
