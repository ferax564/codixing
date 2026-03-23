// Search results tree view for the Codixing activity bar panel.
//
// Displays search results grouped by file. Each file node expands to show
// individual matches with line numbers. Clicking a match navigates to the
// exact location in the editor.

import * as vscode from 'vscode';
import * as path from 'path';

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

export interface SearchResult {
    file: string;
    line: number;
    snippet: string;
    score?: number;
}

type TreeItem = FileNode | ResultNode;

class FileNode extends vscode.TreeItem {
    constructor(
        public readonly filePath: string,
        public readonly results: SearchResult[],
        private readonly workspaceRoot: string,
    ) {
        super(
            path.relative(workspaceRoot, filePath),
            vscode.TreeItemCollapsibleState.Expanded,
        );
        this.description = `${results.length} match${results.length === 1 ? '' : 'es'}`;
        this.iconPath = vscode.ThemeIcon.File;
        this.resourceUri = vscode.Uri.file(filePath);
        this.contextValue = 'codixing.searchFile';
    }
}

class ResultNode extends vscode.TreeItem {
    constructor(
        public readonly result: SearchResult,
    ) {
        super(
            result.snippet.trim() || `Line ${result.line + 1}`,
            vscode.TreeItemCollapsibleState.None,
        );
        this.description = `L${result.line + 1}`;
        this.iconPath = new vscode.ThemeIcon('symbol-text');
        this.contextValue = 'codixing.searchResult';

        // Click to open the file at the matched line
        this.command = {
            command: 'vscode.open',
            title: 'Go to result',
            arguments: [
                vscode.Uri.file(result.file),
                {
                    selection: new vscode.Range(
                        result.line, 0,
                        result.line, 0,
                    ),
                },
            ],
        };
    }
}

// ---------------------------------------------------------------------------
// Tree data provider
// ---------------------------------------------------------------------------

export class SearchResultsProvider
    implements vscode.TreeDataProvider<TreeItem>
{
    private _onDidChangeTreeData = new vscode.EventEmitter<
        TreeItem | undefined | null | void
    >();
    readonly onDidChangeTreeData = this._onDidChangeTreeData.event;

    private results: SearchResult[] = [];
    private query = '';
    private workspaceRoot = '';

    setWorkspaceRoot(root: string): void {
        this.workspaceRoot = root;
    }

    /**
     * Update results and refresh the tree view.
     */
    setResults(query: string, results: SearchResult[]): void {
        this.query = query;
        this.results = results;
        this._onDidChangeTreeData.fire();
    }

    /**
     * Clear all results.
     */
    clear(): void {
        this.results = [];
        this.query = '';
        this._onDidChangeTreeData.fire();
    }

    getTreeItem(element: TreeItem): vscode.TreeItem {
        return element;
    }

    getChildren(element?: TreeItem): TreeItem[] {
        if (!element) {
            // Root level: group results by file
            if (this.results.length === 0) {
                return [];
            }

            const byFile = new Map<string, SearchResult[]>();
            for (const r of this.results) {
                const existing = byFile.get(r.file) ?? [];
                existing.push(r);
                byFile.set(r.file, existing);
            }

            return Array.from(byFile.entries()).map(
                ([filePath, results]) =>
                    new FileNode(filePath, results, this.workspaceRoot),
            );
        }

        if (element instanceof FileNode) {
            return element.results.map((r) => new ResultNode(r));
        }

        return [];
    }

    /**
     * Return the current query for display purposes.
     */
    getQuery(): string {
        return this.query;
    }
}

// ---------------------------------------------------------------------------
// Parse CLI search output into SearchResult[]
// ---------------------------------------------------------------------------

/**
 * Parse output from `codixing search` into structured results.
 *
 * Expected format (one result per block):
 *   [score] file_path:line
 *   snippet text...
 *   ---
 *
 * Also handles simpler formats like:
 *   file_path:line: snippet text
 */
export function parseSearchOutput(
    output: string,
    workspaceRoot: string,
): SearchResult[] {
    const results: SearchResult[] = [];
    const lines = output.split('\n');

    let currentFile = '';
    let currentLine = 0;
    let currentScore: number | undefined;
    let currentSnippet = '';

    for (const line of lines) {
        // Match header line: [0.85] src/main.rs:42
        const headerMatch = line.match(
            /^\[([0-9.]+)]\s+(.+?):(\d+)\s*$/,
        );
        if (headerMatch) {
            // Save previous result if any
            if (currentFile) {
                results.push({
                    file: path.isAbsolute(currentFile)
                        ? currentFile
                        : path.join(workspaceRoot, currentFile),
                    line: currentLine,
                    snippet: currentSnippet.trim(),
                    score: currentScore,
                });
            }
            currentScore = parseFloat(headerMatch[1]);
            currentFile = headerMatch[2];
            currentLine = parseInt(headerMatch[3], 10) - 1; // 0-indexed
            currentSnippet = '';
            continue;
        }

        // Match simple format: file_path:line: snippet
        const simpleMatch = line.match(/^(.+?):(\d+):\s*(.*)$/);
        if (simpleMatch && !currentFile) {
            results.push({
                file: path.isAbsolute(simpleMatch[1])
                    ? simpleMatch[1]
                    : path.join(workspaceRoot, simpleMatch[1]),
                line: parseInt(simpleMatch[2], 10) - 1,
                snippet: simpleMatch[3],
            });
            continue;
        }

        // Separator
        if (line.trim() === '---' || line.trim() === '') {
            if (currentFile) {
                results.push({
                    file: path.isAbsolute(currentFile)
                        ? currentFile
                        : path.join(workspaceRoot, currentFile),
                    line: currentLine,
                    snippet: currentSnippet.trim(),
                    score: currentScore,
                });
                currentFile = '';
                currentSnippet = '';
                currentScore = undefined;
            }
            continue;
        }

        // Snippet continuation line
        if (currentFile) {
            currentSnippet += (currentSnippet ? '\n' : '') + line;
        }
    }

    // Flush last result
    if (currentFile) {
        results.push({
            file: path.isAbsolute(currentFile)
                ? currentFile
                : path.join(workspaceRoot, currentFile),
            line: currentLine,
            snippet: currentSnippet.trim(),
            score: currentScore,
        });
    }

    return results;
}
