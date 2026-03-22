// LSP client setup for Codixing
//
// Manages the lifecycle of the codixing-lsp language server, providing:
// hover, go-to-definition, references, call hierarchy, workspace/document
// symbols, completions, signature help, inlay hints, and complexity diagnostics.

import * as vscode from 'vscode';
import * as fs from 'fs';
import * as path from 'path';
import {
    LanguageClient,
    LanguageClientOptions,
    ServerOptions,
} from 'vscode-languageclient/node';
import { findBinary, getWorkspaceRoot } from './utils';

let lspClient: LanguageClient | null = null;

/**
 * Start the Codixing LSP client if configured and an index exists.
 */
export async function startLspClient(
    context: vscode.ExtensionContext,
    outputChannel: vscode.OutputChannel,
): Promise<void> {
    const root = getWorkspaceRoot();
    if (!root) {
        return;
    }

    // Only start if an index exists
    if (!fs.existsSync(path.join(root, '.codixing'))) {
        return;
    }

    const lspBin = await findBinary('codixing-lsp', false);
    if (!lspBin) {
        return;
    }

    const cfg = vscode.workspace.getConfiguration('codixing');
    const threshold = cfg.get<number>('complexityThreshold', 6);

    const serverOptions: ServerOptions = {
        run: {
            command: lspBin,
            args: ['--root', root, '--complexity-threshold', String(threshold)],
        },
        debug: {
            command: lspBin,
            args: ['--root', root, '--complexity-threshold', String(threshold)],
        },
    };

    const clientOptions: LanguageClientOptions = {
        documentSelector: [
            { scheme: 'file', language: 'rust' },
            { scheme: 'file', language: 'python' },
            { scheme: 'file', language: 'typescript' },
            { scheme: 'file', language: 'typescriptreact' },
            { scheme: 'file', language: 'javascript' },
            { scheme: 'file', language: 'javascriptreact' },
            { scheme: 'file', language: 'go' },
            { scheme: 'file', language: 'java' },
            { scheme: 'file', language: 'c' },
            { scheme: 'file', language: 'cpp' },
            { scheme: 'file', language: 'csharp' },
            { scheme: 'file', language: 'ruby' },
            { scheme: 'file', language: 'swift' },
            { scheme: 'file', language: 'kotlin' },
            { scheme: 'file', language: 'scala' },
            { scheme: 'file', language: 'php' },
            { scheme: 'file', language: 'zig' },
        ],
        outputChannel,
    };

    lspClient = new LanguageClient(
        'codixing',
        'Codixing LSP',
        serverOptions,
        clientOptions,
    );

    context.subscriptions.push(lspClient);
    await lspClient.start();
    outputChannel.appendLine('[Codixing] LSP client started');
}

/**
 * Stop the LSP client gracefully.
 */
export async function stopLspClient(): Promise<void> {
    if (lspClient) {
        await lspClient.stop();
        lspClient = null;
    }
}
