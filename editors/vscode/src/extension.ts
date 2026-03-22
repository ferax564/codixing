// Codixing VS Code / Cursor extension
//
// NOTE: Run `npm install` in editors/vscode/ before building.
// Build:   npm run compile
// Package: npm run package
//
// This extension integrates Codixing into VS Code and Cursor, providing:
//   - Activity bar panel with search results and repo map tree views
//   - Status bar indicator showing index state
//   - Commands for indexing, syncing, searching, and daemon management
//   - LSP client for hover, go-to-definition, references, complexity diagnostics
//   - MCP server registration for Claude Code / Cursor

import * as vscode from 'vscode';
import * as fs from 'fs';
import * as path from 'path';
import { getWorkspaceRoot } from './utils';
import { startLspClient, stopLspClient } from './lsp';
import { SearchResultsProvider } from './views/searchView';
import { RepoMapProvider } from './views/graphView';
import {
    initCommands,
    cmdIndexWorkspace,
    cmdSyncIndex,
    cmdSearch,
    cmdShowRepoMap,
    cmdShowHotspots,
    cmdShowComplexity,
    cmdStartDaemon,
    cmdRegisterMcpServer,
    killDaemon,
} from './commands';

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

let statusBarItem: vscode.StatusBarItem;
let outputChannel: vscode.OutputChannel;

// ---------------------------------------------------------------------------
// Activation
// ---------------------------------------------------------------------------

export function activate(context: vscode.ExtensionContext): void {
    outputChannel = vscode.window.createOutputChannel('Codixing');

    // Status bar item (right side, priority 100 keeps it near the right edge)
    statusBarItem = vscode.window.createStatusBarItem(
        vscode.StatusBarAlignment.Right,
        100,
    );
    statusBarItem.command = 'codixing.search';
    context.subscriptions.push(statusBarItem);

    updateStatusBar();

    // Create tree data providers for the activity bar
    const searchProvider = new SearchResultsProvider();
    const repoMapProvider = new RepoMapProvider();

    const root = getWorkspaceRoot();
    if (root) {
        searchProvider.setWorkspaceRoot(root);
        repoMapProvider.setWorkspaceRoot(root);
    }

    // Register tree views
    context.subscriptions.push(
        vscode.window.createTreeView('codixing.searchResults', {
            treeDataProvider: searchProvider,
            showCollapseAll: true,
        }),
        vscode.window.createTreeView('codixing.repoMap', {
            treeDataProvider: repoMapProvider,
            showCollapseAll: true,
        }),
    );

    // Initialize the commands module with dependencies
    initCommands({
        outputChannel,
        searchProvider,
        repoMapProvider,
        context,
        updateStatusBar,
    });

    // Refresh status bar when workspace folders change or when files are saved
    context.subscriptions.push(
        vscode.workspace.onDidChangeWorkspaceFolders(() => updateStatusBar()),
        vscode.workspace.onDidSaveTextDocument(() => updateStatusBar()),
    );

    // Register commands
    context.subscriptions.push(
        vscode.commands.registerCommand('codixing.indexWorkspace', () =>
            cmdIndexWorkspace(),
        ),
        vscode.commands.registerCommand('codixing.syncIndex', () =>
            cmdSyncIndex(),
        ),
        vscode.commands.registerCommand('codixing.search', () =>
            cmdSearch(),
        ),
        vscode.commands.registerCommand('codixing.showRepoMap', () =>
            cmdShowRepoMap(),
        ),
        vscode.commands.registerCommand('codixing.showHotspots', () =>
            cmdShowHotspots(),
        ),
        vscode.commands.registerCommand('codixing.showComplexity', () =>
            cmdShowComplexity(),
        ),
        vscode.commands.registerCommand('codixing.startDaemon', () =>
            cmdStartDaemon(),
        ),
        vscode.commands.registerCommand('codixing.registerMcpServer', () =>
            cmdRegisterMcpServer(),
        ),
        vscode.commands.registerCommand('codixing.searchView.refresh', () =>
            cmdSearch(),
        ),
        vscode.commands.registerCommand('codixing.searchView.clear', () =>
            searchProvider.clear(),
        ),
    );

    // Auto-start daemon if configured
    const cfg = vscode.workspace.getConfiguration('codixing');
    if (cfg.get<boolean>('autoStartDaemon', false)) {
        cmdStartDaemon();
    }

    // Start LSP client if configured and an index exists
    if (cfg.get<boolean>('lspEnabled', true)) {
        startLspClient(context, outputChannel);
    }
}

export async function deactivate(): Promise<void> {
    await stopLspClient();
    killDaemon();
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

    const indexed = fs.existsSync(path.join(root, '.codixing'));
    statusBarItem.text = indexed
        ? 'Codixing: $(check) indexed'
        : 'Codixing: $(circle-slash) not indexed';
    statusBarItem.tooltip = indexed
        ? 'Codixing index is present. Click to search.'
        : 'No Codixing index found. Click to search (or run "Codixing: Index Workspace").';
    statusBarItem.show();
}
