//! Codixing LSP server — hover, go-to-definition, references, workspace symbols,
//! document sync, live reindex on save, cyclomatic complexity diagnostics,
//! code actions, inlay hints, and completions.
//!
//! # Usage
//!
//! ```bash
//! codixing-lsp --root /path/to/project
//! ```

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use codixing_core::complexity::{count_cyclomatic_complexity, risk_band};
use codixing_core::{Engine, EntityKind};
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};
use tracing::{info, warn};

// ---------------------------------------------------------------------------
// Backend
// ---------------------------------------------------------------------------

struct CodixingBackend {
    client: Client,
    engine: Arc<RwLock<Engine>>,
    /// Open document contents tracked via didOpen/didChange/didClose.
    open_docs: Arc<Mutex<HashMap<Url, String>>>,
    /// Complexity threshold for diagnostics (functions >= this trigger a warning).
    complexity_threshold: usize,
}

#[tower_lsp::async_trait]
impl LanguageServer for CodixingBackend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        info!(root = ?params.root_uri, "LSP initialize");
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::FULL),
                        save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                            include_text: Some(true),
                        })),
                        ..Default::default()
                    },
                )),
                code_action_provider: Some(CodeActionProviderCapability::Options(
                    CodeActionOptions {
                        code_action_kinds: Some(vec![CodeActionKind::QUICKFIX]),
                        ..Default::default()
                    },
                )),
                inlay_hint_provider: Some(OneOf::Left(true)),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![".".to_string(), ":".to_string()]),
                    ..Default::default()
                }),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "codixing-lsp".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        info!("LSP initialized — engine ready");
        self.client
            .log_message(MessageType::INFO, "Codixing LSP ready")
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Document sync
    // -----------------------------------------------------------------------

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        let text = params.text_document.text;
        self.open_docs.lock().unwrap().insert(uri.clone(), text);
        self.publish_complexity_diagnostics(&uri).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        if let Some(change) = params.content_changes.into_iter().last() {
            self.open_docs.lock().unwrap().insert(uri, change.text);
        }
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let uri = params.text_document.uri.clone();

        // Update tracked content if provided.
        if let Some(text) = params.text {
            self.open_docs.lock().unwrap().insert(uri.clone(), text);
        }

        // Live reindex the saved file.
        if let Ok(path) = uri.to_file_path() {
            let reindexed = {
                let mut engine = self.engine.write().unwrap();
                engine.reindex_file(&path).is_ok()
            };
            if reindexed {
                info!(?path, "reindexed on save");
            }
        }

        // Refresh diagnostics.
        self.publish_complexity_diagnostics(&uri).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        self.open_docs.lock().unwrap().remove(&uri);
        // Clear diagnostics for closed files.
        self.client.publish_diagnostics(uri, vec![], None).await;
    }

    // -----------------------------------------------------------------------
    // Hover — signature + doc comment for the symbol under the cursor
    // -----------------------------------------------------------------------
    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let pos = &params.text_document_position_params;
        let word = match self.word_at(pos) {
            Some(w) => w,
            None => return Ok(None),
        };

        let engine = self.engine.read().unwrap();
        let sym = match self.best_symbol(&engine, &word, &pos.text_document.uri) {
            Some(s) => s,
            None => return Ok(None),
        };

        let mut md = format!("**{}** _{}_", sym.name, sym.kind);
        if let Some(sig) = &sym.signature {
            md.push_str(&format!("\n```\n{sig}\n```"));
        }
        md.push_str(&format!("\n\n*{}*", sym.file_path));

        Ok(Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: md,
            }),
            range: None,
        }))
    }

    // -----------------------------------------------------------------------
    // Go-to-definition
    // -----------------------------------------------------------------------
    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let pos = &params.text_document_position_params;
        let word = match self.word_at(pos) {
            Some(w) => w,
            None => return Ok(None),
        };

        let engine = self.engine.read().unwrap();
        let sym = match self.best_symbol(&engine, &word, &pos.text_document.uri) {
            Some(s) => s,
            None => return Ok(None),
        };

        let abs = engine
            .config()
            .resolve_path(&sym.file_path)
            .unwrap_or_else(|| PathBuf::from(&sym.file_path));

        let uri = match Url::from_file_path(&abs) {
            Ok(u) => u,
            Err(_) => {
                warn!(path = ?abs, "cannot convert path to URI");
                return Ok(None);
            }
        };

        Ok(Some(GotoDefinitionResponse::Scalar(Location {
            uri,
            range: line_range(sym.line_start, sym.line_end),
        })))
    }

    // -----------------------------------------------------------------------
    // References
    // -----------------------------------------------------------------------
    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let pos = &params.text_document_position;
        let word = match self.word_at(pos) {
            Some(w) => w,
            None => return Ok(None),
        };

        let engine = self.engine.read().unwrap();
        let usages = engine.search_usages(&word, 40).unwrap_or_default();

        let locations: Vec<Location> = usages
            .into_iter()
            .filter_map(|r| {
                let abs = engine
                    .config()
                    .resolve_path(&r.file_path)
                    .unwrap_or_else(|| PathBuf::from(&r.file_path));
                let uri = Url::from_file_path(&abs).ok()?;
                Some(Location {
                    uri,
                    range: line_range(r.line_start as usize, r.line_end as usize),
                })
            })
            .collect();

        Ok(Some(locations))
    }

    // -----------------------------------------------------------------------
    // Workspace symbol
    // -----------------------------------------------------------------------
    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<Vec<SymbolInformation>>> {
        let query = &params.query;
        let engine = self.engine.read().unwrap();
        let symbols = engine.symbols(query, None).unwrap_or_default();

        #[allow(deprecated)]
        let infos: Vec<SymbolInformation> = symbols
            .into_iter()
            .take(50)
            .filter_map(|sym| {
                let abs = engine
                    .config()
                    .resolve_path(&sym.file_path)
                    .unwrap_or_else(|| PathBuf::from(&sym.file_path));
                let uri = Url::from_file_path(&abs).ok()?;
                Some(SymbolInformation {
                    name: sym.name.clone(),
                    kind: kind_to_lsp(sym.kind),
                    tags: None,
                    deprecated: None,
                    location: Location {
                        uri,
                        range: line_range(sym.line_start, sym.line_end),
                    },
                    container_name: sym.scope.last().cloned(),
                })
            })
            .collect();

        Ok(Some(infos))
    }

    // -----------------------------------------------------------------------
    // Document symbol
    // -----------------------------------------------------------------------
    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let uri = params.text_document.uri;
        let abs = match uri.to_file_path() {
            Ok(p) => p,
            Err(_) => return Ok(None),
        };

        let engine = self.engine.read().unwrap();
        let rel = match engine.config().normalize_path(&abs) {
            Some(r) => r,
            None => return Ok(None),
        };

        let symbols = engine.symbols("", Some(&rel)).unwrap_or_default();

        #[allow(deprecated)]
        let doc_syms: Vec<DocumentSymbol> = symbols
            .into_iter()
            .map(|sym| {
                let range = line_range(sym.line_start, sym.line_end);
                DocumentSymbol {
                    name: sym.name,
                    detail: sym.signature,
                    kind: kind_to_lsp(sym.kind),
                    tags: None,
                    deprecated: None,
                    range,
                    selection_range: range,
                    children: None,
                }
            })
            .collect();

        Ok(Some(DocumentSymbolResponse::Nested(doc_syms)))
    }

    // -----------------------------------------------------------------------
    // Code actions — quickfix for complexity diagnostics
    // -----------------------------------------------------------------------
    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        let uri = &params.text_document.uri;
        let mut actions = Vec::new();

        for diag in &params.context.diagnostics {
            if diag.source.as_deref() != Some("codixing") {
                continue;
            }
            if !diag.message.contains("Cyclomatic complexity") {
                continue;
            }

            // Determine the suppress comment based on file extension.
            let ext = uri
                .to_file_path()
                .ok()
                .and_then(|p| p.extension().map(|e| e.to_string_lossy().to_string()))
                .unwrap_or_default();

            // Read the indentation of the function line.
            let indent = {
                let docs = self.open_docs.lock().unwrap();
                docs.get(uri)
                    .and_then(|text| {
                        text.lines()
                            .nth(diag.range.start.line as usize)
                            .map(|line| {
                                let trimmed = line.trim_start();
                                line[..line.len() - trimmed.len()].to_string()
                            })
                    })
                    .unwrap_or_default()
            };

            let suppress_text = match ext.as_str() {
                "rs" => format!(
                    "{indent}#[allow(clippy::cognitive_complexity)] // codixing: suppress\n"
                ),
                "py" => format!("{indent}# noqa: C901  # codixing: suppress\n"),
                _ => format!("{indent}// codixing:allow-complexity\n"),
            };

            let insert_pos = Position {
                line: diag.range.start.line,
                character: 0,
            };

            let edit = WorkspaceEdit {
                changes: Some(HashMap::from([(
                    uri.clone(),
                    vec![TextEdit {
                        range: Range {
                            start: insert_pos,
                            end: insert_pos,
                        },
                        new_text: suppress_text,
                    }],
                )])),
                ..Default::default()
            };

            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: "Suppress complexity warning".to_string(),
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: Some(vec![diag.clone()]),
                edit: Some(edit),
                ..Default::default()
            }));
        }

        if actions.is_empty() {
            Ok(None)
        } else {
            Ok(Some(actions))
        }
    }

    // -----------------------------------------------------------------------
    // Inlay hints — show complexity scores on function signatures
    // -----------------------------------------------------------------------
    async fn inlay_hint(&self, params: InlayHintParams) -> Result<Option<Vec<InlayHint>>> {
        let uri = &params.text_document.uri;
        let abs = match uri.to_file_path() {
            Ok(p) => p,
            Err(_) => return Ok(None),
        };

        let (source, rel) = {
            let engine = self.engine.read().unwrap();
            let rel = match engine.config().normalize_path(&abs) {
                Some(r) => r,
                None => return Ok(None),
            };
            let src = {
                let docs = self.open_docs.lock().unwrap();
                docs.get(uri).cloned()
            }
            .or_else(|| std::fs::read_to_string(&abs).ok());
            match src {
                Some(s) => (s, rel),
                None => return Ok(None),
            }
        };

        let syms = {
            let engine = self.engine.read().unwrap();
            engine.symbols("", Some(&rel)).unwrap_or_default()
        };

        let lines: Vec<&str> = source.lines().collect();
        let visible = params.range;
        let mut hints = Vec::new();

        for sym in &syms {
            if !matches!(sym.kind, EntityKind::Function | EntityKind::Method) {
                continue;
            }
            let line = sym.line_start as u32;
            if line < visible.start.line || line > visible.end.line {
                continue;
            }

            let cc = count_cyclomatic_complexity(&lines, sym.line_start, sym.line_end);
            if cc < 2 {
                continue;
            }

            let line_len = lines
                .get(sym.line_start)
                .map(|l| l.len() as u32)
                .unwrap_or(0);

            hints.push(InlayHint {
                position: Position {
                    line,
                    character: line_len,
                },
                label: InlayHintLabel::String(format!(" CC:{cc} ({})", risk_band(cc))),
                kind: None,
                tooltip: Some(InlayHintTooltip::String(format!(
                    "Cyclomatic complexity: {cc}. Risk: {}.",
                    risk_band(cc)
                ))),
                padding_left: Some(true),
                padding_right: None,
                text_edits: None,
                data: None,
            });
        }

        Ok(Some(hints))
    }

    // -----------------------------------------------------------------------
    // Completions — symbol names from the index
    // -----------------------------------------------------------------------
    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let prefix = match self.prefix_at(&params.text_document_position) {
            Some(p) if p.len() >= 2 => p,
            _ => return Ok(None),
        };

        let engine = self.engine.read().unwrap();
        let current_file = params
            .text_document_position
            .text_document
            .uri
            .to_file_path()
            .ok()
            .and_then(|p| engine.config().normalize_path(&p));

        let symbols = engine.symbol_table().lookup_prefix(&prefix);
        let mut seen = std::collections::HashSet::new();
        let mut items: Vec<CompletionItem> = Vec::new();

        for sym in symbols {
            if !seen.insert(sym.name.clone()) {
                continue;
            }
            let same_file = current_file.as_deref() == Some(sym.file_path.as_str());
            items.push(CompletionItem {
                label: sym.name.clone(),
                kind: Some(kind_to_completion_kind(sym.kind)),
                detail: Some(sym.file_path.clone()),
                documentation: sym.signature.as_ref().map(|s| {
                    Documentation::MarkupContent(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: format!("```\n{s}\n```"),
                    })
                }),
                sort_text: Some(format!("{}{}", if same_file { "0" } else { "1" }, sym.name)),
                ..Default::default()
            });

            if items.len() >= 50 {
                break;
            }
        }

        if items.is_empty() {
            Ok(None)
        } else {
            Ok(Some(CompletionResponse::Array(items)))
        }
    }
}

// ---------------------------------------------------------------------------
// Complexity diagnostics (P3)
// ---------------------------------------------------------------------------

impl CodixingBackend {
    /// Find the best-matching symbol for `word`, preferring exact name matches
    /// in the current file, then exact matches globally, then substring matches.
    fn best_symbol(&self, engine: &Engine, word: &str, uri: &Url) -> Option<codixing_core::Symbol> {
        let current_file = uri
            .to_file_path()
            .ok()
            .and_then(|p| engine.config().normalize_path(&p));

        let all = engine.symbols(word, None).unwrap_or_default();
        if all.is_empty() {
            return None;
        }

        // Prefer exact name match in the current file.
        if let Some(ref rel) = current_file {
            if let Some(s) = all.iter().find(|s| s.name == word && s.file_path == *rel) {
                return Some(s.clone());
            }
        }

        // Exact name match globally (definition-like kinds first).
        let mut exact: Vec<_> = all.iter().filter(|s| s.name == word).collect();
        exact.sort_by_key(|s| match s.kind {
            EntityKind::Function
            | EntityKind::Method
            | EntityKind::Struct
            | EntityKind::Class
            | EntityKind::Enum
            | EntityKind::Trait
            | EntityKind::Interface
            | EntityKind::TypeAlias => 0,
            _ => 1,
        });
        if let Some(s) = exact.first() {
            return Some((*s).clone());
        }

        // Fall back to first substring match.
        all.into_iter().next()
    }

    /// Extract the identifier prefix up to (but not past) the cursor position.
    /// Used for completions — returns the partial word being typed.
    fn prefix_at(&self, pos: &TextDocumentPositionParams) -> Option<String> {
        let content = {
            let docs = self.open_docs.lock().unwrap();
            docs.get(&pos.text_document.uri).cloned()
        };
        let content = content.or_else(|| {
            let path = pos.text_document.uri.to_file_path().ok()?;
            std::fs::read_to_string(path).ok()
        })?;
        prefix_at_position(&content, pos.position)
    }

    /// Extract the word under the cursor from either the tracked open document
    /// or by reading the file from disk.
    fn word_at(&self, pos: &TextDocumentPositionParams) -> Option<String> {
        let content = {
            let docs = self.open_docs.lock().unwrap();
            docs.get(&pos.text_document.uri).cloned()
        };
        let content = content.or_else(|| {
            let path = pos.text_document.uri.to_file_path().ok()?;
            std::fs::read_to_string(path).ok()
        })?;
        word_at_position(&content, pos.position)
    }

    /// Compute cyclomatic complexity for each function in the file and publish
    /// diagnostics for those exceeding the threshold.
    async fn publish_complexity_diagnostics(&self, uri: &Url) {
        let abs = match uri.to_file_path() {
            Ok(p) => p,
            Err(_) => return,
        };

        let (source, rel) = {
            let engine = self.engine.read().unwrap();
            let rel = match engine.config().normalize_path(&abs) {
                Some(r) => r,
                None => return,
            };
            let src = {
                let docs = self.open_docs.lock().unwrap();
                docs.get(uri).cloned()
            }
            .or_else(|| std::fs::read_to_string(&abs).ok());
            match src {
                Some(s) => (s, rel),
                None => return,
            }
        };

        let syms = {
            let engine = self.engine.read().unwrap();
            engine.symbols("", Some(&rel)).unwrap_or_default()
        };

        let fns: Vec<_> = syms
            .iter()
            .filter(|s| matches!(s.kind, EntityKind::Function | EntityKind::Method))
            .collect();

        let lines: Vec<&str> = source.lines().collect();
        let mut diagnostics = Vec::new();

        for sym in &fns {
            let cc = count_cyclomatic_complexity(&lines, sym.line_start, sym.line_end);
            if cc >= self.complexity_threshold {
                let severity = if cc >= 26 {
                    DiagnosticSeverity::ERROR
                } else if cc >= 11 {
                    DiagnosticSeverity::WARNING
                } else {
                    DiagnosticSeverity::INFORMATION
                };
                diagnostics.push(Diagnostic {
                    range: line_range(sym.line_start, sym.line_start),
                    severity: Some(severity),
                    source: Some("codixing".to_string()),
                    code: Some(NumberOrString::String("complexity".to_string())),
                    message: format!(
                        "Cyclomatic complexity {} ({}) — {}",
                        cc,
                        risk_band(cc),
                        sym.name
                    ),
                    ..Default::default()
                });
            }
        }

        self.client
            .publish_diagnostics(uri.clone(), diagnostics, None)
            .await;
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract the identifier word at a given position within a text buffer.
fn word_at_position(content: &str, position: Position) -> Option<String> {
    let line = content.lines().nth(position.line as usize)?;
    let col = position.character as usize;
    let bytes = line.as_bytes();
    if col >= bytes.len() {
        return None;
    }
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    if !is_ident(bytes[col]) {
        return None;
    }
    let start = (0..col)
        .rev()
        .find(|&i| !is_ident(bytes[i]))
        .map_or(0, |i| i + 1);
    let end = (col + 1..bytes.len())
        .find(|&i| !is_ident(bytes[i]))
        .unwrap_or(bytes.len());
    Some(line[start..end].to_string())
}

fn line_range(start: usize, end: usize) -> Range {
    Range {
        start: Position {
            line: start as u32,
            character: 0,
        },
        end: Position {
            line: end as u32,
            character: 0,
        },
    }
}

fn kind_to_lsp(kind: EntityKind) -> SymbolKind {
    match kind {
        EntityKind::Function => SymbolKind::FUNCTION,
        EntityKind::Method => SymbolKind::METHOD,
        EntityKind::Class => SymbolKind::CLASS,
        EntityKind::Struct => SymbolKind::STRUCT,
        EntityKind::Enum => SymbolKind::ENUM,
        EntityKind::Interface => SymbolKind::INTERFACE,
        EntityKind::Trait => SymbolKind::INTERFACE,
        EntityKind::TypeAlias => SymbolKind::TYPE_PARAMETER,
        EntityKind::Constant | EntityKind::Static => SymbolKind::CONSTANT,
        EntityKind::Module | EntityKind::Namespace => SymbolKind::MODULE,
        EntityKind::Import => SymbolKind::PACKAGE,
        EntityKind::Impl => SymbolKind::OBJECT,
    }
}

/// Extract the identifier prefix ending at the cursor (not past it).
fn prefix_at_position(content: &str, position: Position) -> Option<String> {
    let line = content.lines().nth(position.line as usize)?;
    let col = position.character as usize;
    if col == 0 {
        return None;
    }
    let bytes = line.as_bytes();
    let end = col.min(bytes.len());
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    // Walk backwards from just before cursor to find identifier start.
    if end == 0 || !is_ident(bytes[end - 1]) {
        return None;
    }
    let start = (0..end)
        .rev()
        .find(|&i| !is_ident(bytes[i]))
        .map_or(0, |i| i + 1);
    let prefix = &line[start..end];
    if prefix.is_empty() {
        None
    } else {
        Some(prefix.to_string())
    }
}

fn kind_to_completion_kind(kind: EntityKind) -> CompletionItemKind {
    match kind {
        EntityKind::Function => CompletionItemKind::FUNCTION,
        EntityKind::Method => CompletionItemKind::METHOD,
        EntityKind::Class => CompletionItemKind::CLASS,
        EntityKind::Struct => CompletionItemKind::STRUCT,
        EntityKind::Enum => CompletionItemKind::ENUM,
        EntityKind::Interface | EntityKind::Trait => CompletionItemKind::INTERFACE,
        EntityKind::TypeAlias => CompletionItemKind::TYPE_PARAMETER,
        EntityKind::Constant | EntityKind::Static => CompletionItemKind::CONSTANT,
        EntityKind::Module | EntityKind::Namespace | EntityKind::Import => {
            CompletionItemKind::MODULE
        }
        EntityKind::Impl => CompletionItemKind::CLASS,
    }
}

// count_cyclomatic_complexity and risk_band imported from codixing_core::complexity

// ---------------------------------------------------------------------------
// Tests (P0)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- word_at_position ---------------------------------------------------

    #[test]
    fn word_at_simple_identifier() {
        let text = "fn hello_world() {}";
        let word = word_at_position(
            text,
            Position {
                line: 0,
                character: 5,
            },
        );
        assert_eq!(word.as_deref(), Some("hello_world"));
    }

    #[test]
    fn word_at_start_of_line() {
        let text = "Parser::new()";
        let word = word_at_position(
            text,
            Position {
                line: 0,
                character: 0,
            },
        );
        assert_eq!(word.as_deref(), Some("Parser"));
    }

    #[test]
    fn word_at_non_ident_returns_none() {
        let text = "fn foo() {}";
        let word = word_at_position(
            text,
            Position {
                line: 0,
                character: 6,
            },
        );
        // character 6 is '(' — not an identifier
        assert!(word.is_none());
    }

    #[test]
    fn word_at_past_end_of_line_returns_none() {
        let text = "short";
        let word = word_at_position(
            text,
            Position {
                line: 0,
                character: 99,
            },
        );
        assert!(word.is_none());
    }

    #[test]
    fn word_at_multiline_picks_correct_line() {
        let text = "first_line\nsecond_line\nthird_line";
        let word = word_at_position(
            text,
            Position {
                line: 1,
                character: 3,
            },
        );
        assert_eq!(word.as_deref(), Some("second_line"));
    }

    // -- kind_to_lsp --------------------------------------------------------

    #[test]
    fn entity_kind_maps_function() {
        assert_eq!(kind_to_lsp(EntityKind::Function), SymbolKind::FUNCTION);
    }

    #[test]
    fn entity_kind_maps_trait_to_interface() {
        assert_eq!(kind_to_lsp(EntityKind::Trait), SymbolKind::INTERFACE);
    }

    // -- line_range ---------------------------------------------------------

    #[test]
    fn line_range_produces_correct_positions() {
        let r = line_range(10, 20);
        assert_eq!(r.start.line, 10);
        assert_eq!(r.end.line, 20);
        assert_eq!(r.start.character, 0);
    }

    // CC tests are in codixing_core::complexity::tests

    // -- prefix_at_position -------------------------------------------------

    #[test]
    fn prefix_at_mid_word() {
        let text = "fn hello_world() {}";
        // cursor at col 11 → "hello_wo" (h=3..o=11, exclusive end)
        let prefix = prefix_at_position(
            text,
            Position {
                line: 0,
                character: 11,
            },
        );
        assert_eq!(prefix.as_deref(), Some("hello_wo"));
    }

    #[test]
    fn prefix_at_end_of_word() {
        let text = "fn hello() {}";
        let prefix = prefix_at_position(
            text,
            Position {
                line: 0,
                character: 8,
            },
        );
        assert_eq!(prefix.as_deref(), Some("hello"));
    }

    #[test]
    fn prefix_at_start_returns_none() {
        let text = "fn hello() {}";
        let prefix = prefix_at_position(
            text,
            Position {
                line: 0,
                character: 0,
            },
        );
        assert!(prefix.is_none());
    }

    #[test]
    fn prefix_at_non_ident_returns_none() {
        let text = "fn foo() {}";
        let prefix = prefix_at_position(
            text,
            Position {
                line: 0,
                character: 7,
            },
        );
        // character 7 is ')' — cursor after non-ident
        assert!(prefix.is_none());
    }

    // -- kind_to_completion_kind -------------------------------------------

    #[test]
    fn completion_kind_maps_function() {
        assert_eq!(
            kind_to_completion_kind(EntityKind::Function),
            CompletionItemKind::FUNCTION
        );
    }

    #[test]
    fn completion_kind_maps_struct() {
        assert_eq!(
            kind_to_completion_kind(EntityKind::Struct),
            CompletionItemKind::STRUCT
        );
    }

    #[test]
    fn completion_kind_maps_trait_to_interface() {
        assert_eq!(
            kind_to_completion_kind(EntityKind::Trait),
            CompletionItemKind::INTERFACE
        );
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .init();

    let args: Vec<String> = std::env::args().collect();

    // Parse --root <path>
    let root = args
        .windows(2)
        .find(|w| w[0] == "--root")
        .map(|w| PathBuf::from(&w[1]))
        .unwrap_or_else(|| std::env::current_dir().expect("cannot determine cwd"));

    // Parse --complexity-threshold <N> (default 6 = moderate+)
    let complexity_threshold = args
        .windows(2)
        .find(|w| w[0] == "--complexity-threshold")
        .and_then(|w| w[1].parse().ok())
        .unwrap_or(6);

    info!(?root, complexity_threshold, "opening codixing engine");
    let engine =
        Engine::open(&root).expect("failed to open codixing index — run `codixing init` first");
    let engine = Arc::new(RwLock::new(engine));
    let open_docs = Arc::new(Mutex::new(HashMap::new()));

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::build(|client| CodixingBackend {
        client,
        engine,
        open_docs,
        complexity_threshold,
    })
    .finish();

    Server::new(stdin, stdout, socket).serve(service).await;
}
