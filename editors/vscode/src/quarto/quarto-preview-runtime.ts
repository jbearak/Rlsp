/**
 * Activation-scoped Quarto preview runtime with generation-safe sessions.
 *
 * Generation discipline is the central invariant: `startOrRestart` increments
 * the key's generation and replaces the map entry synchronously before its
 * first await. Every process close, readiness result, URI mapping, panel state
 * update, and panel-dispose stop carries `{ key, generation }` and is ignored
 * when stale. Restart therefore cannot let an old child, probe, mapping, or
 * disposed panel overwrite the replacement session.
 *
 * Source aliases point to the generation that originated a session. Stop can
 * consequently find a running preview after `_quarto.yml` appears or
 * disappears and changes the freshly-computed project key. A restart may find
 * one prior session by its newly-computed key and a different prior session by
 * source alias; it invalidates both map entries synchronously and retires every
 * identity-distinct prior session before spawning the replacement. Retirements
 * remain registered until their shared stop promises settle. Every claimed
 * session records the exact predecessor set it displaced; its memoized
 * teardown concurrently stops itself and recursively tears down that acyclic
 * predecessor graph. Superseding a not-yet-spawned session therefore inherits
 * its entire pending drain instead of spawning around an older live process.
 * Drained predecessor references are released before a replacement spawns and
 * whenever teardown settles, so the live session cannot retain an unbounded
 * history of process wrappers.
 *
 * Explicit and panel-dispose stops keep their session current and retirement-
 * registered until transitive teardown settles. A concurrent replacement can
 * therefore inherit the same memoized drain instead of losing sight of a deep
 * live predecessor. Only the still-current stop owner emits `stopped`; a
 * not-yet-ready child's intentional-close rejection is superseded rather than
 * allowed to emit `failed` or remove that ownership early.
 * Process stop/shutdown and recursive teardown are rejection-safe: injected or
 * defensive failures are logged and treated as settled so one rejected promise
 * cannot poison retirement ownership or a future predecessor drain.
 *
 * Shutdown rejects new starts, snapshots both live and retiring sessions,
 * sends immediate termination concurrently, and applies a cancelable global
 * bound. Thus a child removed from live generation ownership remains visible
 * to deactivation until its stop settles. The activation-level lifecycle
 * coordinates this shutdown with one-shot renders and disposes their shared
 * output channel only after both engines have settled.
 */

import { canonicalOpKey } from '../knit/raven-knit-paths';
import type { QuartoPreviewViewState } from './quarto-messages';
import type {
    QuartoPreviewProcessLike,
    QuartoPreviewReady,
} from './quarto-preview-engine';

export interface QuartoRuntimeProcessArgs {
    key: string;
    generation: number;
    quartoPath: string;
    sourceFsPath: string;
    cwd: string;
    onUnexpectedExit(code: number | null): void;
}

export interface QuartoRuntimeViewUpdate {
    key: string;
    generation: number;
    sourceFsPath: string;
    state: QuartoPreviewViewState;
    /** Raw validated loopback URL, used only for Open in Browser. */
    rawUrl?: string;
}

export interface QuartoRuntimeDeps {
    processFactory(args: QuartoRuntimeProcessArgs): QuartoPreviewProcessLike;
    asExternalUri(rawUrl: string): Promise<string>;
    onViewUpdate(update: QuartoRuntimeViewUpdate): void;
    /** Lifecycle diagnostics; production routes these to the Quarto output. */
    onLifecycleError?(message: string): void;
    shutdownGlobalTimeoutMs?: number;
    /** Dependency-injected only to verify global-bound timer cancellation. */
    shutdownDelay?: (ms: number) => QuartoCancelableDelay;
}

export interface QuartoCancelableDelay {
    promise: Promise<void>;
    cancel(): void;
}

export interface QuartoStartArgs {
    key: string;
    quartoPath: string;
    sourceFsPath: string;
    cwd: string;
}

export type QuartoStartResult =
    | { kind: 'ready'; generation: number; rawUrl: string; externalUrl: string }
    | { kind: 'superseded'; generation: number }
    | { kind: 'failed'; generation: number; error: Error };

export type QuartoStopResult = 'stopped' | 'already-stopping' | 'none';

/** One generation-tagged process slot. */
export class Session {
    process: QuartoPreviewProcessLike | null = null;
    stopping = false;
    exited = false;
    private stopPromise: Promise<void> | null = null;
    private teardownPromise: Promise<void> | null = null;
    private shutdownPromise: Promise<void> | null = null;
    private predecessors: Session[];

    constructor(
        readonly key: string,
        readonly generation: number,
        readonly sourceFsPath: string,
        predecessors: readonly Session[],
        private readonly logError: (message: string) => void,
    ) {
        this.predecessors = [...predecessors];
    }

    stop(): Promise<void> {
        if (this.stopPromise) return this.stopPromise;
        this.stopping = true;
        this.stopPromise = this.settleProcessOperation(
            'stop',
            () => this.process?.stop(),
        );
        return this.stopPromise;
    }

    /** Stop this session and its complete, acyclic predecessor graph once. */
    teardown(): Promise<void> {
        if (this.teardownPromise) return this.teardownPromise;
        // Capture before any concurrent successful start releases the field.
        // The memoized promise remains authoritative after references are cut.
        const predecessors = this.predecessors;
        this.teardownPromise = Promise.all([
            this.stop(),
            ...predecessors.map((predecessor) => predecessor.teardown()),
        ])
            .then(() => undefined)
            .catch((err) => {
                this.reportFailure('teardown', err);
            })
            .finally(() => this.releasePredecessors());
        return this.teardownPromise;
    }

    /** Release drained process history without invalidating memoized teardown. */
    releasePredecessors(): void {
        this.predecessors = [];
    }

    /** Test-only retained predecessor count. */
    getPredecessorCountForTesting(): number {
        return this.predecessors.length;
    }

    shutdown(): Promise<void> {
        if (this.shutdownPromise) return this.shutdownPromise;
        this.stopping = true;
        this.shutdownPromise = this.settleProcessOperation(
            'shutdown',
            () => this.process?.shutdown(),
        );
        return this.shutdownPromise;
    }

    private settleProcessOperation(
        operation: 'stop' | 'shutdown',
        run: () => Promise<void> | undefined,
    ): Promise<void> {
        try {
            const result = run();
            return Promise.resolve(result).catch((err) => {
                this.reportFailure(operation, err);
            });
        } catch (err) {
            this.reportFailure(operation, err);
            return Promise.resolve();
        }
    }

    private reportFailure(operation: string, err: unknown): void {
        try {
            this.logError(
                `[runtime] preview ${operation} failed for ${this.key}` +
                `#${this.generation}: ${errorMessage(err)}`,
            );
        } catch {
            // Logging must not re-poison an otherwise settled teardown.
        }
    }
}

export class QuartoRuntime {
    private readonly sessions = new Map<string, Session>();
    private readonly retiring = new Set<Session>();
    private readonly retirementHolds = new Map<Session, number>();
    private readonly sourceAliases = new Map<string, { key: string; generation: number }>();
    private readonly nextGeneration = new Map<string, number>();
    private deactivating = false;
    private shutdownPromise: Promise<void> | null = null;

    constructor(private readonly deps: QuartoRuntimeDeps) {}

    /**
     * Claim a replacement synchronously, then drain its predecessor teardown.
     */
    async startOrRestart(args: QuartoStartArgs): Promise<QuartoStartResult> {
        if (this.deactivating) {
            throw new Error('Quarto runtime is deactivating; new previews are disabled.');
        }

        const sourceKey = canonicalOpKey({ fsPath: args.sourceFsPath });
        const alias = this.sourceAliases.get(sourceKey);
        const oldByNewKey = this.sessions.get(args.key) ?? null;
        const oldByAlias = alias ? this.current(alias.key, alias.generation) : null;
        const matchingRetirements = [...this.retiring].filter((old) =>
            old.key === args.key
            || canonicalOpKey({ fsPath: old.sourceFsPath }) === sourceKey,
        );
        const oldSessions = [...new Set(
            [oldByNewKey, oldByAlias, ...matchingRetirements]
                .filter((old): old is Session => old !== null),
        )];
        const generation = (this.nextGeneration.get(args.key) ?? 0) + 1;
        this.nextGeneration.set(args.key, generation);
        const session = new Session(
            args.key,
            generation,
            args.sourceFsPath,
            oldSessions,
            (message) => this.deps.onLifecycleError?.(message),
        );

        // Claim before the first await. The new-key and source-alias lookups
        // can identify different sessions after project markers change, so
        // invalidate every old owner before any stop continuation can run.
        // Registering each retirement in the same synchronous section keeps
        // detached children visible to later starts and deactivation.
        for (const old of oldSessions) {
            if (this.sessions.get(old.key) === old) this.sessions.delete(old.key);
            this.deleteAliasFor(old);
            this.registerRetirement(old);
        }
        this.sessions.set(args.key, session);
        this.sourceAliases.set(sourceKey, { key: args.key, generation });
        this.emit(session, { kind: 'starting' });

        if (oldSessions.length > 0) {
            await Promise.all(oldSessions.map((old) => old.teardown()));
        }
        // All inherited work is fully drained. A future superseder needs only
        // this session's own stop, so retaining the old graph would leak every
        // historical process wrapper across sequential restarts.
        session.releasePredecessors();
        if (!this.isCurrent(session) || session.stopping || this.deactivating) {
            return { kind: 'superseded', generation };
        }

        try {
            const process = this.deps.processFactory({
                key: args.key,
                generation,
                quartoPath: args.quartoPath,
                sourceFsPath: args.sourceFsPath,
                cwd: args.cwd,
                onUnexpectedExit: (code) => this.handleUnexpectedExit(
                    args.key,
                    generation,
                    code,
                ),
            });
            session.process = process;
            if (!this.isCurrent(session) || session.stopping || this.deactivating) {
                await session.shutdown();
                return { kind: 'superseded', generation };
            }

            const ready = await process.start();
            if (
                !this.isCurrent(session)
                || session.stopping
                || session.exited
                || this.deactivating
            ) {
                await session.shutdown();
                return { kind: 'superseded', generation };
            }

            const externalUrl = await this.deps.asExternalUri(ready.rawUrl);
            if (
                !this.isCurrent(session)
                || session.stopping
                || session.exited
                || this.deactivating
            ) {
                await session.shutdown();
                return { kind: 'superseded', generation };
            }

            this.emitReady(session, ready, externalUrl);
            return {
                kind: 'ready',
                generation,
                rawUrl: ready.rawUrl,
                externalUrl,
            };
        } catch (err) {
            const error = err instanceof Error ? err : new Error(String(err));
            if (session.stopping || !this.isCurrent(session)) {
                // Intentional stop owns the terminal state/removal. Startup
                // commonly rejects when that stop closes a not-yet-ready
                // child; treating it as failure would steal `stopped`.
                await session.teardown();
                return { kind: 'superseded', generation };
            }
            const startupTail = readStartupTail(error);
            const detail = startupTail === ''
                ? error.message
                : `${error.message}\n\n${startupTail}`;
            this.emit(session, { kind: 'failed', detailText: detail });
            await session.stop();
            if (this.isCurrent(session)) {
                this.sessions.delete(session.key);
                this.deleteAliasFor(session);
            }
            return { kind: 'failed', generation, error };
        }
    }

    /** Stop only when the panel's generation still owns the key. */
    async stopGeneration(key: string, generation: number): Promise<QuartoStopResult> {
        const session = this.current(key, generation);
        if (!session) return 'none';
        return this.stopSession(session);
    }

    /**
     * Stop lookup has no preflight contract: callers pass a freshly-computed
     * key and source path, and this method also consults the historical alias.
     */
    async stopByLookup(key: string, sourceFsPath: string): Promise<QuartoStopResult> {
        let session = this.sessions.get(key) ?? null;
        if (!session) {
            const alias = this.sourceAliases.get(canonicalOpKey({ fsPath: sourceFsPath }));
            if (alias) session = this.current(alias.key, alias.generation);
        }
        if (!session) return 'none';
        return this.stopSession(session);
    }

    hasSession(key: string, sourceFsPath: string): boolean {
        if (this.sessions.has(key)) return true;
        const alias = this.sourceAliases.get(canonicalOpKey({ fsPath: sourceFsPath }));
        return alias ? this.current(alias.key, alias.generation) !== null : false;
    }

    shutdown(): Promise<void> {
        if (this.shutdownPromise) return this.shutdownPromise;
        this.deactivating = true;
        const snapshot = [...new Set([
            ...this.sessions.values(),
            ...this.retiring,
        ])];
        this.sessions.clear();
        this.sourceAliases.clear();
        const all = Promise.allSettled(snapshot.map((session) => session.shutdown()));
        const bound = (this.deps.shutdownDelay ?? cancelableDelay)(
            this.deps.shutdownGlobalTimeoutMs ?? 7_000,
        );
        const shutdown = Promise.race([all, bound.promise])
            .then(() => undefined)
            .finally(() => bound.cancel());
        this.shutdownPromise = shutdown;
        return shutdown;
    }

    /** Test-only snapshot of live generation ownership. */
    getSessionsForTesting(): ReadonlyMap<string, Session> {
        return this.sessions;
    }

    private async stopSession(session: Session): Promise<QuartoStopResult> {
        if (session.stopping) return 'already-stopping';
        // Keep the session map/alias current while its whole predecessor graph
        // drains. Retirement gets a second hold through teardown so both map
        // and shutdown discovery remain intact even if this session has no
        // process and its own stop settles immediately.
        this.registerRetirement(session);
        const teardown = session.teardown();
        this.retainRetirementUntil(session, teardown);
        await teardown;
        if (this.isCurrent(session)) {
            this.emit(session, { kind: 'stopped' });
            this.sessions.delete(session.key);
            this.deleteAliasFor(session);
        }
        return 'stopped';
    }

    /** Keep each detached process discoverable until its own stop settles. */
    private registerRetirement(session: Session): void {
        if (this.retiring.has(session)) return;
        this.retiring.add(session);
        // Teardown may wait on a much deeper predecessor graph, but retirement
        // membership is deliberately scoped to this session's own process.
        // The catch prevents this observer from duplicating teardown's error.
        this.retainRetirementUntil(session, session.stop());
    }

    /** Retain a session in the shutdown-visible set through `settled`. */
    private retainRetirementUntil(session: Session, settled: Promise<unknown>): void {
        this.retirementHolds.set(
            session,
            (this.retirementHolds.get(session) ?? 0) + 1,
        );
        void settled
            .finally(() => {
                const remaining = (this.retirementHolds.get(session) ?? 1) - 1;
                if (remaining === 0) {
                    this.retirementHolds.delete(session);
                    this.retiring.delete(session);
                } else {
                    this.retirementHolds.set(session, remaining);
                }
            })
            .catch(() => undefined);
    }

    private handleUnexpectedExit(
        key: string,
        generation: number,
        code: number | null,
    ): void {
        const session = this.current(key, generation);
        if (!session || session.stopping) return;
        session.exited = true;
        this.emit(session, { kind: 'exited-unexpectedly', code });
    }

    private emitReady(
        session: Session,
        ready: QuartoPreviewReady,
        externalUrl: string,
    ): void {
        if (!this.isCurrent(session)) return;
        this.deps.onViewUpdate({
            key: session.key,
            generation: session.generation,
            sourceFsPath: session.sourceFsPath,
            rawUrl: ready.rawUrl,
            state: { kind: 'serving', externalUrl },
        });
    }

    private emit(session: Session, state: QuartoPreviewViewState): void {
        if (!this.isCurrent(session)) return;
        this.deps.onViewUpdate({
            key: session.key,
            generation: session.generation,
            sourceFsPath: session.sourceFsPath,
            state,
        });
    }

    private current(key: string, generation: number): Session | null {
        const session = this.sessions.get(key);
        return session?.generation === generation ? session : null;
    }

    private isCurrent(session: Session): boolean {
        return this.current(session.key, session.generation) === session;
    }

    private deleteAliasFor(session: Session): void {
        const sourceKey = canonicalOpKey({ fsPath: session.sourceFsPath });
        const alias = this.sourceAliases.get(sourceKey);
        if (alias?.key === session.key && alias.generation === session.generation) {
            this.sourceAliases.delete(sourceKey);
        }
    }
}

function readStartupTail(error: Error): string {
    const candidate = error as Error & { startupTail?: unknown };
    return typeof candidate.startupTail === 'string' ? candidate.startupTail.trim() : '';
}

function errorMessage(err: unknown): string {
    return err instanceof Error ? err.message : String(err);
}

function cancelableDelay(ms: number): QuartoCancelableDelay {
    let timer: NodeJS.Timeout | null = null;
    const promise = new Promise<void>((resolve) => {
        timer = setTimeout(() => {
            timer = null;
            resolve();
        }, ms);
    });
    return {
        promise,
        cancel: () => {
            if (timer) clearTimeout(timer);
            timer = null;
        },
    };
}
