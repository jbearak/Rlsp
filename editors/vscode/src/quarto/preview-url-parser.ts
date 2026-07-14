/**
 * Streaming Quarto preview-output parsing and loopback URL validation.
 *
 * This module is a security boundary. A rendered workspace document can
 * write arbitrary text into the same stdout/stderr streams as Quarto, so a
 * line that merely looks like `Browse at ...` is hostile until its URL has
 * passed `validatePreviewUrl`. Only loopback HTTP URLs without credentials
 * cross this boundary. `Listening on` supplies the trusted origin; `Browse
 * at` supplies only the document-specific path. A rejected candidate stops
 * the scanner as a hard failure so later output cannot silently replace it.
 *
 * The scanner processes each completed line once. Callers that merge process
 * streams must reassemble lines per stream and call `feedLine`; `feed` retains
 * its single-stream decoder for direct use and compatibility. The scanner
 * stops parsing after a correlated result or failure. Lone Browse and
 * Listening candidates remain provisional so either process stream ordering
 * can be correlated during the engine's short grace window. A separately
 * capped raw tail is retained for honest startup-error display without
 * allowing unbounded process output to consume memory.
 */

export interface PreviewUrlResult {
    /** Trusted loopback origin, sourced from `Listening on` when present. */
    origin: string;
    /** Final validated URL used by the readiness probe and preview frame. */
    url: string;
}

export const QUARTO_STARTUP_TAIL_LIMIT = 64 * 1024;

/** Remove CSI/OSC ANSI terminal-control sequences from a string. */
export function stripAnsi(s: string): string {
    return s
        .replace(/\u001B\][^\u0007]*(?:\u0007|\u001B\\)/g, '')
        .replace(/(?:\u001B\[|\u009B)[0-?]*[ -/]*[@-~]/g, '')
        .replace(/\u001B[@-_]/g, '');
}

/**
 * Validate a raw Quarto preview URL and return its normalized origin+path.
 *
 * Accepted URLs use only `http:`, have no username/password, and name the
 * host exactly as `127.0.0.1`, `localhost`, or `[::1]`. WHATWG URL parsing
 * rejects malformed/non-numeric/out-of-range ports; an omitted port is the
 * valid HTTP default (80). The returned `url` deliberately excludes no path,
 * query, or fragment information, while `origin` is suitable for CSP.
 */
export function validatePreviewUrl(raw: string): PreviewUrlResult | null {
    const trimmed = raw.trim();
    // Check the lexical authority as well as WHATWG's parsed hostname.
    // WHATWG deliberately canonicalizes alternate IPv4 spellings such as
    // `2130706433` to `127.0.0.1`; accepting that would violate this
    // boundary's exact-host allowlist. This shape check also rejects an
    // explicitly empty port (`localhost:`), which is neither a numeric port
    // nor the ordinary omitted/default-port form.
    const authority = trimmed.match(/^http:\/\/([^/?#]*)(?:[/?#]|$)/i)?.[1];
    if (!authority) return null;
    const authorityMatch = authority.match(
        /^(127\.0\.0\.1|localhost|\[::1\])(?::(\d+))?$/,
    );
    if (!authorityMatch) return null;

    let parsed: URL;
    try {
        parsed = new URL(trimmed);
    } catch {
        return null;
    }

    if (parsed.protocol !== 'http:') return null;
    if (parsed.username !== '' || parsed.password !== '') return null;
    if (
        parsed.hostname !== '127.0.0.1' &&
        parsed.hostname !== 'localhost' &&
        parsed.hostname !== '[::1]'
    ) {
        return null;
    }
    if (authorityMatch[2] !== undefined && !/^\d+$/.test(authorityMatch[2])) return null;

    const path = `${parsed.pathname}${parsed.search}${parsed.hash}`;
    return {
        origin: parsed.origin,
        url: `${parsed.origin}${path}`,
    };
}

/**
 * Incrementally scan Quarto's combined stdout/stderr for its preview URL.
 *
 * Both `Browse at` and `Listening on` are retained provisionally until they can
 * be correlated. This handles their usual order as well as reverse cross-pipe
 * delivery. `finish()` accepts whichever safe lone candidate remains when
 * end-of-stream proves its partner will not arrive.
 */
export class QuartoPreviewOutputScanner {
    private carry = '';
    private tail = '';
    private browse: PreviewUrlResult | null = null;
    private listening: PreviewUrlResult | null = null;
    private emitted: PreviewUrlResult | null = null;
    private failureDetail: string | null = null;
    private stopped = false;

    constructor(private readonly tailLimit: number = QUARTO_STARTUP_TAIL_LIMIT) {}

    /** Feed one decoded text chunk and return a newly emitted result, if any. */
    feed(chunk: string): PreviewUrlResult | null {
        // Parsing stops after a terminal result, but raw-tail capture does
        // not: readiness probing can still fail or the process can still exit
        // before ready, and those later bytes belong in the surfaced startup
        // detail even though they must never influence URL selection.
        if (this.stopped) {
            this.appendTail(chunk);
            return null;
        }
        this.appendTail(chunk);

        const combined = this.carry + chunk;
        const lines = combined.split('\n');
        this.carry = lines.pop() ?? '';
        if (this.carry.length > this.tailLimit) {
            this.carry = this.carry.slice(-this.tailLimit);
        }

        for (const rawLine of lines) {
            const line = rawLine.endsWith('\r') ? rawLine.slice(0, -1) : rawLine;
            const result = this.processLine(line);
            if (result || this.stopped) return result;
        }
        return null;
    }

    /**
     * Feed one complete line, without its line terminator.
     *
     * Set `captureTail` false when the caller already passed the original raw
     * chunk to `captureRaw`; this keeps parsing and tail ownership separate.
     */
    feedLine(line: string, captureTail: boolean = true): PreviewUrlResult | null {
        if (this.stopped) {
            if (captureTail) this.appendTail(`${line}\n`);
            return null;
        }
        if (captureTail) this.appendTail(`${line}\n`);
        return this.processLine(line);
    }

    /** Capture original process output without feeding it to the parser. */
    captureRaw(chunk: string): void {
        this.appendTail(chunk);
    }

    /**
     * Mark the stream complete, process its final partial line, and allow a
     * validated Browse-only candidate to become the result.
     */
    finish(): PreviewUrlResult | null {
        if (this.stopped) return this.emitted;
        if (this.carry !== '') {
            const line = this.carry.endsWith('\r') ? this.carry.slice(0, -1) : this.carry;
            this.carry = '';
            const result = this.processLine(line);
            if (result || this.stopped) return result;
        }
        if (this.listening) return this.emit(this.listening);
        if (this.browse) return this.emit(this.browse);
        this.stopped = true;
        return null;
    }

    /** Current emitted result, if scanning has completed successfully. */
    result(): PreviewUrlResult | null {
        return this.emitted;
    }

    /**
     * Accept a validated Browse-only candidate after a short correlation
     * window. The preview engine uses this when Quarto never prints a
     * `Listening on` line: waiting for end-of-stream would deadlock because a
     * healthy preview process remains alive until the user stops it.
     *
     * This does not stop scanning or set `result()`: the returned Browse URL
     * is provisional, allowing a late `Listening on` line to supersede it
     * while readiness probing is still pending. Callers SHOULD delay this
     * briefly so the usual later Listening line can supply the trusted origin
     * first. A hard parser failure always wins and prevents this fallback.
     */
    acceptBrowseCandidate(): PreviewUrlResult | null {
        if (this.stopped) return this.emitted;
        if (!this.browse) return null;
        return this.browse;
    }

    /** Accept a validated Listening-only candidate after correlation grace. */
    acceptListeningCandidate(): PreviewUrlResult | null {
        if (this.stopped) return this.emitted;
        return this.listening;
    }

    /** True when a validated Browse candidate is awaiting correlation. */
    hasBrowseCandidate(): boolean {
        return !this.stopped && this.browse !== null;
    }

    /** True when a validated Listening candidate awaits a Browse path. */
    hasListeningCandidate(): boolean {
        return !this.stopped && this.listening !== null;
    }

    /** Human-readable hard-failure detail for a rejected advertised URL. */
    failure(): string | null {
        return this.failureDetail;
    }

    /** Capped raw suffix of startup output for failure display. */
    startupTail(): string {
        return this.tail;
    }

    private appendTail(chunk: string): void {
        if (this.tailLimit <= 0) {
            this.tail = '';
            return;
        }
        this.tail = (this.tail + chunk).slice(-this.tailLimit);
    }

    private processLine(rawLine: string): PreviewUrlResult | null {
        const line = stripAnsi(rawLine);
        const browseMatch = line.match(/(?:^|\s)Browse at\s+(\S+)/);
        if (browseMatch) {
            const candidate = validatePreviewUrl(browseMatch[1]);
            if (!candidate) {
                this.reject(`Rejected unsafe Quarto preview URL: ${browseMatch[1]}`);
                return null;
            }
            this.browse = candidate;
            if (this.listening) return this.emitCorrelated();
        }

        const listeningMatch = line.match(/(?:^|\s)Listening on\s+(\S+)/);
        if (!listeningMatch) return null;
        const listening = validatePreviewUrl(listeningMatch[1]);
        if (!listening) {
            this.reject(`Rejected unsafe Quarto listening URL: ${listeningMatch[1]}`);
            return null;
        }
        this.listening = listening;

        if (!this.browse) return null;
        return this.emitCorrelated();
    }

    private emitCorrelated(): PreviewUrlResult | null {
        if (!this.browse || !this.listening) return null;
        const browseUrl = new URL(this.browse.url);
        const correlated = validatePreviewUrl(
            `${this.listening.origin}` +
            `${browseUrl.pathname}${browseUrl.search}${browseUrl.hash}`,
        );
        if (!correlated) {
            this.reject('Could not correlate Quarto preview URLs safely.');
            return null;
        }
        return this.emit(correlated);
    }

    private emit(result: PreviewUrlResult): PreviewUrlResult {
        this.emitted = result;
        this.stopped = true;
        return result;
    }

    private reject(detail: string): void {
        this.failureDetail = detail;
        this.stopped = true;
    }
}
