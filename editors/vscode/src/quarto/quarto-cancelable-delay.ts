/**
 * A single cancelable timer primitive shared by every bounded Quarto wait.
 *
 * Consumers race a promise against this timer: the preview runtime and command
 * lifecycle bound their shutdown teardown with it, and the command layer also
 * bounds project-context discovery (`resolveContextForSource`) so a wedged
 * filesystem cannot hang Stop or a Preview preflight. Keeping one implementation
 * here means a future change to cancellation semantics or timer `unref` behavior
 * updates every consumer at once instead of leaving one path on stale mechanics.
 */

export interface QuartoCancelableDelay {
    promise: Promise<void>;
    cancel(): void;
}

export function cancelableDelay(ms: number): QuartoCancelableDelay {
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
