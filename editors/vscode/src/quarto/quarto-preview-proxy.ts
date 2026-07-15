/**
 * Response-transforming loopback reverse proxy for Quarto live preview.
 *
 * The listener binds only to IPv4 loopback and every outbound request targets
 * the one validated loopback origin supplied at construction. Absolute-form
 * request targets and client Host headers can therefore never turn this into
 * an open proxy. Two reserved same-origin paths serve packaged theme-bridge
 * assets locally when those packaged assets are available. Full,
 * identity-encoded, ASCII-compatible text/html 200 responses stream through a
 * context-aware scanner and receive the bridge tags before their first real
 * closing-body tag; every other body, including
 * compressed, partial, HEAD, and binary responses, streams byte-for-byte.
 * Entity validators are removed only when a body is changed. Hop-by-hop
 * request and response headers are stripped and keep-alive is disabled on
 * both legs; WebSocket Upgrade semantics remain intact.
 *
 * HTTP and upgraded sockets on both sides are tracked explicitly. Closing the
 * proxy first stops acceptance and then destroys every tracked socket, so an
 * idle HTTP connection or Quarto live-reload WebSocket cannot hold teardown
 * open after the preview generation has been stopped.
 */

import * as http from 'http';
import * as net from 'net';
import { Transform, type TransformCallback } from 'stream';
import { validatePreviewUrl } from './preview-url-parser';

const BRIDGE_JS_PATH = '/_raven-theme-bridge/bridge.js';
const BRIDGE_CSS_PATH = '/_raven-theme-bridge/bridge.css';
// Bridge assets use root-relative external references to remain compatible with
// CSP 'self'. An author-supplied absolute <base href> can re-root them, an
// accepted limitation because standard Quarto/pandoc templates emit no <base>.
const HTML_INJECTION = Buffer.from(
    '<link rel="stylesheet" href="/_raven-theme-bridge/bridge.css">' +
    '<script src="/_raven-theme-bridge/bridge.js"></script>',
);

export interface QuartoPreviewBridgeAssets {
    javascript: string | Uint8Array;
    css: string | Uint8Array;
}

export interface QuartoPreviewProxyReady {
    origin: string;
    url: string;
}

export interface QuartoPreviewProxyLike {
    start(): Promise<QuartoPreviewProxyReady>;
    close(): Promise<void>;
    /**
     * Record the browser-facing URL that `vscode.env.asExternalUri` mapped this
     * proxy's origin to, so its host/origin joins the request allowlist. See
     * {@link QuartoPreviewProxy.setBrowserFacingOrigin}.
     */
    setBrowserFacingOrigin(externalUrl: string): void;
}

export class QuartoPreviewProxy implements QuartoPreviewProxyLike {
    private readonly upstream: URL;
    private readonly upstreamRequestHostname: string;
    private readonly server: http.Server;
    private readonly sockets = new Set<net.Socket>();
    private startPromise: Promise<QuartoPreviewProxyReady> | null = null;
    private closePromise: Promise<void> | null = null;
    private ready: QuartoPreviewProxyReady | null = null;
    private closing = false;

    private readonly bridgeJavaScript: Buffer | null;
    private readonly bridgeCss: Buffer | null;
    // The host/origin that `asExternalUri` maps this proxy to in the browser,
    // learned after start via setBrowserFacingOrigin. null until then, so the
    // request allowlist is loopback-only during startup (only the loopback
    // readiness probe runs before the webview — and thus this call — loads).
    private browserFacingHostname: string | null = null;
    private browserFacingOrigin: string | null = null;

    constructor(
        upstreamOrigin: string,
        bridgeAssets?: QuartoPreviewBridgeAssets,
    ) {
        const validated = validatePreviewUrl(`${upstreamOrigin.replace(/\/$/, '')}/`);
        if (!validated || validated.origin !== upstreamOrigin.replace(/\/$/, '')) {
            throw new Error('Quarto preview proxy requires a validated loopback origin.');
        }
        this.upstream = new URL(validated.origin);
        if (this.upstream.hostname === 'localhost') {
            this.upstream.hostname = '127.0.0.1';
        }
        this.upstreamRequestHostname = unbracketIpv6Hostname(this.upstream.hostname);
        this.bridgeJavaScript = bridgeAssets
            ? Buffer.from(bridgeAssets.javascript)
            : null;
        this.bridgeCss = bridgeAssets ? Buffer.from(bridgeAssets.css) : null;

        this.server = http.createServer(
            (request, response) => this.forwardHttp(request, response),
        );
        this.server.on('connection', (socket) => this.trackSocket(socket));
        this.server.on('upgrade', (request, socket, head) => {
            this.forwardWebSocket(request, socket as net.Socket, head);
        });
        this.server.on('connect', (_request, socket) => {
            rejectSocket(socket as net.Socket, 405, 'CONNECT is not supported');
        });
        this.server.on('clientError', (_error, socket) => {
            if (!socket.destroyed) {
                rejectSocket(socket as net.Socket, 400, 'Bad Request');
            }
        });
        // Bind errors are also observed by start(); retaining a passive error
        // listener prevents a later server-level error from becoming an
        // uncaught EventEmitter exception after startup has settled.
        this.server.on('error', () => undefined);
    }

    start(): Promise<QuartoPreviewProxyReady> {
        if (this.startPromise) return this.startPromise;
        this.startPromise = new Promise<QuartoPreviewProxyReady>((resolve, reject) => {
            if (this.closing) {
                reject(new Error('Quarto preview proxy was closed before it started.'));
                return;
            }

            const onError = (error: Error): void => {
                this.server.off('listening', onListening);
                reject(error);
            };
            const onListening = (): void => {
                this.server.off('error', onError);
                if (this.closing) {
                    reject(new Error('Quarto preview proxy was closed during startup.'));
                    return;
                }
                const address = this.server.address();
                if (!address || typeof address === 'string') {
                    reject(new Error('Quarto preview proxy did not expose a TCP address.'));
                    return;
                }
                const origin = `http://127.0.0.1:${address.port}`;
                this.ready = { origin, url: `${origin}/` };
                resolve(this.ready);
            };
            this.server.once('error', onError);
            this.server.once('listening', onListening);
            this.server.listen({ host: '127.0.0.1', port: 0 });
        });
        return this.startPromise;
    }

    close(): Promise<void> {
        if (this.closePromise) return this.closePromise;
        this.closing = true;
        for (const socket of this.sockets) socket.destroy();

        this.closePromise = new Promise<void>((resolve) => {
            try {
                this.server.close(() => resolve());
            } catch {
                resolve();
            }
        });
        return this.closePromise;
    }

    /**
     * Record the browser-facing URL that `vscode.env.asExternalUri` mapped this
     * proxy to, adding its host/origin to the request allowlist.
     *
     * Every request that a legitimate browser sends carries the `Host` (and,
     * for WebSocket handshakes, the `Origin`) of the URL the iframe was loaded
     * from — the mapped external URL, not the raw loopback origin. Allowing that
     * host/origin alongside loopback lets the proxy always enforce the allowlist
     * (blocking DNS-rebinding and cross-site WebSocket reads in every topology)
     * without guessing whether it is reached over loopback or an authenticated
     * gateway: a rebound attacker domain matches neither and is rejected.
     * A non-URL value leaves the proxy loopback-only.
     */
    setBrowserFacingOrigin(externalUrl: string): void {
        try {
            const url = new URL(externalUrl);
            this.browserFacingHostname = normalizeHostname(url.hostname);
            this.browserFacingOrigin = url.origin;
        } catch {
            // Leave the allowlist loopback-only.
        }
    }

    private isAllowedHost(host: string | undefined): boolean {
        if (isLoopbackHostHeader(host)) return true;
        if (this.browserFacingHostname === null ||
            host === undefined || host.includes('@')) {
            return false;
        }
        try {
            return normalizeHostname(new URL(`http://${host}`).hostname) ===
                this.browserFacingHostname;
        } catch {
            return false;
        }
    }

    /**
     * Whether a request's `Origin` is safe to relay to the loopback upstream.
     * A browser sends `Origin` on every cross-origin fetch/WebSocket, so an
     * absent `Origin` is a non-browser client, not the hijacking vector. A
     * present `Origin` must be **exactly** this proxy's own loopback origin or
     * the browser-facing origin it was mapped to — NOT merely some loopback
     * host: a page served from a different `localhost`/`127.0.0.1` port is a
     * distinct origin and must not be able to open the proxied WebSocket
     * (whose hostile `Origin` would otherwise be rewritten to the trusted
     * upstream). Userinfo (`@`) is rejected because `.origin` discards it.
     */
    private isAllowedOrigin(origin: string | undefined): boolean {
        if (origin === undefined) return true;
        if (origin.includes('@')) return false;
        let parsed: string;
        try {
            parsed = new URL(origin).origin;
        } catch {
            return false;
        }
        return parsed === this.ready?.origin ||
            (this.browserFacingOrigin !== null && parsed === this.browserFacingOrigin);
    }

    private forwardHttp(
        request: http.IncomingMessage,
        response: http.ServerResponse,
    ): void {
        if (request.method?.toUpperCase() === 'CONNECT') {
            response.writeHead(405, { Connection: 'close' });
            response.end('CONNECT is not supported');
            return;
        }
        if (!this.isAllowedHost(request.headers.host) ||
            !this.isAllowedOrigin(request.headers.origin)) {
            response.writeHead(403, { Connection: 'close' });
            response.end('Forbidden');
            return;
        }

        let path: string;
        try {
            path = fixedUpstreamPath(request.url ?? '/');
        } catch {
            response.writeHead(400, { Connection: 'close' });
            response.end('Bad Request');
            return;
        }

        const reservedPath = this.bridgeJavaScript && this.bridgeCss
            ? reservedBridgePath(path)
            : null;
        if (reservedPath) {
            this.serveBridgeAsset(request, response, reservedPath);
            return;
        }

        const headers: http.OutgoingHttpHeaders = {
            ...filteredRequestHeaders(request.headers),
            host: this.upstream.host,
            connection: 'close',
        };
        if (request.headers.origin !== undefined) {
            headers.origin = this.upstream.origin;
        }
        delete headers['accept-encoding'];
        const upstreamRequest = http.request({
            protocol: 'http:',
            hostname: this.upstreamRequestHostname,
            port: this.upstream.port,
            method: request.method,
            path,
            headers,
            agent: false,
        });
        upstreamRequest.on('socket', (socket) => this.trackSocket(socket));
        upstreamRequest.on('response', (upstreamResponse) => {
            const statusCode = upstreamResponse.statusCode ?? 502;
            const inject = this.bridgeJavaScript !== null &&
                this.bridgeCss !== null &&
                shouldInject(request, upstreamResponse);
            const responseHeaders = filteredResponseHeaders(
                upstreamResponse.rawHeaders,
                {
                    inject,
                    statusCode,
                    upstream: this.upstream,
                },
            );
            response.writeHead(
                statusCode,
                upstreamResponse.statusMessage,
                responseHeaders,
            );
            if (!inject) {
                upstreamResponse.on('aborted', () => response.destroy());
                upstreamResponse.on('error', (error) => response.destroy(error));
                upstreamResponse.pipe(response);
                return;
            }

            const transform = new HtmlBridgeInjectionTransform(HTML_INJECTION);
            upstreamResponse.on('aborted', () => {
                transform.destroy();
                response.destroy();
            });
            upstreamResponse.on('error', (error) => transform.destroy(error));
            transform.on('error', (error) => response.destroy(error));
            upstreamResponse.pipe(transform).pipe(response);
        });
        upstreamRequest.on('error', (error) => {
            if (response.headersSent) {
                response.destroy(error);
                return;
            }
            response.writeHead(502, { Connection: 'close' });
            response.end('Bad Gateway');
        });
        request.on('aborted', () => upstreamRequest.destroy());
        response.on('close', () => {
            if (!response.writableFinished) upstreamRequest.destroy();
        });
        request.pipe(upstreamRequest);
    }

    private serveBridgeAsset(
        request: http.IncomingMessage,
        response: http.ServerResponse,
        path: typeof BRIDGE_JS_PATH | typeof BRIDGE_CSS_PATH,
    ): void {
        const method = request.method?.toUpperCase() ?? 'GET';
        if (method !== 'GET' && method !== 'HEAD') {
            response.writeHead(405, {
                Allow: 'GET, HEAD',
                Connection: 'close',
                'Content-Length': '0',
            });
            response.end();
            return;
        }

        const body = path === BRIDGE_JS_PATH ? this.bridgeJavaScript : this.bridgeCss;
        if (body === null) {
            response.writeHead(404, { Connection: 'close', 'Content-Length': '0' });
            response.end();
            return;
        }
        response.writeHead(200, {
            'Cache-Control': 'no-store',
            Connection: 'close',
            'Content-Length': String(body.length),
            'Content-Type': path === BRIDGE_JS_PATH
                ? 'text/javascript; charset=utf-8'
                : 'text/css; charset=utf-8',
        });
        response.end(method === 'HEAD' ? undefined : body);
    }

    private forwardWebSocket(
        request: http.IncomingMessage,
        clientSocket: net.Socket,
        clientHead: Buffer,
    ): void {
        if (request.headers.upgrade?.toLowerCase() !== 'websocket') {
            rejectSocket(clientSocket, 400, 'WebSocket upgrade required');
            return;
        }
        if (!this.isAllowedHost(request.headers.host) ||
            !this.isAllowedOrigin(request.headers.origin)) {
            rejectSocket(clientSocket, 403, 'Forbidden');
            return;
        }

        let path: string;
        try {
            path = fixedUpstreamPath(request.url ?? '/');
        } catch {
            rejectSocket(clientSocket, 400, 'Bad Request');
            return;
        }
        if (reservedBridgePath(path)) {
            rejectSocket(clientSocket, 400, 'WebSocket is not supported for bridge assets');
            return;
        }

        this.trackSocket(clientSocket);
        const headers: http.OutgoingHttpHeaders = {
            ...filteredRequestHeaders(request.headers),
            host: this.upstream.host,
            connection: 'Upgrade',
            upgrade: 'websocket',
        };
        if (request.headers.origin !== undefined) {
            headers.origin = this.upstream.origin;
        }
        delete headers['accept-encoding'];
        const upstreamRequest = http.request({
            protocol: 'http:',
            hostname: this.upstreamRequestHostname,
            port: this.upstream.port,
            method: request.method,
            path,
            headers,
            agent: false,
        });
        upstreamRequest.on('socket', (socket) => this.trackSocket(socket));
        upstreamRequest.on('upgrade', (upstreamResponse, upstreamSocket, upstreamHead) => {
            if (upstreamResponse.statusCode !== 101) {
                upstreamSocket.destroy();
                clientSocket.destroy();
                return;
            }
            this.trackSocket(upstreamSocket);
            const statusLine = `HTTP/${upstreamResponse.httpVersion} 101 ` +
                `${upstreamResponse.statusMessage ?? 'Switching Protocols'}\r\n`;
            clientSocket.write(
                statusLine + rawHeadersText(upstreamResponse.rawHeaders) + '\r\n',
            );
            if (upstreamHead.length > 0) clientSocket.write(upstreamHead);
            if (clientHead.length > 0) upstreamSocket.write(clientHead);
            pipeSockets(clientSocket, upstreamSocket);
        });
        upstreamRequest.on('response', (upstreamResponse) => {
            upstreamResponse.resume();
            clientSocket.destroy();
        });
        upstreamRequest.on('error', () => clientSocket.destroy());
        clientSocket.on('error', () => upstreamRequest.destroy());
        clientSocket.on('close', () => upstreamRequest.destroy());
        upstreamRequest.end();
    }

    private trackSocket(socket: net.Socket): void {
        this.sockets.add(socket);
        socket.once('close', () => this.sockets.delete(socket));
        if (this.closing) socket.destroy();
    }
}

function fixedUpstreamPath(rawTarget: string): string {
    if (rawTarget === '*') return rawTarget;
    if (/^https?:\/\//i.test(rawTarget)) {
        const parsed = new URL(rawTarget);
        return `${parsed.pathname}${parsed.search}`;
    }
    if (!rawTarget.startsWith('/')) throw new Error('Invalid HTTP request target.');
    return rawTarget;
}

function reservedBridgePath(
    path: string,
): typeof BRIDGE_JS_PATH | typeof BRIDGE_CSS_PATH | null {
    // This accepted, astronomically unlikely route collision keeps packaged
    // assets local; authority-based forwarding does not affect path matching.
    if (path === '*') return null;
    // A request target such as `//[` is accepted as origin-form by `fixedUpstreamPath`
    // but is not a valid URL; treat any unparseable target as a non-reserved path
    // (forwarded upstream) rather than letting the throw escape the request handler.
    let pathname: string;
    try {
        pathname = new URL(path, 'http://proxy.invalid').pathname;
    } catch {
        return null;
    }
    if (pathname === BRIDGE_JS_PATH || pathname === BRIDGE_CSS_PATH) return pathname;
    return null;
}

function unbracketIpv6Hostname(hostname: string): string {
    return hostname.startsWith('[') && hostname.endsWith(']')
        ? hostname.slice(1, -1)
        : hostname;
}

// RFC 7230 §6.1 hop-by-hop headers. The request and response filters differ in
// shape (header object vs. rawHeaders array) but strip the same token set.
const HOP_BY_HOP_HEADERS = new Set([
    'connection',
    'keep-alive',
    'proxy-authenticate',
    'proxy-authorization',
    'te',
    'trailer',
    'transfer-encoding',
    'upgrade',
]);

export function filteredRequestHeaders(
    incoming: http.IncomingHttpHeaders,
): http.OutgoingHttpHeaders {
    const stripped = new Set(HOP_BY_HOP_HEADERS);
    for (const token of incoming.connection?.split(',') ?? []) {
        const normalized = token.trim().toLowerCase();
        if (normalized !== '') stripped.add(normalized);
    }

    const headers: http.OutgoingHttpHeaders = {};
    for (const [name, value] of Object.entries(incoming)) {
        if (!stripped.has(name.toLowerCase())) headers[name] = value;
    }
    return headers;
}

function shouldInject(
    request: http.IncomingMessage,
    response: http.IncomingMessage,
): boolean {
    if (request.method?.toUpperCase() === 'HEAD') return false;
    if (response.statusCode !== 200) return false;
    const fetchDestination = incomingHeader(request.headers, 'sec-fetch-dest');
    if (fetchDestination !== undefined && !DOCUMENT_FETCH_DESTINATIONS.has(
        fetchDestination.trim().toLowerCase(),
    )) {
        return false;
    }
    const contentType = response.headers['content-type'];
    if (contentTypeMediaType(contentType) !== 'text/html') return false;
    if (!isInjectableHtmlCharset(contentType)) return false;
    const encoding = response.headers['content-encoding'];
    if (encoding === undefined) return true;
    return typeof encoding === 'string' && encoding.trim().toLowerCase() === 'identity';
}

const DOCUMENT_FETCH_DESTINATIONS = new Set([
    'document',
    'iframe',
    'frame',
    'nested-document',
]);

function incomingHeader(
    headers: http.IncomingHttpHeaders,
    target: string,
): string | undefined {
    for (const [name, value] of Object.entries(headers)) {
        if (name.toLowerCase() !== target) continue;
        return typeof value === 'string' ? value : value?.join(',');
    }
    return undefined;
}

function isInjectableHtmlCharset(contentType: string | undefined): boolean {
    if (contentType === undefined) return true;
    const match = /;\s*charset\s*=\s*(?:"([^"]*)"|'([^']*)'|([^;\s]*))/i.exec(
        contentType,
    );
    if (!match) return true;
    const charset = (match[1] ?? match[2] ?? match[3]).trim().toLowerCase();
    return charset === 'utf-8'
        || charset === 'utf8'
        || charset === 'us-ascii'
        || charset === 'ascii'
        || charset === 'iso-8859-1'
        || charset === 'latin1'
        || charset === 'latin-1';
}

function contentTypeMediaType(contentType: string | undefined): string | null {
    if (contentType === undefined) return null;
    return contentType.split(';', 1)[0].trim().toLowerCase();
}

interface ResponseHeaderOptions {
    inject: boolean;
    statusCode: number;
    upstream: URL;
}

const CHANGED_ENTITY_HEADERS = new Set([
    'content-length',
    'etag',
    'content-md5',
    'digest',
]);

function filteredResponseHeaders(
    rawHeaders: readonly string[],
    options: ResponseHeaderOptions,
): string[] {
    const stripped = new Set(HOP_BY_HOP_HEADERS);
    for (let index = 0; index < rawHeaders.length; index += 2) {
        if (rawHeaders[index].toLowerCase() !== 'connection') continue;
        for (const token of rawHeaders[index + 1].split(',')) {
            const normalized = token.trim().toLowerCase();
            if (normalized !== '') stripped.add(normalized);
        }
    }

    const headers: string[] = [];
    for (let index = 0; index < rawHeaders.length; index += 2) {
        const name = rawHeaders[index];
        const lowerName = name.toLowerCase();
        if (stripped.has(lowerName)) continue;
        if (options.inject && CHANGED_ENTITY_HEADERS.has(lowerName)) continue;

        let value = rawHeaders[index + 1];
        if (lowerName === 'location' && options.statusCode >= 300 && options.statusCode < 400) {
            value = rewriteLocation(value, options.upstream);
        }
        headers.push(name, value);
    }
    headers.push('Connection', 'close');
    return headers;
}

function rewriteLocation(
    location: string,
    upstream: URL,
): string {
    let parsed: URL;
    try {
        parsed = location.startsWith('//')
            ? new URL(`${upstream.protocol}${location}`)
            : new URL(location);
    } catch {
        return location;
    }
    if (parsed.port !== upstream.port || !equivalentUpstreamHost(
        parsed.hostname,
        upstream.hostname,
    )) {
        return location;
    }

    // Resolve against the iframe's currently mapped origin in the browser.
    // This avoids exposing the extension-host loopback address through
    // Remote SSH, WSL, and Dev Container authority forwarding. Already
    // relative Location values never reach this branch and remain unchanged.
    // Root-relative bridge assets share the same limitation under unusual
    // path-prefixed remote tunnels; authority-based VS Code forwarding, used
    // by the supported remote environments, preserves these paths.
    return `${parsed.pathname}${parsed.search}${parsed.hash}`;
}

function equivalentUpstreamHost(left: string, right: string): boolean {
    const normalizedLeft = normalizeHostname(left);
    const normalizedRight = normalizeHostname(right);
    return normalizedLeft === normalizedRight || (
        LOOPBACK_HOSTNAMES.has(normalizedLeft) &&
        LOOPBACK_HOSTNAMES.has(normalizedRight)
    );
}

const LOOPBACK_HOSTNAMES = new Set(['127.0.0.1', 'localhost', '::1']);

function normalizeHostname(hostname: string): string {
    return unbracketIpv6Hostname(hostname).toLowerCase();
}

/**
 * Whether a parsed authority names a loopback host with no userinfo. Userinfo
 * is rejected because a real `Host`/`Origin` never carries it, yet the WHATWG
 * parser would read `attacker.invalid@localhost` as host `localhost` — letting
 * a crafted authority slip a non-loopback label past the hostname check.
 */
function isLoopbackUrl(url: URL): boolean {
    if (url.username !== '' || url.password !== '') return false;
    return LOOPBACK_HOSTNAMES.has(normalizeHostname(url.hostname));
}

/**
 * Whether a request's `Host` header is safe for a directly-reachable loopback
 * listener. An absent `Host` is allowed: a browser same-origin fetch always
 * sends one, so its absence means the request is not the DNS-rebinding vector
 * this guards against. A present `Host` must name `127.0.0.1`, `localhost`, or
 * `[::1]` (port-agnostic) with no userinfo; anything else — including a rebound
 * attacker domain — is rejected. An unparseable value is treated as unsafe.
 */
function isLoopbackHostHeader(host: string | undefined): boolean {
    if (host === undefined) return true;
    if (host.includes('@')) return false;
    let url: URL;
    try {
        url = new URL(`http://${host}`);
    } catch {
        return false;
    }
    return isLoopbackUrl(url);
}

class HtmlBridgeInjectionTransform extends Transform {
    private context: HtmlScannerContext = 'normal';
    private tag: { after: HtmlScannerContext; quote: number | null } | null = null;
    private readonly candidate: number[] = [];
    private commentTail = 0;
    private injected = false;
    // Within a `<script>` element, `</script>` closes the element unless it sits
    // in HTML5's script-data (double-)escaped span — the span opened by `<!--`
    // and closed by `-->`. We model that span conservatively: while `scriptEscaped`
    // is set, a `</script>` is treated as script data, not the element's end tag.
    // This is correct for the realistic cases (the legacy `<!-- ... //-->` comment
    // hack and `<!--<script>...</script>...` double-escaping) and fails safe for
    // pathological unbalanced input — an unmatched `<!--` merely defers injection
    // to end-of-stream (`_flush`) rather than mis-injecting inside script text.
    private scriptEscaped = false;
    // Rolling window of the last up-to-4 bytes emitted as script data, used to
    // detect the `<!--` / `-->` span delimiters.
    private scriptDataTail = 0;

    constructor(private readonly injection: Buffer) {
        super();
    }

    override _transform(
        chunk: Buffer | string,
        encoding: BufferEncoding,
        callback: TransformCallback,
    ): void {
        try {
            const bytes = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk, encoding);
            if (this.injected) {
                this.push(bytes);
                callback();
                return;
            }

            let output = Buffer.allocUnsafe(bytes.length + 16);
            let outputLength = 0;
            const emit = (byte: number): void => {
                this.trackScriptData(byte);
                output[outputLength++] = byte;
            };
            const flushOutput = (): void => {
                if (outputLength > 0) this.push(output.subarray(0, outputLength));
                output = Buffer.allocUnsafe(bytes.length + 16);
                outputLength = 0;
            };

            for (const byte of bytes) this.scanByte(byte, emit, flushOutput);
            flushOutput();
            callback();
        } catch (error) {
            callback(error as Error);
        }
    }

    override _flush(callback: TransformCallback): void {
        try {
            if (this.candidate.length > 0) {
                this.push(Buffer.from(this.candidate));
                this.candidate.length = 0;
            }
            if (!this.injected) {
                this.push(this.injection);
            }
            callback();
        } catch (error) {
            callback(error as Error);
        }
    }

    private scanByte(
        byte: number,
        emit: (byte: number) => void,
        flushOutput: () => void,
    ): void {
        if (this.injected) {
            emit(byte);
            return;
        }
        if (this.tag) {
            emit(byte);
            if (this.tag.quote !== null) {
                if (byte === this.tag.quote) this.tag.quote = null;
                return;
            }
            if (byte === BYTE_DOUBLE_QUOTE || byte === BYTE_SINGLE_QUOTE) {
                this.tag.quote = byte;
            } else if (byte === BYTE_GREATER_THAN) {
                this.enterContext(this.tag.after);
                this.tag = null;
            }
            return;
        }

        if (this.context === 'comment') {
            emit(byte);
            this.commentTail = ((this.commentTail << 8) | byte) & 0xFFFFFF;
            if (this.commentTail === COMMENT_END) {
                this.context = 'normal';
                this.commentTail = 0;
            }
            return;
        }

        if (this.candidate.length === 0) {
            if (byte === BYTE_LESS_THAN) this.candidate.push(byte);
            else emit(byte);
            return;
        }

        this.candidate.push(byte);
        if (this.context === 'normal') {
            this.resolveNormalCandidate(emit, flushOutput);
        } else {
            this.resolveRawTextCandidate(emit, flushOutput);
        }
    }

    private resolveNormalCandidate(
        emit: (byte: number) => void,
        flushOutput: () => void,
    ): void {
        if (asciiEqual(this.candidate, BODY_CLOSE)) {
            flushOutput();
            this.push(this.injection);
            this.injected = true;
            this.emitCandidate(emit);
            return;
        }
        if (asciiEqual(this.candidate, COMMENT_START)) {
            this.emitCandidate(emit);
            this.context = 'comment';
            this.commentTail = 0;
            return;
        }

        const raw = rawOpenCandidate(this.candidate);
        if (raw?.kind === 'matched') {
            this.emitCandidate(emit);
            if (raw.boundary === BYTE_GREATER_THAN) {
                this.enterContext(raw.context);
            } else {
                this.tag = { after: raw.context, quote: null };
            }
            return;
        }
        if (raw?.kind === 'prefix') return;
        if (isAsciiPrefix(this.candidate, BODY_CLOSE) ||
            isAsciiPrefix(this.candidate, COMMENT_START)) {
            return;
        }

        if (looksLikeTagStart(this.candidate[1])) {
            this.emitCandidate(emit);
            this.tag = { after: 'normal', quote: null };
            return;
        }
        this.emitFirstAndRescan(emit, flushOutput);
    }

    private resolveRawTextCandidate(
        emit: (byte: number) => void,
        flushOutput: () => void,
    ): void {
        const context = this.context;
        if (context === 'normal' || context === 'comment') {
            throw new Error('HTML scanner entered raw-text resolution outside raw text.');
        }
        const target = rawTextClose(context);
        const lastByte = this.candidate[this.candidate.length - 1];
        if (isAsciiPrefix(this.candidate, target)) return;
        if (this.candidate.length === target.length + 1 &&
            asciiEqual(this.candidate.slice(0, -1), target) &&
            isTagBoundary(lastByte)) {
            if (context === 'script' && this.scriptEscaped) {
                // Inside the script-data escaped span (opened by `<!--`): this
                // `</script>` is script data, not the element's end tag, so
                // consume it and remain in script context.
                this.emitCandidate(emit);
                return;
            }
            const boundary = lastByte;
            this.emitCandidate(emit);
            if (boundary === BYTE_GREATER_THAN) this.context = 'normal';
            else this.tag = { after: 'normal', quote: null };
            return;
        }
        this.emitFirstAndRescan(emit, flushOutput);
    }

    private emitFirstAndRescan(
        emit: (byte: number) => void,
        flushOutput: () => void,
    ): void {
        const pending = this.candidate.splice(0);
        emit(pending[0]);
        for (let index = 1; index < pending.length; index++) {
            this.scanByte(pending[index], emit, flushOutput);
        }
    }

    private emitCandidate(emit: (byte: number) => void): void {
        for (const byte of this.candidate) emit(byte);
        this.candidate.length = 0;
    }

    private enterContext(context: HtmlScannerContext): void {
        this.context = context;
        if (context === 'script') {
            this.scriptEscaped = false;
            this.scriptDataTail = 0;
        }
    }

    // Track the `<!--` / `-->` script-data escaped span over emitted script
    // bytes. Called for every byte written to output while in script context,
    // so it follows the true output order regardless of candidate rescans.
    private trackScriptData(byte: number): void {
        if (this.context !== 'script') return;
        this.scriptDataTail = ((this.scriptDataTail << 8) | byte) >>> 0;
        if (this.scriptDataTail === SCRIPT_ESCAPE_OPEN) {
            this.scriptEscaped = true;
        } else if ((this.scriptDataTail & 0xFFFFFF) === COMMENT_END) {
            this.scriptEscaped = false;
        }
    }
}

type HtmlScannerContext =
    | 'normal'
    | 'comment'
    | 'script'
    | 'style'
    | 'title'
    | 'textarea';
type HtmlRawTextContext = Exclude<HtmlScannerContext, 'normal' | 'comment'>;

const BYTE_LESS_THAN = 0x3C;
const BYTE_GREATER_THAN = 0x3E;
const BYTE_SINGLE_QUOTE = 0x27;
const BYTE_DOUBLE_QUOTE = 0x22;
const COMMENT_END = 0x2D2D3E;
// `<!--` and `-->` as packed big-endian bytes, for the script-data escaped span.
const SCRIPT_ESCAPE_OPEN = 0x3C212D2D;
const BODY_CLOSE = asciiBytes('</body>');
const COMMENT_START = asciiBytes('<!--');
const SCRIPT_OPEN = asciiBytes('<script');
const STYLE_OPEN = asciiBytes('<style');
const TITLE_OPEN = asciiBytes('<title');
const TEXTAREA_OPEN = asciiBytes('<textarea');
const SCRIPT_CLOSE = asciiBytes('</script');
const STYLE_CLOSE = asciiBytes('</style');
const TITLE_CLOSE = asciiBytes('</title');
const TEXTAREA_CLOSE = asciiBytes('</textarea');

function rawTextClose(context: HtmlRawTextContext): readonly number[] {
    switch (context) {
        case 'script': return SCRIPT_CLOSE;
        case 'style': return STYLE_CLOSE;
        case 'title': return TITLE_CLOSE;
        case 'textarea': return TEXTAREA_CLOSE;
    }
}

function rawOpenCandidate(candidate: readonly number[]):
    | { kind: 'prefix' }
    | { kind: 'matched'; context: HtmlRawTextContext; boundary: number }
    | null {
    for (const [target, context] of [
        [SCRIPT_OPEN, 'script'],
        [STYLE_OPEN, 'style'],
        [TITLE_OPEN, 'title'],
        [TEXTAREA_OPEN, 'textarea'],
    ] as const) {
        const lastByte = candidate[candidate.length - 1];
        if (isAsciiPrefix(candidate, target)) return { kind: 'prefix' };
        if (candidate.length === target.length + 1 &&
            asciiEqual(candidate.slice(0, -1), target) &&
            isTagBoundary(lastByte)) {
            return { kind: 'matched', context, boundary: lastByte };
        }
    }
    return null;
}

function isTagBoundary(byte: number): boolean {
    return byte === BYTE_GREATER_THAN
        || byte === 0x2F
        || byte === 0x09
        || byte === 0x0A
        || byte === 0x0C
        || byte === 0x0D
        || byte === 0x20;
}

function looksLikeTagStart(byte: number | undefined): boolean {
    if (byte === undefined) return false;
    const lower = asciiLower(byte);
    return (lower >= 0x61 && lower <= 0x7A)
        || byte === 0x21
        || byte === 0x2F
        || byte === 0x3F;
}

function isAsciiPrefix(candidate: readonly number[], target: readonly number[]): boolean {
    if (candidate.length > target.length) return false;
    for (let index = 0; index < candidate.length; index++) {
        if (asciiLower(candidate[index]) !== target[index]) return false;
    }
    return true;
}

function asciiEqual(candidate: readonly number[], target: readonly number[]): boolean {
    return candidate.length === target.length && isAsciiPrefix(candidate, target);
}

function asciiLower(byte: number): number {
    return byte >= 0x41 && byte <= 0x5A ? byte + 0x20 : byte;
}

function asciiBytes(value: string): readonly number[] {
    return [...Buffer.from(value, 'ascii')];
}

function rawHeadersText(rawHeaders: readonly string[]): string {
    let text = '';
    for (let index = 0; index < rawHeaders.length; index += 2) {
        text += `${rawHeaders[index]}: ${rawHeaders[index + 1]}\r\n`;
    }
    return text;
}

function pipeSockets(clientSocket: net.Socket, upstreamSocket: net.Socket): void {
    clientSocket.pipe(upstreamSocket);
    upstreamSocket.pipe(clientSocket);
    clientSocket.on('end', () => upstreamSocket.end());
    upstreamSocket.on('end', () => clientSocket.end());
    clientSocket.on('error', () => upstreamSocket.destroy());
    upstreamSocket.on('error', () => clientSocket.destroy());
    clientSocket.on('close', () => upstreamSocket.destroy());
    upstreamSocket.on('close', () => clientSocket.destroy());
}

function rejectSocket(socket: net.Socket, status: number, message: string): void {
    if (socket.destroyed) return;
    // These raw upgrade/CONNECT sockets carry no error listener (Node removes
    // its internal one before emitting `upgrade`/`connect`), so a client RST
    // racing this write would emit an unhandled `error` (EPIPE/ECONNRESET) and
    // crash the extension host. Swallow it and destroy the socket instead.
    socket.on('error', () => socket.destroy());
    socket.end(
        `HTTP/1.1 ${status} ${message}\r\n` +
        'Connection: close\r\n' +
        'Content-Length: 0\r\n\r\n',
    );
}
