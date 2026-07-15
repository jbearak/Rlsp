/**
 * Per-panel live-theme controller for Quarto preview shells.
 *
 * The controller owns sequencing, persisted enabled state, cross-panel
 * broadcast, and listener lifetime. VS Code-specific reads are supplied by
 * the panel through `QuartoThemeControllerDeps`, which keeps this module
 * runnable in Bun while production still reuses Knit's palette/font helpers.
 */

import { githubDark, githubLight, type GithubPalette } from '../knit/github-colors';
import { resolveFontFamilies, type ResolvedFonts } from '../knit/render-html';
import {
    resolveActiveThemePalette,
    type ThemePaletteArgs,
    type ThemePaletteOutcome,
} from '../knit/vscode-theme-palette';
import {
    QUARTO_CSS_COLOR_PATTERN_SOURCE,
    type QuartoThemePayload,
} from './quarto-messages';

export const QUARTO_THEME_PREFERENCE_KEY = 'raven.quarto.applyVSCodeTheme';

interface DisposableLike {
    dispose(): unknown;
}

interface MementoLike {
    get<T>(key: string): T | undefined;
    update?(key: string, value: unknown): PromiseLike<void> | void;
}

export interface QuartoThemeContextLike {
    globalState?: MementoLike;
}

export type QuartoThemeKind =
    | 'light'
    | 'dark'
    | 'high-contrast'
    | 'high-contrast-light';

export interface QuartoConfigurationChangeLike {
    affectsConfiguration(section: string, scope?: unknown): boolean;
}

type ThemeResolverInputs = Omit<
    ThemePaletteArgs,
    'activeEditorBackground' | 'candidateThemeIds' | 'isLight'
>;

export interface QuartoThemeControllerDeps {
    context: QuartoThemeContextLike;
    output: { appendLine(value: string): unknown };
    sourceFsPath: string;
    postThemeUpdate(payload: QuartoThemePayload): PromiseLike<boolean> | boolean;
    requestThemeContext(): PromiseLike<boolean> | boolean;
    activeThemeKind(): QuartoThemeKind;
    getConfiguration<T>(section: string, defaultValue?: T): T | undefined;
    themeResolverInputs(): ThemeResolverInputs;
    resolveFonts(sourceFsPath: string): ResolvedFonts;
    sourceConfigurationScope(sourceFsPath: string): unknown;
    onDidChangeActiveColorTheme(listener: () => void): DisposableLike;
    onDidChangeConfiguration(
        listener: (event: QuartoConfigurationChangeLike) => void,
    ): DisposableLike;
    resolvePalette?(args: ThemePaletteArgs): Promise<ThemePaletteOutcome>;
}

const controllers = new Set<QuartoThemeController>();
let preferenceWriteTail: Promise<void> = Promise.resolve();
let lastKnownThemeEnabled: boolean | undefined;
const CSS_COLOR = new RegExp(QUARTO_CSS_COLOR_PATTERN_SOURCE, 'i');

const THEME_CONFIGURATION_KEYS = [
    'workbench.colorTheme',
    'workbench.preferredLightColorTheme',
    'workbench.preferredDarkColorTheme',
    'window.autoDetectColorScheme',
    'workbench.colorCustomizations',
    'editor.tokenColorCustomizations',
    'editor.semanticTokenColorCustomizations',
] as const;

const FONT_CONFIGURATION_KEYS = [
    'raven.quarto.fontFamily',
    'raven.quarto.monospaceFontFamily',
    'markdown.preview.fontFamily',
    'editor.fontFamily',
] as const;

export class QuartoThemeController implements DisposableLike {
    private sourceFsPath: string;
    private enabled: boolean;
    private latestEditorBackground: string | undefined;
    private pushGeneration = 0;
    private disposed = false;
    private resolveWarned = false;
    private readonly disposables: DisposableLike[];

    constructor(private readonly deps: QuartoThemeControllerDeps) {
        this.sourceFsPath = deps.sourceFsPath;
        this.enabled = lastKnownThemeEnabled ?? readThemePreference(deps.context);
        this.disposables = [
            deps.onDidChangeActiveColorTheme(() => this.handleThemeChange()),
            deps.onDidChangeConfiguration((event) => this.handleConfigurationChange(event)),
        ];
        controllers.add(this);
    }

    get isEnabled(): boolean {
        return this.enabled;
    }

    /** Resolve a complete, safe bridge payload for the current host state. */
    async resolvePayload(): Promise<QuartoThemePayload> {
        const kind = this.deps.activeThemeKind();
        const isLight = kind === 'light' || kind === 'high-contrast-light';
        const candidateThemeIds = this.candidateThemeIds(isLight);
        let palette: GithubPalette;
        let resolvedThemeId: string | undefined;
        try {
            const resolve = this.deps.resolvePalette ?? resolveActiveThemePalette;
            const outcome = await resolve({
                ...this.deps.themeResolverInputs(),
                candidateThemeIds,
                activeEditorBackground: this.latestEditorBackground,
                isLight,
            });
            if (!outcome.ok) {
                this.warnOnce(`${outcome.reason}: ${outcome.detail}`);
                palette = isLight ? githubLight : githubDark;
            } else {
                palette = outcome.palette;
                resolvedThemeId = outcome.themeLabel ?? outcome.themeId;
            }
        } catch (error) {
            this.warnOnce(errorMessage(error));
            palette = isLight ? githubLight : githubDark;
        }

        let fonts: ResolvedFonts;
        try {
            fonts = this.deps.resolveFonts(this.sourceFsPath);
        } catch (error) {
            this.warnOnce(`font resolution failed: ${errorMessage(error)}`);
            fonts = resolveFontFamilies('', '', '', '');
        }

        const colorOverrides = resolveWorkbenchColorOverrides(
            this.deps.getConfiguration<unknown>('workbench.colorCustomizations'),
            resolvedThemeId ?? candidateThemeIds[0],
        );
        return {
            enabled: this.enabled,
            background: colorOverrides.background ?? palette.background,
            foreground: colorOverrides.foreground ?? palette.foreground,
            roles: { ...palette.roles },
            fontText: fonts.text,
            fontMono: fonts.mono,
        };
    }

    /** Resolve and deliver, dropping any result superseded while awaiting IO. */
    async push(): Promise<boolean> {
        if (this.disposed) return false;
        const generation = ++this.pushGeneration;
        const payload = await this.resolvePayload();
        if (this.disposed || generation !== this.pushGeneration) return false;
        try {
            return (await this.deps.postThemeUpdate(payload)) === true;
        } catch {
            return false;
        }
    }

    setEditorBackground(background: string): void {
        if (this.disposed) return;
        const normalized = normalizeEditorBackground(background);
        if (!normalized || normalized === this.latestEditorBackground) return;
        this.latestEditorBackground = normalized;
        void this.push();
    }

    /** Persist and synchronize the toggle across every open Quarto panel. */
    async setEnabled(applied: boolean): Promise<void> {
        if (this.disposed) return;
        // globalState persistence can settle later; this synchronous value is
        // authoritative for panels constructed during that window.
        lastKnownThemeEnabled = applied;
        const writes: PromiseLike<unknown>[] = [];
        const update = this.deps.context.globalState?.update;
        if (typeof update === 'function') {
            // globalState is shared by every panel. Serialize invocation as
            // well as settlement so an older slow write can never land after
            // a newer toggle intent.
            preferenceWriteTail = preferenceWriteTail.then(async () => {
                try {
                    await update.call(
                        this.deps.context.globalState,
                        QUARTO_THEME_PREFERENCE_KEY,
                        applied,
                    );
                } catch {
                    try {
                        this.deps.output.appendLine(
                            '[theme] Quarto VS Code theme preference failed to persist.',
                        );
                    } catch {
                        // The output facade may already be disposed during shutdown.
                    }
                }
            });
            writes.push(preferenceWriteTail);
        }
        for (const controller of controllers) {
            if (controller.disposed) continue;
            controller.enabled = applied;
            writes.push(controller.push());
        }
        await Promise.allSettled(writes);
    }

    updateSource(sourceFsPath: string): void {
        if (this.disposed || sourceFsPath === this.sourceFsPath) return;
        this.sourceFsPath = sourceFsPath;
        void this.push();
    }

    /** Test-only entry point for the same-kind active-theme listener path. */
    handleActiveThemeChangeForTesting(): void {
        this.handleThemeChange();
    }

    dispose(): void {
        if (this.disposed) return;
        this.disposed = true;
        this.pushGeneration++;
        controllers.delete(this);
        for (const disposable of this.disposables) {
            try { disposable.dispose(); } catch { /* best-effort */ }
        }
        this.disposables.length = 0;
    }

    private candidateThemeIds(isLight: boolean): readonly string[] {
        const autoDetect = this.deps.getConfiguration<boolean>(
            'window.autoDetectColorScheme',
            false,
        ) ?? false;
        const candidates: string[] = [];
        if (autoDetect) {
            const preferredLight = this.deps.getConfiguration<string>(
                'workbench.preferredLightColorTheme',
                '',
            );
            const preferredDark = this.deps.getConfiguration<string>(
                'workbench.preferredDarkColorTheme',
                '',
            );
            const first = isLight ? preferredLight : preferredDark;
            const second = isLight ? preferredDark : preferredLight;
            if (first) candidates.push(first);
            if (second && !candidates.includes(second)) candidates.push(second);
        }
        const active = this.deps.getConfiguration<string>('workbench.colorTheme', '');
        if (active && !candidates.includes(active)) candidates.push(active);
        return candidates;
    }

    private handleThemeChange(): void {
        if (this.disposed) return;
        this.latestEditorBackground = undefined;
        try {
            void Promise.resolve(this.deps.requestThemeContext()).catch(() => undefined);
        } catch {
            // Hidden/disposed shells can drop the best-effort request.
        }
        void this.push();
    }

    private handleConfigurationChange(event: QuartoConfigurationChangeLike): void {
        if (this.disposed) return;
        if (THEME_CONFIGURATION_KEYS.some((key) => event.affectsConfiguration(key))) {
            this.handleThemeChange();
            return;
        }
        const scope = this.deps.sourceConfigurationScope(this.sourceFsPath);
        if (FONT_CONFIGURATION_KEYS.some((key) => event.affectsConfiguration(key, scope))) {
            void this.push();
        }
    }

    private warnOnce(detail: string): void {
        if (this.resolveWarned) return;
        this.resolveWarned = true;
        try {
            this.deps.output.appendLine(
                `[theme] Quarto VS Code theme resolution failed (${detail}); ` +
                'using fallback palette.',
            );
        } catch {
            // The output facade may already be disposed during shutdown.
        }
    }
}

export function readThemePreference(context: QuartoThemeContextLike): boolean {
    const state = context.globalState;
    if (!state || typeof state.get !== 'function') return false;
    const value = state.get<unknown>(QUARTO_THEME_PREFERENCE_KEY);
    return typeof value === 'boolean' ? value : false;
}

function normalizeEditorBackground(value: string): string | undefined {
    const normalized = value.trim().toLowerCase();
    return /^#(?:[0-9a-f]{3,4}|[0-9a-f]{6,8})$/.test(normalized)
        ? normalized
        : undefined;
}

function resolveWorkbenchColorOverrides(
    value: unknown,
    activeThemeId: string | undefined,
): { background?: string; foreground?: string } {
    if (!isRecord(value)) return {};
    const result: { background?: string; foreground?: string } = {};
    layerEditorColors(result, value);
    if (!activeThemeId) return result;
    for (const [key, themed] of Object.entries(value)) {
        if (!/^(\[[^\]]+\])+$/.test(key) || !isRecord(themed)) continue;
        const selectors = [...key.matchAll(/\[([^\]]+)\]/g)];
        if (selectors.some((match) => themeSelectorMatches(match[1], activeThemeId))) {
            layerEditorColors(result, themed);
        }
    }
    return result;
}

function themeSelectorMatches(selector: string, activeThemeId: string): boolean {
    const pattern = selector
        .split('*')
        .map((part) => part.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'))
        .join('.*');
    return new RegExp(`^${pattern}$`).test(activeThemeId);
}

function layerEditorColors(
    target: { background?: string; foreground?: string },
    value: Record<string, unknown>,
): void {
    const background = validCssColor(value['editor.background']);
    const foreground = validCssColor(value['editor.foreground']);
    if (background) target.background = background;
    if (foreground) target.foreground = foreground;
}

function validCssColor(value: unknown): string | undefined {
    if (typeof value !== 'string') return undefined;
    const trimmed = value.trim();
    return CSS_COLOR.test(trimmed) ? trimmed : undefined;
}

function isRecord(value: unknown): value is Record<string, unknown> {
    return value !== null && typeof value === 'object' && !Array.isArray(value);
}

function errorMessage(error: unknown): string {
    return error instanceof Error ? error.message : String(error);
}
