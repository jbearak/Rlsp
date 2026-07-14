/**
 * Lazy, resource-scoped Quarto CLI resolution.
 *
 * The effective `raven.quarto.path` value is supplied for the document URI and
 * is also the cache key. This is what makes the resolver safe in multi-root
 * workspaces: two folders with different resource-scoped settings never share
 * a cached binary merely because they use the same resolver instance.
 *
 * Every candidate is identity-probed with `<bin> --help`; an executable that
 * exits successfully but does not print Quarto's `Quarto CLI` marker is
 * rejected. The production probe drains both pipes, captures only a bounded
 * stdout prefix, and has a hard timeout so a wedged configured binary cannot
 * block command progress forever. Probe children lead POSIX process groups;
 * timeout termination uses the same platform-aware tree signal as render and
 * preview. Cache entries are the resolution promises, so concurrent first use
 * for one effective setting shares a single probe; rejected promises are
 * evicted to permit a later retry.
 */

import * as childProcess from 'child_process';
import * as path from 'path';
import { sendSignal } from '../knit/process-signals';
import { isQuartoHelpOutput } from './quarto-probe';

export class QuartoNotFoundError extends Error {
    constructor(message = 'Quarto CLI not found') {
        super(message);
        this.name = 'QuartoNotFoundError';
    }
}

export interface QuartoResolverDeps<Resource> {
    /** Return the effective resource-scoped `raven.quarto.path` value. */
    getConfigured(resource: Resource): string;
    /** Check that an absolute configured/fallback candidate is executable. */
    access(candidate: string): Promise<void>;
    /** Run the identity probe; rejects for non-Quarto candidates. */
    probe(candidate: string): Promise<string>;
    /** Optional platform fallback override for deterministic tests. */
    fallbacks?: () => string[];
}

/** Platform-specific standard Quarto installation paths. */
export function defaultQuartoFallbacks(
    platform: NodeJS.Platform = process.platform,
    env: NodeJS.ProcessEnv = process.env,
): string[] {
    if (platform === 'darwin') {
        return [
            '/opt/homebrew/bin/quarto',
            '/usr/local/bin/quarto',
            '/Applications/RStudio.app/Contents/Resources/app/quarto/bin/quarto',
        ];
    }
    if (platform === 'win32') {
        const candidates: string[] = [];
        if (env.LOCALAPPDATA) {
            candidates.push(path.win32.join(
                env.LOCALAPPDATA,
                'Programs',
                'Quarto',
                'bin',
                'quarto.exe',
            ));
        }
        if (env.PROGRAMFILES) {
            candidates.push(path.win32.join(
                env.PROGRAMFILES,
                'Quarto',
                'bin',
                'quarto.exe',
            ));
        }
        return candidates;
    }
    return ['/usr/local/bin/quarto', '/opt/quarto/bin/quarto'];
}

export class QuartoResolver<Resource> {
    private readonly cache = new Map<string, Promise<string>>();

    constructor(private readonly deps: QuartoResolverDeps<Resource>) {}

    resolve(resource: Resource): Promise<string> {
        const configured = this.deps.getConfigured(resource).trim();
        const cached = this.cache.get(configured);
        if (cached !== undefined) return cached;

        const resolution = this.resolveUncached(configured);
        this.cache.set(configured, resolution);
        void resolution.catch(() => {
            if (this.cache.get(configured) === resolution) {
                this.cache.delete(configured);
            }
        });
        return resolution;
    }

    private async resolveUncached(configured: string): Promise<string> {
        if (configured !== '') {
            try {
                await this.deps.access(configured);
                await this.deps.probe(configured);
                return configured;
            } catch {
                throw new QuartoNotFoundError(
                    `Configured Quarto path is unusable or is not Quarto: ${configured}`,
                );
            }
        }

        try {
            await this.deps.probe('quarto');
            return 'quarto';
        } catch {
            // Continue through platform fallbacks.
        }

        const fallbacks = (this.deps.fallbacks ?? defaultQuartoFallbacks)();
        for (const candidate of fallbacks) {
            try {
                await this.deps.access(candidate);
                await this.deps.probe(candidate);
                return candidate;
            } catch {
                // Try the next standard installation path.
            }
        }

        throw new QuartoNotFoundError();
    }

    invalidate(): void {
        this.cache.clear();
    }
}

export const QUARTO_PROBE_TIMEOUT_MS = 10_000;
const QUARTO_PROBE_CAPTURE_LIMIT = 64 * 1024;

/**
 * Probe `<bin> --help` and verify Quarto's identity marker.
 *
 * stdout is capped while stderr is deliberately drained without capture. A
 * successful exit alone is insufficient: unrelated executables can accept a
 * `--help` argument and exit zero.
 */
export function probeQuartoBinary(
    bin: string,
    timeoutMs: number = QUARTO_PROBE_TIMEOUT_MS,
    spawnProcess: typeof childProcess.spawn = childProcess.spawn,
    signalProcess: typeof sendSignal = sendSignal,
): Promise<string> {
    return new Promise<string>((resolve, reject) => {
        let child: childProcess.ChildProcess;
        try {
            child = spawnProcess(bin, ['--help'], {
                stdio: ['ignore', 'pipe', 'pipe'],
                detached: process.platform !== 'win32',
            });
        } catch (err) {
            reject(err);
            return;
        }

        let stdout = '';
        let settled = false;
        const finish = (fn: () => void): void => {
            if (settled) return;
            settled = true;
            clearTimeout(timer);
            fn();
        };
        const timer = setTimeout(() => {
            finish(() => {
                signalProcess(child, 'SIGKILL');
                reject(new Error(`Quarto probe timed out after ${timeoutMs}ms`));
            });
        }, timeoutMs);

        child.stdout?.on('data', (chunk: Buffer) => {
            if (stdout.length >= QUARTO_PROBE_CAPTURE_LIMIT) return;
            stdout = (stdout + chunk.toString('utf8')).slice(0, QUARTO_PROBE_CAPTURE_LIMIT);
        });
        child.stderr?.on('data', () => { /* drain */ });
        child.on('error', (err) => finish(() => reject(err)));
        child.on('close', (code) => finish(() => {
            const trimmed = stdout.trim();
            if (code !== 0) {
                reject(new Error(`Quarto probe exited with code ${String(code)}`));
            } else if (!isQuartoHelpOutput(trimmed)) {
                reject(new Error('Candidate did not identify itself as Quarto CLI'));
            } else {
                resolve(trimmed);
            }
        }));
    });
}
