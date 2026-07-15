import { describe, test, expect } from 'bun:test';
import { classify } from '../../editors/vscode/src/knit/knit-output';

describe('classify', () => {
    test('spawn error wins over everything', () => {
        const err = Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
        const outcome = classify({
            spawnError: err,
            cancelled: false,
            timedOut: false,
            exitCode: null,
            stdout: '',
            stderr: '',
        }, { cwd: '/wd' });
        expect(outcome.kind).toBe('spawnError');
    });

    test('cancelled beats timedOut and failure', () => {
        const outcome = classify({
            spawnError: null,
            cancelled: true,
            timedOut: false,
            exitCode: 130,
            stdout: '',
            stderr: '',
        }, { cwd: '/wd' });
        expect(outcome.kind).toBe('cancelled');
    });

    test('timedOut beats failure', () => {
        const outcome = classify({
            spawnError: null,
            cancelled: false,
            timedOut: true,
            exitCode: null,
            stdout: '',
            stderr: '',
        }, { cwd: '/wd' });
        expect(outcome.kind).toBe('timedOut');
    });

    test('non-zero exit is "failed"', () => {
        const outcome = classify({
            spawnError: null,
            cancelled: false,
            timedOut: false,
            exitCode: 1,
            stdout: '',
            stderr: '',
        }, { cwd: '/wd' });
        expect(outcome.kind).toBe('failed');
        if (outcome.kind === 'failed') expect(outcome.exitCode).toBe(1);
    });

    test('clean exit with no output path is "noOutput"', () => {
        const outcome = classify({
            spawnError: null,
            cancelled: false,
            timedOut: false,
            exitCode: 0,
            stdout: '\n',
            stderr: '',
        }, { cwd: '/wd' });
        expect(outcome.kind).toBe('noOutput');
    });

    test('clean exit with output path is "ok"', () => {
        const outcome = classify({
            spawnError: null,
            cancelled: false,
            timedOut: false,
            exitCode: 0,
            stdout: 'Output created: out.html\n',
            stderr: '',
        }, { cwd: '/wd' });
        expect(outcome.kind).toBe('ok');
        if (outcome.kind === 'ok') {
            expect(outcome.parsedOutputs).toEqual(['out.html']);
            expect(outcome.cwd).toBe('/wd');
        }
    });

    test('cwd undefined propagates through ok outcome', () => {
        const outcome = classify({
            spawnError: null,
            cancelled: false,
            timedOut: false,
            exitCode: 0,
            stdout: 'Output created: out.html\n',
            stderr: '',
        }, { cwd: undefined });
        expect(outcome.kind).toBe('ok');
        if (outcome.kind === 'ok') expect(outcome.cwd).toBeUndefined();
    });
});
