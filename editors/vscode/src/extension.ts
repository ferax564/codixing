// CodeForge VS Code / Cursor extension
//
// NOTE: Run `npm install` in editors/vscode/ before building.
// Build:   npm run compile
// Package: npm run package
//
// This extension integrates CodeForge into VS Code and Cursor, providing:
//   - Status bar indicator showing index state
//   - Commands for indexing, syncing, searching, and daemon management
//   - MCP server registration for Claude Code / Cursor

import * as vscode from 'vscode';
import * as cp from 'child_process';
import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

let statusBarItem: vscode.StatusBarItem;
let outputChannel: vscode.OutputChannel;
let daemonProcess: cp.ChildProcess | null = null;

// ---------------------------------------------------------------------------
// Activation
// ---------------------------------------------------------------------------

export function activate(context: vscode.ExtensionContext): void {
    outputChannel = vscode.window.createOutputChannel('CodeForge');

    // Status bar item (right side, priority 100 keeps it near the right edge)
    statusBarItem = vscode.window.createStatusBarItem(
        vscode.StatusBarAlignment.Right,
        100,
    );
    statusBarItem.command = 'codeforge.search';
    context.subscriptions.push(statusBarItem);

    updateStatusBar();

    // Refresh status bar when workspace folders change or when files are saved
    context.subscriptions.push(
        vscode.workspace.onDidChangeWorkspaceFolders(() => updateStatusBar()),
        vscode.workspace.onDidSaveTextDocument(() => updateStatusBar()),
    );

    // Register commands
    context.subscriptions.push(
        vscode.commands.registerCommand('codeforge.indexWorkspace', () =>
            cmdIndexWorkspace(),
        ),
        vscode.commands.registerCommand('codeforge.syncIndex', () =>
            cmdSyncIndex(),
        ),
        vscode.commands.registerCommand('codeforge.search', () =>
            cmdSearch(),
        ),
        vscode.commands.registerCommand('codeforge.showRepoMap', () =>
            cmdShowRepoMap(),
        ),
        vscode.commands.registerCommand('codeforge.startDaemon', () =>
            cmdStartDaemon(),
        ),
        vscode.commands.registerCommand('codeforge.registerMcpServer', () =>
            cmdRegisterMcpServer(),
        ),
    );

    // Auto-start daemon if configured
    const cfg = vscode.workspace.getConfiguration('codeforge');
    if (cfg.get<boolean>('autoStartDaemon', false)) {
        cmdStartDaemon();
    }
}

export function deactivate(): void {
    if (daemonProcess && !daemonProcess.killed) {
        daemonProcess.kill();
    }
}

// ---------------------------------------------------------------------------
// Status bar
// ---------------------------------------------------------------------------

function updateStatusBar(): void {
    const root = getWorkspaceRoot();
    if (!root) {
        statusBarItem.hide();
        return;
    }

    const indexed = fs.existsSync(path.join(root, '.codeforge'));
    statusBarItem.text = indexed
        ? 'CodeForge: $(check) indexed'
        : 'CodeForge: $(circle-slash) not indexed';
    statusBarItem.tooltip = indexed
        ? 'CodeForge index is present. Click to search.'
        : 'No CodeForge index found. Click to search (or run "CodeForge: Index Workspace").';
    statusBarItem.show();
}

// ---------------------------------------------------------------------------
// Command: Index Workspace
// ---------------------------------------------------------------------------

async function cmdIndexWorkspace(): Promise<void> {
    const root = getWorkspaceRoot();
    if (!root) {
        vscode.window.showErrorMessage('CodeForge: No workspace folder open.');
        return;
    }

    const bin = await findBinary('codeforge');
    if (!bin) {
        return;
    }

    const cfg = vscode.workspace.getConfiguration('codeforge');
    const embeddings = cfg.get<boolean>('embeddings', false);

    const args = ['init', root];
    if (!embeddings) {
        args.push('--no-embeddings');
    }

    outputChannel.show(true);
    outputChannel.appendLine(`[CodeForge] Indexing workspace: ${root}`);
    outputChannel.appendLine(`[CodeForge] Running: ${bin} ${args.join(' ')}`);

    const terminal = vscode.window.createTerminal({
        name: 'CodeForge: Index',
        cwd: root,
    });
    terminal.sendText(`${shellQuote(bin)} ${args.map(shellQuote).join(' ')}`);
    terminal.show();
}

// ---------------------------------------------------------------------------
// Command: Sync Index
// ---------------------------------------------------------------------------

async function cmdSyncIndex(): Promise<void> {
    const root = getWorkspaceRoot();
    if (!root) {
        vscode.window.showErrorMessage('CodeForge: No workspace folder open.');
        return;
    }

    const bin = await findBinary('codeforge');
    if (!bin) {
        return;
    }

    outputChannel.show(true);
    outputChannel.appendLine(`[CodeForge] Syncing index at: ${root}`);

    const terminal = vscode.window.createTerminal({
        name: 'CodeForge: Sync',
        cwd: root,
    });
    terminal.sendText(`${shellQuote(bin)} sync ${shellQuote(root)}`);
    terminal.show();
}

// ---------------------------------------------------------------------------
// Command: Search
// ---------------------------------------------------------------------------

async function cmdSearch(): Promise<void> {
    const root = getWorkspaceRoot();
    if (!root) {
        vscode.window.showErrorMessage('CodeForge: No workspace folder open.');
        return;
    }

    if (!fs.existsSync(path.join(root, '.codeforge'))) {
        const action = await vscode.window.showWarningMessage(
            'CodeForge: No index found. Index the workspace first.',
            'Index Now',
        );
        if (action === 'Index Now') {
            await cmdIndexWorkspace();
        }
        return;
    }

    const query = await vscode.window.showInputBox({
        prompt: 'CodeForge: Enter search query',
        placeHolder: 'e.g. "parse function arguments"',
    });

    if (!query) {
        return;
    }

    const bin = await findBinary('codeforge');
    if (!bin) {
        return;
    }

    outputChannel.show(true);
    outputChannel.appendLine(`\n[CodeForge] Searching: "${query}"`);
    outputChannel.appendLine('\u2500'.repeat(60));

    // Use execFile (not exec) to avoid shell injection — query goes directly as arg
    runCommandSpawn(bin, ['search', query, '--limit', '20'], root, (output) => {
        outputChannel.append(output);
        updateStatusBar();
    });
}

// ---------------------------------------------------------------------------
// Command: Show Repo Map
// ---------------------------------------------------------------------------

async function cmdShowRepoMap(): Promise<void> {
    const root = getWorkspaceRoot();
    if (!root) {
        vscode.window.showErrorMessage('CodeForge: No workspace folder open.');
        return;
    }

    if (!fs.existsSync(path.join(root, '.codeforge'))) {
        vscode.window.showWarningMessage(
            'CodeForge: No index found. Run "CodeForge: Index Workspace" first.',
        );
        return;
    }

    outputChannel.show(true);
    outputChannel.appendLine('\n[CodeForge] Generating repo map...');

    // Prefer codeforge-mcp for repo map (richer output via JSON-RPC)
    const mcpBin = await findBinary('codeforge-mcp', false);

    if (mcpBin) {
        const initRequest = JSON.stringify({
            jsonrpc: '2.0',
            id: 0,
            method: 'initialize',
            params: {},
        });
        const toolRequest = JSON.stringify({
            jsonrpc: '2.0',
            id: 1,
            method: 'tools/call',
            params: {
                name: 'get_repo_map',
                arguments: { token_budget: 8000 },
            },
        });

        let fullOutput = '';
        // Use spawn with an args array — no shell expansion, no injection risk
        const proc = cp.spawn(mcpBin, ['--root', root], { cwd: root });
        proc.stdout.on('data', (chunk: Buffer) => {
            fullOutput += chunk.toString();
        });
        proc.stderr.on('data', () => {
            // suppress MCP server log noise
        });
        proc.on('close', async () => {
            // The MCP server writes multiple JSON lines; find the response to id:1
            let text = fullOutput;
            for (const line of fullOutput.split('\n')) {
                try {
                    const resp = JSON.parse(line.trim()) as {
                        id?: number;
                        result?: { content?: Array<{ text?: string }> };
                    };
                    if (resp.id === 1 && resp.result?.content?.[0]?.text) {
                        text = resp.result.content[0].text;
                        break;
                    }
                } catch {
                    // not valid JSON, skip
                }
            }
            await showTextDocument('CodeForge Repo Map', text);
        });
        proc.stdin.write(initRequest + '\n');
        proc.stdin.write(toolRequest + '\n');
        proc.stdin.end();
    } else {
        // Fallback: CLI graph --map
        const bin = await findBinary('codeforge');
        if (!bin) {
            return;
        }
        let repoMap = '';
        runCommandSpawn(
            bin,
            ['graph', root, '--map', '--token-budget', '8000'],
            root,
            (chunk) => {
                repoMap += chunk;
            },
            async () => {
                await showTextDocument('CodeForge Repo Map', repoMap);
            },
        );
    }
}

// ---------------------------------------------------------------------------
// Command: Start Daemon
// ---------------------------------------------------------------------------

async function cmdStartDaemon(): Promise<void> {
    const root = getWorkspaceRoot();
    if (!root) {
        vscode.window.showErrorMessage('CodeForge: No workspace folder open.');
        return;
    }

    const mcpBin = await findBinary('codeforge-mcp');
    if (!mcpBin) {
        return;
    }

    if (daemonProcess && !daemonProcess.killed) {
        vscode.window.showInformationMessage(
            'CodeForge: Daemon is already running.',
        );
        return;
    }

    outputChannel.show(true);
    outputChannel.appendLine(
        `[CodeForge] Starting daemon: ${mcpBin} --root ${root} --daemon`,
    );

    // Use spawn with arg array — no shell expansion
    daemonProcess = cp.spawn(mcpBin, ['--root', root, '--daemon'], {
        cwd: root,
        detached: false,
        stdio: ['ignore', 'pipe', 'pipe'],
    });

    daemonProcess.stdout?.on('data', (chunk: Buffer) => {
        outputChannel.append(`[daemon] ${chunk.toString()}`);
    });
    daemonProcess.stderr?.on('data', (chunk: Buffer) => {
        outputChannel.append(`[daemon] ${chunk.toString()}`);
    });
    daemonProcess.on('exit', (code) => {
        outputChannel.appendLine(`[CodeForge] Daemon exited with code ${String(code)}`);
        daemonProcess = null;
        updateStatusBar();
    });

    vscode.window.showInformationMessage(
        'CodeForge: Daemon started. Subsequent MCP calls will be faster.',
    );
}

// ---------------------------------------------------------------------------
// Command: Register MCP Server
// ---------------------------------------------------------------------------

async function cmdRegisterMcpServer(): Promise<void> {
    const root = getWorkspaceRoot();
    if (!root) {
        vscode.window.showErrorMessage('CodeForge: No workspace folder open.');
        return;
    }

    const mcpBin = await findBinary('codeforge-mcp');
    if (!mcpBin) {
        return;
    }

    try {
        await registerMcpServer(mcpBin, root);
        vscode.window.showInformationMessage(
            'CodeForge: MCP server registered in ~/.claude.json and ~/.cursor/mcp.json',
        );
    } catch (err) {
        vscode.window.showErrorMessage(
            `CodeForge: Failed to register MCP server — ${String(err)}`,
        );
    }
}

// ---------------------------------------------------------------------------
// MCP registration helper
// ---------------------------------------------------------------------------

async function registerMcpServer(mcpBin: string, root: string): Promise<void> {
    const entry = {
        type: 'stdio',
        command: mcpBin,
        args: ['--root', root],
    };

    const targets: string[] = [
        path.join(os.homedir(), '.claude.json'),
        path.join(os.homedir(), '.cursor', 'mcp.json'),
    ];

    for (const configPath of targets) {
        // Ensure parent directory exists
        const dir = path.dirname(configPath);
        if (!fs.existsSync(dir)) {
            fs.mkdirSync(dir, { recursive: true });
        }

        // Read existing config or start fresh
        let config: Record<string, unknown> = {};
        if (fs.existsSync(configPath)) {
            try {
                const raw = fs.readFileSync(configPath, 'utf8');
                config = JSON.parse(raw) as Record<string, unknown>;
            } catch {
                // File exists but is not valid JSON — overwrite with fresh object
                config = {};
            }
        }

        // Ensure mcpServers key exists
        if (
            typeof config.mcpServers !== 'object' ||
            config.mcpServers === null
        ) {
            config.mcpServers = {};
        }
        (config.mcpServers as Record<string, unknown>)['codeforge'] = entry;

        fs.writeFileSync(
            configPath,
            JSON.stringify(config, null, 2) + '\n',
            'utf8',
        );
        outputChannel.appendLine(`[CodeForge] Wrote MCP config: ${configPath}`);
    }
}

// ---------------------------------------------------------------------------
// Binary discovery
// ---------------------------------------------------------------------------

/**
 * Find a CodeForge binary by checking, in order:
 *   1. The relevant VS Code setting (binaryPath / mcpBinaryPath)
 *   2. PATH via `which` (Unix) or `where` (Windows) — using execFile to avoid injection
 *   3. Common install locations (~/.cargo/bin, ./target/release)
 *
 * Returns the resolved path, or null if not found.
 * When `showError` is true (default) a user-facing error message is shown.
 */
async function findBinary(
    name: 'codeforge' | 'codeforge-mcp' | 'codeforge-server',
    showError = true,
): Promise<string | null> {
    const cfg = vscode.workspace.getConfiguration('codeforge');
    const settingKey = name === 'codeforge' ? 'binaryPath' : 'mcpBinaryPath';
    const configured = cfg.get<string>(settingKey, '').trim();

    if (configured && fs.existsSync(configured)) {
        return configured;
    }

    // Try PATH — use execFile (not exec) to prevent shell injection
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
            `CodeForge: Cannot find "${name}" binary. Install it with \`cargo install codeforge\` or set the path in settings.`,
            'Open Settings',
        );
        if (action === 'Open Settings') {
            await vscode.commands.executeCommand(
                'workbench.action.openSettings',
                'codeforge.binaryPath',
            );
        }
    }
    return null;
}

/**
 * Resolve a binary name via PATH.
 * Uses execFile (not exec) so the binary name is passed directly — no shell
 * interpolation, no injection risk.
 */
function whichBinary(name: string): Promise<string | null> {
    return new Promise((resolve) => {
        const cmd = process.platform === 'win32' ? 'where' : 'which';
        // execFile avoids a shell — cmd and name are separate argv entries
        cp.execFile(cmd, [name], (err, stdout) => {
            if (err || !stdout.trim()) {
                resolve(null);
            } else {
                resolve(stdout.trim().split('\n')[0].trim());
            }
        });
    });
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

/** Return the absolute path of the first workspace folder, or null. */
function getWorkspaceRoot(): string | null {
    return vscode.workspace.workspaceFolders?.[0]?.uri.fsPath ?? null;
}

/**
 * Spawn a child process via spawn() with a literal argv array (no shell).
 * Streams stdout to `onData`, calls `onDone` when the process exits.
 */
function runCommandSpawn(
    bin: string,
    args: string[],
    cwd: string,
    onData: (chunk: string) => void,
    onDone?: () => void,
): cp.ChildProcess {
    // spawn() with shell:false (default) passes args directly to execvp —
    // no shell injection possible even if args contain special characters.
    const proc = cp.spawn(bin, args, { cwd, shell: false });

    proc.stdout.on('data', (chunk: Buffer) => onData(chunk.toString()));
    proc.stderr.on('data', (chunk: Buffer) =>
        outputChannel.append(`[stderr] ${chunk.toString()}`),
    );
    proc.on('error', (err) =>
        outputChannel.appendLine(`[CodeForge] Error: ${err.message}`),
    );
    proc.on('close', () => {
        if (onDone) {
            onDone();
        }
        updateStatusBar();
    });

    return proc;
}

/** Open a new untitled document displaying the given text (markdown rendering). */
async function showTextDocument(
    _title: string,
    content: string,
): Promise<void> {
    const doc = await vscode.workspace.openTextDocument({
        content,
        language: 'markdown',
    });
    await vscode.window.showTextDocument(doc, { preview: false });
}

/**
 * Shell-quote a single argument for use in a terminal sendText() call.
 * This is only used for the Terminal API (not for child_process), so the
 * quoting is intentional — the terminal will interpret it.
 */
function shellQuote(arg: string): string {
    if (process.platform === 'win32') {
        return `"${arg.replace(/"/g, '\\"')}"`;
    }
    // Unix single-quote: safe against all special characters
    return `'${arg.replace(/'/g, "'\\''")}'`;
}
