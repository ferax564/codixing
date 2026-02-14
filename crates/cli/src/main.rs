use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use codeforge_core::{Engine, IndexConfig, SearchQuery};

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

        /// Only index these languages (comma-separated, e.g. "rust,python").
        #[arg(long, value_delimiter = ',')]
        languages: Vec<String>,
    },

    /// Search the code index using BM25 full-text ranking.
    Search {
        /// The search query.
        query: String,

        /// Maximum number of results.
        #[arg(short, long, default_value = "10")]
        limit: usize,

        /// Filter results to files matching this substring.
        #[arg(short, long)]
        file: Option<String>,
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

    /// Show code graph statistics and top symbols by PageRank.
    Graph,

    /// List symbols that call (reference) the given symbol.
    Callers {
        /// Symbol name to look up.
        symbol: String,
    },

    /// List symbols that are called (referenced) by the given symbol.
    Callees {
        /// Symbol name to look up.
        symbol: String,
    },
}

fn main() -> Result<()> {
    // Initialize tracing (respects RUST_LOG env var).
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Init { path, languages } => cmd_init(path, languages),
        Command::Search { query, limit, file } => cmd_search(query, limit, file),
        Command::Symbols { filter, file } => cmd_symbols(filter, file),
        Command::Graph => cmd_graph(),
        Command::Callers { symbol } => cmd_callers(symbol),
        Command::Callees { symbol } => cmd_callees(symbol),
    }
}

fn cmd_init(path: PathBuf, languages: Vec<String>) -> Result<()> {
    let root = path
        .canonicalize()
        .with_context(|| format!("path not found: {}", path.display()))?;

    let mut config = IndexConfig::new(&root);
    for lang in &languages {
        config.languages.insert(lang.to_lowercase());
    }

    eprintln!("Indexing {}...", root.display());
    let start = Instant::now();

    let engine = Engine::init(&root, config).with_context(|| "failed to initialize index")?;

    let stats = engine.stats();
    let elapsed = start.elapsed();

    eprintln!(
        "Indexed {} files, {} chunks, {} symbols in {:.2}s",
        stats.file_count,
        stats.chunk_count,
        stats.symbol_count,
        elapsed.as_secs_f64(),
    );

    Ok(())
}

fn cmd_search(query: String, limit: usize, file: Option<String>) -> Result<()> {
    let root = std::env::current_dir().context("cannot determine current directory")?;
    let engine = Engine::open(&root).with_context(|| {
        format!(
            "no index found at {} — run `codeforge init` first",
            root.display()
        )
    })?;

    let mut sq = SearchQuery::new(&query).with_limit(limit);
    if let Some(ref f) = file {
        sq = sq.with_file_filter(f);
    }

    let results = engine.search(sq).context("search failed")?;

    if results.is_empty() {
        eprintln!("No results for \"{}\"", query);
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

fn cmd_graph() -> Result<()> {
    let root = std::env::current_dir().context("cannot determine current directory")?;
    let mut engine = Engine::open(&root).with_context(|| {
        format!(
            "no index found at {} — run `codeforge init` first",
            root.display()
        )
    })?;

    let graph = engine.build_graph().context("failed to build code graph")?;
    println!(
        "Graph: {} nodes, {} edges",
        graph.node_count(),
        graph.edge_count()
    );

    let scores = graph.pagerank(0.85, 20);
    let mut ranked: Vec<_> = scores.iter().collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(a.1).unwrap_or(std::cmp::Ordering::Equal));

    let top = ranked.iter().take(10);
    for (rank, (idx, score)) in top.enumerate() {
        if let Some(node) = graph.get_node(**idx) {
            println!(
                "{}. {} ({}) PageRank={:.6}",
                rank + 1,
                node.name,
                node.file,
                score,
            );
        }
    }

    Ok(())
}

fn cmd_callers(symbol: String) -> Result<()> {
    let root = std::env::current_dir().context("cannot determine current directory")?;
    let mut engine = Engine::open(&root).with_context(|| {
        format!(
            "no index found at {} — run `codeforge init` first",
            root.display()
        )
    })?;

    let graph = engine.build_graph().context("failed to build code graph")?;
    let symbol_lower = symbol.to_lowercase();

    let matches: Vec<_> = graph
        .node_indices()
        .filter(|idx| {
            graph
                .get_node(*idx)
                .map(|n| n.name.to_lowercase().contains(&symbol_lower))
                .unwrap_or(false)
        })
        .collect();

    if matches.is_empty() {
        eprintln!("No symbol matching \"{}\"", symbol);
        return Ok(());
    }

    for idx in matches {
        let node = graph.get_node(idx).unwrap();
        println!("{} ({})", node.name, node.file);
        let callers = graph.callers(idx);
        if callers.is_empty() {
            println!("  (no callers)");
        } else {
            for caller in &callers {
                println!("  <- {} ({})", caller.name, caller.file);
            }
        }
    }

    Ok(())
}

fn cmd_callees(symbol: String) -> Result<()> {
    let root = std::env::current_dir().context("cannot determine current directory")?;
    let mut engine = Engine::open(&root).with_context(|| {
        format!(
            "no index found at {} — run `codeforge init` first",
            root.display()
        )
    })?;

    let graph = engine.build_graph().context("failed to build code graph")?;
    let symbol_lower = symbol.to_lowercase();

    let matches: Vec<_> = graph
        .node_indices()
        .filter(|idx| {
            graph
                .get_node(*idx)
                .map(|n| n.name.to_lowercase().contains(&symbol_lower))
                .unwrap_or(false)
        })
        .collect();

    if matches.is_empty() {
        eprintln!("No symbol matching \"{}\"", symbol);
        return Ok(());
    }

    for idx in matches {
        let node = graph.get_node(idx).unwrap();
        println!("{} ({})", node.name, node.file);
        let callees = graph.callees(idx);
        if callees.is_empty() {
            println!("  (no callees)");
        } else {
            for callee in &callees {
                println!("  -> {} ({})", callee.name, callee.file);
            }
        }
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

    // Print tabular output.
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn cli_parses_graph_command() {
        let cli = Cli::try_parse_from(["codeforge", "graph"]).unwrap();
        assert!(matches!(cli.command, Command::Graph));
    }

    #[test]
    fn cli_parses_callers_command() {
        let cli = Cli::try_parse_from(["codeforge", "callers", "main"]).unwrap();
        match cli.command {
            Command::Callers { symbol } => assert_eq!(symbol, "main"),
            _ => panic!("expected Callers"),
        }
    }

    #[test]
    fn cli_parses_callees_command() {
        let cli = Cli::try_parse_from(["codeforge", "callees", "process"]).unwrap();
        match cli.command {
            Command::Callees { symbol } => assert_eq!(symbol, "process"),
            _ => panic!("expected Callees"),
        }
    }
}
