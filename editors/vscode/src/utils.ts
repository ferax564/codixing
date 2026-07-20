// Shared utilities for the Codixing VS Code extension

import * as vscode from 'vscode';
import * as cp from 'child_process';
import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';

/** Return the absolute path of the first workspace folder, or null. */
export function getWorkspaceRoot(): string | null {
    return vscode.workspace.workspaceFolders?.[0]?.uri.fsPath ?? null;
}

export type IndexState = 'missing' | 'incomplete' | 'ready';

function isRealDirectory(target: string): boolean {
    try {
        const stat = fs.lstatSync(target);
        return stat.isDirectory() && !stat.isSymbolicLink();
    } catch {
        return false;
    }
}

function isFile(target: string): boolean {
    try {
        return fs.statSync(target).isFile();
    } catch {
        return false;
    }
}

function isRealFile(target: string): boolean {
    try {
        const stat = fs.lstatSync(target);
        return stat.isFile() && !stat.isSymbolicLink();
    } catch {
        return false;
    }
}

/**
 * Classify the repository's index without starting a long-lived process.
 *
 * Both compatible legacy indexes and atomically published generations need
 * real config.json and meta.json files plus a real Tantivy directory with its
 * committed meta.json. Other sidecars are intentionally outside this cheap
 * structural check. A control directory by itself, including one containing
 * only an unpublished generation, is incomplete rather than ready.
 */
export function getIndexState(root: string): IndexState {
    const controlDir = path.join(root, '.codixing');
    if (!fs.existsSync(controlDir)) {
        return 'missing';
    }
    if (!isRealDirectory(controlDir)) {
        return 'incomplete';
    }

    let indexDir = controlDir;
    const manifestPath = path.join(controlDir, 'active-generation.json');
    if (fs.existsSync(manifestPath)) {
        if (!isFile(manifestPath)) {
            return 'incomplete';
        }
        try {
            const manifest = JSON.parse(fs.readFileSync(manifestPath, 'utf8')) as {
                layout_version?: unknown;
                active?: unknown;
            };
            if (
                manifest.layout_version !== 1 ||
                typeof manifest.active !== 'string' ||
                manifest.active.length > 128 ||
                !/^gen-[A-Za-z0-9-]+$/.test(manifest.active)
            ) {
                return 'incomplete';
            }
            indexDir = path.join(
                controlDir,
                'generations',
                manifest.active,
            );
            if (!isRealDirectory(indexDir)) {
                return 'incomplete';
            }
        } catch {
            return 'incomplete';
        }
    }

    const tantivyDir = path.join(indexDir, 'tantivy');
    return isRealFile(path.join(indexDir, 'config.json')) &&
        isRealFile(path.join(indexDir, 'meta.json')) &&
        isRealDirectory(tantivyDir) &&
        isRealFile(path.join(tantivyDir, 'meta.json'))
        ? 'ready'
        : 'incomplete';
}

/**
 * Find a Codixing binary by checking, in order:
 *   1. The relevant VS Code setting (binaryPath / mcpBinaryPath / lspBinaryPath)
 *   2. PATH via `which` (Unix) or `where` (Windows)
 *   3. Common install locations (~/.cargo/bin, ./target/release)
 *
 * Returns the resolved path, or null if not found.
 * When `showError` is true (default) a user-facing error message is shown.
 */
export async function findBinary(
    name: 'codixing' | 'codixing-mcp' | 'codixing-server' | 'codixing-lsp',
    showError = true,
): Promise<string | null> {
    const cfg = vscode.workspace.getConfiguration('codixing');
    const settingKey =
        name === 'codixing'
            ? 'binaryPath'
            : name === 'codixing-lsp'
              ? 'lspBinaryPath'
              : 'mcpBinaryPath';
    const configured = cfg.get<string>(settingKey, '').trim();

    if (configured && fs.existsSync(configured)) {
        return configured;
    }

    // Try PATH
    const fromPath = await whichBinary(name);
    if (fromPath) {
        return fromPath;
    }

    // Common locations
    const exeSuffix = process.platform === 'win32' ? '.exe' : '';
    const candidates: string[] = [
        path.join(os.homedir(), '.cargo', 'bin', name),
        path.join(os.homedir(), '.cargo', 'bin', name + exeSuffix),
    ];

    // Also check relative to workspace root (development builds)
    const root = getWorkspaceRoot();
    if (root) {
        candidates.push(
            path.join(root, 'target', 'release', name),
            path.join(root, 'target', 'release', name + exeSuffix),
            path.join(root, 'target', 'debug', name),
        );
    }

    for (const candidate of candidates) {
        if (fs.existsSync(candidate)) {
            return candidate;
        }
    }

    if (showError) {
        const action = await vscode.window.showErrorMessage(
            `Codixing: Cannot find "${name}" binary. Install it with \`cargo install codixing\` or set the path in settings.`,
            'Open Settings',
        );
        if (action === 'Open Settings') {
            await vscode.commands.executeCommand(
                'workbench.action.openSettings',
                'codixing.binaryPath',
            );
        }
    }
    return null;
}

/**
 * Resolve a binary name via PATH.
 * Uses execFile (not exec) so the binary name is passed directly.
 */
function whichBinary(name: string): Promise<string | null> {
    return new Promise((resolve) => {
        const cmd = process.platform === 'win32' ? 'where' : 'which';
        cp.execFile(cmd, [name], (err, stdout) => {
            if (err || !stdout.trim()) {
                resolve(null);
            } else {
                resolve(stdout.trim().split('\n')[0].trim());
            }
        });
    });
}

/**
 * Spawn a child process with a literal argv array (no shell).
 * Streams stdout to `onData`, calls `onDone` when the process exits.
 */
export function runCommandSpawn(
    bin: string,
    args: string[],
    cwd: string,
    outputChannel: vscode.OutputChannel,
    onData: (chunk: string) => void,
    onDone?: () => void,
): cp.ChildProcess {
    const proc = cp.spawn(bin, args, { cwd, shell: false });

    proc.stdout.on('data', (chunk: Buffer) => onData(chunk.toString()));
    proc.stderr.on('data', (chunk: Buffer) =>
        outputChannel.append(`[stderr] ${chunk.toString()}`),
    );
    proc.on('error', (err) =>
        outputChannel.appendLine(`[Codixing] Error: ${err.message}`),
    );
    proc.on('close', () => {
        if (onDone) {
            onDone();
        }
    });

    return proc;
}

/**
 * Run a command and collect its full stdout output as a string.
 */
export function runCommandCollect(
    bin: string,
    args: string[],
    cwd: string,
): Promise<string> {
    return new Promise((resolve, reject) => {
        let output = '';
        const proc = cp.spawn(bin, args, { cwd, shell: false });
        proc.stdout.on('data', (chunk: Buffer) => {
            output += chunk.toString();
        });
        proc.stderr.on('data', () => {
            // suppress
        });
        proc.on('error', (err) => reject(err));
        proc.on('close', () => resolve(output));
    });
}

/**
 * Shell-quote a single argument for use in a terminal sendText() call.
 * Only used for the Terminal API (not for child_process).
 */
export function shellQuote(arg: string): string {
    if (process.platform === 'win32') {
        return `"${arg.replace(/"/g, '\\"')}"`;
    }
    return `'${arg.replace(/'/g, "'\\''")}'`;
}

/** Open a new untitled document displaying the given text. */
export async function showTextDocument(
    _title: string,
    content: string,
): Promise<void> {
    const doc = await vscode.workspace.openTextDocument({
        content,
        language: 'markdown',
    });
    await vscode.window.showTextDocument(doc, { preview: false });
}
