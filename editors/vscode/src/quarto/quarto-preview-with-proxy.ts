/**
 * Lifecycle-safe composition of Quarto preview and its loopback proxy.
 *
 * Quarto is started and verified first. When both packaged bridge assets are
 * available, a response-transforming fixed-upstream proxy is bound and the
 * final page is probed through it; otherwise readiness returns Quarto directly.
 * Proxied readiness reports the proxy URL so the existing runtime frames and
 * externally maps the proxy origin without changing its generation discipline. The proxy resource
 * is installed before bind is awaited, and every stop closes its listener and
 * sockets before entering the inner Quarto teardown ladder. Proxy bind or
 * readiness-probe failure is non-fatal: Raven logs one concise diagnostic and
 * frames Quarto directly.
 */

import type * as vscode from 'vscode';
import {
    probeQuartoPreviewUrl,
    type QuartoPreviewProcessLike,
    type QuartoPreviewReady,
} from './quarto-preview-engine';
import {
    QuartoPreviewProxy,
    type QuartoPreviewBridgeAssets,
    type QuartoPreviewProxyLike,
    type QuartoPreviewProxyReady,
} from './quarto-preview-proxy';

export interface QuartoPreviewWithProxyProcessOptions {
    createInner(onUnexpectedExit: (code: number | null) => void): QuartoPreviewProcessLike;
    output: Pick<vscode.OutputChannel, 'appendLine'>;
    onUnexpectedExit(code: number | null): void;
    bridgeAssets?: QuartoPreviewBridgeAssets;
    proxyFactory?: (upstreamOrigin: string) => QuartoPreviewProxyLike;
    probe?: (rawUrl: string, signal: AbortSignal) => Promise<number>;
}

type TeardownMode = 'stop' | 'shutdown';

export class QuartoPreviewWithProxyProcess implements QuartoPreviewProcessLike {
    private readonly inner: QuartoPreviewProcessLike;
    private proxy: QuartoPreviewProxyLike | null = null;
    private startPromise: Promise<QuartoPreviewReady> | null = null;
    private teardownPromise: Promise<void> | null = null;
    private teardownMode: TeardownMode | null = null;
    private innerTeardownStarted = false;
    private activeProbe: AbortController | null = null;
    private stopping = false;
    private resolveStopStarted!: () => void;
    private readonly stopStarted = new Promise<void>((resolve) => {
        this.resolveStopStarted = resolve;
    });

    constructor(private readonly opts: QuartoPreviewWithProxyProcessOptions) {
        this.inner = opts.createInner((code) => this.handleUnexpectedExit(code));
    }

    start(): Promise<QuartoPreviewReady> {
        if (this.startPromise) return this.startPromise;
        this.startPromise = this.startInner();
        return this.startPromise;
    }

    stop(): Promise<void> {
        return this.beginTeardown('stop');
    }

    shutdown(): Promise<void> {
        return this.beginTeardown('shutdown');
    }

    private async startInner(): Promise<QuartoPreviewReady> {
        const quartoReady = await this.inner.start();
        this.throwIfStopping();

        // With no packaged assets there is no response transformation to
        // justify an extra HTTP/WS hop; frame the already-ready Quarto origin.
        if (!this.opts.bridgeAssets) return quartoReady;

        let proxy: QuartoPreviewProxyLike;
        try {
            proxy = (this.opts.proxyFactory ?? ((origin) => (
                new QuartoPreviewProxy(origin, this.opts.bridgeAssets)
            )))(quartoReady.origin);
        } catch (error) {
            this.appendFallbackDiagnostic(error);
            return quartoReady;
        }

        // Stop can now always discover and close this resource, including
        // while its bind promise is still pending.
        this.proxy = proxy;
        let proxyReady: QuartoPreviewProxyReady;
        try {
            const outcome = await Promise.race([
                proxy.start().then((ready) => ({ kind: 'ready' as const, ready })),
                this.stopStarted.then(() => ({ kind: 'stopped' as const })),
            ]);
            if (outcome.kind === 'stopped') {
                throw new Error('Quarto preview proxy startup was stopped.');
            }
            proxyReady = outcome.ready;
        } catch (error) {
            if (this.stopping) throw error;
            await this.closeProxy();
            this.appendFallbackDiagnostic(error);
            return quartoReady;
        }
        return this.probeProxy(quartoReady, proxyReady, proxy);
    }

    private async probeProxy(
        quartoReady: QuartoPreviewReady,
        proxyReady: QuartoPreviewProxyReady,
        proxy: QuartoPreviewProxyLike,
    ): Promise<QuartoPreviewReady> {
        const rawUrl = proxyPageUrl(quartoReady.rawUrl, proxyReady.origin);
        const controller = new AbortController();
        this.activeProbe = controller;
        try {
            const probe = this.opts.probe ?? ((url: string, signal: AbortSignal) => (
                probeQuartoPreviewUrl(url, undefined, undefined, undefined, signal)
            ));
            const statusCode = await probe(rawUrl, controller.signal);
            this.throwIfStopping();
            if (statusCode >= 500 && statusCode < 600) {
                throw new Error(`preview proxy readiness returned HTTP ${statusCode}`);
            }
            return {
                rawUrl,
                browserUrl: quartoReady.browserUrl ?? quartoReady.rawUrl,
                origin: proxyReady.origin,
                statusCode,
            };
        } catch (error) {
            if (this.stopping) throw error;
            if (this.proxy === proxy) await this.closeProxy();
            this.appendFallbackDiagnostic(error);
            return quartoReady;
        } finally {
            if (this.activeProbe === controller) this.activeProbe = null;
        }
    }

    private beginTeardown(mode: TeardownMode): Promise<void> {
        this.stopping = true;
        this.resolveStopStarted();
        this.activeProbe?.abort();
        if (this.teardownPromise) {
            if (mode === 'shutdown' && this.teardownMode === 'stop') {
                this.teardownMode = 'shutdown';
                if (this.innerTeardownStarted) void this.inner.shutdown();
            }
            return this.teardownPromise;
        }
        this.teardownMode = mode;
        this.teardownPromise = this.runTeardown();
        return this.teardownPromise;
    }

    private async runTeardown(): Promise<void> {
        let proxyError: unknown = null;
        try {
            await this.closeProxy();
        } catch (error) {
            proxyError = error;
        }

        this.innerTeardownStarted = true;
        try {
            if (this.teardownMode === 'shutdown') {
                await this.inner.shutdown();
            } else {
                await this.inner.stop();
            }
        } catch (error) {
            if (proxyError === null) throw error;
            throw proxyError;
        }
        if (proxyError !== null) throw proxyError;
    }

    private async closeProxy(): Promise<void> {
        const proxy = this.proxy;
        if (!proxy) return;
        try {
            await proxy.close();
        } finally {
            if (this.proxy === proxy) this.proxy = null;
        }
    }

    private handleUnexpectedExit(code: number | null): void {
        void this.closeProxy().finally(() => this.opts.onUnexpectedExit(code));
    }

    private throwIfStopping(): void {
        if (this.stopping) throw new Error('Quarto preview startup was stopped.');
    }

    private appendFallbackDiagnostic(error: unknown): void {
        try {
            this.opts.output.appendLine(
                `[quarto] Preview proxy unavailable; using Quarto directly: ` +
                errorMessage(error),
            );
        } catch {
            // The activation output can already be disposing during shutdown.
        }
    }
}

function proxyPageUrl(quartoRawUrl: string, proxyOrigin: string): string {
    const quarto = new URL(quartoRawUrl);
    const proxy = new URL(proxyOrigin);
    proxy.pathname = quarto.pathname;
    proxy.search = quarto.search;
    proxy.hash = quarto.hash;
    return proxy.toString();
}

function errorMessage(error: unknown): string {
    return error instanceof Error ? error.message : String(error);
}
