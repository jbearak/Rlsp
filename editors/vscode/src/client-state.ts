import * as vscode from 'vscode';
import { State, type StateChangeEvent } from 'vscode-languageclient/node';

/** Minimal language-client lifecycle surface used by restart reconciliation. */
export interface RestartAwareClient {
    readonly state: State;
    readonly onDidChangeState: vscode.Event<StateChangeEvent>;
    start(): Promise<void>;
}

/**
 * Reconcile client-owned state after every initial or automatic start.
 *
 * vscode-languageclient emits `Running` before its initialization tail has
 * completed. Calling `start()` at that point returns the already-active start
 * promise, so waiting for it gives the callback a fully initialized connection
 * without initiating another start. The listener remains installed across the
 * library's internal crash-restart cleanup.
 */
export function registerRunningStateReconciliation(
    client: RestartAwareClient,
    reconcile: () => void,
    onReconcileError?: (error: unknown) => void,
): vscode.Disposable {
    return client.onDidChangeState(({ newState }) => {
        if (newState !== State.Running) {
            return;
        }

        // The start/restart owner already reports a rejected start promise.
        // This observer handles that branch only to avoid creating a second
        // unhandled rejection from its attached continuation.
        void client.start().then(
            () => {
                if (client.state !== State.Running) {
                    return;
                }
                try {
                    reconcile();
                } catch (error) {
                    onReconcileError?.(error);
                }
            },
            () => undefined,
        );
    });
}
