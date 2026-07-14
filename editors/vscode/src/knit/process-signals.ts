/**
 * Platform-aware signal helpers for the knit, pandoc, and Quarto subprocess
 * engines.
 *
 *   - POSIX: send the signal to the child's process group (`-pid`) so
 *     helpers spawned by the child (pandoc → xelatex → bibtex …) are
 *     reaped together. Requires the child to have been spawned with
 *     `detached: true` so it leads a new group. Falls back to a direct
 *     `child.kill(signal)` if the group is gone (EPERM / ESRCH).
 *   - Windows: POSIX-style signals are not meaningful. Both SIGTERM and
 *     SIGKILL map to `taskkill /T` so the process tree is walked;
 *     SIGKILL adds `/F` for force-termination. SIGINT also walks the
 *     tree but without `/F`, giving Pandoc / R a chance to clean up.
 */

import { ChildProcess, spawn } from 'child_process';

export function sendSignal(child: ChildProcess, signal: 'SIGINT' | 'SIGTERM' | 'SIGKILL'): void {
    try {
        if (process.platform === 'win32') {
            if (signal === 'SIGINT') {
                runTaskkill(child, false);
                return;
            }
            runTaskkill(child, true);
            return;
        }
        if (child.pid !== undefined) {
            try {
                process.kill(-child.pid, signal);
                return;
            } catch (err) {
                void err;
            }
        }
        child.kill(signal);
    } catch {
        // Best effort; the close listener still resolves once the OS reaps the child.
    }
}

function runTaskkill(child: ChildProcess, force: boolean): void {
    if (child.pid === undefined) return;
    const args = force
        ? ['/PID', String(child.pid), '/T', '/F']
        : ['/PID', String(child.pid), '/T'];
    const tk = spawn('taskkill', args, { stdio: 'ignore' });
    tk.on('error', () => { /* swallow */ });
}
