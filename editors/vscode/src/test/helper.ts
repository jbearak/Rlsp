import * as vscode from 'vscode';
import * as path from 'path';

export function getFixturePath(name: string): string {
    // __dirname is out/test, fixtures are in src/test/fixtures
    return path.join(__dirname, '..', '..', 'src', 'test', 'fixtures', name);
}

export function getFixtureUri(name: string): vscode.Uri {
    return vscode.Uri.file(getFixturePath(name));
}

export async function activate(): Promise<void> {
    const ext = vscode.extensions.getExtension('jbearak.raven');
    if (ext && !ext.isActive) {
        await ext.activate();
    }
}

export async function openDocument(fixtureName: string): Promise<vscode.TextDocument> {
    const uri = getFixtureUri(fixtureName);
    const doc = await vscode.workspace.openTextDocument(uri);
    await vscode.window.showTextDocument(doc);
    // Give LSP time to process
    await sleep(2000);
    return doc;
}

export async function waitForDiagnostics(uri: vscode.Uri, timeout = 10000): Promise<vscode.Diagnostic[]> {
    const start = Date.now();
    while (Date.now() - start < timeout) {
        const diagnostics = vscode.languages.getDiagnostics(uri);
        if (diagnostics.length > 0) {
            return diagnostics;
        }
        await sleep(200);
    }
    return vscode.languages.getDiagnostics(uri);
}

export function sleep(ms: number): Promise<void> {
    return new Promise(resolve => setTimeout(resolve, ms));
}
