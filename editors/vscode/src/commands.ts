// Command implementations for the Codixing VS Code extension
//
// Each command is exported as an async function that can be registered
// in the main extension entry point.

import * as vscode from 'vscode';
import * as cp from 'child_process';
import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';
import {
    findBinary,
    getWorkspaceRoot,
    runCommandSpawn,
    runCommandCollect,
    shellQuote,
    showTextDocument,
} from './utils';
import {
    SearchResultsProvider,
    parseSearchOutput,
} from './views/searchView';
import {
    RepoMapProvider,
    parseRepoMapOutput,
    showGraphWebview,
} from './views/graphView';

// ---------------------------------------------------------------------------
// Module state — set by the extension entry point
// ---------------------------------------------------------------------------

let outputChannel: vscode.OutputChannel;
let searchProvider: SearchResultsProvider;
let repoMapProvider: RepoMapProvider;
let daemonProcess: cp.ChildProcess | null = null;
let extensionContext: vscode.ExtensionContext;
let statusBarUpdater: () => void;

export function initCommands(deps: {
    outputChannel: vscode.OutputChannel;
    searchProvider: SearchResultsProvider;
    repoMapProvider: RepoMapProvider;
    context: vscode.ExtensionContext;
    updateStatusBar: () => void;
}): void {
    outputChannel = deps.outputChannel;
    searchProvider = deps.searchProvider;
    repoMapProvider = deps.repoMapProvider;
    extensionContext = deps.context;
    statusBarUpdater = deps.updateStatusBar;
}

// ---------------------------------------------------------------------------
// Command: Index Workspace
// ---------------------------------------------------------------------------

export async function cmdIndexWorkspace(): Promise<void> {
    const root = getWorkspaceRoot();
    if (!root) {
        vscode.window.showErrorMessage('Codixing: No workspace folder open.');
        return;
    }

    const bin = await findBinary('codixing');
    if (!bin) {
        return;
    }

    const cfg = vscode.workspace.getConfiguration('codixing');
    const embeddings = cfg.get<boolean>('embeddings', false);

    const args = ['init', root];
    if (!embeddings) {
        args.push('--no-embeddings');
    }

    outputChannel.show(true);
    outputChannel.appendLine(`[Codixing] Indexing workspace: ${root}`);
    outputChannel.appendLine(`[Codixing] Running: ${bin} ${args.join(' ')}`);

    const terminal = vscode.window.createTerminal({
        name: 'Codixing: Index',
        cwd: root,
    });
    terminal.sendText(`${shellQuote(bin)} ${args.map(shellQuote).join(' ')}`);
    terminal.show();
}

// ---------------------------------------------------------------------------
// Command: Sync Index
// ---------------------------------------------------------------------------

export async function cmdSyncIndex(): Promise<void> {
    const root = getWorkspaceRoot();
    if (!root) {
        vscode.window.showErrorMessage('Codixing: No workspace folder open.');
        return;
    }

    const bin = await findBinary('codixing');
    if (!bin) {
        return;
    }

    outputChannel.show(true);
    outputChannel.appendLine(`[Codixing] Syncing index at: ${root}`);

    const terminal = vscode.window.createTerminal({
        name: 'Codixing: Sync',
        cwd: root,
    });
    terminal.sendText(`${shellQuote(bin)} sync ${shellQuote(root)}`);
    terminal.show();
}

// ---------------------------------------------------------------------------
// Command: Search (with tree view integration)
// ---------------------------------------------------------------------------

export async function cmdSearch(): Promise<void> {
    const root = getWorkspaceRoot();
    if (!root) {
        vscode.window.showErrorMessage('Codixing: No workspace folder open.');
        return;
    }

    if (!fs.existsSync(path.join(root, '.codixing'))) {
        const action = await vscode.window.showWarningMessage(
            'Codixing: No index found. Index the workspace first.',
            'Index Now',
        );
        if (action === 'Index Now') {
            await cmdIndexWorkspace();
        }
        return;
    }

    const query = await vscode.window.showInputBox({
        prompt: 'Codixing: Enter search query',
        placeHolder: 'e.g. "parse function arguments"',
    });

    if (!query) {
        return;
    }

    const bin = await findBinary('codixing');
    if (!bin) {
        return;
    }

    outputChannel.show(true);
    outputChannel.appendLine(`\n[Codixing] Searching: "${query}"`);
    outputChannel.appendLine('\u2500'.repeat(60));

    // Collect results and populate the tree view
    let fullOutput = '';
    runCommandSpawn(
        bin,
        ['search', query, '--limit', '20'],
        root,
        outputChannel,
        (chunk) => {
            fullOutput += chunk;
            outputChannel.append(chunk);
        },
        () => {
            // Parse results and update the tree view
            const results = parseSearchOutput(fullOutput, root);
            searchProvider.setResults(query, results);
            statusBarUpdater();
        },
    );
}

// ---------------------------------------------------------------------------
// Command: Show Repo Map (tree view + webview)
// ---------------------------------------------------------------------------

export async function cmdShowRepoMap(): Promise<void> {
    const root = getWorkspaceRoot();
    if (!root) {
        vscode.window.showErrorMessage('Codixing: No workspace folder open.');
        return;
    }

    if (!fs.existsSync(path.join(root, '.codixing'))) {
        vscode.window.showWarningMessage(
            'Codixing: No index found. Run "Codixing: Index Workspace" first.',
        );
        return;
    }

    outputChannel.show(true);
    outputChannel.appendLine('\n[Codixing] Generating repo map...');

    // Prefer codixing-mcp for repo map (richer output via JSON-RPC)
    const mcpBin = await findBinary('codixing-mcp', false);

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
        const proc = cp.spawn(mcpBin, ['--root', root], { cwd: root });
        proc.stdout.on('data', (chunk: Buffer) => {
            fullOutput += chunk.toString();
        });
        proc.stderr.on('data', () => {
            // suppress MCP server log noise
        });
        proc.on('close', async () => {
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

            // Update tree view
            const entries = parseRepoMapOutput(text, root);
            repoMapProvider.setEntries(entries);

            // Also show as a text document
            await showTextDocument('Codixing Repo Map', text);
        });
        proc.stdin.write(initRequest + '\n');
        proc.stdin.write(toolRequest + '\n');
        proc.stdin.end();
    } else {
        // Fallback: CLI graph --map
        const bin = await findBinary('codixing');
        if (!bin) {
            return;
        }
        let repoMap = '';
        runCommandSpawn(
            bin,
            ['graph', root, '--map', '--token-budget', '8000'],
            root,
            outputChannel,
            (chunk) => {
                repoMap += chunk;
            },
            async () => {
                const entries = parseRepoMapOutput(repoMap, root);
                repoMapProvider.setEntries(entries);
                await showTextDocument('Codixing Repo Map', repoMap);
            },
        );
    }
}

// ---------------------------------------------------------------------------
// Command: Show Hotspots
// ---------------------------------------------------------------------------

export async function cmdShowHotspots(): Promise<void> {
    const root = getWorkspaceRoot();
    if (!root) {
        vscode.window.showErrorMessage('Codixing: No workspace folder open.');
        return;
    }

    if (!fs.existsSync(path.join(root, '.codixing'))) {
        vscode.window.showWarningMessage(
            'Codixing: No index found. Run "Codixing: Index Workspace" first.',
        );
        return;
    }

    const mcpBin = await findBinary('codixing-mcp', false);
    if (!mcpBin) {
        // Fallback: use codixing CLI if no MCP binary
        const bin = await findBinary('codixing');
        if (!bin) {
            return;
        }
        outputChannel.appendLine('[Codixing] Hotspots require codixing-mcp. Falling back to repo map.');
        await cmdShowRepoMap();
        return;
    }

    outputChannel.show(true);
    outputChannel.appendLine('\n[Codixing] Fetching hotspots...');

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
            name: 'get_hotspots',
            arguments: { limit: 30 },
        },
    });

    let fullOutput = '';
    const proc = cp.spawn(mcpBin, ['--root', root], { cwd: root });
    proc.stdout.on('data', (chunk: Buffer) => {
        fullOutput += chunk.toString();
    });
    proc.stderr.on('data', () => {});
    proc.on('close', async () => {
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

        await showTextDocument('Codixing Hotspots', text);
    });
    proc.stdin.write(initRequest + '\n');
    proc.stdin.write(toolRequest + '\n');
    proc.stdin.end();
}

// ---------------------------------------------------------------------------
// Command: Show Complexity
// ---------------------------------------------------------------------------

export async function cmdShowComplexity(): Promise<void> {
    const root = getWorkspaceRoot();
    if (!root) {
        vscode.window.showErrorMessage('Codixing: No workspace folder open.');
        return;
    }

    if (!fs.existsSync(path.join(root, '.codixing'))) {
        vscode.window.showWarningMessage(
            'Codixing: No index found. Run "Codixing: Index Workspace" first.',
        );
        return;
    }

    // Get the current active editor's file
    const editor = vscode.window.activeTextEditor;
    if (!editor) {
        vscode.window.showWarningMessage('Codixing: No active editor. Open a file first.');
        return;
    }

    const filePath = editor.document.uri.fsPath;
    const relPath = path.relative(root, filePath);

    const mcpBin = await findBinary('codixing-mcp', false);
    if (!mcpBin) {
        vscode.window.showWarningMessage(
            'Codixing: codixing-mcp binary not found. Complexity requires MCP.',
        );
        return;
    }

    outputChannel.show(true);
    outputChannel.appendLine(`\n[Codixing] Getting complexity for: ${relPath}`);

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
            name: 'get_complexity',
            arguments: { file: relPath },
        },
    });

    let fullOutput = '';
    const proc = cp.spawn(mcpBin, ['--root', root], { cwd: root });
    proc.stdout.on('data', (chunk: Buffer) => {
        fullOutput += chunk.toString();
    });
    proc.stderr.on('data', () => {});
    proc.on('close', async () => {
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

        await showTextDocument(`Codixing Complexity: ${relPath}`, text);
    });
    proc.stdin.write(initRequest + '\n');
    proc.stdin.write(toolRequest + '\n');
    proc.stdin.end();
}

// ---------------------------------------------------------------------------
// Command: Start Daemon
// ---------------------------------------------------------------------------

export async function cmdStartDaemon(): Promise<void> {
    const root = getWorkspaceRoot();
    if (!root) {
        vscode.window.showErrorMessage('Codixing: No workspace folder open.');
        return;
    }

    const mcpBin = await findBinary('codixing-mcp');
    if (!mcpBin) {
        return;
    }

    if (daemonProcess && !daemonProcess.killed) {
        vscode.window.showInformationMessage(
            'Codixing: Daemon is already running.',
        );
        return;
    }

    outputChannel.show(true);
    outputChannel.appendLine(
        `[Codixing] Starting daemon: ${mcpBin} --root ${root} --daemon`,
    );

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
        outputChannel.appendLine(`[Codixing] Daemon exited with code ${String(code)}`);
        daemonProcess = null;
        statusBarUpdater();
    });

    vscode.window.showInformationMessage(
        'Codixing: Daemon started. Subsequent MCP calls will be faster.',
    );
}

// ---------------------------------------------------------------------------
// Command: Register MCP Server
// ---------------------------------------------------------------------------

export async function cmdRegisterMcpServer(): Promise<void> {
    const root = getWorkspaceRoot();
    if (!root) {
        vscode.window.showErrorMessage('Codixing: No workspace folder open.');
        return;
    }

    const mcpBin = await findBinary('codixing-mcp');
    if (!mcpBin) {
        return;
    }

    try {
        await registerMcpServer(mcpBin, root);
        vscode.window.showInformationMessage(
            'Codixing: MCP server registered in ~/.claude.json and ~/.cursor/mcp.json',
        );
    } catch (err) {
        vscode.window.showErrorMessage(
            `Codixing: Failed to register MCP server — ${String(err)}`,
        );
    }
}

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
        const dir = path.dirname(configPath);
        if (!fs.existsSync(dir)) {
            fs.mkdirSync(dir, { recursive: true });
        }

        let config: Record<string, unknown> = {};
        if (fs.existsSync(configPath)) {
            try {
                const raw = fs.readFileSync(configPath, 'utf8');
                config = JSON.parse(raw) as Record<string, unknown>;
            } catch {
                config = {};
            }
        }

        if (
            typeof config.mcpServers !== 'object' ||
            config.mcpServers === null
        ) {
            config.mcpServers = {};
        }
        (config.mcpServers as Record<string, unknown>)['codixing'] = entry;

        fs.writeFileSync(
            configPath,
            JSON.stringify(config, null, 2) + '\n',
            'utf8',
        );
        outputChannel.appendLine(`[Codixing] Wrote MCP config: ${configPath}`);
    }
}

/**
 * Kill the daemon process on extension deactivation.
 */
export function killDaemon(): void {
    if (daemonProcess && !daemonProcess.killed) {
        daemonProcess.kill();
    }
}
