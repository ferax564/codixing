//! Codixing LSP server — hover, go-to-definition, references, workspace symbols,
//! document sync, live reindex on save, cyclomatic complexity diagnostics,
//! code actions, inlay hints, completions, signature help, rename refactoring,
//! and semantic tokens.
//!
//! # Usage
//!
//! ```bash
//! codixing-lsp --root /path/to/project
//! ```

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use std::path::Path;

use codixing_core::complexity::{count_cyclomatic_complexity, risk_band};
use codixing_core::language::detect_language;
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
                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: Some(vec!["(".to_string(), ",".to_string()]),
                    retrigger_characters: Some(vec![",".to_string()]),
                    ..Default::default()
                }),
                call_hierarchy_provider: Some(CallHierarchyServerCapability::Simple(true)),
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: WorkDoneProgressOptions {
                        work_done_progress: None,
                    },
                })),
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(
                        SemanticTokensOptions {
                            legend: SemanticTokensLegend {
                                token_types: SEMANTIC_TOKEN_TYPES.to_vec(),
                                token_modifiers: SEMANTIC_TOKEN_MODIFIERS.to_vec(),
                            },
                            full: Some(SemanticTokensFullOptions::Bool(true)),
                            range: None,
                            work_done_progress_options: WorkDoneProgressOptions {
                                work_done_progress: None,
                            },
                        },
                    ),
                ),
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

    // -----------------------------------------------------------------------
    // Signature help — show function parameters on "("
    // -----------------------------------------------------------------------
    async fn signature_help(&self, params: SignatureHelpParams) -> Result<Option<SignatureHelp>> {
        // Find the function name being called by scanning back from the cursor.
        let fn_name = self.function_name_before_paren(&params.text_document_position_params);
        let fn_name = match fn_name {
            Some(n) => n,
            None => return Ok(None),
        };

        let engine = self.engine.read().unwrap();
        let sym = match self.best_symbol(
            &engine,
            &fn_name,
            &params.text_document_position_params.text_document.uri,
        ) {
            Some(s) => s,
            None => return Ok(None),
        };

        let signature_text = sym
            .signature
            .as_ref()
            .filter(|s| !s.is_empty())
            .cloned()
            .unwrap_or_else(|| format!("{}()", sym.name));

        // Extract parameter names from the signature.
        let params_list = extract_parameters(&signature_text);
        let parameters: Vec<ParameterInformation> = params_list
            .iter()
            .map(|p| ParameterInformation {
                label: ParameterLabel::Simple(p.clone()),
                documentation: None,
            })
            .collect();

        // Determine active parameter from comma count before cursor.
        let active_param = self
            .count_commas_before_cursor(&params.text_document_position_params)
            .unwrap_or(0);

        Ok(Some(SignatureHelp {
            signatures: vec![SignatureInformation {
                label: signature_text,
                documentation: Some(Documentation::String(format!(
                    "Defined in {} [L{}-L{}]",
                    sym.file_path, sym.line_start, sym.line_end
                ))),
                parameters: Some(parameters),
                active_parameter: Some(active_param),
            }],
            active_signature: Some(0),
            active_parameter: Some(active_param),
        }))
    }

    // -----------------------------------------------------------------------
    // Call hierarchy — cross-language callers/callees
    // -----------------------------------------------------------------------

    async fn prepare_call_hierarchy(
        &self,
        params: CallHierarchyPrepareParams,
    ) -> Result<Option<Vec<CallHierarchyItem>>> {
        let pos = &params.text_document_position_params;
        let uri = &pos.text_document.uri;
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

        // Find the symbol whose range contains the cursor position.
        let cursor_line = pos.position.line as usize;
        let sym = match symbols
            .iter()
            .find(|s| s.line_start <= cursor_line && cursor_line <= s.line_end)
        {
            Some(s) => s,
            None => return Ok(None),
        };

        let sym_abs = engine
            .config()
            .resolve_path(&sym.file_path)
            .unwrap_or_else(|| PathBuf::from(&sym.file_path));
        let sym_uri = match Url::from_file_path(&sym_abs) {
            Ok(u) => u,
            Err(_) => return Ok(None),
        };

        let range = line_range(sym.line_start, sym.line_end);
        let selection_range = line_range(sym.line_start, sym.line_start);

        Ok(Some(vec![CallHierarchyItem {
            name: sym.name.clone(),
            kind: kind_to_lsp(sym.kind.clone()),
            tags: None,
            detail: Some(sym.file_path.clone()),
            uri: sym_uri,
            range,
            selection_range,
            data: None,
        }]))
    }

    async fn incoming_calls(
        &self,
        params: CallHierarchyIncomingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyIncomingCall>>> {
        let item = &params.item;
        let engine = self.engine.read().unwrap();
        let callers = engine.symbol_callers_precise(&item.name, 50);

        if callers.is_empty() {
            return Ok(Some(vec![]));
        }

        let mut results = Vec::new();
        for caller in &callers {
            let abs = engine
                .config()
                .resolve_path(&caller.file_path)
                .unwrap_or_else(|| PathBuf::from(&caller.file_path));
            let uri = match Url::from_file_path(&abs) {
                Ok(u) => u,
                Err(_) => continue,
            };

            // Try to find the enclosing symbol for this caller reference so we
            // can report a proper CallHierarchyItem with a full range.
            let caller_rel = &caller.file_path;
            let file_syms = engine.symbols("", Some(caller_rel)).unwrap_or_default();
            let enclosing = file_syms
                .iter()
                .find(|s| s.line_start <= caller.line && caller.line <= s.line_end);

            let (name, kind, range, selection_range) = if let Some(enc) = enclosing {
                (
                    enc.name.clone(),
                    kind_to_lsp(enc.kind.clone()),
                    line_range(enc.line_start, enc.line_end),
                    line_range(enc.line_start, enc.line_start),
                )
            } else {
                // Fallback: use context as the name, Function as kind.
                let display_name = if caller.context.is_empty() {
                    format!("{}:{}", caller.file_path, caller.line)
                } else {
                    caller.context.clone()
                };
                let r = line_range(caller.line, caller.line);
                (display_name, SymbolKind::FUNCTION, r, r)
            };

            let call_range = line_range(caller.line, caller.line);

            results.push(CallHierarchyIncomingCall {
                from: CallHierarchyItem {
                    name,
                    kind,
                    tags: None,
                    detail: Some(caller.file_path.clone()),
                    uri,
                    range,
                    selection_range,
                    data: None,
                },
                from_ranges: vec![call_range],
            });
        }

        Ok(Some(results))
    }

    async fn outgoing_calls(
        &self,
        params: CallHierarchyOutgoingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyOutgoingCall>>> {
        let item = &params.item;
        let engine = self.engine.read().unwrap();
        let callees = engine.symbol_callees_precise(&item.name, item.detail.as_deref());

        if callees.is_empty() {
            return Ok(Some(vec![]));
        }

        let mut results = Vec::new();
        for callee_name in &callees {
            // Resolve the callee to a Symbol for location info.
            let syms = engine.symbols(callee_name, None).unwrap_or_default();
            let sym = match syms.iter().find(|s| s.name == *callee_name) {
                Some(s) => s,
                None => continue,
            };

            let abs = engine
                .config()
                .resolve_path(&sym.file_path)
                .unwrap_or_else(|| PathBuf::from(&sym.file_path));
            let uri = match Url::from_file_path(&abs) {
                Ok(u) => u,
                Err(_) => continue,
            };

            let range = line_range(sym.line_start, sym.line_end);
            let selection_range = line_range(sym.line_start, sym.line_start);

            results.push(CallHierarchyOutgoingCall {
                to: CallHierarchyItem {
                    name: sym.name.clone(),
                    kind: kind_to_lsp(sym.kind.clone()),
                    tags: None,
                    detail: Some(sym.file_path.clone()),
                    uri,
                    range,
                    selection_range,
                    data: None,
                },
                from_ranges: vec![line_range(
                    item.selection_range.start.line as usize,
                    item.selection_range.start.line as usize,
                )],
            });
        }

        Ok(Some(results))
    }

    // -----------------------------------------------------------------------
    // Rename refactoring
    // -----------------------------------------------------------------------

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        let word = match self.word_at(&params) {
            Some(w) => w,
            None => return Ok(None),
        };

        let engine = self.engine.read().unwrap();
        // Only allow renaming if the word is a known symbol.
        if self
            .best_symbol(&engine, &word, &params.text_document.uri)
            .is_none()
        {
            return Ok(None);
        }

        // Compute the exact range of the word under the cursor.
        let range = match self.word_range_at(&params) {
            Some(r) => r,
            None => return Ok(None),
        };

        Ok(Some(PrepareRenameResponse::RangeWithPlaceholder {
            range,
            placeholder: word,
        }))
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let pos = &params.text_document_position;
        let old_name = match self.word_at(pos) {
            Some(w) => w,
            None => return Ok(None),
        };
        let new_name = &params.new_name;

        // Validate: new name must be a valid identifier.
        if new_name.is_empty()
            || !new_name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Err(tower_lsp::jsonrpc::Error {
                code: tower_lsp::jsonrpc::ErrorCode::InvalidParams,
                message: "New name must be a valid identifier".into(),
                data: None,
            });
        }

        let engine = self.engine.read().unwrap();

        // Use engine's validate_rename for conflict detection.
        let validation = engine.validate_rename(&old_name, new_name, None);

        // Reject ALL conflict types (NameCollision, ImportConflict, Shadowing).
        if !validation.conflicts.is_empty() {
            let msg = validation
                .conflicts
                .iter()
                .map(|c| c.message.as_str())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(tower_lsp::jsonrpc::Error {
                code: tower_lsp::jsonrpc::ErrorCode::InvalidParams,
                message: format!("Rename conflict: {msg}").into(),
                data: None,
            });
        }

        // Build WorkspaceEdit from affected files.
        let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
        let root = engine.config().root.clone();

        for file_rel in &validation.affected_files {
            let abs = root.join(file_rel);
            let content = match std::fs::read_to_string(&abs) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let uri = match Url::from_file_path(&abs) {
                Ok(u) => u,
                Err(_) => continue,
            };

            let mut edits = Vec::new();
            for (line_idx, line) in content.lines().enumerate() {
                let mut col = 0usize;
                while let Some(offset) = line[col..].find(&old_name) {
                    let start_col = col + offset;
                    let end_col = start_col + old_name.len();

                    // Check word boundaries to avoid partial matches.
                    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
                    let before_ok = start_col == 0 || !is_ident(line.as_bytes()[start_col - 1]);
                    let after_ok = end_col >= line.len() || !is_ident(line.as_bytes()[end_col]);

                    if before_ok && after_ok {
                        edits.push(TextEdit {
                            range: Range {
                                start: Position {
                                    line: line_idx as u32,
                                    character: start_col as u32,
                                },
                                end: Position {
                                    line: line_idx as u32,
                                    character: end_col as u32,
                                },
                            },
                            new_text: new_name.to_string(),
                        });
                    }
                    col = end_col;
                }
            }

            if !edits.is_empty() {
                changes.insert(uri, edits);
            }
        }

        Ok(Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }))
    }

    // -----------------------------------------------------------------------
    // Semantic tokens — AST-based syntax highlighting
    // -----------------------------------------------------------------------

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let uri = &params.text_document.uri;
        let abs = match uri.to_file_path() {
            Ok(p) => p,
            Err(_) => return Ok(None),
        };

        let content = {
            let docs = self.open_docs.lock().unwrap();
            docs.get(uri).cloned()
        }
        .or_else(|| std::fs::read_to_string(&abs).ok());

        let content = match content {
            Some(c) => c,
            None => return Ok(None),
        };

        let tokens = compute_semantic_tokens(&abs, content.as_bytes());

        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data: tokens,
        })))
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

    /// Collect the text before the cursor, joining with previous lines for
    /// multi-line call support.  Returns up to 5 preceding lines concatenated.
    fn text_before_cursor(&self, pos: &TextDocumentPositionParams) -> Option<String> {
        let content = {
            let docs = self.open_docs.lock().unwrap();
            docs.get(&pos.text_document.uri).cloned()
        };
        let content = content.or_else(|| {
            let path = pos.text_document.uri.to_file_path().ok()?;
            std::fs::read_to_string(path).ok()
        })?;

        let lines: Vec<&str> = content.lines().collect();
        let cur_line = pos.position.line as usize;
        if cur_line >= lines.len() {
            return None;
        }
        let col = (pos.position.character as usize).min(lines[cur_line].len());
        // Join up to 5 preceding lines to handle multi-line calls.
        let start_line = cur_line.saturating_sub(5);
        let mut buf = String::new();
        for item in lines.iter().take(cur_line).skip(start_line) {
            buf.push_str(item);
            buf.push(' '); // collapse newlines to spaces
        }
        buf.push_str(&lines[cur_line][..col]);
        Some(buf)
    }

    /// Find the unmatched open-paren scanning backwards (depth-aware), then
    /// extract the identifier before it.  Handles `outer(inner(a, b), c|)`.
    fn function_name_before_paren(&self, pos: &TextDocumentPositionParams) -> Option<String> {
        let before = self.text_before_cursor(pos)?;

        // Scan backwards to find the unmatched '(' (depth 0).
        let paren_idx = find_unmatched_open_paren(&before)?;
        let before_paren = before[..paren_idx].trim_end();
        if before_paren.is_empty() {
            return None;
        }

        // Extract the last identifier-like token before the paren.
        let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
        let bytes = before_paren.as_bytes();
        let end = bytes.len();
        let start = (0..end)
            .rev()
            .find(|&i| !is_ident(bytes[i]))
            .map_or(0, |i| i + 1);

        let name = &before_paren[start..end];
        if name.is_empty() {
            None
        } else {
            Some(name.to_string())
        }
    }

    /// Count the number of commas between the enclosing open paren and the
    /// cursor, skipping nested parens/brackets.
    fn count_commas_before_cursor(&self, pos: &TextDocumentPositionParams) -> Option<u32> {
        let before = self.text_before_cursor(pos)?;

        let paren_idx = find_unmatched_open_paren(&before)?;
        let inside = &before[paren_idx + 1..];

        // Count commas at depth 0 (skip nested parens/brackets).
        let mut depth = 0i32;
        let mut commas = 0u32;
        for ch in inside.chars() {
            match ch {
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => depth -= 1,
                ',' if depth == 0 => commas += 1,
                _ => {}
            }
        }
        Some(commas)
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

    /// Extract the word under the cursor and return its Range in the document.
    fn word_range_at(&self, pos: &TextDocumentPositionParams) -> Option<Range> {
        let content = {
            let docs = self.open_docs.lock().unwrap();
            docs.get(&pos.text_document.uri).cloned()
        };
        let content = content.or_else(|| {
            let path = pos.text_document.uri.to_file_path().ok()?;
            std::fs::read_to_string(path).ok()
        })?;
        word_range_at_position(&content, pos.position)
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

/// Extract the Range of the identifier word at a given position.
fn word_range_at_position(content: &str, position: Position) -> Option<Range> {
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
    Some(Range {
        start: Position {
            line: position.line,
            character: start as u32,
        },
        end: Position {
            line: position.line,
            character: end as u32,
        },
    })
}

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
        EntityKind::Variable => SymbolKind::VARIABLE,
        EntityKind::Type => SymbolKind::TYPE_PARAMETER,
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

/// Extract parameter strings from a function signature like "fn foo(a: i32, b: &str) -> bool".
fn extract_parameters(signature: &str) -> Vec<String> {
    // Find the first '(' and matching ')'.
    let open = match signature.find('(') {
        Some(i) => i,
        None => return vec![],
    };
    let mut depth = 0i32;
    let mut close = signature.len();
    for (i, ch) in signature[open..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    close = open + i;
                    break;
                }
            }
            _ => {}
        }
    }
    let inside = &signature[open + 1..close];
    if inside.trim().is_empty() {
        return vec![];
    }
    // Split by ',' at depth 0.
    // Handle `->` as a non-bracket token so it doesn't decrement depth.
    let mut params = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    let bytes = inside.as_bytes();
    for (i, ch) in inside.char_indices() {
        match ch {
            '(' | '<' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            '>'
                // Only decrement for '>' if not preceded by '-' (i.e. skip '->')
                if (i == 0 || bytes[i - 1] != b'-') => {
                    depth -= 1;
                }
            ',' if depth == 0 => {
                let p = inside[start..i].trim();
                if !p.is_empty() {
                    params.push(p.to_string());
                }
                start = i + 1;
            }
            _ => {}
        }
    }
    let last = inside[start..].trim();
    if !last.is_empty() {
        params.push(last.to_string());
    }
    // Filter out `self` / `&self` / `&mut self`.
    params.retain(|p| !p.contains("self"));
    params
}

/// Find the position of the unmatched open-paren scanning backwards.
/// For `outer(inner(a, b), c|)` returns the position of `(` after `outer`.
fn find_unmatched_open_paren(text: &str) -> Option<usize> {
    let mut depth = 0i32;
    for (i, ch) in text.char_indices().rev() {
        match ch {
            ')' | ']' | '}' => depth += 1,
            '(' => {
                if depth == 0 {
                    return Some(i);
                }
                depth -= 1;
            }
            '[' | '{' if depth > 0 => {
                depth -= 1;
            }
            _ => {}
        }
    }
    None
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
        EntityKind::Variable => CompletionItemKind::VARIABLE,
        EntityKind::Type => CompletionItemKind::TYPE_PARAMETER,
    }
}

// count_cyclomatic_complexity and risk_band imported from codixing_core::complexity

// ---------------------------------------------------------------------------
// Semantic token legend constants
// ---------------------------------------------------------------------------

/// Token types registered with the client in `initialize()`.
/// The index in this array is the `token_type` value in `SemanticToken`.
const SEMANTIC_TOKEN_TYPES: &[SemanticTokenType] = &[
    SemanticTokenType::NAMESPACE, // 0
    SemanticTokenType::TYPE,      // 1
    SemanticTokenType::CLASS,     // 2
    SemanticTokenType::ENUM,      // 3
    SemanticTokenType::INTERFACE, // 4
    SemanticTokenType::STRUCT,    // 5
    SemanticTokenType::PARAMETER, // 6
    SemanticTokenType::VARIABLE,  // 7
    SemanticTokenType::PROPERTY,  // 8
    SemanticTokenType::FUNCTION,  // 9
    SemanticTokenType::METHOD,    // 10
    SemanticTokenType::KEYWORD,   // 11
    SemanticTokenType::MODIFIER,  // 12
    SemanticTokenType::COMMENT,   // 13
    SemanticTokenType::STRING,    // 14
    SemanticTokenType::NUMBER,    // 15
    SemanticTokenType::OPERATOR,  // 16
    SemanticTokenType::MACRO,     // 17
];

/// Token modifiers registered with the client.
/// Bit positions correspond to array indices.
const SEMANTIC_TOKEN_MODIFIERS: &[SemanticTokenModifier] = &[
    SemanticTokenModifier::DECLARATION, // bit 0
    SemanticTokenModifier::DEFINITION,  // bit 1
    SemanticTokenModifier::READONLY,    // bit 2
    SemanticTokenModifier::STATIC,      // bit 3
    SemanticTokenModifier::DEPRECATED,  // bit 4
    SemanticTokenModifier::ABSTRACT,    // bit 5
];

const TT_NAMESPACE: u32 = 0;
const TT_TYPE: u32 = 1;
const TT_CLASS: u32 = 2;
const TT_ENUM: u32 = 3;
const TT_INTERFACE: u32 = 4;
const TT_STRUCT: u32 = 5;
const TT_VARIABLE: u32 = 7;
const TT_PROPERTY: u32 = 8;
const TT_FUNCTION: u32 = 9;
const TT_METHOD: u32 = 10;
const TT_KEYWORD: u32 = 11;
const TT_COMMENT: u32 = 13;
const TT_STRING: u32 = 14;
const TT_NUMBER: u32 = 15;
const TT_OPERATOR: u32 = 16;
const TT_MACRO: u32 = 17;

const MOD_DECLARATION: u32 = 1 << 0;
const MOD_DEFINITION: u32 = 1 << 1;

// ---------------------------------------------------------------------------
// Semantic token computation
// ---------------------------------------------------------------------------

/// Map a tree-sitter node kind to a semantic token type index + modifier bitset.
/// Returns `None` for node kinds we don't want to highlight.
fn map_rust_node(kind: &str, parent_kind: &str) -> Option<(u32, u32)> {
    match kind {
        // Keywords
        "fn" | "let" | "mut" | "const" | "static" | "struct" | "enum" | "trait" | "impl"
        | "mod" | "pub" | "use" | "crate" | "self" | "super" | "as" | "for" | "in" | "if"
        | "else" | "match" | "loop" | "while" | "return" | "break" | "continue" | "where"
        | "type" | "async" | "await" | "move" | "ref" | "unsafe" | "extern" | "dyn" => {
            Some((TT_KEYWORD, 0))
        }

        // Comments
        "line_comment" | "block_comment" => Some((TT_COMMENT, 0)),

        // String / char / raw literals
        "string_literal" | "raw_string_literal" | "char_literal" | "string_content" => {
            Some((TT_STRING, 0))
        }

        // Numbers
        "integer_literal" | "float_literal" => Some((TT_NUMBER, 0)),

        // Operators / punctuation with semantic meaning
        "!" | "!=" | "%" | "&" | "&&" | "*" | "+" | "-" | ".." | "..=" | "/" | "<" | "<<"
        | "<=" | "==" | ">" | ">=" | ">>" | "?" | "^" | "|" | "||" | "=>" | "->" => {
            Some((TT_OPERATOR, 0))
        }

        // Identifiers — context-sensitive mapping
        "identifier" => match parent_kind {
            "function_item" | "function_signature_item" => {
                Some((TT_FUNCTION, MOD_DEFINITION | MOD_DECLARATION))
            }
            "call_expression" => Some((TT_FUNCTION, 0)),
            "struct_item" => Some((TT_STRUCT, MOD_DEFINITION | MOD_DECLARATION)),
            "enum_item" => Some((TT_ENUM, MOD_DEFINITION | MOD_DECLARATION)),
            "trait_item" => Some((TT_INTERFACE, MOD_DEFINITION | MOD_DECLARATION)),
            "impl_item" => Some((TT_CLASS, 0)),
            "mod_item" => Some((TT_NAMESPACE, MOD_DEFINITION | MOD_DECLARATION)),
            "field_declaration" => Some((TT_PROPERTY, MOD_DEFINITION)),
            "field_expression" => Some((TT_PROPERTY, 0)),
            "let_declaration" | "parameter" | "closure_parameters" => Some((TT_VARIABLE, 0)),
            "type_identifier" => Some((TT_TYPE, 0)),
            "const_item" | "static_item" => Some((TT_VARIABLE, MOD_DEFINITION)),
            _ => None,
        },

        // Type identifiers
        "type_identifier" => Some((TT_TYPE, 0)),
        "primitive_type" => Some((TT_TYPE, 0)),

        // Field access
        "field_identifier" => Some((TT_PROPERTY, 0)),

        // Macros
        "macro_invocation" => Some((TT_MACRO, 0)),

        // Attribute (e.g., #[derive(...)]) — treat as macro
        "attribute_item" => Some((TT_MACRO, 0)),

        _ => None,
    }
}

/// Map Python tree-sitter node kinds to semantic token types.
fn map_python_node(kind: &str, parent_kind: &str) -> Option<(u32, u32)> {
    match kind {
        // Keywords
        "def" | "class" | "return" | "if" | "else" | "elif" | "for" | "while" | "import"
        | "from" | "as" | "with" | "try" | "except" | "finally" | "raise" | "pass" | "break"
        | "continue" | "and" | "or" | "not" | "in" | "is" | "lambda" | "yield" | "global"
        | "nonlocal" | "assert" | "del" | "async" | "await" => Some((TT_KEYWORD, 0)),

        // Special identifiers
        "true" | "false" | "none" | "True" | "False" | "None" => Some((TT_KEYWORD, 0)),

        // Comments
        "comment" => Some((TT_COMMENT, 0)),

        // Strings
        "string" | "string_start" | "string_content" | "string_end" | "concatenated_string" => {
            Some((TT_STRING, 0))
        }

        // Numbers
        "integer" | "float" => Some((TT_NUMBER, 0)),

        // Operators
        "+" | "-" | "*" | "/" | "//" | "%" | "**" | "==" | "!=" | "<" | ">" | "<=" | ">="
        | "<<" | ">>" | "&" | "|" | "^" | "~" | "->" => Some((TT_OPERATOR, 0)),

        // Identifiers — context-sensitive
        "identifier" => match parent_kind {
            "function_definition" => Some((TT_FUNCTION, MOD_DEFINITION | MOD_DECLARATION)),
            "class_definition" => Some((TT_CLASS, MOD_DEFINITION | MOD_DECLARATION)),
            "call" => Some((TT_FUNCTION, 0)),
            "attribute" => Some((TT_PROPERTY, 0)),
            "parameters" | "default_parameter" | "typed_parameter" | "typed_default_parameter" => {
                Some((TT_VARIABLE, 0))
            }
            _ => None,
        },

        // Decorator
        "decorator" => Some((TT_MACRO, 0)),

        _ => None,
    }
}

/// Map TypeScript/JavaScript tree-sitter node kinds to semantic token types.
fn map_typescript_node(kind: &str, parent_kind: &str) -> Option<(u32, u32)> {
    match kind {
        // Keywords
        "function" | "const" | "let" | "var" | "return" | "if" | "else" | "for" | "while"
        | "do" | "switch" | "case" | "break" | "continue" | "class" | "extends" | "new"
        | "this" | "import" | "export" | "default" | "from" | "as" | "try" | "catch"
        | "finally" | "throw" | "typeof" | "instanceof" | "in" | "of" | "async" | "await"
        | "yield" | "interface" | "type" | "enum" | "implements" | "abstract" | "readonly"
        | "private" | "protected" | "public" | "static" | "void" => Some((TT_KEYWORD, 0)),

        // Comments
        "comment" | "line_comment" | "block_comment" => Some((TT_COMMENT, 0)),

        // Strings
        "string" | "string_fragment" | "template_string" | "template_substitution" => {
            Some((TT_STRING, 0))
        }

        // Numbers
        "number" => Some((TT_NUMBER, 0)),

        // Operators
        "!" | "!=" | "!==" | "%" | "&" | "&&" | "*" | "+" | "-" | "/" | "<" | "<=" | "=="
        | "===" | ">" | ">=" | "??" | "?." | "^" | "|" | "||" | "=>" => Some((TT_OPERATOR, 0)),

        // Identifiers
        "identifier" | "property_identifier" => match parent_kind {
            "function_declaration" | "function" | "arrow_function" => {
                Some((TT_FUNCTION, MOD_DEFINITION))
            }
            "method_definition" => Some((TT_METHOD, MOD_DEFINITION)),
            "call_expression" => Some((TT_FUNCTION, 0)),
            "class_declaration" => Some((TT_CLASS, MOD_DEFINITION)),
            "interface_declaration" => Some((TT_INTERFACE, MOD_DEFINITION)),
            "enum_declaration" => Some((TT_ENUM, MOD_DEFINITION)),
            "member_expression" => Some((TT_PROPERTY, 0)),
            "pair" | "property_signature" => Some((TT_PROPERTY, 0)),
            _ => None,
        },

        "type_identifier" => Some((TT_TYPE, 0)),

        _ => None,
    }
}

/// Map Go tree-sitter node kinds to semantic token types.
fn map_go_node(kind: &str, parent_kind: &str) -> Option<(u32, u32)> {
    match kind {
        // Keywords
        "func" | "var" | "const" | "type" | "struct" | "interface" | "map" | "chan" | "package"
        | "import" | "return" | "if" | "else" | "for" | "range" | "switch" | "case" | "default"
        | "break" | "continue" | "go" | "defer" | "select" | "fallthrough" | "goto" => {
            Some((TT_KEYWORD, 0))
        }

        // Comments
        "comment" => Some((TT_COMMENT, 0)),

        // Strings
        "raw_string_literal" | "interpreted_string_literal" | "rune_literal" => {
            Some((TT_STRING, 0))
        }

        // Numbers
        "int_literal" | "float_literal" | "imaginary_literal" => Some((TT_NUMBER, 0)),

        // Operators
        "+" | "-" | "*" | "/" | "%" | "&" | "|" | "^" | "<<" | ">>" | "==" | "!=" | "<" | ">"
        | "<=" | ">=" | "&&" | "||" | "!" | "<-" | ":=" => Some((TT_OPERATOR, 0)),

        // Identifiers
        "identifier" | "field_identifier" => match parent_kind {
            "function_declaration" => Some((TT_FUNCTION, MOD_DEFINITION)),
            "method_declaration" => Some((TT_METHOD, MOD_DEFINITION)),
            "call_expression" => Some((TT_FUNCTION, 0)),
            "type_spec" => Some((TT_TYPE, MOD_DEFINITION)),
            "field_declaration" => Some((TT_PROPERTY, MOD_DEFINITION)),
            "selector_expression" => Some((TT_PROPERTY, 0)),
            _ => None,
        },

        "type_identifier" => Some((TT_TYPE, 0)),
        "package_identifier" => Some((TT_NAMESPACE, 0)),

        _ => None,
    }
}

/// Compute semantic tokens for a file by walking its tree-sitter AST.
fn compute_semantic_tokens(path: &Path, source: &[u8]) -> Vec<SemanticToken> {
    let lang = match detect_language(path) {
        Some(l) if l.is_tree_sitter() => l,
        _ => return vec![],
    };

    // Select the mapper function based on language.
    let mapper: fn(&str, &str) -> Option<(u32, u32)> = match lang {
        codixing_core::language::Language::Rust => map_rust_node,
        codixing_core::language::Language::Python => map_python_node,
        codixing_core::language::Language::TypeScript
        | codixing_core::language::Language::Tsx
        | codixing_core::language::Language::JavaScript => map_typescript_node,
        codixing_core::language::Language::Go => map_go_node,
        _ => return vec![],
    };

    // Create a fresh tree-sitter parser for this language.
    let registry = codixing_core::language::LanguageRegistry::new();
    let lang_support = match registry.get(lang) {
        Some(ls) => ls,
        None => return vec![],
    };

    let mut parser = tree_sitter::Parser::new();
    if parser
        .set_language(&lang_support.tree_sitter_language())
        .is_err()
    {
        return vec![];
    }

    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return vec![],
    };

    // Collect raw (line, col, length, type, modifiers) tuples, then convert
    // to delta-encoded SemanticTokens.
    let mut raw: Vec<(u32, u32, u32, u32, u32)> = Vec::new();

    // Walk the AST with a cursor.
    let mut cursor = tree.walk();
    walk_tree(&mut cursor, source, mapper, &mut raw);

    // Sort by (line, col) — tree-sitter DFS generally gives this order, but
    // it's important for delta encoding.
    raw.sort_by_key(|&(line, col, _, _, _)| (line, col));

    // Delta-encode.
    let mut prev_line = 0u32;
    let mut prev_start = 0u32;
    let mut tokens = Vec::with_capacity(raw.len());

    for (line, col, length, token_type, modifiers) in raw {
        let delta_line = line - prev_line;
        let delta_start = if delta_line == 0 {
            col - prev_start
        } else {
            col
        };
        tokens.push(SemanticToken {
            delta_line,
            delta_start,
            length,
            token_type,
            token_modifiers_bitset: modifiers,
        });
        prev_line = line;
        prev_start = col;
    }

    tokens
}

/// Recursively walk the tree-sitter AST, collecting semantic tokens.
fn walk_tree(
    cursor: &mut tree_sitter::TreeCursor,
    source: &[u8],
    mapper: fn(&str, &str) -> Option<(u32, u32)>,
    out: &mut Vec<(u32, u32, u32, u32, u32)>,
) {
    loop {
        let node = cursor.node();
        let kind = node.kind();
        let parent_kind = node.parent().map(|p| p.kind()).unwrap_or("");

        if let Some((token_type, modifiers)) = mapper(kind, parent_kind) {
            let start = node.start_position();
            let end = node.end_position();

            // Only emit tokens that fit on a single line (multi-line tokens like
            // block comments are emitted line-by-line for correctness).
            if start.row == end.row {
                let length = (end.column - start.column) as u32;
                if length > 0 {
                    out.push((
                        start.row as u32,
                        start.column as u32,
                        length,
                        token_type,
                        modifiers,
                    ));
                }
            } else {
                // Multi-line token: emit the first line from start to EOL.
                let lines: Vec<&[u8]> = source.split(|&b| b == b'\n').collect();
                for row in start.row..=end.row {
                    if row >= lines.len() {
                        break;
                    }
                    let col_start = if row == start.row { start.column } else { 0 };
                    let col_end = if row == end.row {
                        end.column
                    } else {
                        lines[row].len()
                    };
                    if col_end > col_start {
                        out.push((
                            row as u32,
                            col_start as u32,
                            (col_end - col_start) as u32,
                            token_type,
                            modifiers,
                        ));
                    }
                }
            }

            // For leaf-like tokens (keywords, literals, operators), skip children.
            if !matches!(
                kind,
                "identifier"
                    | "type_identifier"
                    | "field_identifier"
                    | "property_identifier"
                    | "package_identifier"
                    | "primitive_type"
            ) && node.child_count() == 0
            {
                // Leaf node — no children to visit.
                if cursor.goto_next_sibling() {
                    continue;
                }
                // Walk up until we can go to a sibling.
                loop {
                    if !cursor.goto_parent() {
                        return;
                    }
                    if cursor.goto_next_sibling() {
                        break;
                    }
                }
                continue;
            }
        }

        // Recurse into children.
        if cursor.goto_first_child() {
            continue;
        }

        // No children — try next sibling.
        if cursor.goto_next_sibling() {
            continue;
        }

        // Walk up until we can go to a sibling.
        loop {
            if !cursor.goto_parent() {
                return;
            }
            if cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests (P0)
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
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

    // -- extract_parameters --------------------------------------------------

    #[test]
    fn extract_params_from_rust_fn() {
        let params = extract_parameters("pub fn search(query: &str, limit: usize) -> Vec<Result>");
        assert_eq!(params, vec!["query: &str", "limit: usize"]);
    }

    #[test]
    fn extract_params_filters_self() {
        let params =
            extract_parameters("pub fn apply_boost(&mut self, results: &mut [SearchResult])");
        assert_eq!(params, vec!["results: &mut [SearchResult]"]);
    }

    #[test]
    fn extract_params_empty_parens() {
        let params = extract_parameters("fn main()");
        assert!(params.is_empty());
    }

    #[test]
    fn extract_params_nested_generics() {
        let params = extract_parameters("fn foo(map: HashMap<String, Vec<u8>>, count: usize)");
        assert_eq!(
            params,
            vec!["map: HashMap<String, Vec<u8>>", "count: usize"]
        );
    }

    #[test]
    fn extract_params_with_arrow_return_type() {
        // The `->` should NOT be treated as a closing bracket.
        let params = extract_parameters("fn bar(x: i32, f: fn(i32) -> bool)");
        assert_eq!(params, vec!["x: i32", "f: fn(i32) -> bool"]);
    }

    // -- find_unmatched_open_paren -------------------------------------------

    #[test]
    fn unmatched_paren_simple() {
        assert_eq!(find_unmatched_open_paren("foo(a, b"), Some(3));
    }

    #[test]
    fn unmatched_paren_nested() {
        // outer(inner(a, b), c  — cursor after c
        let text = "outer(inner(a, b), c";
        assert_eq!(find_unmatched_open_paren(text), Some(5));
    }

    #[test]
    fn unmatched_paren_nested_closed() {
        // outer(inner(a, b), c)  — all parens matched
        assert_eq!(find_unmatched_open_paren("outer(inner(a, b), c)"), None);
    }

    #[test]
    fn unmatched_paren_no_paren() {
        assert_eq!(find_unmatched_open_paren("hello world"), None);
    }

    // -- word_range_at_position -----------------------------------------------

    #[test]
    fn word_range_simple() {
        let text = "fn hello_world() {}";
        let range = word_range_at_position(
            text,
            Position {
                line: 0,
                character: 5,
            },
        );
        let r = range.unwrap();
        assert_eq!(r.start.line, 0);
        assert_eq!(r.start.character, 3); // 'h' of hello_world
        assert_eq!(r.end.character, 14); // after 'd'
    }

    #[test]
    fn word_range_non_ident_returns_none() {
        let text = "fn foo() {}";
        let range = word_range_at_position(
            text,
            Position {
                line: 0,
                character: 6,
            },
        );
        assert!(range.is_none());
    }

    // -- semantic tokens (unit-level) -----------------------------------------

    #[test]
    fn semantic_tokens_rust_basic() {
        let source = b"fn main() {}\n";
        let tokens = compute_semantic_tokens(Path::new("test.rs"), source);
        // Should produce at least the "fn" keyword token.
        assert!(
            !tokens.is_empty(),
            "expected at least one semantic token for Rust source"
        );
        // First token should be the "fn" keyword at (0, 0, length=2).
        let first = &tokens[0];
        assert_eq!(first.delta_line, 0);
        assert_eq!(first.delta_start, 0);
        assert_eq!(first.length, 2);
        assert_eq!(first.token_type, TT_KEYWORD);
    }

    #[test]
    fn semantic_tokens_rust_function_def() {
        let source = b"fn compute(x: i32) -> bool { true }\n";
        let tokens = compute_semantic_tokens(Path::new("test.rs"), source);
        // Find the "compute" identifier token — should be tagged as FUNCTION + definition.
        let fn_keyword = tokens.iter().find(|t| t.token_type == TT_FUNCTION);
        assert!(
            fn_keyword.is_some(),
            "expected a function token for 'compute'"
        );
        let ft = fn_keyword.unwrap();
        assert_eq!(ft.length, 7); // "compute" has 7 chars
        assert_ne!(ft.token_modifiers_bitset & MOD_DEFINITION, 0);
    }

    #[test]
    fn semantic_tokens_python_basic() {
        let source = b"def hello():\n    pass\n";
        let tokens = compute_semantic_tokens(Path::new("test.py"), source);
        assert!(!tokens.is_empty(), "expected tokens for Python source");
        // "def" should be a keyword.
        let first = &tokens[0];
        assert_eq!(first.token_type, TT_KEYWORD);
        assert_eq!(first.length, 3); // "def"
    }

    #[test]
    fn semantic_tokens_unsupported_lang_returns_empty() {
        let source = b"key: value\n";
        let tokens = compute_semantic_tokens(Path::new("config.yaml"), source);
        assert!(
            tokens.is_empty(),
            "expected empty tokens for unsupported language"
        );
    }

    #[test]
    fn semantic_tokens_delta_encoding() {
        // Two keywords on same line: "fn" at col 0 and "let" at col 18
        let source = b"fn main() { let x = 1; }\n";
        let tokens = compute_semantic_tokens(Path::new("test.rs"), source);
        // Verify that delta encoding works: if two tokens are on the same line,
        // delta_line == 0 and delta_start is the column difference.
        let same_line: Vec<_> = tokens.iter().filter(|t| t.delta_line == 0).collect();
        // At least the second token on line 0 should have delta_line == 0.
        assert!(
            !same_line.is_empty(),
            "expected at least one same-line delta token"
        );
    }

    // -- map_rust_node -------------------------------------------------------

    #[test]
    fn rust_keyword_mapping() {
        assert_eq!(map_rust_node("fn", ""), Some((TT_KEYWORD, 0)));
        assert_eq!(map_rust_node("let", ""), Some((TT_KEYWORD, 0)));
        assert_eq!(map_rust_node("struct", ""), Some((TT_KEYWORD, 0)));
    }

    #[test]
    fn rust_identifier_in_function_item() {
        assert_eq!(
            map_rust_node("identifier", "function_item"),
            Some((TT_FUNCTION, MOD_DEFINITION | MOD_DECLARATION))
        );
    }

    #[test]
    fn rust_identifier_unknown_parent() {
        assert_eq!(map_rust_node("identifier", "unknown_parent"), None);
    }

    #[test]
    fn rust_type_identifier_mapping() {
        assert_eq!(map_rust_node("type_identifier", ""), Some((TT_TYPE, 0)));
    }

    #[test]
    fn rust_comment_mapping() {
        assert_eq!(map_rust_node("line_comment", ""), Some((TT_COMMENT, 0)));
        assert_eq!(map_rust_node("block_comment", ""), Some((TT_COMMENT, 0)));
    }

    #[test]
    fn rust_number_mapping() {
        assert_eq!(map_rust_node("integer_literal", ""), Some((TT_NUMBER, 0)));
        assert_eq!(map_rust_node("float_literal", ""), Some((TT_NUMBER, 0)));
    }

    // -- map_python_node ----------------------------------------------------

    #[test]
    fn python_keyword_mapping() {
        assert_eq!(map_python_node("def", ""), Some((TT_KEYWORD, 0)));
        assert_eq!(map_python_node("class", ""), Some((TT_KEYWORD, 0)));
        assert_eq!(map_python_node("import", ""), Some((TT_KEYWORD, 0)));
    }

    #[test]
    fn python_function_def_mapping() {
        assert_eq!(
            map_python_node("identifier", "function_definition"),
            Some((TT_FUNCTION, MOD_DEFINITION | MOD_DECLARATION))
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
