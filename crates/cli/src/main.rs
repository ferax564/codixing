mod daemon_proxy;

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use codixing_core::{
    EmbedTimingStats, EmbeddingModel, Engine, FederatedEngine, FederationConfig, FreshnessOptions,
    FreshnessTier, GrepOptions, HtmlExportOptions, IndexConfig, RepoMapOptions, SearchQuery,
    Strategy, discover_projects, to_federation_config,
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

        /// Enable vector embeddings alongside BM25+graph.
        ///
        /// Default since v0.33: `init` builds BM25 + symbol graph only.
        /// Embeddings are slow to build (14 min on 10K files, 25+ min on the
        /// Linux kernel) and grow the index by 2GB+. Agent code exploration
        /// works well on BM25+graph alone — pass `--embed` only if your
        /// workflow relies on natural-language concept search.
        #[arg(long)]
        embed: bool,

        /// Embedding model to use. Only meaningful with --embed.
        /// Options: bge-small-en, bge-base-en, bge-large-en,
        ///          jina-embed-code, nomic-embed-code,
        ///          snowflake-arctic-l, qwen3
        #[arg(long, value_name = "MODEL", default_value = "bge-base-en")]
        model: String,

        /// Load the BGE-Reranker-Base cross-encoder model (~270 MB) to enable
        /// the `deep` strategy. Increases startup time by ~2 s. Only meaningful
        /// with --embed.
        #[arg(long)]
        reranker: bool,

        /// With --embed: embed in the background and return after BM25+graph
        /// is ready. Search is available immediately; vector hits phase in as
        /// the background drain completes. No effect without --embed.
        #[arg(long)]
        defer_embeddings: bool,

        /// With --embed: block until embeddings complete before returning.
        /// No effect without --embed.
        #[arg(long)]
        wait: bool,
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

        /// Only return code/config results (no documentation).
        #[arg(long)]
        code_only: bool,

        /// Only return documentation results (no code/config).
        #[arg(long)]
        docs_only: bool,

        /// Print only the result count, not the full results.
        #[arg(long)]
        count: bool,
    },

    /// List symbols (functions, structs, classes, etc.) in the index.
    Symbols {
        /// Filter symbols by name (case-insensitive substring match).
        #[arg(default_value = "")]
        filter: String,

        /// Only show symbols from this file.
        #[arg(short, long)]
        file: Option<String>,

        /// Print only the result count, not the full results.
        #[arg(long)]
        count: bool,
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

        /// Run community detection and print results grouped by community.
        #[arg(long)]
        communities: bool,

        /// Show top N surprising edges (anomaly detection).
        #[arg(long)]
        surprises: Option<usize>,

        /// Export graph as a self-contained HTML visualization.
        #[arg(long)]
        html: Option<Option<PathBuf>>,

        /// Export graph as GraphML (for Gephi/yEd).
        #[arg(long)]
        graphml: Option<Option<PathBuf>>,

        /// Export graph as Neo4j Cypher statements.
        #[arg(long)]
        cypher: Option<Option<PathBuf>>,

        /// Export graph as an Obsidian vault directory.
        #[arg(long)]
        obsidian: Option<Option<PathBuf>>,
    },

    /// Manage git hooks for automatic index updates.
    Hook {
        #[command(subcommand)]
        action: HookAction,
    },

    /// Find the shortest path between two files in the dependency graph.
    Path {
        /// Source file (relative path).
        from: String,
        /// Target file (relative path).
        to: String,
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

    /// Literal or regex text scan across indexed files (trigram-accelerated).
    ///
    /// Unlike `search` (BM25/vector-ranked semantic retrieval), `grep` does a
    /// direct file-content scan and emits every matching line. Use it for
    /// exact identifiers, string literals, drift audits, version lookups, and
    /// anything where "I want to see the line, not a ranked chunk" is the
    /// right shape of answer. Respects the indexed file set (no `target/`,
    /// `node_modules/`, `.git/` noise) and uses the trigram index to skip
    /// files that can't possibly match.
    Grep {
        /// Pattern to search for. Interpreted as a regex (RE2 syntax) unless
        /// `--literal` is passed.
        pattern: String,

        /// Restrict scanning to a single file (relative path). Bypasses the
        /// trigram pre-filter but keeps ignore rules.
        #[arg(long)]
        file: Option<String>,

        /// Glob pattern to restrict scanned files (e.g. `*.rs`, `src/**/*.py`).
        #[arg(long)]
        glob: Option<String>,

        /// Treat pattern as a plain literal string; regex metacharacters are
        /// escaped before compilation.
        #[arg(long)]
        literal: bool,

        /// Case-insensitive matching (ASCII).
        #[arg(short = 'i', long = "ignore-case")]
        case_insensitive: bool,

        /// Emit lines that do NOT match the pattern. Disables the trigram
        /// pre-filter — every indexed file is scanned.
        #[arg(long)]
        invert: bool,

        /// Symmetric surrounding context lines (max 5). Overridden by
        /// `--before-context` / `--after-context` if those are set.
        #[arg(short = 'C', long)]
        context: Option<usize>,

        /// Lines of context before each match (max 5).
        #[arg(short = 'B', long = "before-context")]
        before: Option<usize>,

        /// Lines of context after each match (max 5).
        #[arg(short = 'A', long = "after-context")]
        after: Option<usize>,

        /// Print only a count summary instead of per-line output.
        #[arg(long)]
        count: bool,

        /// Print only the set of file paths that contain at least one match.
        #[arg(long = "files-with-matches")]
        files_with_matches: bool,

        /// Emit one JSON object per match (deterministic field order).
        #[arg(long)]
        json: bool,

        /// Maximum matches to return. Default is 50 for per-line output. When
        /// `--count` or `--files-with-matches` is set and no explicit limit is
        /// passed, the cap is dropped so totals reflect the real match set.
        #[arg(long)]
        limit: Option<usize>,
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

        /// Print only the result count, not the full results.
        #[arg(long)]
        count: bool,
    },

    /// Re-index files changed since the last git commit (git-diff aware incremental update).
    Update {
        /// Project root to update (defaults to current directory).
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Re-index a single file instead of scanning git status.
        ///
        /// Path must be relative to the project root (e.g. `src/main.rs`).
        /// Used by the PostToolUse plugin hook to keep the index fresh after
        /// each Edit/Write tool call. Exits 0 silently if no `.codixing/`
        /// index exists (so non-codixing projects see no noise).
        #[arg(long)]
        file: Option<String>,

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

        /// Skip the vector-embedding step.
        ///
        /// Sync will still update BM25, symbols, trigrams, file hashes,
        /// and the dependency graph — only the vector index stays stale.
        /// Use this to avoid runaway CPU on an existing hybrid index, or
        /// when the change set is large and you want BM25+graph fresh now
        /// and will embed later via `codixing embed`.
        #[arg(long)]
        no_embed: bool,

        /// Force a full graph rebuild after the incremental sync.
        ///
        /// Re-parses all indexed files to re-extract import and call edges,
        /// clears the existing graph, recomputes PageRank, and persists the
        /// result. BM25, symbols, trigrams, and vectors are left untouched.
        ///
        /// Faster than `codixing init` when only the call graph is stale
        /// (e.g. after a large refactor that moved many imports around).
        #[arg(long)]
        rebuild_graph: bool,
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

    /// Debug and exercise the TOML output-filter pipeline.
    ///
    /// The filter pipeline compresses MCP tool output before it reaches
    /// the agent (token-tight loops) and tees the full output to
    /// `.codixing/tee/` for recovery. This subcommand lets you check
    /// that `.codixing/filters.toml` parses, lists the rules in effect,
    /// and runs the pipeline on sample input without having to boot
    /// the MCP server.
    Filter {
        #[command(subcommand)]
        action: FilterAction,
    },

    /// Audit files for freshness: finds stale and orphaned files that may need attention.
    ///
    /// Combines git recency, orphan detection, and import graph analysis to classify
    /// files into three tiers:
    ///   Critical — no importers AND not modified in threshold_days (dead code candidates)
    ///   Warning  — stale but still imported by other files
    ///   Info     — recently orphaned (no importers but freshly modified)
    Audit {
        /// Flag files not modified in this many days.
        #[arg(long, default_value = "21")]
        threshold_days: u64,

        /// File pattern to include (substring match, e.g. "crates/", "*.rs").
        #[arg(long)]
        include: Option<String>,

        /// File pattern to exclude (substring match, e.g. "test", "vendor").
        #[arg(long)]
        exclude: Option<String>,

        /// Project root directory (defaults to current directory).
        #[arg(default_value = ".")]
        path: PathBuf,
    },

    /// Analyze the blast radius of changing a file: direct dependents,
    /// transitive dependents, and affected tests.
    Impact {
        /// File to analyze (relative path).
        file: String,

        /// Output results as JSON.
        #[arg(long)]
        json: bool,
    },

    /// List the public API surface of a file.
    Api {
        /// File to analyze (relative path).
        file: String,

        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Show type relationships for a symbol (implements, extends, returns, contains).
    Types {
        /// Symbol name to look up.
        symbol: String,

        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Find usage examples for a symbol (tests, call sites, doc blocks).
    Examples {
        /// Symbol name to look up.
        symbol: String,

        /// Maximum number of examples.
        #[arg(short, long, default_value = "5")]
        limit: usize,

        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Assemble cross-file context for understanding a code location.
    Context {
        /// File path (relative to project root).
        file: String,

        /// Line number (0-indexed).
        #[arg(long, default_value = "0")]
        line: u64,

        /// Token budget for context assembly.
        #[arg(long, default_value = "4096")]
        token_budget: usize,

        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
}

/// Subcommands for the `codixing filter` command.
#[derive(Subcommand)]
enum FilterAction {
    /// Parse and validate the repo-local `.codixing/filters.toml` (if
    /// present) on top of the built-in defaults and list the resulting
    /// ruleset. Exits non-zero if the local file is invalid TOML.
    Check {
        /// Project root (defaults to current directory). The command
        /// reads `<root>/.codixing/filters.toml` if it exists.
        #[arg(default_value = ".")]
        path: PathBuf,
    },

    /// Run the pipeline on sample input from stdin and print the
    /// filtered result. Use `--tool <name>` to simulate the tool name
    /// so rules that match by tool are exercised.
    Run {
        /// Simulated MCP tool name (affects which rules match).
        #[arg(long, default_value = "code_search")]
        tool: String,

        /// Project root (defaults to current directory).
        #[arg(long, default_value = ".")]
        path: PathBuf,
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

/// Git hook management actions.
#[derive(Subcommand)]
enum HookAction {
    /// Install a post-commit hook for automatic git-sync.
    Install,
    /// Remove the codixing post-commit hook.
    Uninstall,
    /// Show hook installation status.
    Status,
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
    /// Embedding-free semantic matching using behavioral signatures.
    Semantic,
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
            StrategyArg::Semantic => Strategy::Semantic,
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
            embed,
            model,
            reranker,
            defer_embeddings,
            wait,
        } => cmd_init(
            path,
            also,
            languages,
            embed,
            model,
            reranker,
            defer_embeddings,
            wait,
        ),
        Command::Search {
            query,
            limit,
            file,
            strategy,
            format,
            json,
            token_budget,
            code_only,
            docs_only,
            count,
        } => {
            let doc_filter = if code_only {
                Some(codixing_core::DocFilter::CodeOnly)
            } else if docs_only {
                Some(codixing_core::DocFilter::DocsOnly)
            } else {
                None
            };
            cmd_search(
                query,
                limit,
                file,
                strategy,
                format || token_budget.is_some(),
                json,
                token_budget,
                doc_filter,
                count,
            )
        }
        Command::Symbols {
            filter,
            file,
            count,
        } => cmd_symbols(filter, file, count),
        Command::Graph {
            path,
            token_budget,
            map,
            communities,
            surprises,
            html,
            graphml,
            cypher,
            obsidian,
        } => cmd_graph(
            path,
            token_budget,
            map,
            communities,
            surprises,
            html,
            graphml,
            cypher,
            obsidian,
        ),
        Command::Hook { action } => cmd_hook(action),
        Command::Path { from, to } => cmd_path(from, to),
        Command::Callers { file, depth } => cmd_callers(file, depth),
        Command::Callees { file, depth } => cmd_callees(file, depth),
        Command::Dependencies { file, depth } => cmd_dependencies(file, depth),
        Command::CrossImports { from, to } => cmd_cross_imports(from, to),
        Command::Grep {
            pattern,
            file,
            glob,
            literal,
            case_insensitive,
            invert,
            context,
            before,
            after,
            count,
            files_with_matches,
            json,
            limit,
        } => cmd_grep(GrepArgs {
            pattern,
            file,
            glob,
            literal,
            case_insensitive,
            invert,
            context,
            before,
            after,
            count,
            files_with_matches,
            json,
            limit,
        }),
        Command::Usages {
            symbol,
            limit,
            file,
            count,
        } => cmd_usages(symbol, limit, file, count),
        Command::Update {
            path,
            dry_run,
            file,
        } => cmd_update(path, dry_run, file),
        Command::Sync {
            path,
            no_embed,
            rebuild_graph,
        } => cmd_sync(path, no_embed, rebuild_graph),
        Command::GitSync { path } => cmd_git_sync(path),
        Command::Embed { path } => cmd_embed(path),
        Command::BenchEmbed { path, force, json } => cmd_bench_embed(path, force, json),
        Command::Serve { host, port, path } => cmd_serve(host, port, path).await,
        Command::Federation { action } => cmd_federation(action),
        Command::Filter { action } => cmd_filter(action),
        Command::Audit {
            threshold_days,
            include,
            exclude,
            path,
        } => cmd_audit(path, threshold_days, include, exclude),
        Command::Impact { file, json } => cmd_impact(file, json),
        Command::Api { file, json } => cmd_api(file, json),
        Command::Types { symbol, json } => cmd_types(symbol, json),
        Command::Examples {
            symbol,
            limit,
            json,
        } => cmd_examples(symbol, limit, json),
        Command::Context {
            file,
            line,
            token_budget,
            json,
        } => cmd_context(file, line, token_budget, json),
    }
}

/// Check if an error indicates write-lock contention and print a helpful message.
fn check_write_lock_error(err: &anyhow::Error) -> bool {
    let msg = format!("{err:#}");
    if msg.contains("read-only mode") || msg.contains("ReadOnly") || msg.contains("write lock") {
        eprintln!("Error: another process holds the Tantivy write lock (likely the MCP server).");
        eprintln!();
        eprintln!("Options:");
        eprintln!("  1. Use the sync_index or git_sync_index MCP tool");
        eprintln!("     (the running server can sync for you)");
        eprintln!("  2. Stop the MCP server, sync, then restart");
        true
    } else {
        false
    }
}

/// Wrap a core engine error with context, exiting the process on write-lock
/// conflicts (so callers get a friendly message rather than a raw panic trace).
fn handle_engine_err<E: std::fmt::Display>(e: E, ctx: impl FnOnce() -> String) -> anyhow::Error {
    let err = anyhow::anyhow!("{e}");
    if check_write_lock_error(&err) {
        std::process::exit(1);
    }
    err.context(ctx())
}

/// Print a non-empty list of file paths (one per line to stdout) with a count
/// summary to stderr. Caller must guarantee the slice is non-empty.
fn print_nonempty_file_list(items: &[String], count_label: &str) {
    for item in items {
        println!("{item}");
    }
    eprintln!("\n{} {count_label}.", items.len());
}

/// Print a file list or an empty-message if the list is empty.
fn print_file_list(items: &[String], empty_msg: &str, count_label: &str) {
    if items.is_empty() {
        eprintln!("{empty_msg}");
    } else {
        print_nonempty_file_list(items, count_label);
    }
}

/// Handle a daemon-proxy response for a file-list command (callers/callees).
/// Takes the raw text blob returned by the daemon, prints it or an empty
/// message, and returns `Ok(())`.
fn emit_daemon_file_list(text: String, empty_msg: &str, count_label: &str) -> Result<()> {
    if text.is_empty() {
        eprintln!("{empty_msg}");
    } else {
        print!("{text}");
        let count = text.lines().count();
        eprintln!("\n{count} {count_label}.");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_init(
    path: PathBuf,
    also: Vec<PathBuf>,
    languages: Vec<String>,
    embed: bool,
    model: String,
    reranker: bool,
    defer_embeddings: bool,
    wait: bool,
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

    // v0.33: BM25+graph is the default. Embeddings only fire with --embed.
    config.embedding.enabled = embed;

    if embed {
        config.embedding.model = parse_embedding_model(&model)?;
        if reranker {
            config.embedding.reranker_enabled = true;
        }
        if defer_embeddings {
            config.embedding.enabled = false;
            eprintln!(
                "Deferring embeddings — BM25+graph available immediately. Run `codixing embed` to add vectors."
            );
        }
    } else if reranker || defer_embeddings {
        eprintln!(
            "note: --reranker and --defer-embeddings are no-ops without --embed. \
             Pass --embed to build vector embeddings."
        );
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
    if embed && defer_embeddings {
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

    if embed && !engine.embeddings_ready() {
        let (done, total) = engine.embedding_progress();
        eprintln!("Embedding {total} chunks in background ({done}/{total} complete)...");
        eprintln!("Search is available now (BM25+graph ready; vector hits phase in).");

        if wait {
            eprintln!("Waiting for embeddings to complete (--wait)...");
            engine.wait_for_embeddings();
            eprintln!("Embeddings complete.");
        }
    }

    Ok(())
}

fn parse_embedding_model(s: &str) -> Result<EmbeddingModel> {
    match s.to_lowercase().as_str() {
        "bge-small-en" | "bge-small" | "small" => Ok(EmbeddingModel::BgeSmallEn),
        "bge-small-en-q" | "bge-small-q" | "small-q" => Ok(EmbeddingModel::BgeSmallEnQ),
        "bge-base-en" | "bge-base" | "base" => Ok(EmbeddingModel::BgeBaseEn),
        "bge-large-en" | "bge-large" | "large" => Ok(EmbeddingModel::BgeLargeEn),
        "jina" | "jina-embed-code" => Ok(EmbeddingModel::JinaEmbedCode),
        "nomic-embed-code" | "nomic" => Ok(EmbeddingModel::NomicEmbedCode),
        "snowflake-arctic-xs-q" | "arctic-xs-q" | "arctic-xs" => {
            Ok(EmbeddingModel::SnowflakeArcticEmbedXSQ)
        }
        "snowflake-arctic-l" | "arctic-l" | "arctic" => Ok(EmbeddingModel::SnowflakeArcticEmbedL),
        "qwen3" | "qwen" => {
            #[cfg(feature = "qwen3")]
            return Ok(EmbeddingModel::Qwen3SmallEmbedding);
            #[cfg(not(feature = "qwen3"))]
            anyhow::bail!("qwen3 model requires building with --features codixing-core/qwen3")
        }
        "model2vec" | "m2v" | "potion" | "potion-8m" => Ok(EmbeddingModel::Model2Vec),
        "model2vec-retrieval" | "m2v-retrieval" | "potion-retrieval" | "potion-32m" => {
            Ok(EmbeddingModel::Model2VecRetrieval)
        }
        "jina-code-int8" | "jina-int8" => Ok(EmbeddingModel::JinaCodeInt8),
        "model2vec-jina-code" | "m2v-jina" | "jina-m2v" => Ok(EmbeddingModel::Model2VecJinaCode),
        other => anyhow::bail!(
            "unknown model '{}'. Valid: bge-small-en, bge-base-en, bge-large-en, jina-embed-code, jina-code-int8, nomic-embed-code, snowflake-arctic-l, qwen3, model2vec, model2vec-retrieval, model2vec-jina-code",
            other
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn cmd_search(
    query: String,
    limit: usize,
    file: Option<String>,
    strategy: StrategyArg,
    format: bool,
    json: bool,
    token_budget: Option<usize>,
    doc_filter: Option<codixing_core::DocFilter>,
    count: bool,
) -> Result<()> {
    let root = std::env::current_dir().context("cannot determine current directory")?;

    // Fast path: if a codixing-mcp daemon is running at .codixing/daemon.sock,
    // proxy the search through it. The daemon holds the engine in memory, so
    // this avoids the ~4s cold-process startup on large hybrid indexes.
    //
    // We only take the fast path when the caller wants plain or formatted
    // output. --json wants structured results that the MCP text body doesn't
    // preserve, and --file / specific strategies / token_budget require
    // flags the code_search tool doesn't fully expose, so those fall through
    // to the in-process path.
    // --count also bypasses the daemon: we need to parse individual results to
    // get an accurate count, and the daemon returns a formatted text block.
    // The `format` flag doesn't change anything here because the MCP body is
    // already a formatted markdown text block — the daemon path effectively
    // always produces "formatted" output, which is what agents and humans
    // both want from a warm-index search.
    {
        let _ = format;
        let can_use_daemon = !json
            && !count
            && file.is_none()
            && token_budget.is_none()
            && doc_filter.is_none()
            && matches!(strategy, StrategyArg::Auto);
        if can_use_daemon {
            if let Some(text) = daemon_proxy::try_search(&root, &query, limit) {
                print!("{text}");
                return Ok(());
            }
        }
    }

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
    if let Some(f) = doc_filter {
        sq = sq.with_doc_filter(f);
    }

    let results = engine.search(sq).context("search failed")?;

    if count {
        println!("{} result(s) found", results.len());
        return Ok(());
    }

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
        let is_doc = result.is_doc();

        if is_doc {
            let breadcrumb = if result.scope_chain.is_empty() {
                String::new()
            } else {
                format!(" \u{00a7} {}", result.scope_chain.join(" > "))
            };
            println!(
                "{}. {}{} [doc] score={:.3}",
                i + 1,
                result.file_path,
                breadcrumb,
                result.score,
            );
        } else {
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
        }

        // Snippet (3 lines) — same for both.
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

struct GrepArgs {
    pattern: String,
    file: Option<String>,
    glob: Option<String>,
    literal: bool,
    case_insensitive: bool,
    invert: bool,
    context: Option<usize>,
    before: Option<usize>,
    after: Option<usize>,
    count: bool,
    files_with_matches: bool,
    json: bool,
    limit: Option<usize>,
}

fn cmd_grep(args: GrepArgs) -> Result<()> {
    let root = std::env::current_dir().context("cannot determine current directory")?;

    let sym = args.context.unwrap_or(0);
    let before_context = args.before.unwrap_or(sym).min(5);
    let after_context = args.after.unwrap_or(sym).min(5);

    // Count / files-with-matches need the true total, not a 50-hit cap.
    // When the user explicitly passes --limit we still honour it.
    let effective_limit = match args.limit {
        Some(n) => n,
        None if args.count || args.files_with_matches => usize::MAX,
        None => 50,
    };

    // `--file` targets a single file — glob it directly and skip the trigram
    // prefilter so we see every match even in files outside the indexed set.
    let effective_glob = match (&args.file, &args.glob) {
        (Some(f), _) => Some(f.clone()),
        (None, Some(g)) => Some(g.clone()),
        (None, None) => None,
    };

    // Fast path: daemon proxy. Only available when no --json / --file (the
    // daemon handler ignores args it doesn't recognize, but we want the CLI's
    // JSON rendering to match byte-for-byte whether warm or cold).
    if !args.json && args.file.is_none() {
        if let Some(text) = daemon_proxy::try_grep(
            &root,
            &args.pattern,
            args.literal,
            args.case_insensitive,
            args.invert,
            effective_glob.as_deref(),
            before_context,
            after_context,
            args.count,
            args.files_with_matches,
            effective_limit,
        ) {
            print!("{text}");
            if !text.ends_with('\n') {
                println!();
            }
            return Ok(());
        }
    }

    let engine = Engine::open(&root).with_context(|| {
        format!(
            "no index found at {} — run `codixing init` first",
            root.display()
        )
    })?;

    let opts = GrepOptions {
        pattern: args.pattern.clone(),
        literal: args.literal,
        case_insensitive: args.case_insensitive,
        invert: args.invert,
        file_glob: effective_glob,
        before_context,
        after_context,
        limit: effective_limit,
        count_mode: args.count || args.files_with_matches,
    };

    let matches = engine
        .grep_code_opts(&opts)
        .map_err(|e| handle_engine_err(e, || "grep failed".into()))?;

    if args.count {
        let files: std::collections::BTreeSet<&str> =
            matches.iter().map(|m| m.file_path.as_str()).collect();
        println!("{} matches across {} files", matches.len(), files.len());
        return Ok(());
    }

    if args.files_with_matches {
        let files: std::collections::BTreeSet<&str> =
            matches.iter().map(|m| m.file_path.as_str()).collect();
        for f in files {
            println!("{f}");
        }
        return Ok(());
    }

    if args.json {
        for m in &matches {
            let obj = serde_json::json!({
                "path": m.file_path,
                "line": m.line_number + 1,
                "col": m.match_start + 1,
                "text": m.line,
                "match_start": m.match_start,
                "match_end": m.match_end,
                "before": m.before,
                "after": m.after,
            });
            println!("{obj}");
        }
        return Ok(());
    }

    // Default format: path:line:col:text (1-indexed line/col to match grep -n).
    for m in &matches {
        println!(
            "{}:{}:{}:{}",
            m.file_path,
            m.line_number + 1,
            m.match_start + 1,
            m.line
        );
    }
    if matches.is_empty() {
        eprintln!("No matches for `{}`.", args.pattern);
    }

    Ok(())
}

fn cmd_symbols(filter: String, file: Option<String>, count: bool) -> Result<()> {
    let root = std::env::current_dir().context("cannot determine current directory")?;

    // Fast path: proxy through the daemon if one is running.
    // Skip daemon proxy for --count: we need the raw symbol list to count accurately.
    if !filter.is_empty() && !count {
        if let Some(text) = daemon_proxy::try_symbols(&root, &filter, file.as_deref()) {
            print!("{text}");
            return Ok(());
        }
    }

    let engine = Engine::open(&root).with_context(|| {
        format!(
            "no index found at {} — run `codixing init` first",
            root.display()
        )
    })?;

    let symbols = engine
        .symbols(&filter, file.as_deref())
        .context("symbol lookup failed")?;

    if count {
        println!("{} symbol(s) found", symbols.len());
        return Ok(());
    }

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

#[allow(clippy::too_many_arguments)]
fn cmd_graph(
    path: PathBuf,
    token_budget: usize,
    map: bool,
    communities: bool,
    surprises: Option<usize>,
    html: Option<Option<PathBuf>>,
    graphml: Option<Option<PathBuf>>,
    cypher: Option<Option<PathBuf>>,
    obsidian: Option<Option<PathBuf>>,
) -> Result<()> {
    let root = path
        .canonicalize()
        .with_context(|| format!("path not found: {}", path.display()))?;

    // Fast path for `graph --map`: proxy through the daemon's
    // `get_repo_map` tool when the other flags are absent.
    if map
        && html.is_none()
        && graphml.is_none()
        && cypher.is_none()
        && obsidian.is_none()
        && !communities
        && surprises.is_none()
    {
        if let Some(text) = daemon_proxy::try_repo_map(&root, Some(token_budget)) {
            print!("{text}");
            return Ok(());
        }
    }

    let mut engine = Engine::open(&root).with_context(|| {
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

    // --html: export interactive visualization.
    if let Some(html_path) = html {
        let output = html_path.unwrap_or_else(|| PathBuf::from("graph.html"));
        // Run community detection first so the HTML has community colors.
        engine.detect_communities();
        let opts = HtmlExportOptions {
            output_path: output.clone(),
            ..Default::default()
        };
        engine
            .export_html(opts)
            .with_context(|| "failed to export HTML graph")?;
        println!("Graph exported to {}", output.display());
        return Ok(());
    }

    // --graphml: export as GraphML.
    if let Some(graphml_path) = graphml {
        let output = graphml_path.unwrap_or_else(|| PathBuf::from("graph.graphml"));
        engine.detect_communities();
        let opts = codixing_core::GraphmlExportOptions {
            output_path: output.clone(),
        };
        engine
            .export_graphml(opts)
            .with_context(|| "failed to export GraphML")?;
        println!("GraphML exported to {}", output.display());
        return Ok(());
    }

    // --cypher: export as Neo4j Cypher.
    if let Some(cypher_path) = cypher {
        let output = cypher_path.unwrap_or_else(|| PathBuf::from("graph.cypher"));
        engine.detect_communities();
        let opts = codixing_core::CypherExportOptions {
            output_path: output.clone(),
        };
        engine
            .export_cypher(opts)
            .with_context(|| "failed to export Cypher")?;
        println!("Cypher exported to {}", output.display());
        return Ok(());
    }

    // --obsidian: export as Obsidian vault.
    if let Some(obsidian_path) = obsidian {
        let output = obsidian_path.unwrap_or_else(|| PathBuf::from("codixing-vault"));
        engine.detect_communities();
        let opts = codixing_core::ObsidianExportOptions {
            output_dir: output.clone(),
        };
        let count = engine
            .export_obsidian(opts)
            .with_context(|| "failed to export Obsidian vault")?;
        println!(
            "Obsidian vault exported to {} ({} notes)",
            output.display(),
            count
        );
        return Ok(());
    }

    // --communities: run Louvain community detection.
    if communities {
        match engine.detect_communities() {
            Some(result) => {
                println!("Community Detection (Louvain)");
                println!("  Communities found: {}", result.community_count);
                println!("  Modularity:        {:.4}", result.modularity);
                println!();

                // Group files by community.
                let mut by_community: std::collections::BTreeMap<usize, Vec<String>> =
                    std::collections::BTreeMap::new();
                for (path, comm) in &result.assignments {
                    by_community.entry(*comm).or_default().push(path.clone());
                }
                for (comm_id, mut files) in by_community {
                    files.sort();
                    println!("  Community {} ({} files):", comm_id, files.len());
                    for f in files.iter().take(20) {
                        println!("    {f}");
                    }
                    if files.len() > 20 {
                        println!("    ... and {} more", files.len() - 20);
                    }
                    println!();
                }
            }
            None => eprintln!("Graph not available — re-run `codixing init`"),
        }
        return Ok(());
    }

    // --surprises: anomaly detection.
    if let Some(top_n) = surprises {
        let n = if top_n == 0 { 10 } else { top_n };
        // Run community detection first for cross-community scoring.
        engine.detect_communities();
        let edges = engine.surprising_edges(n);
        if edges.is_empty() {
            println!("No surprising edges found.");
        } else {
            println!("Top {} Surprising Edges", edges.len());
            println!();
            for (i, edge) in edges.iter().enumerate() {
                println!(
                    "  {}. {} -> {} (score: {:.2})",
                    i + 1,
                    edge.from,
                    edge.to,
                    edge.score
                );
                for reason in &edge.reasons {
                    println!("     - {reason}");
                }
            }
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
            println!("  Doc edges:         {}", stats.doc_edges);
            println!("  Symbol nodes:      {}", stats.symbol_nodes);
            println!("  Symbol edges:      {}", stats.symbol_edges);
            let (v, h, m, l) = stats.confidence_counts;
            println!("  Confidence:        verified={v} high={h} medium={m} low={l}");
        }
        None => eprintln!("Graph not available — re-run `codixing init`"),
    }
    Ok(())
}

fn cmd_path(from: String, to: String) -> Result<()> {
    let root = std::env::current_dir().context("cannot determine current directory")?;
    let engine = Engine::open(&root).with_context(|| {
        format!(
            "no index found at {} — run `codixing init` first",
            root.display()
        )
    })?;

    match engine.shortest_path(&from, &to) {
        Some(path) => {
            println!("{}", path.join(" -> "));
        }
        None => {
            eprintln!("No path found between \"{}\" and \"{}\"", from, to);
        }
    }

    Ok(())
}

fn cmd_callers(file: String, depth: usize) -> Result<()> {
    let root = std::env::current_dir().context("cannot determine current directory")?;

    // Fast path: proxy through the daemon when asking for direct callers only
    // (depth <= 1). Transitive callers require multi-hop graph traversal that
    // the daemon's `file_callers` tool doesn't expose, so those fall through.
    if depth <= 1 {
        if let Some(text) = daemon_proxy::try_callers(&root, &file) {
            return emit_daemon_file_list(
                text,
                &format!("No callers found for \"{file}\"."),
                "caller(s) found",
            );
        }
    }

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
        // Distinguish "file exists and is a true leaf" from "file isn't in
        // the graph" (either the index is stale or the path doesn't match).
        // Probe the callees side too — if BOTH sides are empty, the file is
        // almost certainly not represented in the graph at all.
        let callees = engine.callees(&file);
        let file_exists_on_disk = root.join(&file).exists();
        if callees.is_empty() {
            if file_exists_on_disk {
                eprintln!(
                    "No callers found for \"{}\".\n\
                     The file exists on disk but has no graph edges in either direction. \
                     This usually means the index doesn't know about this file yet — \
                     run `codixing sync` or `codixing init` to rebuild, or check the \
                     path spelling (paths are relative to the repo root).",
                    file
                );
            } else {
                eprintln!(
                    "No callers found for \"{}\".\n\
                     The file does not exist on disk at {}. Check the path — codixing \
                     expects repo-root-relative paths like `crates/core/src/lib.rs`.",
                    file,
                    root.join(&file).display()
                );
            }
        } else {
            eprintln!(
                "No callers found for \"{}\" — the file is imported by nothing, but it does \
                 import {} other file(s). It is likely an entry point (main, lib root, binary), \
                 a test harness, or a leaf module that no one uses yet.",
                file,
                callees.len()
            );
        }
        return Ok(());
    }

    // Empty case was returned above; this branch is always non-empty.
    print_nonempty_file_list(&callers, "caller(s) found");
    Ok(())
}

fn cmd_callees(file: String, depth: usize) -> Result<()> {
    let root = std::env::current_dir().context("cannot determine current directory")?;

    // Fast path: proxy through the daemon when asking for direct callees only
    // (depth <= 1). Transitive callees require multi-hop traversal not exposed
    // by the `file_callees` tool, so those fall through to in-process.
    if depth <= 1 {
        if let Some(text) = daemon_proxy::try_callees(&root, &file) {
            return emit_daemon_file_list(
                text,
                &format!("No dependencies found for \"{file}\""),
                "dependency/dependencies found",
            );
        }
    }

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
    print_file_list(
        &callees,
        &format!("No dependencies found for \"{}\"", file),
        "dependency/dependencies found",
    );
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
    print_file_list(
        &deps,
        &format!("No transitive dependencies found for \"{}\"", file),
        "transitive dependency/dependencies found",
    );
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

    let ranked = engine.cross_imports_ranked(&from, &to, None);
    if ranked.is_empty() {
        eprintln!("No files in \"{}\" import from \"{}\"", from, to);
        return Ok(());
    }

    for (f, score) in &ranked {
        println!("{f} (score: {score:.3})");
    }
    eprintln!(
        "\n{} file(s) in \"{}\" import from \"{}\".",
        ranked.len(),
        from,
        to
    );
    Ok(())
}

fn cmd_usages(symbol: String, limit: usize, file: Option<String>, count: bool) -> Result<()> {
    let root = std::env::current_dir().context("cannot determine current directory")?;

    // Fast path: proxy through the daemon when no file filter (the MCP
    // search_usages tool doesn't expose a file filter parameter).
    // Skip daemon proxy for --count: we need the raw result list to count accurately.
    if file.is_none() && !count {
        let _ = limit; // MCP tool uses its own limit; CLI limit is approximated
        if let Some(text) = daemon_proxy::try_usages(&root, &symbol) {
            print!("{text}");
            return Ok(());
        }
    }

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

    if count {
        println!("{} usage(s) found", results.len());
        return Ok(());
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

fn cmd_update(path: PathBuf, dry_run: bool, file: Option<String>) -> Result<()> {
    let root = path
        .canonicalize()
        .with_context(|| format!("path not found: {}", path.display()))?;

    // Fast path: single-file surgical re-index for PostToolUse hook.
    if let Some(ref rel) = file {
        // Silent no-op when no index exists — non-codixing projects must not
        // see errors from the plugin hook.
        if !root.join(".codixing").is_dir() {
            return Ok(());
        }
        let rel_path = std::path::Path::new(rel);
        if rel_path.is_absolute()
            || rel_path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            anyhow::bail!(
                "--file must be a project-relative path with no '..' components, got: {rel}"
            );
        }
        if dry_run {
            eprintln!("(dry run) would reindex: {rel}");
            return Ok(());
        }
        let abs = match root.join(rel_path).canonicalize() {
            Ok(p) => p,
            Err(_) => {
                // Silent no-op when the target file doesn't exist — the hook
                // shouldn't error on freshly-deleted or never-created files.
                return Ok(());
            }
        };
        if !abs.starts_with(&root) {
            anyhow::bail!("--file must stay within the project root, got: {rel}");
        }
        let mut engine = Engine::open(&root).with_context(|| {
            format!(
                "no index found at {} — run `codixing init` first",
                root.display()
            )
        })?;
        let start = Instant::now();
        engine
            .reindex_file(&abs)
            .map_err(|e| handle_engine_err(e, || format!("failed to reindex {rel}")))?;
        engine.save().map_err(|e| {
            handle_engine_err(e, || "failed to save index after --file update".into())
        })?;
        eprintln!("reindexed {} in {:.2}s", rel, start.elapsed().as_secs_f64());
        return Ok(());
    }

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
            Err(e) => {
                let anyhow_err = anyhow::anyhow!("{e}");
                if check_write_lock_error(&anyhow_err) {
                    std::process::exit(1);
                }
                eprintln!("  warning: skipped {} — {e}", rel_path.display());
            }
        }
    }

    for rel_path in &to_remove {
        match engine.remove_file(rel_path) {
            Ok(()) => removed += 1,
            Err(e) => {
                let anyhow_err = anyhow::anyhow!("{e}");
                if check_write_lock_error(&anyhow_err) {
                    std::process::exit(1);
                }
                eprintln!("  warning: remove failed {} — {e}", rel_path.display());
            }
        }
    }

    match engine.save() {
        Ok(()) => {}
        Err(e) => {
            let anyhow_err = anyhow::anyhow!("{e}");
            if check_write_lock_error(&anyhow_err) {
                std::process::exit(1);
            }
            return Err(anyhow_err).context("failed to save index after update");
        }
    }

    eprintln!(
        "\nUpdated {updated} file(s), removed {removed} file(s) in {:.2}s",
        start.elapsed().as_secs_f64()
    );

    Ok(())
}

fn cmd_sync(path: PathBuf, no_embed: bool, rebuild_graph: bool) -> Result<()> {
    let root = path
        .canonicalize()
        .with_context(|| format!("path not found: {}", path.display()))?;

    let mut engine =
        Engine::open(&root).with_context(|| "no index found — run `codixing init` first")?;

    if no_embed {
        eprintln!("[sync] --no-embed active — vector index will be left stale");
    }
    if rebuild_graph {
        eprintln!("[sync] --rebuild-graph active — full graph rebuild after sync");
    }

    let start = Instant::now();
    let stage_start = std::sync::Arc::new(std::sync::Mutex::new(Instant::now()));
    let stage_start_cb = stage_start.clone();
    let options = codixing_core::SyncOptions {
        skip_embed: no_embed,
        rebuild_graph,
    };
    let stats = match engine.sync_with_options(options, move |msg| {
        let mut t = stage_start_cb.lock().unwrap_or_else(|e| e.into_inner());
        let elapsed = t.elapsed();
        *t = Instant::now();
        eprintln!("[sync +{:>5.1}s] {}", elapsed.as_secs_f64(), msg);
    }) {
        Ok(s) => s,
        Err(e) => {
            let anyhow_err = anyhow::anyhow!("{e}");
            if check_write_lock_error(&anyhow_err) {
                std::process::exit(1);
            }
            return Err(anyhow_err);
        }
    };

    eprintln!(
        "sync complete: {} added, {} modified, {} removed, {} unchanged ({:.2}s)",
        stats.added,
        stats.modified,
        stats.removed,
        stats.unchanged,
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
    let stats = match engine.git_sync() {
        Ok(s) => s,
        Err(e) => {
            let anyhow_err = anyhow::anyhow!("{e}");
            if check_write_lock_error(&anyhow_err) {
                std::process::exit(1);
            }
            return Err(anyhow_err);
        }
    };

    if stats.unchanged {
        eprintln!(
            "index already up-to-date (no git changes since last index, {:.2}s)",
            start.elapsed().as_secs_f64()
        );
    } else {
        eprintln!(
            "git-sync complete: {} modified, {} removed ({:.2}s)",
            stats.modified,
            stats.removed,
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

    let engine =
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
        eprintln!("  Chunks:            {}", stats.embedded_chunks);
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

fn cmd_filter(action: FilterAction) -> Result<()> {
    use codixing_core::filter_pipeline::FilterPipeline;

    match action {
        FilterAction::Check { path } => {
            let root = path
                .canonicalize()
                .with_context(|| format!("path not found: {}", path.display()))?;
            let codixing_dir = root.join(".codixing");
            if !codixing_dir.is_dir() {
                eprintln!(
                    "note: no .codixing/ at {} — loading built-in defaults only",
                    root.display()
                );
            }
            let local_path = codixing_dir.join("filters.toml");
            if local_path.is_file() {
                // Read + validate the local file before composing with
                // defaults, so we can report parse errors cleanly.
                let content = std::fs::read_to_string(&local_path)
                    .with_context(|| format!("failed to read {}", local_path.display()))?;
                codixing_core::filter_pipeline::parse_filter_rules(&content)
                    .with_context(|| format!("invalid TOML in {}", local_path.display()))?;
                println!("✓ {} parsed cleanly", local_path.display());
            } else {
                println!("(no repo-local filters.toml — using built-in defaults only)");
            }

            // Load the composed pipeline and list the active rules. The
            // ruleset field is private so we print the effect instead
            // of the raw struct.
            let _pipeline = FilterPipeline::load(&codixing_dir);
            println!();
            println!("Filter pipeline loaded from {}", codixing_dir.display());
            println!("(use `codixing filter run --tool <name>` to exercise a rule)");
            Ok(())
        }

        FilterAction::Run { tool, path } => {
            use std::io::Read;
            let root = path
                .canonicalize()
                .with_context(|| format!("path not found: {}", path.display()))?;
            let codixing_dir = root.join(".codixing");
            let pipeline = FilterPipeline::load(&codixing_dir);

            let mut input = String::new();
            std::io::stdin()
                .read_to_string(&mut input)
                .context("failed to read stdin — pipe input via: echo ... | codixing filter run")?;

            let result = pipeline.apply(&input, &tool);

            if result.was_filtered {
                eprintln!(
                    "[filter] rule={} input={}B output={}B tee={}",
                    result.rule_name.as_deref().unwrap_or("<unknown>"),
                    input.len(),
                    result.output.len(),
                    result.tee_path.as_deref().unwrap_or("<none>"),
                );
            } else {
                eprintln!(
                    "[filter] no matching rule — output passed through unchanged ({}B)",
                    input.len()
                );
            }

            print!("{}", result.output);
            Ok(())
        }
    }
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

fn cmd_audit(
    path: PathBuf,
    threshold_days: u64,
    include: Option<String>,
    exclude: Option<String>,
) -> Result<()> {
    let root = path
        .canonicalize()
        .with_context(|| format!("path not found: {}", path.display()))?;

    let engine = Engine::open(&root).with_context(|| {
        format!(
            "no index found at {} — run `codixing init` first",
            root.display()
        )
    })?;

    let options = FreshnessOptions {
        threshold_days,
        include_pattern: include,
        exclude_pattern: exclude,
    };

    let report = engine.audit_freshness(options);

    if report.entries.is_empty() {
        if report.files_audited == 0 {
            eprintln!(
                "audit found 0 indexed files — the index is empty or its metadata failed to \
                 reload. Run `codixing sync` or rebuild with `codixing init` to repopulate."
            );
        } else {
            eprintln!(
                "All {} indexed file(s) are fresh — no stale or orphaned files detected.",
                report.files_audited
            );
        }
        return Ok(());
    }

    let critical: Vec<_> = report
        .entries
        .iter()
        .filter(|e| e.tier == FreshnessTier::Critical)
        .collect();
    let warning: Vec<_> = report
        .entries
        .iter()
        .filter(|e| e.tier == FreshnessTier::Warning)
        .collect();
    let info: Vec<_> = report
        .entries
        .iter()
        .filter(|e| e.tier == FreshnessTier::Info)
        .collect();

    if !critical.is_empty() {
        println!("\nCritical (orphan + stale, {}+ days):", threshold_days);
        println!("{}", "-".repeat(80));
        for e in &critical {
            let days_str = if e.days_old == u64::MAX {
                "very old".to_string()
            } else {
                format!("{} days", e.days_old)
            };
            println!("  [CRITICAL] {} ({}) — {}", e.file_path, days_str, e.reason);
        }
    }

    if !warning.is_empty() {
        println!("\nWarning (stale but connected, {}+ days):", threshold_days);
        println!("{}", "-".repeat(80));
        for e in &warning {
            let days_str = if e.days_old == u64::MAX {
                "very old".to_string()
            } else {
                format!("{} days", e.days_old)
            };
            println!("  [WARNING] {} ({}) — {}", e.file_path, days_str, e.reason);
        }
    }

    if !info.is_empty() {
        println!("\nInfo (recently orphaned):");
        println!("{}", "-".repeat(80));
        for e in &info {
            let days_str = if e.days_old == u64::MAX {
                "very old".to_string()
            } else {
                format!("{} days", e.days_old)
            };
            println!("  [INFO] {} ({}) — {}", e.file_path, days_str, e.reason);
        }
    }

    eprintln!(
        "\nSummary: {} file(s) audited — {} critical, {} warning, {} info",
        report.files_audited,
        critical.len(),
        warning.len(),
        info.len(),
    );

    Ok(())
}

fn cmd_impact(file: String, json: bool) -> Result<()> {
    let root = std::env::current_dir().context("cannot determine current directory")?;

    // Fast path: proxy through the daemon (plain output only — JSON mode
    // needs the structured ChangeImpact type, which the MCP text body
    // loses).
    if !json {
        if let Some(text) = daemon_proxy::try_impact(&root, &file) {
            print!("{text}");
            return Ok(());
        }
    }

    let engine = Engine::open(&root).with_context(|| {
        format!(
            "no index found at {} — run `codixing init` first",
            root.display()
        )
    })?;

    let impact = engine.change_impact(&file);

    if json {
        println!("{}", serde_json::to_string_pretty(&impact)?);
        return Ok(());
    }

    println!("# Change Impact: {}", impact.file_path);
    println!();
    println!("Blast radius: {} files", impact.blast_radius);
    println!();

    if !impact.direct_dependents.is_empty() {
        println!("## Direct dependents ({}):", impact.direct_dependents.len());
        for d in &impact.direct_dependents {
            println!("  {d}");
        }
        println!();
    }

    if !impact.transitive_dependents.is_empty() {
        println!(
            "## Transitive dependents ({}):",
            impact.transitive_dependents.len()
        );
        for t in &impact.transitive_dependents {
            println!("  {t}");
        }
        println!();
    }

    if !impact.affected_tests.is_empty() {
        println!("## Affected tests ({}):", impact.affected_tests.len());
        for t in &impact.affected_tests {
            println!("  {t}");
        }
    }

    Ok(())
}

fn cmd_api(file: String, json: bool) -> Result<()> {
    let root = std::env::current_dir().context("cannot determine current directory")?;
    let engine = Engine::open(&root).with_context(|| {
        format!(
            "no index found at {} — run `codixing init` first",
            root.display()
        )
    })?;

    let symbols = engine.api_surface(&file);

    if json {
        let entries: Vec<serde_json::Value> = symbols
            .iter()
            .map(|s| {
                serde_json::json!({
                    "name": s.name,
                    "kind": format!("{:?}", s.kind),
                    "file": &s.file_path,
                    "line": s.line_start,
                    "signature": s.signature,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(());
    }

    if symbols.is_empty() {
        println!("No public API symbols found in {file}");
        return Ok(());
    }

    println!("# Public API: {file}\n");
    for s in &symbols {
        let sig = s.signature.as_deref().unwrap_or(&s.name);
        println!("  {:?} {} (line {})", s.kind, sig, s.line_start);
    }
    Ok(())
}

fn cmd_types(symbol: String, json: bool) -> Result<()> {
    let root = std::env::current_dir().context("cannot determine current directory")?;
    let engine = Engine::open(&root).with_context(|| {
        format!(
            "no index found at {} — run `codixing init` first",
            root.display()
        )
    })?;

    let symbols = engine
        .symbols(&symbol, None)
        .context("symbol lookup failed")?;

    if symbols.is_empty() {
        println!("No symbol found matching '{symbol}'");
        return Ok(());
    }

    if json {
        let entries: Vec<serde_json::Value> = symbols
            .iter()
            .flat_map(|s| {
                s.type_relations.iter().map(move |tr| {
                    serde_json::json!({
                        "symbol": &s.name,
                        "file": &s.file_path,
                        "relation": format!("{}", tr.kind),
                        "target": &tr.target,
                    })
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(());
    }

    println!("# Type Relations: {symbol}\n");
    let mut found = false;
    for s in &symbols {
        if s.type_relations.is_empty() {
            continue;
        }
        found = true;
        println!("  {} ({}:{})", s.name, s.file_path, s.line_start);
        for tr in &s.type_relations {
            println!("    {} → {}", tr.kind, tr.target);
        }
    }
    if !found {
        println!("No type relations found for '{symbol}'");
    }
    Ok(())
}

fn cmd_examples(symbol: String, limit: usize, json: bool) -> Result<()> {
    let root = std::env::current_dir().context("cannot determine current directory")?;
    let engine = Engine::open(&root).with_context(|| {
        format!(
            "no index found at {} — run `codixing init` first",
            root.display()
        )
    })?;

    let examples = engine.find_usage_examples(&symbol, limit);

    if examples.is_empty() {
        println!("No usage examples found for '{symbol}'");
        return Ok(());
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&examples)?);
        return Ok(());
    }

    println!("# Usage Examples: {symbol}\n");
    for (i, ex) in examples.iter().enumerate() {
        let kind_label = match ex.kind {
            codixing_core::engine::examples::ExampleKind::Test => "TEST",
            codixing_core::engine::examples::ExampleKind::CallSite => "CALL",
            codixing_core::engine::examples::ExampleKind::DocBlock => "DOC",
        };
        println!(
            "  {}. [{}] {}:{}-{}",
            i + 1,
            kind_label,
            ex.file_path,
            ex.line_start,
            ex.line_end
        );
        // Indent context lines for readability.
        for line in ex.context.lines() {
            println!("     {line}");
        }
        println!();
    }
    Ok(())
}

fn cmd_context(file: String, line: u64, token_budget: usize, json: bool) -> Result<()> {
    let root = std::env::current_dir().context("cannot determine current directory")?;
    let engine = Engine::open(&root).with_context(|| {
        format!(
            "no index found at {} — run `codixing init` first",
            root.display()
        )
    })?;

    let ctx = engine.assemble_context_for_location(&file, line, token_budget);

    if json {
        println!("{}", serde_json::to_string_pretty(&ctx)?);
        return Ok(());
    }

    println!("# Context: {}:{}\n", ctx.primary.file_path, line);
    println!(
        "Token budget: {} | Used: {}\n",
        token_budget, ctx.total_tokens
    );

    println!(
        "## Primary chunk (L{}-L{})",
        ctx.primary.line_start, ctx.primary.line_end
    );
    println!("```");
    println!("{}", ctx.primary.content);
    println!("```\n");

    if !ctx.imports.is_empty() {
        println!("## Imports ({}):", ctx.imports.len());
        for imp in &ctx.imports {
            println!(
                "  {} (L{}-L{}, relevance: {:.2})",
                imp.file_path, imp.line_start, imp.line_end, imp.relevance
            );
            for line_content in imp.content.lines() {
                println!("    {line_content}");
            }
        }
        println!();
    }

    if !ctx.callees.is_empty() {
        println!("## Callees ({}):", ctx.callees.len());
        for callee in &ctx.callees {
            println!(
                "  {} (L{}-L{}, relevance: {:.2})",
                callee.file_path, callee.line_start, callee.line_end, callee.relevance
            );
            for line_content in callee.content.lines() {
                println!("    {line_content}");
            }
        }
        println!();
    }

    if !ctx.examples.is_empty() {
        println!("## Usage examples ({}):", ctx.examples.len());
        for (i, ex) in ctx.examples.iter().enumerate() {
            let kind_label = match ex.kind {
                codixing_core::engine::examples::ExampleKind::Test => "TEST",
                codixing_core::engine::examples::ExampleKind::CallSite => "CALL",
                codixing_core::engine::examples::ExampleKind::DocBlock => "DOC",
            };
            println!(
                "  {}. [{}] {}:{}-{}",
                i + 1,
                kind_label,
                ex.file_path,
                ex.line_start,
                ex.line_end
            );
            for line_content in ex.context.lines() {
                println!("     {line_content}");
            }
            println!();
        }
    }

    Ok(())
}

fn cmd_hook(action: HookAction) -> Result<()> {
    // Use git rev-parse to resolve the hooks directory correctly, even in worktrees.
    let hooks_dir = std::process::Command::new("git")
        .args(["rev-parse", "--git-path", "hooks"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                String::from_utf8(o.stdout)
                    .ok()
                    .map(|s| PathBuf::from(s.trim()))
            } else {
                None
            }
        })
        .context("not a git repository — run from a project with .git/")?;

    let hook_path = hooks_dir.join("post-commit");
    let codixing_marker = "# codixing: auto-sync index after commit";
    let codixing_line = "codixing git-sync . 2>/dev/null &";

    match action {
        HookAction::Install => {
            std::fs::create_dir_all(&hooks_dir)?;

            if hook_path.exists() {
                let content = std::fs::read_to_string(&hook_path)?;
                if content.contains(codixing_marker) {
                    println!("Codixing hook already installed in {}", hook_path.display());
                    return Ok(());
                }
                // Append to existing hook.
                let mut new_content = content;
                new_content.push_str(&format!("\n{codixing_marker}\n{codixing_line}\n"));
                std::fs::write(&hook_path, new_content)?;
            } else {
                let content = format!("#!/bin/sh\n{codixing_marker}\n{codixing_line}\n");
                std::fs::write(&hook_path, content)?;
            }

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&hook_path, std::fs::Permissions::from_mode(0o755))?;
            }

            println!("Installed post-commit hook: {}", hook_path.display());
            println!("Index will auto-sync after each commit.");
        }
        HookAction::Uninstall => {
            if !hook_path.exists() {
                println!("No post-commit hook found.");
                return Ok(());
            }
            let content = std::fs::read_to_string(&hook_path)?;
            if !content.contains(codixing_marker) {
                println!("No codixing hook found in {}", hook_path.display());
                return Ok(());
            }
            // Only remove lines matching our exact marker and command.
            let new_content: String = content
                .lines()
                .filter(|l| l.trim() != codixing_marker && l.trim() != codixing_line)
                .collect::<Vec<_>>()
                .join("\n");
            let trimmed = new_content.trim();
            if trimmed.is_empty() || trimmed == "#!/bin/sh" {
                std::fs::remove_file(&hook_path)?;
                println!("Removed post-commit hook (was codixing-only).");
            } else {
                std::fs::write(&hook_path, format!("{}\n", new_content.trim()))?;
                println!("Removed codixing lines from post-commit hook.");
            }
        }
        HookAction::Status => {
            if !hook_path.exists() {
                println!("No post-commit hook installed.");
            } else {
                let content = std::fs::read_to_string(&hook_path)?;
                if content.contains(codixing_marker) {
                    println!("Codixing post-commit hook: installed");
                } else {
                    println!("Post-commit hook exists but does not contain codixing.");
                }
            }
        }
    }
    Ok(())
}
