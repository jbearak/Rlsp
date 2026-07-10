import * as assert from 'assert';
import * as vscode from 'vscode';
import { State, type StateChangeEvent } from 'vscode-languageclient/node';
import {
    registerRunningStateReconciliation,
    type RestartAwareClient,
} from '../client-state';

function deferred(): { promise: Promise<void>; resolve: () => void } {
    let resolve!: () => void;
    const promise = new Promise<void>((done) => {
        resolve = done;
    });
    return { promise, resolve };
}

suite('Client state reconciliation', () => {
    test('waits for every Running transition to finish starting', async () => {
        const changes = new vscode.EventEmitter<StateChangeEvent>();
        let state = State.Stopped;
        let currentStart = deferred();
        let startCalls = 0;
        let reconciliations = 0;
        const client: RestartAwareClient = {
            get state() {
                return state;
            },
            onDidChangeState: changes.event,
            start() {
                startCalls += 1;
                return currentStart.promise;
            },
        };
        const registration = registerRunningStateReconciliation(
            client,
            () => {
                reconciliations += 1;
            },
        );

        try {
            changes.fire({ oldState: State.Stopped, newState: State.Starting });
            assert.strictEqual(startCalls, 0);

            state = State.Running;
            changes.fire({ oldState: State.Starting, newState: State.Running });
            assert.strictEqual(startCalls, 1);
            assert.strictEqual(reconciliations, 0);
            currentStart.resolve();
            await currentStart.promise;
            await Promise.resolve();
            assert.strictEqual(reconciliations, 1);

            state = State.Stopped;
            changes.fire({ oldState: State.Running, newState: State.Stopped });
            currentStart = deferred();
            state = State.Running;
            changes.fire({ oldState: State.Starting, newState: State.Running });
            assert.strictEqual(startCalls, 2);
            assert.strictEqual(reconciliations, 1);
            currentStart.resolve();
            await currentStart.promise;
            await Promise.resolve();
            assert.strictEqual(reconciliations, 2);
        } finally {
            registration.dispose();
            changes.dispose();
        }
    });
});
