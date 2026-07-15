'use strict';

const assert = require('assert/strict');
const http = require('http');
const net = require('net');

const bundlePath = process.argv[2];
const scenario = process.argv[3];
const { QuartoPreviewProxy } = require(bundlePath);
const bridgeAssets = { javascript: '', css: '' };

void main().then(() => {
    process.stdout.write('ok\n');
}, (error) => {
    process.stderr.write(`${error?.stack ?? String(error)}\n`);
    process.exitCode = 1;
});

async function main() {
    if (scenario === 'happy') return happyPath();
    if (scenario === 'broken') return brokenUpstream();
    if (scenario === 'teardown') return teardownSockets();
    throw new Error(`Unknown proxy test scenario: ${scenario}`);
}

async function happyPath() {
    let seenOrigin;
    const upstream = websocketEchoServer((request) => {
        seenOrigin = request.headers.origin;
    });
    const upstreamOrigin = await listen(upstream);
    const proxy = new QuartoPreviewProxy(upstreamOrigin, bridgeAssets);
    let client;
    try {
        const ready = await proxy.start();
        const opened = await openWebSocket(ready.origin, 'client-head');
        client = opened.socket;
        assert.match(opened.bytes, /101 Switching Protocols/);
        assert.match(opened.bytes, /upstream-head/);
        assert.match(opened.bytes, /client-head/);
        assert.equal(seenOrigin, upstreamOrigin);
        const echoed = waitForData(client, 'later-payload');
        client.write('later-payload');
        await echoed;
    } finally {
        client?.destroy();
        await proxy.close();
        await closeServer(upstream);
    }
}

async function brokenUpstream() {
    const upstream = http.createServer();
    upstream.on('upgrade', (_request, socket) => {
        socket.end(
            'HTTP/1.1 426 Upgrade Required\r\n' +
            'Connection: close\r\nContent-Length: 0\r\n\r\n',
        );
    });
    const upstreamOrigin = await listen(upstream);
    const proxy = new QuartoPreviewProxy(upstreamOrigin, bridgeAssets);
    try {
        const ready = await proxy.start();
        const target = new URL(ready.origin);
        const socket = await connect(target);
        const closed = waitForClose(socket);
        socket.write(handshake(target));
        await within(closed, 500, 'client stayed open after refused upgrade');
        assert.equal(socket.destroyed, true);
    } finally {
        await proxy.close();
        await closeServer(upstream);
    }
}

async function teardownSockets() {
    const upstream = websocketEchoServer();
    const upstreamOrigin = await listen(upstream);
    const proxy = new QuartoPreviewProxy(upstreamOrigin, bridgeAssets);
    const ready = await proxy.start();
    const target = new URL(ready.origin);
    const idleHttp = await connect(target);
    idleHttp.write('GET / HTTP/1.1\r\nHost: preview\r\n');
    const websocket = (await openWebSocket(ready.origin)).socket;
    const httpClosed = waitForClose(idleHttp);
    const websocketClosed = waitForClose(websocket);

    await within(proxy.close(), 500, 'proxy close did not settle');
    await within(Promise.all([httpClosed, websocketClosed]), 500, 'sockets stayed open');
    await assert.rejects(connect(target));
    await closeServer(upstream);
}

function websocketEchoServer(onUpgrade) {
    const server = http.createServer();
    server.on('upgrade', (request, socket, head) => {
        onUpgrade?.(request);
        socket.write(
            'HTTP/1.1 101 Switching Protocols\r\n' +
            'Upgrade: websocket\r\n' +
            'Connection: Upgrade\r\n' +
            'Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n' +
            'upstream-head',
        );
        if (head.length > 0) socket.write(head);
        socket.on('data', (chunk) => socket.write(chunk));
        socket.on('error', () => undefined);
    });
    return server;
}

async function openWebSocket(origin, clientHead = '') {
    const target = new URL(origin);
    const socket = await connect(target);
    const expected = clientHead === '' ? 'upstream-head' : clientHead;
    const received = waitForData(socket, expected);
    socket.write(handshake(target) + clientHead);
    return { socket, bytes: (await within(received, 500, 'upgrade did not complete')).toString() };
}

function handshake(target) {
    return 'GET /reload HTTP/1.1\r\n' +
        `Host: ${target.host}\r\n` +
        `Origin: ${target.origin}\r\n` +
        'Connection: Upgrade\r\n' +
        'Upgrade: websocket\r\n' +
        'Sec-WebSocket-Version: 13\r\n' +
        'Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n';
}

function waitForData(socket, expected) {
    return new Promise((resolve, reject) => {
        let bytes = Buffer.alloc(0);
        const onData = (chunk) => {
            bytes = Buffer.concat([bytes, chunk]);
            if (bytes.includes(Buffer.from(expected))) {
                cleanup();
                resolve(bytes);
            }
        };
        const onError = (error) => {
            cleanup();
            reject(error);
        };
        const cleanup = () => {
            socket.off('data', onData);
            socket.off('error', onError);
        };
        socket.on('data', onData);
        socket.on('error', onError);
    });
}

function waitForClose(socket) {
    if (socket.destroyed) return Promise.resolve();
    return new Promise((resolve) => socket.once('close', resolve));
}

function connect(target) {
    return new Promise((resolve, reject) => {
        const socket = net.createConnection({
            host: target.hostname,
            port: Number(target.port),
        });
        socket.once('connect', () => resolve(socket));
        socket.once('error', reject);
    });
}

function listen(server) {
    return new Promise((resolve, reject) => {
        server.once('error', reject);
        server.listen(0, '127.0.0.1', () => {
            server.off('error', reject);
            const address = server.address();
            resolve(`http://127.0.0.1:${address.port}`);
        });
    });
}

function closeServer(server) {
    return new Promise((resolve) => server.close(resolve));
}

async function within(promise, ms, message) {
    let timer;
    try {
        return await Promise.race([
            promise,
            new Promise((_resolve, reject) => {
                timer = setTimeout(() => reject(new Error(message)), ms);
            }),
        ]);
    } finally {
        clearTimeout(timer);
    }
}
