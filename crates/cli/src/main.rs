use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use codeforge_core::{Engine, IndexConfig, RepoMapOptions, SearchQuery, Strategy, SyncStats};

#[derive(Parser)]
#[command(name = "codeforge", about = "Code retrieval engine for AI agents")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Initialize a new code index for the current (or specified) directory.
    Init {
        /// Project root directory to index (defaults to current directory).
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Additional directories to index alongside the primary root.
        /// File paths from each extra root are prefixed with its directory name.
        /// Example: --also ../shared-lib produces paths like `shared-lib/src/types.rs`
        /// Can be specified multiple times: --also ../shared-lib --also ../api-types
        #[arg(long = "also", value_name = "DIR")]
        also: Vec<PathBuf>,

        /// Only index these languages (comma-separated, e.g. "rust,python").
        #[arg(long, value_delimiter = ',')]
        languages: Vec<String>,

        /// Disable vector embeddings (BM25-only mode, faster init).
        #[arg(long)]
        no_embeddings: bool,

        /// Load the BGE-Reranker-Base cross-encoder model (~270 MB) to enable
        /// the `deep` strategy. Increases startup time by ~2 s.
        #[arg(long)]
        reranker: bool,
    },

    /// Search the code index.
    Search {
        /// The search query.
        query: String,

        /// Maximum number of results.
        #[arg(short, long, default_value = "10")]
        limit: usize,

        /// Filter results to files matching this substring.
        #[arg(short, long)]
        file: Option<String>,

        /// Retrieval strategy: instant (BM25 only), fast (hybrid, default), thorough (hybrid+MMR).
        #[arg(long, default_value = "fast")]
        strategy: StrategyArg,

        /// Print formatted context block (token-budget aware).
        #[arg(long)]
        format: bool,

        /// Token budget for formatted output (implies --format).
        #[arg(long)]
        token_budget: Option<usize>,
    },

    /// List symbols (functions, structs, classes, etc.) in the index.
    Symbols {
        /// Filter symbols by name (case-insensitive substring match).
        #[arg(default_value = "")]
        filter: String,

        /// Only show symbols from this file.
        #[arg(short, long)]
        file: Option<String>,
    },

    /// Show dependency graph stats and optionally generate a repo map.
    Graph {
        /// Project root (defaults to current directory).
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Token budget for the repo map output.
        #[arg(long, default_value = "4096")]
        token_budget: usize,

        /// Print a full repo map instead of just stats.
        #[arg(long)]
        map: bool,
    },

    /// Show files that import the given file.
    Callers {
        /// Target file (relative path).
        file: String,

        /// Transitive depth (default 1 = direct callers only).
        #[arg(long, default_value = "1")]
        depth: usize,
    },

    /// Show files imported by the given file.
    Callees {
        /// Target file (relative path).
        file: String,

        /// Transitive depth (default 1 = direct callees only).
        #[arg(long, default_value = "1")]
        depth: usize,
    },

    /// Show transitive dependencies of the given file.
    Dependencies {
        /// Target file (relative path).
        file: String,

        /// Transitive depth.
        #[arg(long, default_value = "2")]
        depth: usize,
    },

    /// Find all code locations that reference a symbol (call sites, imports, usages).
    Usages {
        /// Symbol name to search for (exact or partial identifier).
        symbol: String,

        /// Maximum number of results.
        #[arg(short, long, default_value = "20")]
        limit: usize,

        /// Filter results to files matching this substring.
        #[arg(short, long)]
        file: Option<String>,
    },

    /// Re-index files changed since the last git commit (git-diff aware incremental update).
    Update {
        /// Project root to update (defaults to current directory).
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Show what would be updated without making any changes.
        #[arg(long)]
        dry_run: bool,
    },

    /// Sync the index with current filesystem state using stored content hashes.
    /// Re-indexes only files whose content changed since the last init/sync.
    /// Works without git; handles any form of file drift.
    Sync {
        /// Project root to sync (defaults to current directory).
        #[arg(default_value = ".")]
        path: PathBuf,
    },

    /// Embed all un-embedded chunks in an existing BM25-only index.
    ///
    /// Run this after initialising with `--no-embeddings` to add vector search
    /// capability without a full re-index.  Only chunks that lack vector
    /// representations are embedded; existing vectors are not re-computed.
    Embed {
        /// Project root to embed (defaults to current directory).
        #[arg(default_value = ".")]
        path: PathBuf,
    },

    /// Start the REST API server.
    Serve {
        /// Host to bind to.
        #[arg(long, default_value = "127.0.0.1")]
        host: String,

        /// Port to listen on.
        #[arg(long, default_value = "3000")]
        port: u16,

        /// Project root to serve (defaults to current directory).
        #[arg(default_value = ".")]
        path: PathBuf,
    },
}

/// Clap-parseable strategy argument.
#[derive(Debug, Clone, clap::ValueEnum)]
enum StrategyArg {
    Instant,
    Fast,
    Thorough,
    /// BM25 + graph expansion: surfaces files transitively connected via imports.
    Explore,
    /// Two-stage: hybrid first-pass then BGE-Reranker cross-encoder re-scoring.
    /// Requires reranker_enabled = true (set via --reranker on init).
    Deep,
}

impl From<StrategyArg> for Strategy {
    fn from(s: StrategyArg) -> Self {
        match s {
            StrategyArg::Instant => Strategy::Instant,
            StrategyArg::Fast => Strategy::Fast,
            StrategyArg::Thorough => Strategy::Thorough,
            StrategyArg::Explore => Strategy::Explore,
            StrategyArg::Deep => Strategy::Deep,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Init {
            path,
            also,
            languages,
            no_embeddings,
            reranker,
        } => cmd_init(path, also, languages, no_embeddings, reranker),
        Command::Search {
            query,
            limit,
            file,
            strategy,
            format,
            token_budget,
        } => cmd_search(
            query,
            limit,
            file,
            strategy,
            format || token_budget.is_some(),
            token_budget,
        ),
        Command::Symbols { filter, file } => cmd_symbols(filter, file),
        Command::Graph {
            path,
            token_budget,
            map,
        } => cmd_graph(path, token_budget, map),
        Command::Callers { file, depth } => cmd_callers(file, depth),
        Command::Callees { file, depth } => cmd_callees(file, depth),
        Command::Dependencies { file, depth } => cmd_dependencies(file, depth),
        Command::Usages {
            symbol,
            limit,
            file,
        } => cmd_usages(symbol, limit, file),
        Command::Update { path, dry_run } => cmd_update(path, dry_run),
        Command::Sync { path } => cmd_sync(path),
        Command::Embed { path } => cmd_embed(path),
        Command::Serve { host, port, path } => cmd_serve(host, port, path).await,
    }
}

fn cmd_init(
    path: PathBuf,
    also: Vec<PathBuf>,
    languages: Vec<String>,
    no_embeddings: bool,
    reranker: bool,
) -> Result<()> {
    let root = path
        .canonicalize()
        .with_context(|| format!("path not found: {}", path.display()))?;

    let mut config = IndexConfig::new(&root);

    // Resolve and register extra roots.
    for extra in also {
        let extra_abs = extra
            .canonicalize()
            .with_context(|| format!("--also path not found: {}", extra.display()))?;
        config.extra_roots.push(extra_abs);
    }

    for lang in &languages {
        config.languages.insert(lang.to_lowercase());
    }
    if no_embeddings {
        config.embedding.enabled = false;
    }
    if reranker {
        config.embedding.reranker_enabled = true;
    }

    if config.extra_roots.is_empty() {
        eprintln!("Indexing {}...", root.display());
    } else {
        eprintln!(
            "Indexing {} (+ {} extra roots)...",
            root.display(),
            config.extra_roots.len()
        );
        for extra in &config.extra_roots {
            eprintln!("  + {}", extra.display());
        }
    }
    let start = Instant::now();

    let engine = Engine::init(&root, config).with_context(|| "failed to initialize index")?;

    let stats = engine.stats();
    let elapsed = start.elapsed();

    eprintln!(
        "Indexed {} files, {} chunks, {} symbols, {} vectors in {:.2}s",
        stats.file_count,
        stats.chunk_count,
        stats.symbol_count,
        stats.vector_count,
        elapsed.as_secs_f64(),
    );

    Ok(())
}

fn cmd_search(
    query: String,
    limit: usize,
    file: Option<String>,
    strategy: StrategyArg,
    format: bool,
    token_budget: Option<usize>,
) -> Result<()> {
    let root = std::env::current_dir().context("cannot determine current directory")?;
    let engine = Engine::open(&root).with_context(|| {
        format!(
            "no index found at {} — run `codeforge init` first",
            root.display()
        )
    })?;

    let mut sq = SearchQuery::new(&query)
        .with_limit(limit)
        .with_strategy(strategy.into());
    if let Some(ref f) = file {
        sq = sq.with_file_filter(f);
    }
    if let Some(b) = token_budget {
        sq = sq.with_token_budget(b);
    }

    let results = engine.search(sq).context("search failed")?;

    if results.is_empty() {
        eprintln!("No results for \"{}\"", query);
        return Ok(());
    }

    if format {
        let ctx = engine.format_results(&results, token_budget);
        print!("{ctx}");
        return Ok(());
    }

    for (i, result) in results.iter().enumerate() {
        println!(
            "{}. {} [L{}-L{}] ({}) score={:.3}",
            i + 1,
            result.file_path,
            result.line_start,
            result.line_end,
            result.language,
            result.score,
        );
        if !result.signature.is_empty() {
            println!("   {}", result.signature);
        }
        // Show a snippet of the content (first 3 lines).
        let snippet: String = result
            .content
            .lines()
            .take(3)
            .map(|l| format!("   | {l}"))
            .collect::<Vec<_>>()
            .join("\n");
        if !snippet.is_empty() {
            println!("{snippet}");
        }
        println!();
    }

    Ok(())
}

fn cmd_symbols(filter: String, file: Option<String>) -> Result<()> {
    let root = std::env::current_dir().context("cannot determine current directory")?;
    let engine = Engine::open(&root).with_context(|| {
        format!(
            "no index found at {} — run `codeforge init` first",
            root.display()
        )
    })?;

    let symbols = engine
        .symbols(&filter, file.as_deref())
        .context("symbol lookup failed")?;

    if symbols.is_empty() {
        if filter.is_empty() {
            eprintln!("No symbols in index.");
        } else {
            eprintln!("No symbols matching \"{}\"", filter);
        }
        return Ok(());
    }

    println!("{:<12} {:<40} {:<30} LINES", "KIND", "NAME", "FILE");
    println!("{}", "-".repeat(90));
    for sym in &symbols {
        println!(
            "{:<12} {:<40} {:<30} L{}-L{}",
            format!("{:?}", sym.kind),
            sym.name,
            sym.file_path,
            sym.line_start,
            sym.line_end,
        );
    }
    eprintln!("\n{} symbol(s) found.", symbols.len());

    Ok(())
}

fn cmd_graph(path: PathBuf, token_budget: usize, map: bool) -> Result<()> {
    let root = path
        .canonicalize()
        .with_context(|| format!("path not found: {}", path.display()))?;
    let engine = Engine::open(&root).with_context(|| {
        format!(
            "no index found at {} — run `codeforge init` first",
            root.display()
        )
    })?;

    if map {
        let opts = RepoMapOptions {
            token_budget,
            ..Default::default()
        };
        match engine.repo_map(opts) {
            Some(text) => print!("{text}"),
            None => eprintln!("Graph not available — re-run `codeforge init`"),
        }
        return Ok(());
    }

    match engine.graph_stats() {
        Some(stats) => {
            println!("Graph Statistics");
            println!("  Nodes (files):     {}", stats.node_count);
            println!("  Edges (imports):   {}", stats.edge_count);
            println!("  Resolved edges:    {}", stats.resolved_edges);
            println!("  External edges:    {}", stats.external_edges);
        }
        None => eprintln!("Graph not available — re-run `codeforge init`"),
    }
    Ok(())
}

fn cmd_callers(file: String, depth: usize) -> Result<()> {
    let root = std::env::current_dir().context("cannot determine current directory")?;
    let engine = Engine::open(&root).with_context(|| {
        format!(
            "no index found at {} — run `codeforge init` first",
            root.display()
        )
    })?;

    let callers = if depth <= 1 {
        engine.callers(&file)
    } else {
        engine.dependencies(&file, depth)
    };

    if callers.is_empty() {
        eprintln!("No callers found for \"{}\"", file);
        return Ok(());
    }

    for c in &callers {
        println!("{c}");
    }
    eprintln!("\n{} caller(s) found.", callers.len());
    Ok(())
}

fn cmd_callees(file: String, depth: usize) -> Result<()> {
    let root = std::env::current_dir().context("cannot determine current directory")?;
    let engine = Engine::open(&root).with_context(|| {
        format!(
            "no index found at {} — run `codeforge init` first",
            root.display()
        )
    })?;

    let callees = engine.callees(&file);
    if callees.is_empty() {
        eprintln!("No dependencies found for \"{}\"", file);
        return Ok(());
    }

    for c in &callees {
        println!("{c}");
    }
    let _ = depth; // depth not yet used for direct callees (always 1)
    eprintln!("\n{} dependency/dependencies found.", callees.len());
    Ok(())
}

fn cmd_dependencies(file: String, depth: usize) -> Result<()> {
    let root = std::env::current_dir().context("cannot determine current directory")?;
    let engine = Engine::open(&root).with_context(|| {
        format!(
            "no index found at {} — run `codeforge init` first",
            root.display()
        )
    })?;

    let deps = engine.dependencies(&file, depth);
    if deps.is_empty() {
        eprintln!("No transitive dependencies found for \"{}\"", file);
        return Ok(());
    }

    for d in &deps {
        println!("{d}");
    }
    eprintln!("\n{} transitive dependency/dependencies found.", deps.len());
    Ok(())
}

fn cmd_usages(symbol: String, limit: usize, file: Option<String>) -> Result<()> {
    let root = std::env::current_dir().context("cannot determine current directory")?;
    let engine = Engine::open(&root).with_context(|| {
        format!(
            "no index found at {} — run `codeforge init` first",
            root.display()
        )
    })?;

    let mut results = engine
        .search_usages(&symbol, limit)
        .context("usage search failed")?;

    if let Some(ref f) = file {
        results.retain(|r| r.file_path.contains(f.as_str()));
    }

    if results.is_empty() {
        eprintln!("No usages found for \"{}\"", symbol);
        return Ok(());
    }

    println!("{:<50} {:<12} PREVIEW", "FILE [LINES]", "SCORE");
    println!("{}", "-".repeat(90));
    for r in &results {
        let loc = format!("{} [L{}-L{}]", r.file_path, r.line_start, r.line_end);
        let preview: String = r
            .content
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .trim()
            .chars()
            .take(40)
            .collect();
        println!("{:<50} {:<12.3} {}", loc, r.score, preview);
    }
    eprintln!("\n{} usage location(s) found.", results.len());

    Ok(())
}

fn cmd_update(path: PathBuf, dry_run: bool) -> Result<()> {
    let root = path
        .canonicalize()
        .with_context(|| format!("path not found: {}", path.display()))?;

    // Ask git for all paths that differ from the last commit (staged + unstaged).
    // --porcelain gives a stable machine-readable format: "XY filename"
    let output = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&root)
        .output()
        .context("failed to run 'git status' — is git installed and is this a git repository?")?;

    if !output.status.success() {
        anyhow::bail!(
            "git status failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    let mut to_reindex: Vec<PathBuf> = Vec::new();
    let mut to_remove: Vec<PathBuf> = Vec::new();

    for line in stdout.lines() {
        // Porcelain format: "XY path" where X=staged, Y=unstaged (each 1 char).
        // Minimum meaningful line is "?? f" (4 chars).
        if line.len() < 4 {
            continue;
        }
        let status = &line[..2];

        // Skip ignored files ("!!" in porcelain).
        if status == "!!" {
            continue;
        }

        let rest = line[3..].trim();

        // Renamed files are reported as "R  old -> new".
        if status.contains('R') && rest.contains(" -> ") {
            let mut parts = rest.splitn(2, " -> ");
            if let (Some(old), Some(new)) = (parts.next(), parts.next()) {
                to_remove.push(PathBuf::from(old.trim()));
                let new_path = PathBuf::from(new.trim());
                if root.join(&new_path).exists() {
                    to_reindex.push(new_path);
                }
            }
            continue;
        }

        let file_path = PathBuf::from(rest);
        let abs = root.join(&file_path);
        if abs.is_file() {
            to_reindex.push(file_path);
        } else if !abs.exists() {
            // File was deleted — remove from index (skip directories).
            to_remove.push(file_path);
        }
        // abs.is_dir() → untracked directory reported by git, skip entirely.
    }

    if to_reindex.is_empty() && to_remove.is_empty() {
        eprintln!("Index is up to date — no changed files detected by git.");
        return Ok(());
    }

    if !to_reindex.is_empty() {
        eprintln!("Files to reindex ({}):", to_reindex.len());
        for f in &to_reindex {
            eprintln!("  + {}", f.display());
        }
    }
    if !to_remove.is_empty() {
        eprintln!("Files to remove ({}):", to_remove.len());
        for f in &to_remove {
            eprintln!("  - {}", f.display());
        }
    }

    if dry_run {
        eprintln!("\n(dry run — no changes made)");
        return Ok(());
    }

    let mut engine = Engine::open(&root).with_context(|| {
        format!(
            "no index found at {} — run `codeforge init` first",
            root.display()
        )
    })?;

    let start = Instant::now();
    let mut updated = 0usize;
    let mut removed = 0usize;

    for rel_path in &to_reindex {
        let abs = root.join(rel_path);
        match engine.reindex_file(&abs) {
            Ok(()) => updated += 1,
            Err(e) => eprintln!("  warning: skipped {} — {e}", rel_path.display()),
        }
    }

    for rel_path in &to_remove {
        match engine.remove_file(rel_path) {
            Ok(()) => removed += 1,
            Err(e) => eprintln!("  warning: remove failed {} — {e}", rel_path.display()),
        }
    }

    engine.save().context("failed to save index after update")?;

    eprintln!(
        "\nUpdated {updated} file(s), removed {removed} file(s) in {:.2}s",
        start.elapsed().as_secs_f64()
    );

    Ok(())
}

fn cmd_sync(path: PathBuf) -> Result<()> {
    let root = path
        .canonicalize()
        .with_context(|| format!("path not found: {}", path.display()))?;

    let mut engine =
        Engine::open(&root).with_context(|| "no index found — run `codeforge init` first")?;

    let start = Instant::now();
    let SyncStats {
        added,
        modified,
        removed,
        unchanged,
    } = engine.sync()?;

    eprintln!(
        "sync complete: {} added, {} modified, {} removed, {} unchanged ({:.2}s)",
        added,
        modified,
        removed,
        unchanged,
        start.elapsed().as_secs_f64(),
    );

    Ok(())
}

fn cmd_embed(path: PathBuf) -> Result<()> {
    let root = path
        .canonicalize()
        .with_context(|| format!("path not found: {}", path.display()))?;

    let mut engine =
        Engine::open(&root).with_context(|| "no index found — run `codeforge init` first")?;

    let start = Instant::now();
    let embedded = engine.embed_remaining()?;

    if embedded == 0 {
        eprintln!("all chunks already embedded; nothing to do");
    } else {
        eprintln!(
            "embedded {} chunk(s) in {:.2}s",
            embedded,
            start.elapsed().as_secs_f64()
        );
    }

    Ok(())
}

async fn cmd_serve(host: String, port: u16, path: PathBuf) -> Result<()> {
    let root = path
        .canonicalize()
        .with_context(|| format!("path not found: {}", path.display()))?;

    eprintln!("Starting CodeForge server at http://{}:{}", host, port);
    eprintln!("Serving index at: {}", root.display());

    // Try to start the standalone codeforge-server binary first.
    // If it's not in PATH, fall back to a helpful error message.
    let server_result = tokio::process::Command::new("codeforge-server")
        .args(["--host", &host, "--port", &port.to_string()])
        .arg(&root)
        .status()
        .await;

    match server_result {
        Ok(status) if !status.success() => {
            anyhow::bail!("codeforge-server exited with status: {status}");
        }
        Err(_) => {
            anyhow::bail!(
                "codeforge-server binary not found in PATH.\n\
                 Run it directly: codeforge-server --host {} --port {} {}",
                host,
                port,
                root.display()
            );
        }
        Ok(_) => Ok(()),
    }
}
