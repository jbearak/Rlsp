import { afterAll, describe, expect, it } from 'bun:test';
import { spawn } from 'child_process';
import { mkdtemp, rm } from 'fs/promises';
import * as http from 'http';
import * as net from 'net';
import { tmpdir } from 'os';
import { join } from 'path';
import { gzipSync } from 'zlib';
import {
    filteredRequestHeaders,
    QuartoPreviewProxy,
} from '../../editors/vscode/src/quarto/quarto-preview-proxy';

const BRIDGE_JS = Buffer.from('window.__ravenBridgeTest = true;');
const BRIDGE_CSS = Buffer.from('html.raven-vscode-theme { color: red; }');
const BRIDGE_ASSETS = { javascript: BRIDGE_JS, css: BRIDGE_CSS };
const INJECTION =
    '<link rel="stylesheet" href="/_raven-theme-bridge/bridge.css">' +
    '<script src="/_raven-theme-bridge/bridge.js"></script>';

interface RawResponse {
    statusCode: number;
    headers: http.IncomingHttpHeaders;
    body: Buffer;
}

describe('QuartoPreviewProxy HTTP passthrough', () => {
    it('forwards GET and POST method, path, headers, status, and body', async () => {
        const upstream = http.createServer(async (request, response) => {
            const body = await readBody(request);
            response.writeHead(207, {
                'Content-Type': 'application/octet-stream',
                'X-Upstream-Method': request.method ?? '',
                'X-Upstream-Path': request.url ?? '',
                'X-Request-Header': request.headers['x-request-header'] ?? '',
            });
            response.end(body.length === 0 ? Buffer.from('get-body') : body);
        });
        const upstreamOrigin = await listen(upstream);
        const proxy = new QuartoPreviewProxy(upstreamOrigin, BRIDGE_ASSETS);
        try {
            const ready = await proxy.start();
            const get = await rawRequest(`${ready.origin}/chapter/?x=1`);
            expect(get.statusCode).toBe(207);
            expect(get.headers['x-upstream-method']).toBe('GET');
            expect(get.headers['x-upstream-path']).toBe('/chapter/?x=1');
            expect(get.body.toString()).toBe('get-body');

            const payload = Buffer.from([0, 1, 2, 3, 255]);
            const post = await rawRequest(`${ready.origin}/submit?q=yes`, {
                method: 'POST',
                headers: {
                    'Content-Length': payload.length,
                    'X-Request-Header': 'preserved',
                },
                body: payload,
            });
            expect(post.statusCode).toBe(207);
            expect(post.headers['content-type']).toBe('application/octet-stream');
            expect(post.headers['x-upstream-method']).toBe('POST');
            expect(post.headers['x-upstream-path']).toBe('/submit?q=yes');
            expect(post.headers['x-request-header']).toBe('preserved');
            expect(post.body).toEqual(payload);
        } finally {
            await proxy.close();
            await closeServer(upstream);
        }
    });

    it('rewrites a forwarded HTTP Origin only when the client supplied one', async () => {
        const seenOrigins: Array<string | undefined> = [];
        const upstream = http.createServer((request, response) => {
            seenOrigins.push(request.headers.origin);
            response.end('ok');
        });
        const upstreamOrigin = await listen(upstream);
        const proxy = new QuartoPreviewProxy(upstreamOrigin, BRIDGE_ASSETS);
        try {
            const ready = await proxy.start();
            await rawRequest(`${ready.origin}/with-origin`, {
                headers: { Origin: ready.origin },
            });
            await rawRequest(`${ready.origin}/without-origin`);
            expect(seenOrigins).toEqual([upstreamOrigin, undefined]);
        } finally {
            await proxy.close();
            await closeServer(upstream);
        }
    });

    it('passes compressed binary response bytes and entity headers untouched', async () => {
        const source = Buffer.from(Array.from({ length: 512 }, (_, index) => index % 251));
        const compressed = gzipSync(source);
        const upstream = http.createServer((_request, response) => {
            response.writeHead(200, {
                'Content-Type': 'application/octet-stream',
                'Content-Encoding': 'gzip',
                'Content-Length': compressed.length,
                ETag: '"opaque-compressed-etag"',
            });
            response.end(compressed);
        });
        const upstreamOrigin = await listen(upstream);
        const proxy = new QuartoPreviewProxy(upstreamOrigin, BRIDGE_ASSETS);
        try {
            const ready = await proxy.start();
            const response = await rawRequest(ready.url, {
                headers: { 'Accept-Encoding': 'br, gzip' },
            });
            expect(response.headers['content-encoding']).toBe('gzip');
            expect(response.headers['content-length']).toBe(String(compressed.length));
            expect(response.headers.etag).toBe('"opaque-compressed-etag"');
            expect(response.body).toEqual(compressed);
        } finally {
            await proxy.close();
            await closeServer(upstream);
        }
    });

    it('ignores absolute-form authority for upstream selection', async () => {
        let seenUrl = '';
        let seenHost = '';
        const upstream = http.createServer((request, response) => {
            seenUrl = request.url ?? '';
            seenHost = request.headers.host ?? '';
            response.end('fixed-upstream');
        });
        const upstreamOrigin = await listen(upstream);
        const proxy = new QuartoPreviewProxy(upstreamOrigin, BRIDGE_ASSETS);
        try {
            const ready = await proxy.start();
            const response = await rawSocketRequest(
                ready.origin,
                'GET http://example.invalid/foreign/path?q=1 HTTP/1.1\r\n' +
                `Host: ${new URL(ready.origin).host}\r\nConnection: close\r\n\r\n`,
            );
            expect(response.toString()).toContain('fixed-upstream');
            expect(seenUrl).toBe('/foreign/path?q=1');
            expect(seenHost).toBe(new URL(upstreamOrigin).host);
        } finally {
            await proxy.close();
            await closeServer(upstream);
        }
    });

    it('rejects a non-loopback Host header when enforcing (the default)', async () => {
        let upstreamHit = false;
        const upstream = http.createServer((_request, response) => {
            upstreamHit = true;
            response.end('fixed-upstream');
        });
        const upstreamOrigin = await listen(upstream);
        const proxy = new QuartoPreviewProxy(upstreamOrigin, BRIDGE_ASSETS);
        try {
            const ready = await proxy.start();
            const response = await rawSocketRequest(
                ready.origin,
                'GET /page/ HTTP/1.1\r\n' +
                'Host: attacker.invalid:9999\r\nConnection: close\r\n\r\n',
            );
            expect(response.toString()).toContain('403 Forbidden');
            expect(upstreamHit).toBe(false);
        } finally {
            await proxy.close();
            await closeServer(upstream);
        }
    });

    it('admits the browser-facing host from asExternalUri but still rejects others', async () => {
        let seenHost = '';
        const upstream = http.createServer((request, response) => {
            seenHost = request.headers.host ?? '';
            response.end('fixed-upstream');
        });
        const upstreamOrigin = await listen(upstream);
        const proxy = new QuartoPreviewProxy(upstreamOrigin, BRIDGE_ASSETS);
        try {
            const ready = await proxy.start();
            // Simulate a gateway mapping (e.g. Codespaces in the browser).
            proxy.setBrowserFacingOrigin('https://preview.example.dev/page/');

            const allowed = await rawSocketRequest(
                ready.origin,
                'GET /page/ HTTP/1.1\r\n' +
                'Host: preview.example.dev\r\nConnection: close\r\n\r\n',
            );
            expect(allowed.toString()).toContain('fixed-upstream');
            expect(seenHost).toBe(new URL(upstreamOrigin).host);

            const rejected = await rawSocketRequest(
                ready.origin,
                'GET /page/ HTTP/1.1\r\n' +
                'Host: attacker.invalid:9999\r\nConnection: close\r\n\r\n',
            );
            expect(rejected.toString()).toContain('403 Forbidden');
        } finally {
            await proxy.close();
            await closeServer(upstream);
        }
    });

    it('rejects a Host that hides a non-loopback label behind userinfo', async () => {
        let upstreamHit = false;
        const upstream = http.createServer((_request, response) => {
            upstreamHit = true;
            response.end('fixed-upstream');
        });
        const upstreamOrigin = await listen(upstream);
        const proxy = new QuartoPreviewProxy(upstreamOrigin, BRIDGE_ASSETS);
        try {
            const ready = await proxy.start();
            const response = await rawSocketRequest(
                ready.origin,
                'GET /page/ HTTP/1.1\r\n' +
                'Host: attacker.invalid@localhost\r\nConnection: close\r\n\r\n',
            );
            expect(response.toString()).toContain('403 Forbidden');
            expect(upstreamHit).toBe(false);
        } finally {
            await proxy.close();
            await closeServer(upstream);
        }
    });

    it('rejects an HTTP request carrying a cross-site Origin even on a loopback Host', async () => {
        let upstreamHit = false;
        const upstream = http.createServer((_request, response) => {
            upstreamHit = true;
            response.end('fixed-upstream');
        });
        const upstreamOrigin = await listen(upstream);
        const proxy = new QuartoPreviewProxy(upstreamOrigin, BRIDGE_ASSETS);
        try {
            const ready = await proxy.start();
            const response = await rawSocketRequest(
                ready.origin,
                'POST /submit HTTP/1.1\r\n' +
                `Host: ${new URL(ready.origin).host}\r\n` +
                'Origin: https://evil.example\r\n' +
                'Content-Length: 0\r\nConnection: close\r\n\r\n',
            );
            expect(response.toString()).toContain('403 Forbidden');
            expect(upstreamHit).toBe(false);
        } finally {
            await proxy.close();
            await closeServer(upstream);
        }
    });

    it('does not crash on a request target that is accepted but is not a valid URL', async () => {
        const upstream = http.createServer((_request, response) => response.end('ok'));
        const upstreamOrigin = await listen(upstream);
        const proxy = new QuartoPreviewProxy(upstreamOrigin, BRIDGE_ASSETS);
        try {
            const ready = await proxy.start();
            const host = new URL(ready.origin).host;
            const malformed = await rawSocketRequest(
                ready.origin,
                `GET //[ HTTP/1.1\r\nHost: ${host}\r\nConnection: close\r\n\r\n`,
            );
            expect(malformed.toString()).toContain('HTTP/1.1');
            // The proxy is still alive and serving after the odd request.
            const ok = await rawRequest(ready.url);
            expect(ok.statusCode).toBe(200);
        } finally {
            await proxy.close();
            await closeServer(upstream);
        }
    });

    it('rejects a WebSocket upgrade carrying a cross-site Origin when enforcing', async () => {
        // The proxy contacts the upstream only after the Host/Origin check
        // passes, so a rejected handshake never reaches the upstream's upgrade
        // handler; the client socket is closed rather than relayed.
        let upgradeReached = false;
        const upstream = http.createServer();
        upstream.on('upgrade', (_request, socket) => {
            upgradeReached = true;
            socket.destroy();
        });
        const upstreamOrigin = await listen(upstream);
        const proxy = new QuartoPreviewProxy(upstreamOrigin, BRIDGE_ASSETS);
        try {
            const ready = await proxy.start();
            const response = await rawSocketRequest(
                ready.origin,
                'GET /live-reload HTTP/1.1\r\n' +
                `Host: ${new URL(ready.origin).host}\r\n` +
                'Upgrade: websocket\r\nConnection: Upgrade\r\n' +
                'Origin: https://evil.example\r\n\r\n',
            );
            expect(upgradeReached).toBe(false);
            expect(response.toString()).not.toContain('101');
        } finally {
            await proxy.close();
            await closeServer(upstream);
        }
    });

    it('rejects a WebSocket Origin that hides a foreign label behind userinfo', async () => {
        let upgradeReached = false;
        const upstream = http.createServer();
        upstream.on('upgrade', (_request, socket) => {
            upgradeReached = true;
            socket.destroy();
        });
        const upstreamOrigin = await listen(upstream);
        const proxy = new QuartoPreviewProxy(upstreamOrigin, BRIDGE_ASSETS);
        try {
            const ready = await proxy.start();
            proxy.setBrowserFacingOrigin('https://preview.example.dev/page/');
            const response = await rawSocketRequest(
                ready.origin,
                'GET /live-reload HTTP/1.1\r\n' +
                'Host: preview.example.dev\r\n' +
                'Upgrade: websocket\r\nConnection: Upgrade\r\n' +
                'Origin: https://attacker.invalid@preview.example.dev\r\n\r\n',
            );
            expect(upgradeReached).toBe(false);
            expect(response.toString()).not.toContain('101');
        } finally {
            await proxy.close();
            await closeServer(upstream);
        }
    });

    it('injects before the first normal case-insensitive body close and drops stale entity headers', async () => {
        const upstream = http.createServer((_request, response) => {
            const body = '<html><body>first</BoDy><body>second</body></html>';
            response.writeHead(200, {
                'Content-Type': 'Text/HTML; Charset=UTF-8',
                'Content-Length': Buffer.byteLength(body),
                ETag: '"stale"',
                'Content-MD5': 'stale-md5',
                Digest: 'sha-256=stale',
            });
            response.end(body);
        });
        const upstreamOrigin = await listen(upstream);
        const proxy = new QuartoPreviewProxy(upstreamOrigin, BRIDGE_ASSETS);
        try {
            const ready = await proxy.start();
            const result = await rawRequest(ready.url);
            expect(result.body.toString()).toBe(
                `<html><body>first${INJECTION}</BoDy><body>second</body></html>`,
            );
            expect(result.headers['content-length']).toBeUndefined();
            expect(result.headers.etag).toBeUndefined();
            expect(result.headers['content-md5']).toBeUndefined();
            expect(result.headers.digest).toBeUndefined();
        } finally {
            await proxy.close();
            await closeServer(upstream);
        }
    });

    it('does not inject at a closing-body string inside an earlier inline script', async () => {
        const upstream = http.createServer((_request, response) => {
            response.writeHead(200, { 'Content-Type': 'text/html' });
            response.end(
                '<html><body><script>const marker = "</body>";</script>' +
                '<main>real body</main></body></html>',
            );
        });
        const upstreamOrigin = await listen(upstream);
        const proxy = new QuartoPreviewProxy(upstreamOrigin, BRIDGE_ASSETS);
        try {
            const result = await rawRequest((await proxy.start()).url);
            expect(result.body.toString()).toBe(
                '<html><body><script>const marker = "</body>";</script>' +
                `<main>real body</main>${INJECTION}</body></html>`,
            );
        } finally {
            await proxy.close();
            await closeServer(upstream);
        }
    });

    it('keeps scanning past a </script> that is script-data double-escaped', async () => {
        // The inner </script> sits in the script-data escaped span opened by
        // <!-- and closed by -->, so it does not end the element; the </body>
        // inside the string must not be treated as the document's body close.
        const upstream = http.createServer((_request, response) => {
            response.writeHead(200, { 'Content-Type': 'text/html' });
            response.end(
                '<html><body>' +
                '<script>var t = "<!--<script></script></body>-->";</script>' +
                '<main>real body</main></body></html>',
            );
        });
        const upstreamOrigin = await listen(upstream);
        const proxy = new QuartoPreviewProxy(upstreamOrigin, BRIDGE_ASSETS);
        try {
            const result = await rawRequest((await proxy.start()).url);
            expect(result.body.toString()).toBe(
                '<html><body>' +
                '<script>var t = "<!--<script></script></body>-->";</script>' +
                `<main>real body</main>${INJECTION}</body></html>`,
            );
        } finally {
            await proxy.close();
            await closeServer(upstream);
        }
    });

    it('never mis-injects inside script data with an unmatched <!-- (defers to EOF)', async () => {
        // Conservative fallback: an escaped span opened by <!-- with no closing
        // --> keeps the scanner in script data to end-of-stream, so the bridge
        // is appended (never injected inside the script string, which would
        // corrupt the JavaScript). The whole document body is preserved intact.
        const body =
            '<html><body>' +
            '<script>const marker = "<!--<script></script></body>";</script>' +
            '<main>real body</main></body></html>';
        const upstream = http.createServer((_request, response) => {
            response.writeHead(200, { 'Content-Type': 'text/html' });
            response.end(body);
        });
        const upstreamOrigin = await listen(upstream);
        const proxy = new QuartoPreviewProxy(upstreamOrigin, BRIDGE_ASSETS);
        try {
            const result = await rawRequest((await proxy.start()).url);
            expect(result.body.toString()).toBe(`${body}${INJECTION}`);
        } finally {
            await proxy.close();
            await closeServer(upstream);
        }
    });

    it('closes a script normally after a balanced legacy <!-- ... //--> comment', async () => {
        // The classic comment hack: <!-- ... //--> clears the escaped span, so
        // the following </script> closes the element and injection lands at the
        // real body close.
        const upstream = http.createServer((_request, response) => {
            response.writeHead(200, { 'Content-Type': 'text/html' });
            response.end(
                '<html><body><script><!--\nvar x = 1;\n//--></script>' +
                '<main>real body</main></body></html>',
            );
        });
        const upstreamOrigin = await listen(upstream);
        const proxy = new QuartoPreviewProxy(upstreamOrigin, BRIDGE_ASSETS);
        try {
            const result = await rawRequest((await proxy.start()).url);
            expect(result.body.toString()).toBe(
                '<html><body><script><!--\nvar x = 1;\n//--></script>' +
                `<main>real body</main>${INJECTION}</body></html>`,
            );
        } finally {
            await proxy.close();
            await closeServer(upstream);
        }
    });

    it('does not inject at a closing-body string inside an earlier textarea', async () => {
        const upstream = http.createServer((_request, response) => {
            response.writeHead(200, { 'Content-Type': 'text/html' });
            response.end(
                '<html><body><textarea name="source">literal </body> text</TEXTAREA>' +
                '<main>real body</main></body></html>',
            );
        });
        const upstreamOrigin = await listen(upstream);
        const proxy = new QuartoPreviewProxy(upstreamOrigin, BRIDGE_ASSETS);
        try {
            const result = await rawRequest((await proxy.start()).url);
            expect(result.body.toString()).toBe(
                '<html><body><textarea name="source">literal </body> text</TEXTAREA>' +
                `<main>real body</main>${INJECTION}</body></html>`,
            );
        } finally {
            await proxy.close();
            await closeServer(upstream);
        }
    });

    it('does not inject at a closing-body string inside an earlier title', async () => {
        const upstream = http.createServer((_request, response) => {
            response.writeHead(200, { 'Content-Type': 'text/html' });
            response.end(
                '<html><head><TiTlE>literal </body> text</title></head>' +
                '<body><main>real body</main></body></html>',
            );
        });
        const upstreamOrigin = await listen(upstream);
        const proxy = new QuartoPreviewProxy(upstreamOrigin, BRIDGE_ASSETS);
        try {
            const result = await rawRequest((await proxy.start()).url);
            expect(result.body.toString()).toBe(
                '<html><head><TiTlE>literal </body> text</title></head>' +
                `<body><main>real body</main>${INJECTION}</body></html>`,
            );
        } finally {
            await proxy.close();
            await closeServer(upstream);
        }
    });

    it('chooses the real body close before a trailing script containing body text', async () => {
        const upstream = http.createServer((_request, response) => {
            response.writeHead(200, { 'Content-Type': 'text/html' });
            response.end(
                '<html><body><!-- </body> --><style>.x::after{content:"</body>"}</style>' +
                '<main>content</main></body><script>const late = "</body>";</script></html>',
            );
        });
        const upstreamOrigin = await listen(upstream);
        const proxy = new QuartoPreviewProxy(upstreamOrigin, BRIDGE_ASSETS);
        try {
            const result = await rawRequest((await proxy.start()).url);
            expect(result.body.toString()).toBe(
                '<html><body><!-- </body> --><style>.x::after{content:"</body>"}</style>' +
                `<main>content</main>${INJECTION}</body>` +
                '<script>const late = "</body>";</script></html>',
            );
        } finally {
            await proxy.close();
            await closeServer(upstream);
        }
    });

    it('matches a body close split across upstream chunks', async () => {
        const upstream = http.createServer((_request, response) => {
            response.writeHead(200, { 'Content-Type': 'text/html' });
            response.write('<html><body>split</bo');
            setTimeout(() => response.end('dy></html>'), 5);
        });
        const upstreamOrigin = await listen(upstream);
        const proxy = new QuartoPreviewProxy(upstreamOrigin, BRIDGE_ASSETS);
        try {
            const result = await rawRequest((await proxy.start()).url);
            expect(result.body.toString()).toBe(
                `<html><body>split${INJECTION}</body></html>`,
            );
        } finally {
            await proxy.close();
            await closeServer(upstream);
        }
    });

    it('appends bridge tags when HTML has no body close', async () => {
        const upstream = http.createServer((_request, response) => {
            response.writeHead(200, { 'Content-Type': 'text/html' });
            response.end('<main>fragment</main>');
        });
        const upstreamOrigin = await listen(upstream);
        const proxy = new QuartoPreviewProxy(upstreamOrigin, BRIDGE_ASSETS);
        try {
            const result = await rawRequest((await proxy.start()).url);
            expect(result.body.toString()).toBe(`<main>fragment</main>${INJECTION}`);
        } finally {
            await proxy.close();
            await closeServer(upstream);
        }
    });

    it('injects only document fetch destinations or requests without the header', async () => {
        const body = '<html><body>destination</body></html>';
        const upstream = http.createServer((_request, response) => {
            response.writeHead(200, {
                'Content-Type': 'text/html',
                'Content-Length': Buffer.byteLength(body),
            });
            response.end(body);
        });
        const upstreamOrigin = await listen(upstream);
        const proxy = new QuartoPreviewProxy(upstreamOrigin, BRIDGE_ASSETS);
        try {
            const origin = (await proxy.start()).origin;
            const subresource = await rawRequest(`${origin}/subresource`, {
                headers: { 'Sec-Fetch-Dest': 'empty' },
            });
            const document = await rawRequest(`${origin}/document`, {
                headers: { 'Sec-Fetch-Dest': 'document' },
            });
            const fallback = await rawRequest(`${origin}/fallback`);

            expect(subresource.body.toString()).toBe(body);
            expect(subresource.headers['content-length']).toBe(String(Buffer.byteLength(body)));
            expect(document.body.toString()).toBe(
                `<html><body>destination${INJECTION}</body></html>`,
            );
            expect(fallback.body.toString()).toBe(
                `<html><body>destination${INJECTION}</body></html>`,
            );
        } finally {
            await proxy.close();
            await closeServer(upstream);
        }
    });

    it('streams a large HTML body before upstream completion without whole-body buffering', async () => {
        let releaseUpstream!: () => void;
        const upstreamReleased = new Promise<void>((resolve) => {
            releaseUpstream = resolve;
        });
        const firstChunk = '<html><body>' + 'x'.repeat(1024 * 1024);
        const upstream = http.createServer(async (_request, response) => {
            response.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' });
            response.write(firstChunk);
            await upstreamReleased;
            response.end('</body></html>');
        });
        const upstreamOrigin = await listen(upstream);
        const proxy = new QuartoPreviewProxy(upstreamOrigin, BRIDGE_ASSETS);
        try {
            const url = (await proxy.start()).url;
            let resolveFirst!: (chunk: Buffer) => void;
            const first = new Promise<Buffer>((resolve) => { resolveFirst = resolve; });
            const done = new Promise<Buffer>((resolve, reject) => {
                const request = http.request(url, { agent: false }, (response) => {
                    const chunks: Buffer[] = [];
                    response.once('data', (chunk: Buffer) => resolveFirst(Buffer.from(chunk)));
                    response.on('data', (chunk: Buffer) => chunks.push(Buffer.from(chunk)));
                    response.on('end', () => resolve(Buffer.concat(chunks)));
                    response.on('error', reject);
                });
                request.on('error', reject);
                request.end();
            });

            expect((await first).length).toBeGreaterThan(0);
            releaseUpstream();
            expect((await done).toString()).toBe(
                `${firstChunk}${INJECTION}</body></html>`,
            );
        } finally {
            releaseUpstream();
            await proxy.close();
            await closeServer(upstream);
        }
    });

    it('never injects non-HTML, partial, HEAD, 304, or encoded responses', async () => {
        const binary = Buffer.from([0, 255, 60, 47, 98, 111, 100, 121, 62, 1]);
        const encoded = gzipSync(Buffer.from('<html><body>gzip</body></html>'));
        const upstream = http.createServer((request, response) => {
            if (request.url === '/binary') {
                response.writeHead(200, {
                    'Content-Type': 'application/octet-stream',
                    'Content-Length': binary.length,
                    ETag: '"binary"',
                });
                response.end(binary);
            } else if (request.url === '/partial') {
                response.writeHead(206, {
                    'Content-Type': 'text/html',
                    'Content-Range': 'bytes 0-15/50',
                });
                response.end('<body>part</body>');
            } else if (request.url === '/not-modified') {
                response.writeHead(304, { 'Content-Type': 'text/html', ETag: '"same"' });
                response.end();
            } else if (request.url === '/encoded') {
                response.writeHead(200, {
                    'Content-Type': 'text/html',
                    'Content-Encoding': 'gzip',
                    'Content-Length': encoded.length,
                });
                response.end(encoded);
            } else if (request.url === '/utf16') {
                const utf16 = Buffer.from('<html><body>utf16</body></html>', 'utf16le');
                response.writeHead(200, {
                    'Content-Type': 'text/html; charset=utf-16',
                    'Content-Length': utf16.length,
                });
                response.end(utf16);
            } else {
                response.writeHead(200, {
                    'Content-Type': 'text/html',
                    'Content-Length': '24',
                });
                response.end('<body>head-only</body>');
            }
        });
        const upstreamOrigin = await listen(upstream);
        const proxy = new QuartoPreviewProxy(upstreamOrigin, BRIDGE_ASSETS);
        try {
            const origin = (await proxy.start()).origin;
            const binaryResult = await rawRequest(`${origin}/binary`);
            expect(binaryResult.body).toEqual(binary);
            expect(binaryResult.headers.etag).toBe('"binary"');
            expect((await rawRequest(`${origin}/partial`)).body.toString())
                .toBe('<body>part</body>');
            const notModified = await rawRequest(`${origin}/not-modified`);
            expect(notModified.statusCode).toBe(304);
            expect(notModified.body.length).toBe(0);
            expect(notModified.headers.etag).toBe('"same"');
            expect((await rawRequest(`${origin}/encoded`)).body).toEqual(encoded);
            const utf16 = await rawRequest(`${origin}/utf16`);
            expect(utf16.body).toEqual(
                Buffer.from('<html><body>utf16</body></html>', 'utf16le'),
            );
            expect(utf16.headers['content-length']).toBe(String(utf16.body.length));
            const head = await rawRequest(`${origin}/head`, { method: 'HEAD' });
            expect(head.body.length).toBe(0);
            expect(head.headers['content-length']).toBe('24');
        } finally {
            await proxy.close();
            await closeServer(upstream);
        }
    });

    it('streams non-HTML response chunks without waiting for upstream completion', async () => {
        let releaseUpstream!: () => void;
        const upstreamReleased = new Promise<void>((resolve) => {
            releaseUpstream = resolve;
        });
        const upstream = http.createServer(async (_request, response) => {
            response.writeHead(200, { 'Content-Type': 'application/octet-stream' });
            response.write('first');
            await upstreamReleased;
            response.end('second');
        });
        const upstreamOrigin = await listen(upstream);
        const proxy = new QuartoPreviewProxy(upstreamOrigin, BRIDGE_ASSETS);
        try {
            const url = (await proxy.start()).url;
            let resolveFirst!: (chunk: Buffer) => void;
            const first = new Promise<Buffer>((resolve) => { resolveFirst = resolve; });
            const done = new Promise<Buffer>((resolve, reject) => {
                const request = http.request(url, { agent: false }, (response) => {
                    const chunks: Buffer[] = [];
                    response.once('data', (chunk: Buffer) => resolveFirst(Buffer.from(chunk)));
                    response.on('data', (chunk: Buffer) => chunks.push(Buffer.from(chunk)));
                    response.on('end', () => resolve(Buffer.concat(chunks)));
                    response.on('error', reject);
                });
                request.on('error', reject);
                request.end();
            });

            expect((await first).toString()).toBe('first');
            releaseUpstream();
            expect((await done).toString()).toBe('firstsecond');
        } finally {
            releaseUpstream();
            await proxy.close();
            await closeServer(upstream);
        }
    });

    it('strips Accept-Encoding while preserving Range and If-Range', async () => {
        let seenEncoding: string | undefined;
        let seenRange: string | undefined;
        let seenIfRange: string | undefined;
        const upstream = http.createServer((request, response) => {
            seenEncoding = request.headers['accept-encoding'];
            seenRange = request.headers.range;
            seenIfRange = request.headers['if-range'];
            response.end('ok');
        });
        const upstreamOrigin = await listen(upstream);
        const proxy = new QuartoPreviewProxy(upstreamOrigin, BRIDGE_ASSETS);
        try {
            await rawRequest((await proxy.start()).url, {
                headers: {
                    'Accept-Encoding': 'br, gzip',
                    Range: 'bytes=10-20',
                    'If-Range': '"entity"',
                },
            });
            expect(seenEncoding).toBeUndefined();
            expect(seenRange).toBe('bytes=10-20');
            expect(seenIfRange).toBe('"entity"');
        } finally {
            await proxy.close();
            await closeServer(upstream);
        }
    });

    it('serves reserved bridge assets locally with the correct media types', async () => {
        let forwarded = 0;
        const upstream = http.createServer((_request, response) => {
            forwarded += 1;
            response.end('upstream');
        });
        const upstreamOrigin = await listen(upstream);
        const proxy = new QuartoPreviewProxy(upstreamOrigin, BRIDGE_ASSETS);
        try {
            const origin = (await proxy.start()).origin;
            const javascript = await rawRequest(
                `${origin}/_raven-theme-bridge/bridge.js?cache=1`,
            );
            const css = await rawRequest(`${origin}/_raven-theme-bridge/bridge.css`);
            expect(javascript.statusCode).toBe(200);
            expect(javascript.headers['content-type']).toStartWith('text/javascript');
            expect(javascript.headers['cache-control']).toBe('no-store');
            expect(javascript.body).toEqual(BRIDGE_JS);
            expect(css.headers['content-type']).toStartWith('text/css');
            expect(css.body).toEqual(BRIDGE_CSS);
            expect(forwarded).toBe(0);
        } finally {
            await proxy.close();
            await closeServer(upstream);
        }
    });

    it('rewrites loopback-equivalent absolute and protocol-relative redirects', async () => {
        let upstreamOrigin = '';
        const upstream = http.createServer((request, response) => {
            const upstream = new URL(upstreamOrigin);
            response.writeHead(302, {
                Location: request.url === '/absolute'
                    ? `${upstreamOrigin}/chapter/?x=1#section`
                    : request.url === '/localhost'
                        ? `http://localhost:${upstream.port}/x?y#z`
                    : request.url === '/protocol-relative'
                        ? `//localhost:${upstream.port}/appendix/?y=2#part`
                        : request.url === '/foreign'
                            ? 'http://example.com/x'
                            : '/chapter/?x=1',
            });
            response.end();
        });
        upstreamOrigin = await listen(upstream);
        const proxy = new QuartoPreviewProxy(upstreamOrigin, BRIDGE_ASSETS);
        try {
            const origin = (await proxy.start()).origin;
            expect((await rawRequest(`${origin}/absolute`)).headers.location)
                .toBe('/chapter/?x=1#section');
            expect((await rawRequest(`${origin}/localhost`)).headers.location)
                .toBe('/x?y#z');
            expect((await rawRequest(`${origin}/protocol-relative`)).headers.location)
                .toBe('/appendix/?y=2#part');
            expect((await rawRequest(`${origin}/foreign`)).headers.location)
                .toBe('http://example.com/x');
            expect((await rawRequest(`${origin}/relative`)).headers.location)
                .toBe('/chapter/?x=1');
        } finally {
            await proxy.close();
            await closeServer(upstream);
        }
    });

    it('strips standard and Connection-named hop-by-hop request headers', async () => {
        let seen: http.IncomingHttpHeaders = {};
        const upstream = http.createServer((request, response) => {
            seen = request.headers;
            response.end('ok');
        });
        const upstreamOrigin = await listen(upstream);
        const proxy = new QuartoPreviewProxy(upstreamOrigin, BRIDGE_ASSETS);
        try {
            const origin = (await proxy.start()).origin;
            await rawRequest(`${origin}/headers`, {
                headers: {
                    Connection: 'X-Remove, keep-alive',
                    'X-Remove': 'secret',
                    'Keep-Alive': 'timeout=10',
                    'Proxy-Authenticate': 'Basic',
                    'Proxy-Authorization': 'Basic secret',
                    'X-End-To-End': 'kept',
                },
            });
            expect(seen.connection).toBe('close');
            expect(seen['x-remove']).toBeUndefined();
            expect(seen['keep-alive']).toBeUndefined();
            expect(seen.te).toBeUndefined();
            expect(seen.trailer).toBeUndefined();
            expect(seen.upgrade).toBeUndefined();
            expect(seen['proxy-authenticate']).toBeUndefined();
            expect(seen['proxy-authorization']).toBeUndefined();
            expect(seen['x-end-to-end']).toBe('kept');
            expect(filteredRequestHeaders({
                connection: 'X-Token',
                'x-token': 'secret',
                'keep-alive': 'timeout=10',
                te: 'trailers',
                trailer: 'X-Later',
                upgrade: 'h2c',
                'proxy-authenticate': 'Basic',
                'proxy-authorization': 'Basic secret',
                'x-end-to-end': 'kept',
            })).toEqual({ 'x-end-to-end': 'kept' });
        } finally {
            await proxy.close();
            await closeServer(upstream);
        }
    });

    it('forwards requests to a bracketed IPv6 loopback upstream', async () => {
        let seenUrl = '';
        const upstream = http.createServer((request, response) => {
            seenUrl = request.url ?? '';
            response.end('ipv6-ok');
        });
        const upstreamOrigin = await listen(upstream, '::1');
        const proxy = new QuartoPreviewProxy(upstreamOrigin, BRIDGE_ASSETS);
        try {
            const result = await rawRequest(`${(await proxy.start()).origin}/ipv6`);
            expect(result.statusCode).toBe(200);
            expect(seenUrl).toBe('/ipv6');
            expect(result.body.toString()).toBe('ipv6-ok');
        } finally {
            await proxy.close();
            await closeServer(upstream);
        }
    });

    it('keeps proxy framing available when bridge assets are absent', async () => {
        const upstream = http.createServer((_request, response) => {
            response.writeHead(200, { 'Content-Type': 'text/html' });
            response.end('<html><body>unbridged</body></html>');
        });
        const upstreamOrigin = await listen(upstream);
        const proxy = new QuartoPreviewProxy(upstreamOrigin);
        try {
            const result = await rawRequest((await proxy.start()).url);
            expect(result.statusCode).toBe(200);
            expect(result.body.toString()).toBe('<html><body>unbridged</body></html>');
        } finally {
            await proxy.close();
            await closeServer(upstream);
        }
    });

    it('strips standard and Connection-named hop-by-hop response headers', async () => {
        const upstream = http.createServer((_request, response) => {
            response.writeHead(200, {
                Connection: 'X-Remove, keep-alive',
                'Keep-Alive': 'timeout=10',
                'Proxy-Authenticate': 'Basic',
                Upgrade: 'h2c',
                'X-Remove': 'secret',
                'X-End-To-End': 'kept',
            });
            response.end('plain');
        });
        const upstreamOrigin = await listen(upstream);
        const proxy = new QuartoPreviewProxy(upstreamOrigin, BRIDGE_ASSETS);
        try {
            const result = await rawRequest((await proxy.start()).url);
            expect(result.headers.connection).toBe('close');
            expect(result.headers['keep-alive']).toBeUndefined();
            expect(result.headers['proxy-authenticate']).toBeUndefined();
            expect(result.headers.upgrade).toBeUndefined();
            expect(result.headers['x-remove']).toBeUndefined();
            expect(result.headers['x-end-to-end']).toBe('kept');
        } finally {
            await proxy.close();
            await closeServer(upstream);
        }
    });
});

describe('QuartoPreviewProxy WebSocket and teardown', () => {
    it('forwards a WebSocket upgrade plus client and upstream head bytes', async () => {
        expect(await runNodeWebSocketScenario('happy')).toBe('ok');
    });

    it('closes the client promptly when upstream refuses the upgrade', async () => {
        expect(await runNodeWebSocketScenario('broken')).toBe('ok');
    });

    it('destroys open HTTP and WebSocket sockets and closes its listener promptly', async () => {
        expect(await runNodeWebSocketScenario('teardown')).toBe('ok');
    });
});

function connect(target: URL): Promise<net.Socket> {
    return new Promise((resolve, reject) => {
        const socket = net.createConnection({
            host: target.hostname,
            port: Number(target.port),
        });
        socket.once('connect', () => resolve(socket));
        socket.once('error', reject);
    });
}

function rawSocketRequest(origin: string, request: string): Promise<Buffer> {
    return new Promise(async (resolve, reject) => {
        try {
            const socket = await connect(new URL(origin));
            const chunks: Buffer[] = [];
            socket.on('data', (chunk) => chunks.push(chunk));
            socket.on('end', () => resolve(Buffer.concat(chunks)));
            socket.on('error', reject);
            socket.write(request);
        } catch (error) {
            reject(error);
        }
    });
}

function rawRequest(
    url: string,
    options: {
        method?: string;
        headers?: http.OutgoingHttpHeaders;
        body?: Buffer;
    } = {},
): Promise<RawResponse> {
    return new Promise((resolve, reject) => {
        const request = http.request(url, {
            method: options.method ?? 'GET',
            headers: options.headers,
            agent: false,
        }, (response) => {
            const chunks: Buffer[] = [];
            response.on('data', (chunk) => chunks.push(chunk));
            response.on('end', () => resolve({
                statusCode: response.statusCode ?? 0,
                headers: response.headers,
                body: Buffer.concat(chunks),
            }));
        });
        request.on('error', reject);
        request.end(options.body);
    });
}

function readBody(request: http.IncomingMessage): Promise<Buffer> {
    return new Promise((resolve, reject) => {
        const chunks: Buffer[] = [];
        request.on('data', (chunk) => chunks.push(chunk));
        request.on('end', () => resolve(Buffer.concat(chunks)));
        request.on('error', reject);
    });
}

function listen(server: http.Server, host: string = '127.0.0.1'): Promise<string> {
    return new Promise((resolve, reject) => {
        server.once('error', reject);
        server.listen(0, host, () => {
            server.off('error', reject);
            const address = server.address();
            if (!address || typeof address === 'string') {
                reject(new Error('server did not expose a TCP address'));
                return;
            }
            const authority = host.includes(':') ? `[${host}]` : host;
            resolve(`http://${authority}:${address.port}`);
        });
    });
}

function closeServer(server: http.Server): Promise<void> {
    return new Promise((resolve) => server.close(() => resolve()));
}

let nodeBundle: Promise<string> | null = null;
let nodeBundleDir: string | null = null;

async function runNodeWebSocketScenario(
    scenario: 'happy' | 'broken' | 'teardown',
): Promise<string> {
    const bundle = await (nodeBundle ??= buildNodeProxyBundle());
    return new Promise((resolve, reject) => {
        const helper = join(import.meta.dir, '_helpers', 'quarto-preview-proxy-node.cjs');
        const child = spawn('node', [helper, bundle, scenario], {
            stdio: ['ignore', 'pipe', 'pipe'],
        });
        let stdout = '';
        let stderr = '';
        child.stdout.setEncoding('utf8');
        child.stdout.on('data', (chunk) => { stdout += chunk; });
        child.stderr.setEncoding('utf8');
        child.stderr.on('data', (chunk) => { stderr += chunk; });
        child.on('error', reject);
        child.on('close', (code) => {
            if (code === 0) resolve(stdout.trim() || 'ok');
            else reject(new Error(stderr.trim() || `Node helper exited with ${String(code)}`));
        });
    });
}

async function buildNodeProxyBundle(): Promise<string> {
    nodeBundleDir = await mkdtemp(join(tmpdir(), 'raven-quarto-proxy-'));
    const result = await Bun.build({
        entrypoints: [join(
            import.meta.dir,
            '..',
            '..',
            'editors',
            'vscode',
            'src',
            'quarto',
            'quarto-preview-proxy.ts',
        )],
        outdir: nodeBundleDir,
        target: 'node',
        format: 'cjs',
    });
    if (!result.success || result.outputs.length !== 1) {
        throw new Error('Could not bundle the proxy for Node WebSocket tests.');
    }
    return result.outputs[0].path;
}

afterAll(async () => {
    if (nodeBundleDir) await rm(nodeBundleDir, { recursive: true, force: true });
});
