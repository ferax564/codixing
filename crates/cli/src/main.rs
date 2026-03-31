use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use codixing_core::{
    EmbedTimingStats, EmbeddingModel, Engine, FederatedEngine, FederationConfig, GitSyncStats,
    IndexConfig, RepoMapOptions, SearchQuery, Strategy, SyncStats, discover_projects,
    to_federation_config,
};

#[derive(Parser)]
#[command(name = "codixing", about = "Code retrieval engine for AI agents")]
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

        /// Embedding model to use [default: bge-base-en].
        /// Options: bge-small-en, bge-base-en, bge-large-en,
        ///          jina-embed-code, nomic-embed-code,
        ///          snowflake-arctic-l, qwen3
        #[arg(long, value_name = "MODEL")]
        model: Option<String>,

        /// Load the BGE-Reranker-Base cross-encoder model (~270 MB) to enable
        /// the `deep` strategy. Increases startup time by ~2 s.
        #[arg(long)]
        reranker: bool,

        /// Skip embedding during init — index with BM25 only, embed later
        /// with `codixing embed`. Useful for large repos where you want
        /// search available immediately.
        #[arg(long)]
        defer_embeddings: bool,
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

        /// Retrieval strategy: auto (detect from query, default), instant (BM25 only),
        /// fast (hybrid), thorough (hybrid+MMR), explore (graph), deep (reranker).
        #[arg(long, default_value = "auto")]
        strategy: StrategyArg,

        /// Print formatted context block (token-budget aware).
        #[arg(long)]
        format: bool,

        /// Output results as JSON (one object per result).
        #[arg(long)]
        json: bool,

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

    /// Find files in one directory that import from another directory.
    ///
    /// Answers cross-package queries like "which gateway files import from
    /// the security module?" by walking the import graph.
    CrossImports {
        /// Source directory (files that do the importing).
        #[arg(long)]
        from: String,

        /// Target directory (module being imported from).
        #[arg(long)]
        to: String,
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

    /// Fast git-aware sync: re-indexes only files changed since the last indexed git commit.
    ///
    /// Reads the git commit hash stored during the last `init` / `sync` / `git-sync`,
    /// runs `git diff --name-status <stored_commit>` to compute the exact file delta,
    /// and passes only those files to the incremental re-indexer.
    /// Runs `apply_changes` with a single Tantivy commit and a single PageRank pass.
    ///
    /// Much faster than `sync` after `git pull` on large repos because it skips
    /// hash-scanning every file.  Falls back gracefully if git is unavailable.
    GitSync {
        /// Project root to git-sync (defaults to current directory).
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

    /// Benchmark embedding speed on the current index.
    ///
    /// Measures wall-clock time, throughput (chunks/sec), worker count, and
    /// late-chunking hit rate.  Results are printed to stderr by default or
    /// as JSON to stdout when --json is given (suitable for CI integration).
    BenchEmbed {
        /// Project root to benchmark (defaults to current directory).
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Re-embed all chunks even if they already have vectors.
        /// Without this flag only un-embedded chunks are processed.
        #[arg(long)]
        force: bool,

        /// Output results as JSON to stdout instead of human-readable stderr.
        #[arg(long)]
        json: bool,
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

    /// Manage federated cross-repo search configurations.
    Federation {
        #[command(subcommand)]
        action: FederationAction,
    },
}

/// Subcommands for managing federation configurations.
#[derive(Subcommand)]
enum FederationAction {
    /// Create a new empty federation config file.
    Init {
        /// Path for the new federation config file.
        #[arg(default_value = "codixing-federation.json")]
        path: PathBuf,
    },

    /// Add a project to the federation config.
    Add {
        /// Root directory of the project to add (must contain a .codixing/ index).
        path: PathBuf,

        /// Per-project weight for RRF fusion (higher = ranked higher).
        #[arg(long, default_value = "1.0")]
        weight: f32,

        /// Path to the federation config file.
        #[arg(long, default_value = "codixing-federation.json")]
        config: PathBuf,
    },

    /// Remove a project from the federation config by directory name.
    Remove {
        /// Directory name of the project to remove.
        name: String,

        /// Path to the federation config file.
        #[arg(long, default_value = "codixing-federation.json")]
        config: PathBuf,
    },

    /// List all projects in the federation config.
    List {
        /// Path to the federation config file.
        #[arg(default_value = "codixing-federation.json")]
        config: PathBuf,
    },

    /// Search across all federated projects.
    Search {
        /// The search query.
        query: String,

        /// Maximum number of results.
        #[arg(short, long, default_value = "10")]
        limit: usize,

        /// Path to the federation config file.
        #[arg(long, default_value = "codixing-federation.json")]
        config: PathBuf,
    },

    /// Auto-discover workspace projects (Cargo, npm, pnpm, Go, git submodules).
    ///
    /// Scans the given root for multi-project workspace patterns and prints
    /// discovered projects. Use --output to write a federation config file.
    Discover {
        /// Root directory to scan for workspace projects.
        #[arg(default_value = ".")]
        root: PathBuf,

        /// Write the discovered projects to this federation config file.
        /// If omitted, just prints the discovery results.
        #[arg(long)]
        output: Option<PathBuf>,
    },
}

/// Clap-parseable strategy argument.
#[derive(Debug, Clone, clap::ValueEnum)]
enum StrategyArg {
    /// Auto-detect based on query characteristics and available capabilities.
    Auto,
    Instant,
    Fast,
    Thorough,
    /// BM25 + graph expansion: surfaces files transitively connected via imports.
    Explore,
    /// Two-stage: hybrid first-pass then BGE-Reranker cross-encoder re-scoring.
    /// Requires reranker_enabled = true (set via --reranker on init).
    Deep,
    /// Trigram index fast-path for exact identifier lookups.
    Exact,
}

impl StrategyArg {
    /// Resolve this argument to a concrete [`Strategy`], using auto-detection when `Auto`.
    fn resolve(self, engine: &Engine, query: &str) -> Strategy {
        match self {
            StrategyArg::Auto => engine.detect_strategy(query),
            StrategyArg::Instant => Strategy::Instant,
            StrategyArg::Fast => Strategy::Fast,
            StrategyArg::Thorough => Strategy::Thorough,
            StrategyArg::Explore => Strategy::Explore,
            StrategyArg::Deep => Strategy::Deep,
            StrategyArg::Exact => Strategy::Exact,
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
            model,
            reranker,
            defer_embeddings,
        } => cmd_init(
            path,
            also,
            languages,
            no_embeddings,
            model,
            reranker,
            defer_embeddings,
        ),
        Command::Search {
            query,
            limit,
            file,
            strategy,
            format,
            json,
            token_budget,
        } => cmd_search(
            query,
            limit,
            file,
            strategy,
            format || token_budget.is_some(),
            json,
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
        Command::CrossImports { from, to } => cmd_cross_imports(from, to),
        Command::Usages {
            symbol,
            limit,
            file,
        } => cmd_usages(symbol, limit, file),
        Command::Update { path, dry_run } => cmd_update(path, dry_run),
        Command::Sync { path } => cmd_sync(path),
        Command::GitSync { path } => cmd_git_sync(path),
        Command::Embed { path } => cmd_embed(path),
        Command::BenchEmbed { path, force, json } => cmd_bench_embed(path, force, json),
        Command::Serve { host, port, path } => cmd_serve(host, port, path).await,
        Command::Federation { action } => cmd_federation(action),
    }
}

fn cmd_init(
    path: PathBuf,
    also: Vec<PathBuf>,
    languages: Vec<String>,
    no_embeddings: bool,
    model: Option<String>,
    reranker: bool,
    defer_embeddings: bool,
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
    if let Some(m) = model {
        config.embedding.model = parse_embedding_model(&m)?;
        // Specifying a model implies embeddings should be enabled.
        if !no_embeddings {
            config.embedding.enabled = true;
        }
    }
    if reranker {
        config.embedding.reranker_enabled = true;
    }
    if defer_embeddings {
        config.embedding.enabled = false;
        eprintln!("Deferring embeddings — BM25 index only. Run `codixing embed` to add vectors.");
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

    // Re-enable embeddings in the saved config so `codixing embed` works later.
    // Engine::init() persists config.json early, and defer_embeddings set
    // enabled=false. Without this fix, `codixing embed` would fail because
    // Engine::open() reads the saved config and skips loading the embedder.
    if defer_embeddings {
        let config_path = root.join(".codixing").join("config.json");
        if let Ok(text) = std::fs::read_to_string(&config_path) {
            if let Ok(mut saved) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(emb) = saved.get_mut("embedding") {
                    emb["enabled"] = serde_json::Value::Bool(true);
                    let _ = std::fs::write(
                        &config_path,
                        serde_json::to_string_pretty(&saved).unwrap_or_default(),
                    );
                }
            }
        }
    }

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

fn parse_embedding_model(s: &str) -> Result<EmbeddingModel> {
    match s.to_lowercase().as_str() {
        "bge-small-en" | "bge-small" | "small" => Ok(EmbeddingModel::BgeSmallEn),
        "bge-base-en" | "bge-base" | "base" => Ok(EmbeddingModel::BgeBaseEn),
        "bge-large-en" | "bge-large" | "large" => Ok(EmbeddingModel::BgeLargeEn),
        "jina" | "jina-embed-code" => Ok(EmbeddingModel::JinaEmbedCode),
        "nomic-embed-code" | "nomic" => Ok(EmbeddingModel::NomicEmbedCode),
        "snowflake-arctic-l" | "arctic-l" | "arctic" => Ok(EmbeddingModel::SnowflakeArcticEmbedL),
        "qwen3" | "qwen" => {
            #[cfg(feature = "qwen3")]
            return Ok(EmbeddingModel::Qwen3SmallEmbedding);
            #[cfg(not(feature = "qwen3"))]
            anyhow::bail!("qwen3 model requires building with --features codixing-core/qwen3")
        }
        other => anyhow::bail!(
            "unknown model '{}'. Valid: bge-small-en, bge-base-en, bge-large-en, jina-embed-code, nomic-embed-code, snowflake-arctic-l, qwen3",
            other
        ),
    }
}

fn cmd_search(
    query: String,
    limit: usize,
    file: Option<String>,
    strategy: StrategyArg,
    format: bool,
    json: bool,
    token_budget: Option<usize>,
) -> Result<()> {
    let root = std::env::current_dir().context("cannot determine current directory")?;
    let engine = Engine::open(&root).with_context(|| {
        format!(
            "no index found at {} — run `codixing init` first",
            root.display()
        )
    })?;

    let resolved_strategy = strategy.resolve(&engine, &query);
    let mut sq = SearchQuery::new(&query)
        .with_limit(limit)
        .with_strategy(resolved_strategy);
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

    if json {
        let json_results: Vec<serde_json::Value> = results
            .iter()
            .map(|r| {
                serde_json::json!({
                    "file": r.file_path,
                    "line_start": r.line_start,
                    "line_end": r.line_end,
                    "language": r.language,
                    "score": r.score,
                    "signature": r.signature,
                    "content": r.content,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json_results).unwrap_or_default()
        );
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
            "no index found at {} — run `codixing init` first",
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
            "no index found at {} — run `codixing init` first",
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
            None => eprintln!("Graph not available — re-run `codixing init`"),
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
            println!("  Call edges:        {}", stats.call_edges);
            println!("  Symbol nodes:      {}", stats.symbol_nodes);
            println!("  Symbol edges:      {}", stats.symbol_edges);
        }
        None => eprintln!("Graph not available — re-run `codixing init`"),
    }
    Ok(())
}

fn cmd_callers(file: String, depth: usize) -> Result<()> {
    let root = std::env::current_dir().context("cannot determine current directory")?;
    let engine = Engine::open(&root).with_context(|| {
        format!(
            "no index found at {} — run `codixing init` first",
            root.display()
        )
    })?;

    let callers = if depth <= 1 {
        engine.callers(&file)
    } else {
        engine.transitive_callers(&file, depth)
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
            "no index found at {} — run `codixing init` first",
            root.display()
        )
    })?;

    let callees = if depth <= 1 {
        engine.callees(&file)
    } else {
        engine.transitive_callees(&file, depth)
    };
    if callees.is_empty() {
        eprintln!("No dependencies found for \"{}\"", file);
        return Ok(());
    }

    for c in &callees {
        println!("{c}");
    }
    eprintln!("\n{} dependency/dependencies found.", callees.len());
    Ok(())
}

fn cmd_dependencies(file: String, depth: usize) -> Result<()> {
    let root = std::env::current_dir().context("cannot determine current directory")?;
    let engine = Engine::open(&root).with_context(|| {
        format!(
            "no index found at {} — run `codixing init` first",
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

fn cmd_cross_imports(from: String, to: String) -> Result<()> {
    let root = std::env::current_dir().context("cannot determine current directory")?;
    let engine = Engine::open(&root).with_context(|| {
        format!(
            "no index found at {} — run `codixing init` first",
            root.display()
        )
    })?;

    let files = engine.cross_imports(&from, &to);
    if files.is_empty() {
        eprintln!("No files in \"{}\" import from \"{}\"", from, to);
        return Ok(());
    }

    for f in &files {
        println!("{f}");
    }
    eprintln!(
        "\n{} file(s) in \"{}\" import from \"{}\".",
        files.len(),
        from,
        to
    );
    Ok(())
}

fn cmd_usages(symbol: String, limit: usize, file: Option<String>) -> Result<()> {
    let root = std::env::current_dir().context("cannot determine current directory")?;
    let engine = Engine::open(&root).with_context(|| {
        format!(
            "no index found at {} — run `codixing init` first",
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
            "no index found at {} — run `codixing init` first",
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
        Engine::open(&root).with_context(|| "no index found — run `codixing init` first")?;

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

fn cmd_git_sync(path: PathBuf) -> Result<()> {
    let root = path
        .canonicalize()
        .with_context(|| format!("path not found: {}", path.display()))?;

    let mut engine =
        Engine::open(&root).with_context(|| "no index found — run `codixing init` first")?;

    let start = Instant::now();
    let GitSyncStats {
        modified,
        removed,
        unchanged,
    } = engine.git_sync()?;

    if unchanged {
        eprintln!(
            "index already up-to-date (no git changes since last index, {:.2}s)",
            start.elapsed().as_secs_f64()
        );
    } else {
        eprintln!(
            "git-sync complete: {} modified, {} removed ({:.2}s)",
            modified,
            removed,
            start.elapsed().as_secs_f64()
        );
    }

    Ok(())
}

fn cmd_embed(path: PathBuf) -> Result<()> {
    let root = path
        .canonicalize()
        .with_context(|| format!("path not found: {}", path.display()))?;

    let mut engine =
        Engine::open(&root).with_context(|| "no index found — run `codixing init` first")?;

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

fn cmd_bench_embed(path: PathBuf, force: bool, json: bool) -> Result<()> {
    let root = path
        .canonicalize()
        .with_context(|| format!("path not found: {}", path.display()))?;

    let mut engine =
        Engine::open(&root).with_context(|| "no index found — run `codixing init` first")?;

    let pending = if force {
        // Re-embed all chunks, whether or not they already have vectors.
        let pending = dashmap::DashMap::new();
        for entry in engine.chunk_meta_ref().iter() {
            pending.insert(*entry.key(), entry.value().file_path.clone());
        }
        if pending.is_empty() {
            anyhow::bail!("no chunks in index — run `codixing init` first");
        }
        pending
    } else {
        engine.find_unembedded_chunks()?
    };

    if pending.is_empty() {
        eprintln!("All chunks already have embeddings. Use --force to re-embed.");
        return Ok(());
    }

    eprintln!("Benchmarking embedding of {} chunks...", pending.len());
    let stats: EmbedTimingStats = engine.bench_embed(&pending)?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&stats.to_json()).unwrap_or_default()
        );
    } else {
        eprintln!("\n── Embedding Benchmark Results ──────────────────");
        eprintln!("  Chunks:            {}", stats.total_chunks);
        eprintln!("  Files:             {}", stats.total_files);
        eprintln!(
            "  Wall clock:        {:.2}s",
            stats.wall_clock.as_secs_f64()
        );
        eprintln!(
            "  Throughput:        {:.1} chunks/sec",
            stats.chunks_per_sec()
        );
        eprintln!("  Workers:           {}", stats.workers);
        eprintln!(
            "  Late chunking:     {:.0}% ({}/{})",
            stats.late_chunking_rate() * 100.0,
            stats.late_chunking_files,
            stats.total_files
        );
        eprintln!("─────────────────────────────────────────────────");
    }
    Ok(())
}

fn cmd_federation(action: FederationAction) -> Result<()> {
    match action {
        FederationAction::Init { path } => {
            FederationConfig::init_template(&path).with_context(|| {
                format!("failed to create federation config at {}", path.display())
            })?;
            eprintln!("Created federation config at: {}", path.display());
            eprintln!("Edit it to add project roots, then use `codixing federation add`.");
            Ok(())
        }
        FederationAction::Add {
            path,
            weight,
            config,
        } => {
            let mut cfg = FederationConfig::load(&config).with_context(|| {
                format!("failed to load federation config from {}", config.display())
            })?;

            let abs_path = path
                .canonicalize()
                .with_context(|| format!("project path not found: {}", path.display()))?;

            let name = abs_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| abs_path.display().to_string());

            cfg.add_project(&abs_path, weight);
            cfg.save(&config)?;

            eprintln!(
                "Added project `{name}` (weight: {weight:.1}) to {}",
                config.display()
            );
            eprintln!("{} project(s) total.", cfg.projects.len());
            Ok(())
        }
        FederationAction::Remove { name, config } => {
            let mut cfg = FederationConfig::load(&config).with_context(|| {
                format!("failed to load federation config from {}", config.display())
            })?;

            let before = cfg.projects.len();
            cfg.remove_project(&name);

            if cfg.projects.len() == before {
                anyhow::bail!("no project named `{name}` found in {}", config.display());
            }

            cfg.save(&config)?;
            eprintln!(
                "Removed project `{name}` from {}. {} project(s) remaining.",
                config.display(),
                cfg.projects.len()
            );
            Ok(())
        }
        FederationAction::List { config } => {
            let cfg = FederationConfig::load(&config).with_context(|| {
                format!("failed to load federation config from {}", config.display())
            })?;

            if cfg.projects.is_empty() {
                eprintln!("No projects configured in {}", config.display());
                return Ok(());
            }

            println!("{:<5} {:<40} {:<8}", "#", "ROOT", "WEIGHT");
            println!("{}", "-".repeat(55));
            for (i, proj) in cfg.projects.iter().enumerate() {
                println!(
                    "{:<5} {:<40} {:<8.1}",
                    i + 1,
                    proj.root.display(),
                    proj.weight
                );
            }
            eprintln!(
                "\n{} project(s) in {}",
                cfg.projects.len(),
                config.display()
            );
            Ok(())
        }
        FederationAction::Search {
            query,
            limit,
            config,
        } => {
            let cfg = FederationConfig::load(&config).with_context(|| {
                format!("failed to load federation config from {}", config.display())
            })?;

            let fed =
                FederatedEngine::new(cfg).with_context(|| "failed to create FederatedEngine")?;

            let sq = SearchQuery::new(&query).with_limit(limit);
            let results = fed.search(sq).with_context(|| "federated search failed")?;

            if results.is_empty() {
                eprintln!("No results for \"{}\"", query);
                return Ok(());
            }

            for (i, r) in results.iter().enumerate() {
                println!(
                    "{}. [{}] {} [L{}-L{}] score={:.3}",
                    i + 1,
                    r.project,
                    r.result.file_path,
                    r.result.line_start,
                    r.result.line_end,
                    r.result.score,
                );
                let snippet: String = r
                    .result
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
        FederationAction::Discover { root, output } => {
            let abs_root = root
                .canonicalize()
                .with_context(|| format!("root path not found: {}", root.display()))?;

            let start = Instant::now();
            let projects = discover_projects(&abs_root);
            let elapsed = start.elapsed();

            if projects.is_empty() {
                eprintln!(
                    "No workspace projects discovered under {} ({:.2}s)",
                    abs_root.display(),
                    elapsed.as_secs_f64()
                );
                return Ok(());
            }

            // Print table of discovered projects
            println!(
                "{:<5} {:<20} {:<18} {:<8} ROOT",
                "#", "NAME", "TYPE", "WEIGHT"
            );
            println!("{}", "-".repeat(80));
            for (i, proj) in projects.iter().enumerate() {
                println!(
                    "{:<5} {:<20} {:<18} {:<8.1} {}",
                    i + 1,
                    proj.name,
                    proj.project_type,
                    proj.weight,
                    proj.root.display(),
                );
            }
            eprintln!(
                "\nDiscovered {} project(s) in {:.2}s",
                projects.len(),
                elapsed.as_secs_f64()
            );

            // If --output is given, write the federation config
            if let Some(output_path) = output {
                let config = to_federation_config(&projects);
                config.save(&output_path).with_context(|| {
                    format!(
                        "failed to write federation config to {}",
                        output_path.display()
                    )
                })?;
                eprintln!("Wrote federation config to: {}", output_path.display());
            }

            Ok(())
        }
    }
}

async fn cmd_serve(host: String, port: u16, path: PathBuf) -> Result<()> {
    let root = path
        .canonicalize()
        .with_context(|| format!("path not found: {}", path.display()))?;

    eprintln!("Starting Codixing server at http://{}:{}", host, port);
    eprintln!("Serving index at: {}", root.display());

    // Try to start the standalone codixing-server binary first.
    // If it's not in PATH, fall back to a helpful error message.
    let server_result = tokio::process::Command::new("codixing-server")
        .args(["--host", &host, "--port", &port.to_string()])
        .arg(&root)
        .status()
        .await;

    match server_result {
        Ok(status) if !status.success() => {
            anyhow::bail!("codixing-server exited with status: {status}");
        }
        Err(_) => {
            anyhow::bail!(
                "codixing-server binary not found in PATH.\n\
                 Run it directly: codixing-server --host {} --port {} {}",
                host,
                port,
                root.display()
            );
        }
        Ok(_) => Ok(()),
    }
}
