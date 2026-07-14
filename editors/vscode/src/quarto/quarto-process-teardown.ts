/**
 * Shared, per-child Quarto process teardown coordination.
 *
 * One child owns at most one signal ladder. A graceful stop starts with
 * SIGINT; a later deactivation request reuses that promise, interrupts only
 * the remaining graceful wait, and continues with the shorter shutdown
 * cadence. A shutdown-first teardown starts at SIGTERM. Both paths prefer a
 * confirmed `close`, bound the final post-SIGKILL wait, and swallow output
 * errors from the abandonment warning because the activation channel may
 * already be disposing.
 */

import type { ChildProcess } from 'child_process';
import { sendSignal } from '../knit/process-signals';

interface LineOutput {
    appendLine(value: string): void;
}

export interface QuartoProcessTeardownOptions {
    child: ChildProcess;
    output: LineOutput;
    processKind: 'preview' | 'render';
    closePromise: Promise<void>;
    isClosed(): boolean;
    detachOutput(): void;
    stopGraceMs: number;
    shutdownTermGraceMs: number;
    killWaitMs: number;
}

type TeardownMode = 'stop' | 'shutdown';

export class QuartoProcessTeardown {
    private mode: TeardownMode | null = null;
    private completionPromise: Promise<void> | null = null;
    private interruptWait: (() => void) | null = null;

    constructor(private readonly opts: QuartoProcessTeardownOptions) {}

    stop(): Promise<void> {
        return this.begin('stop');
    }

    shutdown(): Promise<void> {
        return this.begin('shutdown');
    }

    private begin(requestedMode: TeardownMode): Promise<void> {
        try {
            this.opts.detachOutput();
        } catch {
            // Detachment is best-effort when the host is already disposing.
        }
        if (this.completionPromise) {
            if (requestedMode === 'shutdown' && this.mode === 'stop') {
                this.mode = 'shutdown';
                this.interruptWait?.();
            }
            return this.completionPromise;
        }

        this.mode = requestedMode;
        this.completionPromise = this.runLadder();
        return this.completionPromise;
    }

    private async runLadder(): Promise<void> {
        if (this.opts.isClosed()) return;

        if (this.mode === 'stop') {
            sendSignal(this.opts.child, 'SIGINT');
            if (await this.waitForClose(this.opts.stopGraceMs, true)) return;
        }

        sendSignal(this.opts.child, 'SIGTERM');
        const termGraceMs = this.mode === 'shutdown'
            ? this.opts.shutdownTermGraceMs
            : this.opts.stopGraceMs;
        if (await this.waitForClose(termGraceMs, this.mode === 'stop')) return;

        sendSignal(this.opts.child, 'SIGKILL');
        if (!await this.waitForClose(this.opts.killWaitMs, false)) {
            this.appendAbandonWarning();
        }
    }

    /** Wait for close, optionally allowing shutdown to shorten this phase. */
    private async waitForClose(
        timeoutMs: number,
        interruptibleByShutdown: boolean,
    ): Promise<boolean> {
        if (this.opts.isClosed()) return true;

        let timer: NodeJS.Timeout | undefined;
        let interrupt: (() => void) | null = null;
        const races: Array<Promise<boolean>> = [
            this.opts.closePromise.then(() => true),
            new Promise<boolean>((resolve) => {
                timer = setTimeout(() => resolve(false), timeoutMs);
            }),
        ];
        if (interruptibleByShutdown) {
            races.push(new Promise<boolean>((resolve) => {
                interrupt = () => resolve(false);
                this.interruptWait = interrupt;
            }));
        }

        const result = await Promise.race(races);
        if (timer) clearTimeout(timer);
        if (this.interruptWait === interrupt) this.interruptWait = null;
        return result;
    }

    private appendAbandonWarning(): void {
        try {
            this.opts.output.appendLine(
                `[quarto] ${this.opts.processKind} process ` +
                `${String(this.opts.child.pid ?? 'unknown')} did not confirm exit ` +
                'after SIGKILL; abandoning',
            );
        } catch {
            // Output channels can be disposed during bounded deactivation.
        }
    }
}
