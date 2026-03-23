//! Federation management MCP tools.
//!
//! Provides tools for initializing, managing, and querying federated
//! configurations that span multiple Codixing-indexed projects.

use std::path::PathBuf;

use serde_json::Value;

use codixing_core::{
    FederatedEngine, FederationConfig, SearchQuery, discover_projects, to_federation_config,
};

/// `federation_init` — create a template federation config file.
pub fn call_federation_init(args: &Value) -> (String, bool) {
    let path = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) => PathBuf::from(p),
        None => {
            return (
                "Missing required parameter 'path' (file path for the new config).".to_string(),
                true,
            );
        }
    };

    match FederationConfig::init_template(&path) {
        Ok(()) => (
            format!(
                "Created federation config template at `{}`.\nEdit it to add project roots.",
                path.display()
            ),
            false,
        ),
        Err(e) => (format!("Failed to create federation config: {e}"), true),
    }
}

/// `federation_add_project` — add a project to an existing federation config.
pub fn call_federation_add_project(args: &Value) -> (String, bool) {
    let config_path = match args.get("config").and_then(|v| v.as_str()) {
        Some(p) => PathBuf::from(p),
        None => {
            return (
                "Missing required parameter 'config' (path to federation config file).".to_string(),
                true,
            );
        }
    };

    let project_path = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) => {
            // Canonicalize to absolute path so configs are portable across cwd changes.
            match PathBuf::from(p).canonicalize() {
                Ok(abs) => abs,
                Err(_) => PathBuf::from(p),
            }
        }
        None => {
            return (
                "Missing required parameter 'path' (root directory of the project to add)."
                    .to_string(),
                true,
            );
        }
    };

    let weight = args.get("weight").and_then(|v| v.as_f64()).unwrap_or(1.0) as f32;

    let mut config = match FederationConfig::load(&config_path) {
        Ok(c) => c,
        Err(e) => return (format!("Failed to load federation config: {e}"), true),
    };

    let name = project_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| project_path.display().to_string());

    config.add_project(project_path, weight);

    match config.save(&config_path) {
        Ok(()) => (
            format!(
                "Added project `{name}` (weight: {weight:.1}) to `{}`.\n{} project(s) total.",
                config_path.display(),
                config.projects.len()
            ),
            false,
        ),
        Err(e) => (format!("Failed to save federation config: {e}"), true),
    }
}

/// `federation_remove_project` — remove a project from the federation config.
pub fn call_federation_remove_project(args: &Value) -> (String, bool) {
    let config_path = match args.get("config").and_then(|v| v.as_str()) {
        Some(p) => PathBuf::from(p),
        None => {
            return (
                "Missing required parameter 'config' (path to federation config file).".to_string(),
                true,
            );
        }
    };

    let name = match args.get("name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => {
            return (
                "Missing required parameter 'name' (project directory name to remove).".to_string(),
                true,
            );
        }
    };

    let mut config = match FederationConfig::load(&config_path) {
        Ok(c) => c,
        Err(e) => return (format!("Failed to load federation config: {e}"), true),
    };

    let before = config.projects.len();
    config.remove_project(name);
    let after = config.projects.len();

    if before == after {
        return (
            format!("No project named `{name}` found in the federation config."),
            true,
        );
    }

    match config.save(&config_path) {
        Ok(()) => (
            format!(
                "Removed project `{name}` from `{}`.\n{} project(s) remaining.",
                config_path.display(),
                after
            ),
            false,
        ),
        Err(e) => (format!("Failed to save federation config: {e}"), true),
    }
}

/// `federation_list` — list projects in the federation.
pub fn call_federation_list(args: &Value, federation: Option<&FederatedEngine>) -> (String, bool) {
    // If a live FederatedEngine is available, use it for rich status info.
    if let Some(fed) = federation {
        let projects = fed.projects();
        let stats = fed.stats();

        let mut out = String::from("## Federated Projects\n\n");
        out.push_str(&format!(
            "**Registered:** {} | **Loaded:** {} | **Total files:** {} | **Total chunks:** {} | **Total symbols:** {}\n\n",
            stats.project_count, stats.loaded_count, stats.total_files, stats.total_chunks, stats.total_symbols,
        ));

        if projects.is_empty() {
            out.push_str("No projects registered.\n");
        } else {
            out.push_str("| # | Project | Root | Loaded | Files |\n");
            out.push_str("|---|---------|------|--------|-------|\n");
            for (i, proj) in projects.iter().enumerate() {
                let status = if proj.loaded { "yes" } else { "no" };
                out.push_str(&format!(
                    "| {} | {} | {} | {} | {} |\n",
                    i + 1,
                    proj.name,
                    proj.root.display(),
                    status,
                    proj.file_count,
                ));
            }
        }

        return (out, false);
    }

    // Fall back to reading from config file.
    let config_path = match args.get("config").and_then(|v| v.as_str()) {
        Some(p) => PathBuf::from(p),
        None => {
            return (
                "No federation engine active and no 'config' parameter provided.\n\
                 Either start the server with --federation or pass the config file path."
                    .to_string(),
                true,
            );
        }
    };

    let config = match FederationConfig::load(&config_path) {
        Ok(c) => c,
        Err(e) => return (format!("Failed to load federation config: {e}"), true),
    };

    let mut out = format!(
        "## Federation Config: `{}`\n\n**Projects:** {}\n\n",
        config_path.display(),
        config.projects.len()
    );

    if config.projects.is_empty() {
        out.push_str("No projects configured.\n");
    } else {
        out.push_str("| # | Root | Weight |\n");
        out.push_str("|---|------|--------|\n");
        for (i, proj) in config.projects.iter().enumerate() {
            out.push_str(&format!(
                "| {} | {} | {:.1} |\n",
                i + 1,
                proj.root.display(),
                proj.weight
            ));
        }
    }

    (out, false)
}

/// `federation_search` — search across federated projects.
pub fn call_federation_search(
    args: &Value,
    federation: Option<&FederatedEngine>,
) -> (String, bool) {
    let fed = match federation {
        Some(f) => f,
        None => {
            return (
                "Federation is not enabled. Start the server with --federation <config.json> \
                 to use cross-repo search."
                    .to_string(),
                true,
            );
        }
    };

    let query = match args.get("query").and_then(|v| v.as_str()) {
        Some(q) => q,
        None => {
            return (
                "Missing required parameter 'query' (search query string).".to_string(),
                true,
            );
        }
    };

    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize;

    let sq = SearchQuery::new(query).with_limit(limit);

    match fed.search(sq) {
        Ok(results) => {
            if results.is_empty() {
                return (format!("No federated results for \"{query}\"."), false);
            }

            let mut out = format!(
                "## Federated Search: \"{query}\" ({} results)\n\n",
                results.len()
            );

            for (i, r) in results.iter().enumerate() {
                out.push_str(&format!(
                    "{}. **[{}]** `{}` L{}-L{} (score: {:.3})\n",
                    i + 1,
                    r.project,
                    r.result.file_path,
                    r.result.line_start,
                    r.result.line_end,
                    r.result.score,
                ));
                // Show first 3 lines of content as preview.
                let preview: String = r
                    .result
                    .content
                    .lines()
                    .take(3)
                    .map(|l| format!("   | {l}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                if !preview.is_empty() {
                    out.push_str(&preview);
                    out.push('\n');
                }
                out.push('\n');
            }

            (out, false)
        }
        Err(e) => (format!("Federated search failed: {e}"), true),
    }
}

/// `federation_discover` — auto-discover workspace projects under a root directory.
pub fn call_federation_discover(args: &Value) -> (String, bool) {
    let root_str = match args.get("root").and_then(|v| v.as_str()) {
        Some(r) => r,
        None => {
            return (
                "Missing required parameter 'root' (directory to scan for workspace projects)."
                    .to_string(),
                true,
            );
        }
    };

    let root = match PathBuf::from(root_str).canonicalize() {
        Ok(r) => r,
        Err(e) => {
            return (format!("Cannot resolve root path `{root_str}`: {e}"), true);
        }
    };

    let projects = discover_projects(&root);

    if projects.is_empty() {
        return (
            format!(
                "No workspace projects discovered under `{}`.",
                root.display()
            ),
            false,
        );
    }

    let mut out = format!(
        "## Discovered Projects ({})\n\nRoot: `{}`\n\n",
        projects.len(),
        root.display()
    );

    out.push_str("| # | Name | Type | Weight | Root |\n");
    out.push_str("|---|------|------|--------|------|\n");
    for (i, proj) in projects.iter().enumerate() {
        out.push_str(&format!(
            "| {} | {} | {} | {:.1} | {} |\n",
            i + 1,
            proj.name,
            proj.project_type,
            proj.weight,
            proj.root.display(),
        ));
    }

    // If --output was requested, also write the config
    if let Some(output_path) = args.get("output").and_then(|v| v.as_str()) {
        let config = to_federation_config(&projects);
        match config.save(&PathBuf::from(output_path)) {
            Ok(()) => {
                out.push_str(&format!("\nWrote federation config to `{output_path}`."));
            }
            Err(e) => {
                out.push_str(&format!("\nFailed to write federation config: {e}"));
            }
        }
    }

    (out, false)
}
