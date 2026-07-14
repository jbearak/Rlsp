/**
 * Activation-scoped ownership for asynchronous Quarto command continuations.
 *
 * `run` invokes the command body synchronously so it can register pending
 * Preview intent before yielding, then retains its promise through preflight,
 * binary discovery, subprocess work, and outcome handling. Shutdown rejects
 * new command bodies and waits for a snapshot under a cancelable global bound.
 */

export interface QuartoCommandCancelableDelay {
    promise: Promise<void>;
    cancel(): void;
}

export interface QuartoCommandLifecycleOptions {
    shutdownTimeoutMs?: number;
    delay?: (ms: number) => QuartoCommandCancelableDelay;
}

export class QuartoCommandLifecycle {
    private readonly inFlight = new Set<Promise<unknown>>();
    private deactivating = false;
    private shutdownPromise: Promise<void> | null = null;

    constructor(private readonly opts: QuartoCommandLifecycleOptions = {}) {}

    run<T>(factory: () => Promise<T>): Promise<T | undefined> {
        if (this.deactivating) return Promise.resolve(undefined);
        let work: Promise<T>;
        try {
            work = factory();
        } catch (err) {
            work = Promise.reject(err);
        }
        this.inFlight.add(work);
        void work.finally(() => this.inFlight.delete(work)).catch(() => undefined);
        return work;
    }

    shutdown(): Promise<void> {
        if (this.shutdownPromise) return this.shutdownPromise;
        this.deactivating = true;
        const all = Promise.allSettled([...this.inFlight]);
        const bound = (this.opts.delay ?? cancelableDelay)(
            this.opts.shutdownTimeoutMs ?? 12_000,
        );
        this.shutdownPromise = Promise.race([all, bound.promise])
            .then(() => undefined)
            .finally(() => bound.cancel());
        return this.shutdownPromise;
    }
}

function cancelableDelay(ms: number): QuartoCommandCancelableDelay {
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
