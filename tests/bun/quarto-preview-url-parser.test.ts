import { describe, expect, test } from 'bun:test';
import {
    QUARTO_STARTUP_TAIL_LIMIT,
    QuartoPreviewOutputScanner,
    stripAnsi,
    validatePreviewUrl,
} from '../../editors/vscode/src/quarto/preview-url-parser';

const QUARTO_1_9_38_STDERR =
    'pandoc \n' +
    '  to: html\n' +
    '  output-file: spike.html\n\n' +
    'Output created: spike.html\n\n' +
    '\x1b[32mWatching files for changes\x1b[39m\n' +
    '\x1b[32mBrowse at \x1b[39m\x1b[4m\x1b[32mhttp://localhost:3715/\x1b[39m\x1b[24m\n' +
    'Listening on http://127.0.0.1:3715/\n';

describe('Quarto preview URL parser', () => {
    test('strips ANSI and correlates the real Quarto 1.9.38 output', () => {
        expect(stripAnsi('\x1b[32mBrowse at\x1b[39m')).toBe('Browse at');
        const scanner = new QuartoPreviewOutputScanner();
        expect(scanner.feed(QUARTO_1_9_38_STDERR)).toEqual({
            origin: 'http://127.0.0.1:3715',
            url: 'http://127.0.0.1:3715/',
        });
    });

    test('handles lines split across seven-byte chunks without rescanning', () => {
        const scanner = new QuartoPreviewOutputScanner();
        let result = null;
        for (let offset = 0; offset < QUARTO_1_9_38_STDERR.length; offset += 7) {
            result = scanner.feed(QUARTO_1_9_38_STDERR.slice(offset, offset + 7)) ?? result;
        }
        expect(result).toEqual({
            origin: 'http://127.0.0.1:3715',
            url: 'http://127.0.0.1:3715/',
        });
    });

    test('uses Listening origin and Browse path/query', () => {
        const scanner = new QuartoPreviewOutputScanner();
        expect(
            scanner.feed(
                'Browse at http://localhost:4444/chapter/?preview=1#top\n' +
                'Listening on http://127.0.0.1:4444/\n',
            ),
        ).toEqual({
            origin: 'http://127.0.0.1:4444',
            url: 'http://127.0.0.1:4444/chapter/?preview=1#top',
        });
    });

    test('handles Browse-only at end-of-stream and Listening-only immediately', () => {
        const browseOnly = new QuartoPreviewOutputScanner();
        expect(browseOnly.feed('Browse at http://localhost:3210/document/\n')).toBeNull();
        expect(browseOnly.finish()).toEqual({
            origin: 'http://localhost:3210',
            url: 'http://localhost:3210/document/',
        });

        const listeningOnly = new QuartoPreviewOutputScanner();
        expect(listeningOnly.feed('Listening on http://[::1]:7777/preview\n')).toEqual({
            origin: 'http://[::1]:7777',
            url: 'http://[::1]:7777/preview',
        });
    });

    test('keeps Browse-only acceptance provisional for late Listening correlation', () => {
        const scanner = new QuartoPreviewOutputScanner();
        scanner.feed('Browse at http://localhost:3210/document/\n');
        expect(scanner.acceptBrowseCandidate()).toEqual({
            origin: 'http://localhost:3210',
            url: 'http://localhost:3210/document/',
        });
        expect(scanner.result()).toBeNull();
        expect(scanner.feedLine('Listening on http://127.0.0.1:6543/')).toEqual({
            origin: 'http://127.0.0.1:6543',
            url: 'http://127.0.0.1:6543/document/',
        });
    });

    test('returns no result for output with no advertised URL', () => {
        const scanner = new QuartoPreviewOutputScanner();
        expect(scanner.feed('Output created: report.html\nWatching files for changes\n')).toBeNull();
        expect(scanner.finish()).toBeNull();
        expect(scanner.result()).toBeNull();
    });

    test('accepts only credential-free loopback HTTP URLs with valid ports', () => {
        expect(validatePreviewUrl('http://127.0.0.1/')).toEqual({
            origin: 'http://127.0.0.1',
            url: 'http://127.0.0.1/',
        });
        expect(validatePreviewUrl('http://localhost:80/path')).toEqual({
            origin: 'http://localhost',
            url: 'http://localhost/path',
        });
        expect(validatePreviewUrl('http://[::1]:8080/')).toEqual({
            origin: 'http://[::1]:8080',
            url: 'http://[::1]:8080/',
        });

        for (const hostile of [
            'https://evil.example/',
            'http://localhost.evil/',
            'http://user:pass@127.0.0.1:1/',
            'ftp://127.0.0.1/',
            'http://localhost:not-a-port/',
            'http://localhost:/',
            'http://localhost:65536/',
            'http://localhost:1.5/',
            'http://2130706433/',
            'http://0x7f000001/',
        ]) {
            expect(validatePreviewUrl(hostile)).toBeNull();
        }
    });

    test('treats hostile advertised URLs as hard failures', () => {
        for (const hostile of [
            'https://evil.example/',
            'http://localhost.evil/',
            'http://user:pass@127.0.0.1:1/',
            'ftp://127.0.0.1/',
        ]) {
            const scanner = new QuartoPreviewOutputScanner();
            expect(scanner.feed(`Browse at ${hostile}\n`)).toBeNull();
            expect(scanner.failure()).toContain('Rejected unsafe');
            expect(scanner.feed('Listening on http://127.0.0.1:9999/\n')).toBeNull();
        }
    });

    test('caps the raw startup tail', () => {
        const scanner = new QuartoPreviewOutputScanner();
        const output = 'prefix-' + 'x'.repeat(QUARTO_STARTUP_TAIL_LIMIT + 100) + '-suffix';
        scanner.feed(output);
        expect(scanner.startupTail().length).toBe(QUARTO_STARTUP_TAIL_LIMIT);
        expect(scanner.startupTail()).toEndWith('-suffix');

        const tiny = new QuartoPreviewOutputScanner(8);
        tiny.feed('0123456789');
        expect(tiny.startupTail()).toBe('23456789');
    });

    test('keeps capturing raw tail after URL parsing stops', () => {
        const scanner = new QuartoPreviewOutputScanner();
        scanner.feed('Listening on http://127.0.0.1:9999/\n');
        expect(scanner.feed('fatal error after URL advertisement\n')).toBeNull();
        expect(scanner.startupTail()).toEndWith('fatal error after URL advertisement\n');
        expect(scanner.result()?.url).toBe('http://127.0.0.1:9999/');
    });
});
