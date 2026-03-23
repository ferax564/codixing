// Dependency graph webview for the Codixing activity bar panel.
//
// Renders a repo map as an interactive tree view and provides a webview panel
// for full dependency graph visualization.

import * as vscode from 'vscode';
import * as path from 'path';

// ---------------------------------------------------------------------------
// Repo map tree view (activity bar panel)
// ---------------------------------------------------------------------------

export interface RepoMapEntry {
    file: string;
    symbols: string[];
    rank?: number;
}

type MapTreeItem = FileMapNode | SymbolMapNode;

class FileMapNode extends vscode.TreeItem {
    constructor(
        public readonly entry: RepoMapEntry,
        private readonly workspaceRoot: string,
    ) {
        super(
            path.relative(workspaceRoot, entry.file),
            entry.symbols.length > 0
                ? vscode.TreeItemCollapsibleState.Collapsed
                : vscode.TreeItemCollapsibleState.None,
        );
        this.description = entry.rank !== undefined
            ? `rank: ${entry.rank.toFixed(3)}`
            : `${entry.symbols.length} symbols`;
        this.iconPath = vscode.ThemeIcon.File;
        this.resourceUri = vscode.Uri.file(entry.file);
        this.contextValue = 'codixing.repoMapFile';

        // Click to open the file
        this.command = {
            command: 'vscode.open',
            title: 'Open file',
            arguments: [vscode.Uri.file(entry.file)],
        };
    }
}

class SymbolMapNode extends vscode.TreeItem {
    constructor(
        public readonly symbolName: string,
        private readonly filePath: string,
    ) {
        super(symbolName, vscode.TreeItemCollapsibleState.None);
        this.iconPath = new vscode.ThemeIcon('symbol-function');
        this.contextValue = 'codixing.repoMapSymbol';
    }
}

export class RepoMapProvider
    implements vscode.TreeDataProvider<MapTreeItem>
{
    private _onDidChangeTreeData = new vscode.EventEmitter<
        MapTreeItem | undefined | null | void
    >();
    readonly onDidChangeTreeData = this._onDidChangeTreeData.event;

    private entries: RepoMapEntry[] = [];
    private workspaceRoot = '';

    setWorkspaceRoot(root: string): void {
        this.workspaceRoot = root;
    }

    /**
     * Update entries and refresh the tree view.
     */
    setEntries(entries: RepoMapEntry[]): void {
        this.entries = entries;
        this._onDidChangeTreeData.fire();
    }

    clear(): void {
        this.entries = [];
        this._onDidChangeTreeData.fire();
    }

    getTreeItem(element: MapTreeItem): vscode.TreeItem {
        return element;
    }

    getChildren(element?: MapTreeItem): MapTreeItem[] {
        if (!element) {
            return this.entries.map(
                (entry) => new FileMapNode(entry, this.workspaceRoot),
            );
        }

        if (element instanceof FileMapNode) {
            return element.entry.symbols.map(
                (sym) => new SymbolMapNode(sym, element.entry.file),
            );
        }

        return [];
    }
}

// ---------------------------------------------------------------------------
// Parse repo map output
// ---------------------------------------------------------------------------

/**
 * Parse output from `codixing graph --map` or MCP get_repo_map into entries.
 *
 * Expected format (repo map text):
 *   file_path
 *     symbol1
 *     symbol2
 *   ...
 *
 * Or with ranking:
 *   [0.123] file_path
 *     symbol1
 */
export function parseRepoMapOutput(
    output: string,
    workspaceRoot: string,
): RepoMapEntry[] {
    const entries: RepoMapEntry[] = [];
    const lines = output.split('\n');

    let currentEntry: RepoMapEntry | null = null;

    for (const line of lines) {
        if (!line.trim()) {
            continue;
        }

        // Check for a file header line (not indented)
        const rankMatch = line.match(/^\[([0-9.]+)]\s+(.+)$/);
        if (rankMatch) {
            if (currentEntry) {
                entries.push(currentEntry);
            }
            const filePath = rankMatch[2].trim();
            currentEntry = {
                file: path.isAbsolute(filePath)
                    ? filePath
                    : path.join(workspaceRoot, filePath),
                symbols: [],
                rank: parseFloat(rankMatch[1]),
            };
            continue;
        }

        // Non-indented line without rank = file path
        if (!line.startsWith(' ') && !line.startsWith('\t')) {
            if (currentEntry) {
                entries.push(currentEntry);
            }
            const filePath = line.trim();
            currentEntry = {
                file: path.isAbsolute(filePath)
                    ? filePath
                    : path.join(workspaceRoot, filePath),
                symbols: [],
            };
            continue;
        }

        // Indented line = symbol
        if (currentEntry) {
            const sym = line.trim();
            if (sym && sym !== '---') {
                currentEntry.symbols.push(sym);
            }
        }
    }

    if (currentEntry) {
        entries.push(currentEntry);
    }

    return entries;
}

// ---------------------------------------------------------------------------
// Full dependency graph webview
// ---------------------------------------------------------------------------

/**
 * Show an interactive dependency graph in a webview panel.
 * Displays a tree-style visualization of the project structure.
 */
export function showGraphWebview(
    context: vscode.ExtensionContext,
    entries: RepoMapEntry[],
): void {
    const panel = vscode.window.createWebviewPanel(
        'codixing.graph',
        'Codixing: Dependency Graph',
        vscode.ViewColumn.One,
        { enableScripts: true },
    );

    panel.webview.html = buildGraphHtml(entries);
}

function buildGraphHtml(entries: RepoMapEntry[]): string {
    const rows = entries
        .map((entry) => {
            const syms = entry.symbols
                .map((s) => `<li class="symbol">${escapeHtml(s)}</li>`)
                .join('\n');
            const rankBadge =
                entry.rank !== undefined
                    ? `<span class="rank">${entry.rank.toFixed(3)}</span>`
                    : '';
            return `
        <details open>
          <summary class="file">${rankBadge}${escapeHtml(path.basename(entry.file))}
            <span class="filepath">${escapeHtml(entry.file)}</span>
          </summary>
          <ul>${syms}</ul>
        </details>`;
        })
        .join('\n');

    return `<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <title>Codixing Dependency Graph</title>
  <style>
    body {
      font-family: var(--vscode-font-family, monospace);
      font-size: var(--vscode-font-size, 13px);
      color: var(--vscode-foreground);
      background: var(--vscode-editor-background);
      padding: 16px;
    }
    h1 { font-size: 1.4em; margin-bottom: 16px; }
    details { margin: 4px 0; }
    summary.file {
      cursor: pointer;
      padding: 4px 8px;
      border-radius: 3px;
      font-weight: 600;
    }
    summary.file:hover {
      background: var(--vscode-list-hoverBackground);
    }
    .filepath {
      font-weight: 400;
      opacity: 0.6;
      margin-left: 8px;
      font-size: 0.9em;
    }
    .rank {
      display: inline-block;
      background: var(--vscode-badge-background);
      color: var(--vscode-badge-foreground);
      border-radius: 3px;
      padding: 1px 6px;
      font-size: 0.8em;
      margin-right: 8px;
    }
    ul { list-style: none; padding-left: 24px; margin: 4px 0; }
    li.symbol {
      padding: 2px 0;
      opacity: 0.85;
    }
    li.symbol::before {
      content: "fn ";
      opacity: 0.5;
    }
  </style>
</head>
<body>
  <h1>Repository Map</h1>
  <p>${entries.length} files, ${entries.reduce((n, e) => n + e.symbols.length, 0)} symbols</p>
  ${rows}
</body>
</html>`;
}

function escapeHtml(text: string): string {
    return text
        .replace(/&/g, '&amp;')
        .replace(/</g, '&lt;')
        .replace(/>/g, '&gt;')
        .replace(/"/g, '&quot;');
}
