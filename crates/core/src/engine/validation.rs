use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;

use super::indexing::walk_source_files;
use super::{ConflictKind, Engine, RenameConflict, RenameValidation, StaleReport};

impl Engine {
    /// Check how stale the index is relative to the current filesystem state.
    ///
    /// Uses `stat()` calls only (mtime + size comparison) — no file content is
    /// read, keeping this fast even on large projects.
    pub fn check_staleness(&self) -> StaleReport {
        use std::collections::HashSet;

        // Load stored v2 hashes for mtime+size comparison.
        let old_hashes: HashMap<PathBuf, crate::persistence::FileHashEntry> = self
            .store
            .load_tree_hashes_v2()
            .unwrap_or_default()
            .into_iter()
            .collect();

        // Walk current source files.
        let current_files = match walk_source_files(&self.config.root, &self.config) {
            Ok(f) => f,
            Err(_) => {
                return StaleReport {
                    is_stale: false,
                    modified_files: 0,
                    new_files: 0,
                    deleted_files: 0,
                    last_sync: None,
                    suggestion: "Unable to walk source files.".to_string(),
                };
            }
        };

        let mut modified = 0usize;
        let mut new_files = 0usize;
        let mut seen: HashSet<PathBuf> = HashSet::new();

        for abs_path in &current_files {
            seen.insert(abs_path.clone());

            let (current_mtime, current_size) = fs::metadata(abs_path)
                .map(|m| (m.modified().ok(), m.len()))
                .unwrap_or((None, 0));

            match old_hashes.get(abs_path) {
                Some(cached) => {
                    if cached.file_might_have_changed(current_mtime, current_size) {
                        modified += 1;
                    }
                }
                None => {
                    new_files += 1;
                }
            }
        }

        // Check for deleted files.
        let deleted = old_hashes.keys().filter(|p| !seen.contains(*p)).count();

        // Parse last sync time from stored meta.
        let last_sync = self.store.load_meta().ok().and_then(|meta| {
            meta.last_indexed
                .parse::<u64>()
                .ok()
                .map(|secs| SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
        });

        let is_stale = modified > 0 || new_files > 0 || deleted > 0;

        let suggestion = if !is_stale {
            "Index is up to date.".to_string()
        } else {
            let total_changes = modified + new_files + deleted;
            format!("{total_changes} file(s) changed. Run `codixing sync .` to update the index.")
        };

        StaleReport {
            is_stale,
            modified_files: modified,
            new_files,
            deleted_files: deleted,
            last_sync,
            suggestion,
        }
    }

    /// Validate a proposed rename before applying it.
    ///
    /// Checks for name collisions (the new name already exists as a symbol),
    /// shadowing (the new name exists in files that also contain the old name),
    /// and import conflicts. No files are modified.
    pub fn validate_rename(
        &self,
        old_name: &str,
        new_name: &str,
        file_filter: Option<&str>,
    ) -> RenameValidation {
        let root = &self.config.root;

        // Find all indexed files (via symbol table).
        let all_syms = self.symbols.filter("", None);
        let mut all_files: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for s in &all_syms {
            all_files.insert(s.file_path.clone());
        }

        // Apply file filter.
        let files: Vec<String> = all_files
            .into_iter()
            .filter(|f| file_filter.map(|ff| f.contains(ff)).unwrap_or(true))
            .collect();

        let mut affected_files = Vec::new();
        let mut occurrence_count = 0usize;
        let mut conflicts = Vec::new();

        // Check if new_name already exists as a defined symbol anywhere.
        let existing_new_symbols = self.symbols.filter(new_name, None);
        let exact_new_matches: Vec<_> = existing_new_symbols
            .iter()
            .filter(|s| s.name == new_name)
            .collect();

        for file_rel in &files {
            let abs_path = root.join(file_rel);
            let content = match fs::read_to_string(&abs_path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            if !content.contains(old_name) {
                continue;
            }

            let count = content.matches(old_name).count();
            occurrence_count += count;
            affected_files.push(file_rel.clone());

            // Check: does new_name already exist as a symbol defined in this file?
            for sym in &exact_new_matches {
                if sym.file_path == *file_rel {
                    conflicts.push(RenameConflict {
                        file_path: file_rel.clone(),
                        line: sym.line_start,
                        kind: ConflictKind::NameCollision,
                        message: format!(
                            "Symbol `{new_name}` already defined at line {} in `{file_rel}`",
                            sym.line_start
                        ),
                    });
                }
            }

            // Check: does new_name appear in imports in this file?
            for (line_num, line) in content.lines().enumerate() {
                let trimmed = line.trim();
                let is_import = trimmed.starts_with("use ")
                    || trimmed.starts_with("import ")
                    || trimmed.starts_with("from ")
                    || trimmed.starts_with("require(")
                    || trimmed.starts_with("#include");
                if is_import && line.contains(new_name) {
                    conflicts.push(RenameConflict {
                        file_path: file_rel.clone(),
                        line: line_num + 1,
                        kind: ConflictKind::ImportConflict,
                        message: format!(
                            "Import at line {} in `{file_rel}` already references `{new_name}`",
                            line_num + 1
                        ),
                    });
                }
            }

            // Check: does new_name already appear as a defined symbol in
            // files that also contain old_name? (shadowing)
            for sym in &exact_new_matches {
                if sym.file_path != *file_rel && affected_files.contains(&sym.file_path) {
                    // Only add once per file.
                    let already = conflicts
                        .iter()
                        .any(|c| c.file_path == sym.file_path && c.kind == ConflictKind::Shadowing);
                    if !already {
                        conflicts.push(RenameConflict {
                            file_path: sym.file_path.clone(),
                            line: sym.line_start,
                            kind: ConflictKind::Shadowing,
                            message: format!(
                                "Symbol `{new_name}` exists in `{}` (line {}) which also uses `{old_name}` \
                                 -- renaming may cause shadowing",
                                sym.file_path, sym.line_start
                            ),
                        });
                    }
                }
            }
        }

        // Deduplicate conflicts.
        conflicts.sort_by(|a, b| a.file_path.cmp(&b.file_path).then(a.line.cmp(&b.line)));
        conflicts
            .dedup_by(|a, b| a.file_path == b.file_path && a.line == b.line && a.kind == b.kind);

        let is_safe = conflicts.is_empty();

        RenameValidation {
            is_safe,
            conflicts,
            affected_files,
            occurrence_count,
        }
    }
}
