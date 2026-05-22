//! Standalone HTML graph export with interactive visualization.
//!
//! Generates a self-contained HTML file with an inline force-directed graph
//! visualization. Uses a minimal canvas-based renderer (no external CDN, no
//! framework). The embedded app provides search, layer filtering, a guided
//! tour, a path finder, diff-impact highlighting, and a node detail panel —
//! all computed client-side over the embedded graph JSON.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Serialize;

use super::CodeGraph;
use crate::error::Result;
use crate::graph::surprise;

/// Options for HTML graph export.
#[derive(Debug, Clone)]
pub struct HtmlExportOptions {
    /// Maximum number of nodes to include (for performance). Default: 2000.
    pub max_nodes: usize,
    /// Whether to include `__ext__` pseudo-nodes. Default: false.
    pub show_external: bool,
    /// Output file path.
    pub output_path: PathBuf,
    /// Project name shown in the dashboard header. Default: "Codixing".
    pub project_name: String,
    /// Files changed in a diff (for the diff-impact overlay). Empty = no overlay.
    pub changed_files: Vec<String>,
    /// Files in the blast radius of the changed files (impact analysis result).
    pub affected_files: Vec<String>,
}

impl Default for HtmlExportOptions {
    fn default() -> Self {
        Self {
            max_nodes: 2000,
            show_external: false,
            output_path: PathBuf::from("graph.html"),
            project_name: "Codixing".to_string(),
            changed_files: Vec::new(),
            affected_files: Vec::new(),
        }
    }
}

/// JSON-serializable node for the HTML visualization.
#[derive(Debug, Serialize)]
struct HtmlNode {
    id: String,
    /// File basename, for compact labels.
    label: String,
    /// Top-level directory segment(s), used for color-by-directory and layer naming.
    dir: String,
    /// Display language name (e.g. "Rust").
    language: String,
    pagerank: f32,
    community: Option<usize>,
    in_degree: usize,
    out_degree: usize,
}

/// JSON-serializable edge for the HTML visualization.
#[derive(Debug, Serialize)]
struct HtmlEdge {
    source: String,
    target: String,
    confidence: String,
    surprise: f32,
    reasons: Vec<String>,
}

/// A named architectural layer (derived from a Louvain community).
#[derive(Debug, Serialize)]
struct HtmlLayer {
    id: usize,
    name: String,
    node_ids: Vec<String>,
}

/// One step in the deterministic guided tour.
#[derive(Debug, Serialize)]
struct HtmlTourStep {
    order: usize,
    title: String,
    description: String,
    node_ids: Vec<String>,
}

/// Diff-impact overlay data.
#[derive(Debug, Serialize)]
struct HtmlDiff {
    changed: Vec<String>,
    affected: Vec<String>,
}

/// JSON-serializable stats for the HTML visualization.
#[derive(Debug, Serialize)]
struct HtmlStats {
    node_count: usize,
    edge_count: usize,
    community_count: usize,
    language_count: usize,
}

/// Full JSON data structure embedded in the HTML.
#[derive(Debug, Serialize)]
struct HtmlGraphData {
    project: String,
    nodes: Vec<HtmlNode>,
    edges: Vec<HtmlEdge>,
    layers: Vec<HtmlLayer>,
    tour: Vec<HtmlTourStep>,
    diff: Option<HtmlDiff>,
    stats: HtmlStats,
}

/// Basename of a forward-slash path.
fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// First `n` path segments joined with '/', for grouping/coloring by directory.
fn top_segments(path: &str, n: usize) -> String {
    let segs: Vec<&str> = path.split('/').collect();
    if segs.len() <= 1 {
        return ".".to_string();
    }
    // Drop the filename, keep up to `n` leading directory segments.
    let dirs = &segs[..segs.len() - 1];
    let take = dirs.len().min(n);
    if take == 0 {
        ".".to_string()
    } else {
        dirs[..take].join("/")
    }
}

/// Build named layers from community assignments. Each community becomes a
/// layer whose name is the most common 2-segment directory prefix among its
/// members — a deterministic stand-in for an LLM "architecture analyzer".
fn build_layers(nodes: &[HtmlNode]) -> Vec<HtmlLayer> {
    let mut by_comm: HashMap<usize, Vec<&HtmlNode>> = HashMap::new();
    for n in nodes {
        if let Some(c) = n.community {
            by_comm.entry(c).or_default().push(n);
        }
    }
    let mut layers: Vec<HtmlLayer> = by_comm
        .into_iter()
        .map(|(id, members)| {
            // Pick the most frequent directory prefix as the layer name.
            let mut dir_counts: HashMap<String, usize> = HashMap::new();
            for m in &members {
                *dir_counts.entry(top_segments(&m.id, 2)).or_default() += 1;
            }
            let name = dir_counts
                .into_iter()
                .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)))
                .map(|(d, _)| d)
                .unwrap_or_else(|| format!("layer {id}"));
            // Members sorted by PageRank desc for stable, meaningful ordering.
            let mut node_ids: Vec<(String, f32)> =
                members.iter().map(|m| (m.id.clone(), m.pagerank)).collect();
            node_ids.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            HtmlLayer {
                id,
                name,
                node_ids: node_ids.into_iter().map(|(id, _)| id).collect(),
            }
        })
        .collect();
    // Stable order by community id for a deterministic legend.
    layers.sort_by_key(|l| l.id);
    layers
}

/// Build a deterministic guided tour: a "hotspots" step (globally most
/// depended-upon files) followed by one step per major layer. No LLM — the
/// ordering is PageRank- and dependency-driven.
fn build_tour(nodes: &[HtmlNode], layers: &[HtmlLayer]) -> Vec<HtmlTourStep> {
    if nodes.is_empty() {
        return Vec::new();
    }
    let mut steps = Vec::new();
    let id_to_node: HashMap<&str, &HtmlNode> = nodes.iter().map(|n| (n.id.as_str(), n)).collect();

    // Step 1: global hotspots — top files by PageRank (most central / depended-upon).
    let mut by_pr: Vec<&HtmlNode> = nodes.iter().collect();
    by_pr.sort_by(|a, b| {
        b.pagerank
            .partial_cmp(&a.pagerank)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let hotspots: Vec<&HtmlNode> = by_pr.iter().take(6).copied().collect();
    if !hotspots.is_empty() {
        let names: Vec<String> = hotspots
            .iter()
            .map(|n| basename(&n.id).to_string())
            .collect();
        steps.push(HtmlTourStep {
            order: 0,
            title: "Start: the hotspots".to_string(),
            description: format!(
                "The most central files by PageRank — the ones most depended upon. Read these first: {}.",
                names.join(", ")
            ),
            node_ids: hotspots.iter().map(|n| n.id.clone()).collect(),
        });
    }

    // Following steps: walk the layers, biggest first by aggregate PageRank,
    // showing each layer's top files — most architecturally significant first.
    let mut ordered: Vec<&HtmlLayer> = layers.iter().collect();
    ordered.sort_by(|a, b| {
        let sa: f32 = a
            .node_ids
            .iter()
            .filter_map(|id| id_to_node.get(id.as_str()))
            .map(|n| n.pagerank)
            .sum();
        let sb: f32 = b
            .node_ids
            .iter()
            .filter_map(|id| id_to_node.get(id.as_str()))
            .map(|n| n.pagerank)
            .sum();
        sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
    });
    for (i, layer) in ordered.iter().take(8).enumerate() {
        let top: Vec<&str> = layer.node_ids.iter().take(5).map(|s| s.as_str()).collect();
        if top.is_empty() {
            continue;
        }
        let names: Vec<String> = top.iter().map(|id| basename(id).to_string()).collect();
        steps.push(HtmlTourStep {
            order: i + 1,
            title: format!("Layer: {}", layer.name),
            description: format!(
                "{} files in this area. Key ones: {}.",
                layer.node_ids.len(),
                names.join(", ")
            ),
            node_ids: top.iter().map(|s| s.to_string()).collect(),
        });
    }
    steps
}

/// Export the dependency graph as a self-contained interactive HTML file.
pub fn export_html(graph: &CodeGraph, options: &HtmlExportOptions) -> Result<()> {
    // Files the diff overlay must keep regardless of the PageRank cap, so a
    // changed/affected file that ranks below `max_nodes` doesn't get dropped —
    // which would make the overlay silently vanish on large repos.
    let force_keep: std::collections::HashSet<&str> = options
        .changed_files
        .iter()
        .chain(options.affected_files.iter())
        .map(|s| s.as_str())
        .collect();

    // Collect nodes by PageRank: take the top `max_nodes`, then additionally
    // pull in any diff file beyond the cap that exists in the graph.
    let ranked: Vec<_> = graph
        .nodes_by_pagerank()
        .into_iter()
        .filter(|n| options.show_external || !n.file_path.starts_with("__ext__:"))
        .collect();
    let mut chosen: Vec<&_> = ranked.iter().take(options.max_nodes).copied().collect();
    if !force_keep.is_empty() {
        let mut taken: std::collections::HashSet<&str> =
            chosen.iter().map(|n| n.file_path.as_str()).collect();
        for n in ranked.iter().skip(options.max_nodes) {
            if force_keep.contains(n.file_path.as_str()) && taken.insert(n.file_path.as_str()) {
                chosen.push(n);
            }
        }
    }
    let nodes: Vec<HtmlNode> = chosen
        .into_iter()
        .map(|n| HtmlNode {
            label: basename(&n.file_path).to_string(),
            dir: top_segments(&n.file_path, 2),
            language: format!("{:?}", n.language),
            id: n.file_path.clone(),
            pagerank: n.pagerank,
            community: n.community,
            in_degree: n.in_degree,
            out_degree: n.out_degree,
        })
        .collect();

    // Build a set of included node IDs for edge filtering.
    let node_set: std::collections::HashSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();

    // Get surprises for edge annotation.
    let surprises = surprise::detect_surprises(graph, 1000);
    let surprise_map: HashMap<(String, String), (f32, Vec<String>)> = surprises
        .into_iter()
        .map(|s| ((s.from.clone(), s.to.clone()), (s.score, s.reasons)))
        .collect();

    // Collect edges.
    let edges: Vec<HtmlEdge> = graph
        .all_edges()
        .into_iter()
        .filter(|(from, to, _)| node_set.contains(from) && node_set.contains(to))
        .map(|(from, to, edge)| {
            let (surprise_score, reasons) = surprise_map
                .get(&(from.to_string(), to.to_string()))
                .cloned()
                .unwrap_or((0.0, vec![]));
            HtmlEdge {
                source: from.to_string(),
                target: to.to_string(),
                confidence: format!("{:?}", edge.confidence),
                surprise: surprise_score,
                reasons,
            }
        })
        .collect();

    // Count communities and distinct languages.
    let community_count = {
        let mut seen = std::collections::HashSet::new();
        for n in &nodes {
            if let Some(c) = n.community {
                seen.insert(c);
            }
        }
        seen.len()
    };
    let language_count = {
        let mut seen = std::collections::HashSet::new();
        for n in &nodes {
            seen.insert(n.language.clone());
        }
        seen.len()
    };

    let layers = build_layers(&nodes);
    let tour = build_tour(&nodes, &layers);

    // Diff overlay: only include changed/affected files that are in the graph.
    let diff = if options.changed_files.is_empty() {
        None
    } else {
        let changed: Vec<String> = options
            .changed_files
            .iter()
            .filter(|f| node_set.contains(f.as_str()))
            .cloned()
            .collect();
        let affected: Vec<String> = options
            .affected_files
            .iter()
            .filter(|f| node_set.contains(f.as_str()))
            .cloned()
            .collect();
        if changed.is_empty() {
            None
        } else {
            Some(HtmlDiff { changed, affected })
        }
    };

    let data = HtmlGraphData {
        project: options.project_name.clone(),
        stats: HtmlStats {
            node_count: nodes.len(),
            edge_count: edges.len(),
            community_count,
            language_count,
        },
        nodes,
        edges,
        layers,
        tour,
        diff,
    };

    let json_data = serde_json::to_string(&data)
        .map_err(|e| crate::error::CodixingError::Serialization(format!("HTML export: {e}")))?;

    // Escape </script> sequences to prevent script-tag breakout (XSS).
    let json_data = json_data.replace("</script>", "<\\/script>");

    let html = TEMPLATE.replace("__GRAPH_DATA_JSON__", &json_data);
    std::fs::write(&options.output_path, html)?;

    Ok(())
}

/// The dashboard template. `__GRAPH_DATA_JSON__` is replaced with the embedded
/// graph JSON at export time. Written as a plain raw string (no `format!`) so
/// CSS/JS braces stay single and the markup is readable. All DOM is built with
/// `textContent` / `createElement` — no `innerHTML` with dynamic data.
const TEMPLATE: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>Codixing — Graph Dashboard</title>
<style>
:root {
  --bg: #0a0d13;
  --panel: #11151d;
  --panel-2: #161b25;
  --border: #232a36;
  --border-soft: #1b212c;
  --text: #d6dde8;
  --muted: #7d8694;
  --muted-2: #586070;
  --accent: #4f9cf9;
  --accent-soft: rgba(79,156,249,0.14);
  --warn: #f0883e;
  --danger: #f85149;
  --good: #3fb950;
  --shadow: 0 8px 30px rgba(0,0,0,0.45);
}
* { margin: 0; padding: 0; box-sizing: border-box; }
html, body { height: 100%; }
body {
  font-family: ui-sans-serif, -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif;
  background: var(--bg); color: var(--text); overflow: hidden;
  font-size: 13px; line-height: 1.45; -webkit-font-smoothing: antialiased;
}
button { font-family: inherit; cursor: pointer; }
::-webkit-scrollbar { width: 10px; height: 10px; }
::-webkit-scrollbar-thumb { background: #2a313d; border-radius: 6px; border: 2px solid var(--panel); }
::-webkit-scrollbar-track { background: transparent; }

/* Top bar */
#topbar {
  position: fixed; top: 0; left: 0; right: 0; height: 52px; z-index: 30;
  display: flex; align-items: center; gap: 18px; padding: 0 16px;
  background: linear-gradient(180deg, rgba(17,21,29,0.96), rgba(17,21,29,0.88));
  border-bottom: 1px solid var(--border); backdrop-filter: blur(8px);
}
#brand { display: flex; align-items: center; gap: 9px; font-weight: 600; font-size: 14px; letter-spacing: .2px; }
#brand .dot { width: 9px; height: 9px; border-radius: 50%; background: var(--accent); box-shadow: 0 0 12px var(--accent); }
#brand .proj { color: var(--muted); font-weight: 500; }
.chips { display: flex; gap: 7px; flex-wrap: wrap; }
.chip {
  display: inline-flex; align-items: baseline; gap: 5px; padding: 4px 9px;
  background: var(--panel-2); border: 1px solid var(--border-soft); border-radius: 999px;
  font-size: 11px; color: var(--muted); white-space: nowrap;
}
.chip b { color: var(--text); font-variant-numeric: tabular-nums; font-weight: 600; }
.spacer { flex: 1; }
.tbtn {
  display: inline-flex; align-items: center; gap: 6px; height: 30px; padding: 0 11px;
  background: var(--panel-2); border: 1px solid var(--border); border-radius: 8px;
  color: var(--text); font-size: 12px; transition: .12s;
}
.tbtn:hover { border-color: var(--accent); color: #fff; }
.tbtn.on { background: var(--accent-soft); border-color: var(--accent); color: #fff; }
#colorby { background: var(--panel-2); border: 1px solid var(--border); border-radius: 8px; color: var(--text); height: 30px; padding: 0 8px; font-size: 12px; }
.seg { display: inline-flex; background: var(--panel-2); border: 1px solid var(--border); border-radius: 8px; overflow: hidden; }
.seg button { height: 30px; padding: 0 12px; background: transparent; border: none; color: var(--muted); font-size: 12px; transition: .12s; }
.seg button:hover { color: var(--text); }
.seg button.on { background: var(--accent-soft); color: #fff; }
.exp-wrap { position: relative; }
.exp-menu { position: absolute; top: 36px; right: 0; background: var(--panel); border: 1px solid var(--border); border-radius: 8px; box-shadow: var(--shadow); overflow: hidden; z-index: 60; min-width: 130px; }
.exp-menu button { display: block; width: 100%; text-align: left; padding: 9px 13px; background: transparent; border: none; color: var(--text); font-size: 12px; }
.exp-menu button:hover { background: var(--panel-2); color: var(--accent); }
.ov-label { font-size: 10px; text-transform: uppercase; letter-spacing: .5px; color: var(--muted-2); margin-bottom: 6px; }
.ov-bar { display: flex; align-items: center; gap: 7px; margin: 3px 0; font-size: 11px; }
.ov-bar-l { width: 62px; color: var(--text); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.ov-bar-t { flex: 1; height: 7px; background: var(--panel-2); border-radius: 4px; overflow: hidden; }
.ov-bar-f { display: block; height: 100%; border-radius: 4px; }
.ov-bar-c { color: var(--muted-2); font-variant-numeric: tabular-nums; width: 24px; text-align: right; }
#filetree { max-height: 240px; overflow-y: auto; font-size: 12px; }
.ft-row { display: flex; align-items: center; gap: 5px; padding: 3px 4px; border-radius: 5px; cursor: pointer; white-space: nowrap; }
.ft-row:hover { background: var(--panel-2); }
.ft-row.ft-file { color: var(--text); }
.ft-row.ft-file:hover { color: var(--accent); }
.ft-dir { color: var(--text-secondary, #aab2bd); font-weight: 500; }
.ft-tw { color: var(--muted-2); width: 10px; display: inline-block; }
.ft-children { display: none; }
.ft-children.open { display: block; }

/* Sidebar */
#sidebar {
  position: fixed; top: 52px; left: 0; bottom: 0; width: 288px; z-index: 20;
  background: var(--panel); border-right: 1px solid var(--border);
  display: flex; flex-direction: column; overflow-y: auto; transition: transform .2s;
}
#sidebar.collapsed { transform: translateX(-288px); }
.section { border-bottom: 1px solid var(--border-soft); padding: 13px 14px; }
.section h2 {
  font-size: 10.5px; text-transform: uppercase; letter-spacing: 1px; color: var(--muted-2);
  margin-bottom: 10px; display: flex; align-items: center; justify-content: space-between;
}
.section h2 .count { color: var(--muted); font-weight: 400; }
#searchbox {
  width: 100%; padding: 9px 11px; background: var(--panel-2); border: 1px solid var(--border);
  border-radius: 8px; color: var(--text); font-size: 13px; outline: none; transition: .12s;
}
#searchbox:focus { border-color: var(--accent); box-shadow: 0 0 0 3px var(--accent-soft); }
.hint { color: var(--muted-2); font-size: 11px; margin-top: 7px; }

.layer-row, .surprise-row {
  display: flex; align-items: center; gap: 9px; padding: 6px 7px; border-radius: 7px;
  cursor: pointer; transition: .1s; font-size: 12px;
}
.layer-row:hover, .surprise-row:hover { background: var(--panel-2); }
.layer-row.off { opacity: .4; }
.swatch { width: 11px; height: 11px; border-radius: 3px; flex-shrink: 0; }
.layer-row .lname { flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.layer-row .lcount { color: var(--muted-2); font-size: 11px; font-variant-numeric: tabular-nums; }
.layer-row .eye { color: var(--muted-2); font-size: 12px; }

.toolrow { display: flex; gap: 7px; margin-top: 9px; }
.sbtn {
  flex: 1; height: 32px; background: var(--panel-2); border: 1px solid var(--border);
  border-radius: 8px; color: var(--text); font-size: 12px; transition: .12s;
}
.sbtn:hover { border-color: var(--accent); }
.sbtn:disabled { opacity: .4; cursor: default; }
.sbtn.primary { background: var(--accent-soft); border-color: var(--accent); color: #fff; }
#tour-desc { color: var(--muted); font-size: 12px; margin-top: 9px; min-height: 18px; }
#tour-title { font-weight: 600; color: var(--text); }
.pf-slot {
  display: flex; align-items: center; gap: 8px; padding: 7px 9px; margin-top: 7px;
  background: var(--panel-2); border: 1px solid var(--border-soft); border-radius: 7px; font-size: 12px;
}
.pf-slot .role { color: var(--muted-2); font-size: 10px; text-transform: uppercase; letter-spacing: .5px; width: 34px; }
.pf-slot .val { flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; color: var(--text); }
.pf-slot .val.empty { color: var(--muted-2); font-style: italic; }
.surprise-row .sscore { color: var(--danger); font-variant-numeric: tabular-nums; font-size: 11px; }
.surprise-row .spath { flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }

/* Detail panel */
#detail {
  position: fixed; top: 52px; right: 0; bottom: 0; width: 320px; z-index: 20;
  background: var(--panel); border-left: 1px solid var(--border);
  overflow-y: auto; transform: translateX(320px); transition: transform .2s;
}
#detail.open { transform: translateX(0); }
#detail .dhead { padding: 16px 16px 13px; border-bottom: 1px solid var(--border-soft); position: relative; }
#detail .dpath { font-size: 11px; color: var(--muted); word-break: break-all; margin-bottom: 6px; }
#detail .dname { font-size: 16px; font-weight: 600; word-break: break-all; }
#detail .dclose { position: absolute; top: 13px; right: 13px; width: 26px; height: 26px; border-radius: 6px; background: var(--panel-2); border: 1px solid var(--border); color: var(--muted); font-size: 15px; }
#detail .dclose:hover { color: #fff; border-color: var(--accent); }
.metrics { display: grid; grid-template-columns: 1fr 1fr; gap: 1px; background: var(--border-soft); }
.metric { background: var(--panel); padding: 12px 14px; }
.metric .mlabel { font-size: 10px; text-transform: uppercase; letter-spacing: .5px; color: var(--muted-2); }
.metric .mval { font-size: 17px; font-weight: 600; margin-top: 3px; font-variant-numeric: tabular-nums; word-break: break-all; }
.rel { padding: 13px 16px; border-top: 1px solid var(--border-soft); }
.rel h3 { font-size: 10.5px; text-transform: uppercase; letter-spacing: 1px; color: var(--muted-2); margin-bottom: 8px; }
.rel-item {
  display: block; width: 100%; text-align: left; padding: 6px 8px; border-radius: 6px;
  background: transparent; border: none; color: var(--text); font-size: 12px;
  overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
}
.rel-item:hover { background: var(--panel-2); color: var(--accent); }
.rel-empty { color: var(--muted-2); font-size: 12px; font-style: italic; }
.badge { display: inline-block; padding: 2px 7px; border-radius: 5px; font-size: 10px; font-weight: 600; margin-top: 8px; margin-right: 6px; }
.badge.changed { background: rgba(240,136,62,0.16); color: var(--warn); border: 1px solid rgba(240,136,62,0.4); }
.badge.affected { background: rgba(248,81,73,0.14); color: var(--danger); border: 1px solid rgba(248,81,73,0.35); }

/* Canvas */
#canvas { display: block; position: fixed; top: 52px; left: 0; width: 100vw; height: calc(100vh - 52px); }
#tooltip {
  position: fixed; display: none; background: rgba(10,13,19,0.97); border: 1px solid var(--border);
  border-radius: 8px; padding: 9px 12px; font-size: 12px; z-index: 40; pointer-events: none;
  max-width: 340px; box-shadow: var(--shadow);
}
#tooltip .tt-name { font-weight: 600; margin-bottom: 3px; }
#tooltip .tt-row { color: var(--muted); font-size: 11px; }
#toast {
  position: fixed; bottom: 18px; left: 50%; transform: translateX(-50%); z-index: 50;
  background: var(--panel-2); border: 1px solid var(--border); border-radius: 9px;
  padding: 10px 16px; font-size: 12.5px; display: none; box-shadow: var(--shadow); max-width: 70vw;
}
#sidebar-toggle {
  position: fixed; top: 60px; left: 296px; z-index: 25; width: 26px; height: 30px;
  background: var(--panel-2); border: 1px solid var(--border); border-radius: 0 8px 8px 0;
  border-left: none; color: var(--muted); transition: left .2s;
}
.empty-state { color: var(--muted-2); font-size: 12px; text-align: center; padding: 14px 0; }
#help-hint { position: fixed; bottom: 12px; right: 16px; z-index: 15; color: var(--muted-2); font-size: 11px; pointer-events: none; }
#breadcrumb { position: fixed; top: 64px; left: 304px; z-index: 16; display: flex; align-items: center; gap: 8px; background: var(--panel-2); border: 1px solid var(--border); border-radius: 999px; padding: 5px 13px; font-size: 12px; box-shadow: var(--shadow); }
#breadcrumb .bc-home { background: transparent; border: none; color: var(--accent); font-size: 12px; padding: 0; }
#breadcrumb .bc-home:hover { text-decoration: underline; }
#breadcrumb .bc-sep { color: var(--muted-2); }
#breadcrumb .bc-cur { color: var(--text); }
@media (max-width: 720px) {
  #sidebar { width: 80vw; } #sidebar-toggle { left: 80vw; }
  #detail { width: 86vw; transform: translateX(86vw); }
  .chips { display: none; }
  #topbar { gap: 10px; padding: 0 10px; }
  #brand .proj { display: none; }
  #reset-btn { display: none; }
}
</style>
</head>
<body>
<div id="topbar">
  <div id="brand"><span class="dot"></span> Codixing <span class="proj" id="proj-name"></span></div>
  <div class="chips" id="chips"></div>
  <div class="spacer"></div>
  <div id="viewmode" class="seg">
    <button data-mode="graph" class="on">Graph</button>
    <button data-mode="blocks">Blocks</button>
  </div>
  <label class="chip" style="gap:7px;cursor:pointer;">color by
    <select id="colorby">
      <option value="layer">layer</option>
      <option value="language">language</option>
      <option value="directory">directory</option>
    </select>
  </label>
  <button class="tbtn" id="diff-btn" style="display:none;">Diff impact</button>
  <div class="exp-wrap">
    <button class="tbtn" id="export-btn" title="Export the current view">Export ▾</button>
    <div id="export-menu" class="exp-menu" style="display:none;">
      <button data-fmt="png">PNG image</button>
      <button data-fmt="svg">SVG vector</button>
      <button data-fmt="json">JSON data</button>
    </div>
  </div>
  <button class="tbtn" id="reset-btn" title="Reset view">Reset</button>
</div>

<aside id="sidebar">
  <div class="section" id="overview-section">
    <h2>Overview</h2>
    <div id="overview-body"></div>
  </div>
  <div class="section">
    <h2>Files</h2>
    <div id="filetree"></div>
  </div>
  <div class="section">
    <h2>Search</h2>
    <input id="searchbox" type="text" placeholder="Filter files… (press /)" autocomplete="off" />
    <div class="hint" id="search-hint"></div>
  </div>
  <div class="section">
    <h2>Layers <span class="count" id="layer-count"></span></h2>
    <div id="layer-list"></div>
  </div>
  <div class="section">
    <h2>Guided tour <span class="count" id="tour-count"></span></h2>
    <div id="tour-title">Press Start to walk the architecture</div>
    <div id="tour-desc"></div>
    <div class="toolrow">
      <button class="sbtn" id="tour-prev" disabled>‹ Prev</button>
      <button class="sbtn primary" id="tour-play">Start</button>
      <button class="sbtn" id="tour-next" disabled>Next ›</button>
    </div>
  </div>
  <div class="section">
    <h2>Path finder</h2>
    <div class="pf-slot"><span class="role">From</span><span class="val empty" id="pf-from">click a node → Set</span></div>
    <div class="pf-slot"><span class="role">To</span><span class="val empty" id="pf-to">click a node → Set</span></div>
    <div class="toolrow">
      <button class="sbtn" id="pf-set-from" disabled>Set From</button>
      <button class="sbtn" id="pf-set-to" disabled>Set To</button>
    </div>
    <div class="toolrow">
      <button class="sbtn primary" id="pf-find" disabled>Find path</button>
      <button class="sbtn" id="pf-clear">Clear</button>
    </div>
  </div>
  <div class="section">
    <h2>Surprising edges <span class="count" id="surprise-count"></span></h2>
    <div id="surprise-list"></div>
  </div>
</aside>
<button id="sidebar-toggle" title="Toggle sidebar">‹</button>

<canvas id="canvas"></canvas>
<div id="breadcrumb" style="display:none;"></div>
<div id="tooltip"></div>
<div id="toast"></div>
<div id="help-hint">scroll = zoom · drag = pan · click = inspect · / = search · esc = clear</div>

<aside id="detail">
  <div class="dhead">
    <button class="dclose" id="detail-close">×</button>
    <div class="dpath" id="d-path"></div>
    <div class="dname" id="d-name"></div>
    <div id="d-badges"></div>
  </div>
  <div class="metrics" id="d-metrics"></div>
  <div class="rel"><h3>Depends on (callees) <span id="d-callee-n"></span></h3><div id="d-callees"></div></div>
  <div class="rel"><h3>Depended on by (callers) <span id="d-caller-n"></span></h3><div id="d-callers"></div></div>
</aside>

<script>
const DATA = __GRAPH_DATA_JSON__;

// Distinct, perceptually-spaced palette for communities/languages/dirs.
const COLORS = [
  '#4f9cf9','#f0883e','#3fb950','#db61a2','#a371f7',
  '#56d4dd','#f85149','#e3b341','#7ee787','#ff7b72',
  '#79c0ff','#d2a8ff','#ffa657','#39c5cf','#bc8cff'
];
function hashColor(s) {
  let h = 0; for (let i = 0; i < s.length; i++) h = (h * 31 + s.charCodeAt(i)) | 0;
  return COLORS[Math.abs(h) % COLORS.length];
}
function emptyState(text) {
  const d = document.createElement('div'); d.className = 'empty-state'; d.textContent = text; return d;
}

const canvas = document.getElementById('canvas');
const ctx = canvas.getContext('2d');
const tooltip = document.getElementById('tooltip');
const toast = document.getElementById('toast');

let W, H;
function resize() {
  const rect = canvas.getBoundingClientRect();
  W = rect.width; H = rect.height;
  canvas.width = W * devicePixelRatio; canvas.height = H * devicePixelRatio;
  ctx.setTransform(devicePixelRatio, 0, 0, devicePixelRatio, 0, 0);
}

// --- Data prep -------------------------------------------------------------
const nodes = DATA.nodes.map(function(n) {
  return Object.assign({}, n, { x: 0, y: 0, vx: 0, vy: 0, hidden: false });
});
const edges = DATA.edges;
const nodeMap = {};
nodes.forEach(function(n, i) { nodeMap[n.id] = i; });
// Seed positions on a phyllotaxis spiral so the initial layout is non-degenerate.
nodes.forEach(function(n, i) {
  const a = i * 2.399963; const r = 16 * Math.sqrt(i);
  n.x = Math.cos(a) * r; n.y = Math.sin(a) * r;
});

// Adjacency: callees[id] = files this file imports; callers[id] = reverse.
const callees = {}, callers = {};
edges.forEach(function(e) {
  (callees[e.source] = callees[e.source] || []).push(e.target);
  (callers[e.target] = callers[e.target] || []).push(e.source);
});

const layerOf = {};      // node id -> community id
const layerName = {};    // community id -> name
DATA.layers.forEach(function(l) {
  layerName[l.id] = l.name;
  l.node_ids.forEach(function(id) { layerOf[id] = l.id; });
});
const diffChanged = {}, diffAffected = {};
if (DATA.diff) {
  DATA.diff.changed.forEach(function(id){ diffChanged[id] = true; });
  DATA.diff.affected.forEach(function(id){ diffAffected[id] = true; });
}

// --- State -----------------------------------------------------------------
let scale = 0.9, tx = 0, ty = 0;
let dragging = null, dragMoved = false, panning = false;
let panStartX = 0, panStartY = 0, panTx = 0, panTy = 0, downX = 0, downY = 0;
let selected = null, hoverNode = null, searchTerm = '';
let colorMode = 'layer';
const hiddenLayers = {};
let diffMode = false;
let tour = { active: false, idx: -1 };
let pf = { from: null, to: null, path: null, pathEdges: {} };
let alpha = 1.0;
let viewMode = 'graph';   // 'graph' = force layout, 'blocks' = module boxes
let blocks = [];          // [{name, x, y, w, h, count, cx, cy}] in blocks mode
let blockEdges = [];      // aggregated directed module->module edges in blocks mode

// --- Helpers ---------------------------------------------------------------
function screenToWorld(sx, sy) { return [(sx - tx) / scale, (sy - ty) / scale]; }
function nodeRadius(n) {
  if (viewMode === 'blocks') return Math.min(7, 3 + Math.sqrt(Math.max(n.pagerank, 0)) * 6);
  return 4 + Math.sqrt(Math.max(n.pagerank, 0)) * 16;
}
function colorFor(n) {
  if (colorMode === 'language') return hashColor(n.language);
  if (colorMode === 'directory') return hashColor(n.dir);
  if (n.community != null) return COLORS[n.community % COLORS.length];
  return '#6e7681';
}
function showToast(msg) {
  toast.textContent = msg; toast.style.display = 'block';
  clearTimeout(showToast._t); showToast._t = setTimeout(function(){ toast.style.display = 'none'; }, 3400);
}

// --- Top bar / chips -------------------------------------------------------
document.getElementById('proj-name').textContent = DATA.project ? '· ' + DATA.project : '';
(function(){
  const chips = document.getElementById('chips');
  [['files', DATA.stats.node_count], ['edges', DATA.stats.edge_count],
   ['layers', DATA.stats.community_count], ['langs', DATA.stats.language_count]
  ].forEach(function(c){
    const el = document.createElement('span'); el.className = 'chip';
    const b = document.createElement('b'); b.textContent = c[1];
    el.appendChild(document.createTextNode(c[0] + ' ')); el.appendChild(b);
    chips.appendChild(el);
  });
})();

// --- Layers list -----------------------------------------------------------
(function(){
  const list = document.getElementById('layer-list');
  document.getElementById('layer-count').textContent = DATA.layers.length;
  if (!DATA.layers.length) { list.appendChild(emptyState('No communities detected')); return; }
  DATA.layers.slice().sort(function(a,b){ return b.node_ids.length - a.node_ids.length; }).forEach(function(l){
    const row = document.createElement('div'); row.className = 'layer-row'; row.dataset.layer = l.id;
    const sw = document.createElement('div'); sw.className = 'swatch'; sw.style.background = COLORS[l.id % COLORS.length];
    const name = document.createElement('div'); name.className = 'lname'; name.textContent = l.name; name.title = l.name;
    const count = document.createElement('div'); count.className = 'lcount'; count.textContent = l.node_ids.length;
    const eye = document.createElement('div'); eye.className = 'eye'; eye.textContent = '👁';
    row.appendChild(sw); row.appendChild(name); row.appendChild(count); row.appendChild(eye);
    row.addEventListener('click', function(ev){
      if (ev.shiftKey) { drillIntoLayer(l.id); return; }
      const off = !hiddenLayers[l.id]; hiddenLayers[l.id] = off; row.classList.toggle('off', off);
      nodes.forEach(function(n){ if (layerOf[n.id] === l.id) n.hidden = off; });
      reheat();
    });
    list.appendChild(row);
  });
  const hint = document.createElement('div'); hint.className = 'hint'; hint.textContent = 'click = toggle · shift-click = drill in';
  list.appendChild(hint);
})();

// --- Surprises list --------------------------------------------------------
(function(){
  const list = document.getElementById('surprise-list');
  const surp = edges.filter(function(e){ return e.surprise > 0.3; })
    .sort(function(a,b){ return b.surprise - a.surprise; }).slice(0, 12);
  document.getElementById('surprise-count').textContent = surp.length;
  if (!surp.length) { list.appendChild(emptyState('None — clean architecture')); return; }
  surp.forEach(function(e){
    const row = document.createElement('div'); row.className = 'surprise-row';
    const sc = document.createElement('span'); sc.className = 'sscore'; sc.textContent = e.surprise.toFixed(2);
    const p = document.createElement('span'); p.className = 'spath';
    p.textContent = e.source.split('/').pop() + ' → ' + e.target.split('/').pop();
    p.title = e.source + '  →  ' + e.target;
    row.appendChild(sc); row.appendChild(p);
    row.addEventListener('click', function(){ if (nodeMap[e.source] != null) selectNode(nodeMap[e.source]); });
    list.appendChild(row);
  });
})();

// --- Overview panel --------------------------------------------------------
(function(){
  const body = document.getElementById('overview-body');
  const langCount = {};
  nodes.forEach(function(n){ langCount[n.language] = (langCount[n.language]||0) + 1; });
  const langs = Object.keys(langCount).sort(function(a,b){ return langCount[b]-langCount[a]; });
  const maxLang = langCount[langs[0]] || 1;
  const lh = document.createElement('div'); lh.className='ov-label'; lh.textContent='Languages'; body.appendChild(lh);
  langs.slice(0,5).forEach(function(l){
    const row=document.createElement('div'); row.className='ov-bar';
    const lab=document.createElement('span'); lab.className='ov-bar-l'; lab.textContent=l; lab.title=l;
    const track=document.createElement('span'); track.className='ov-bar-t';
    const fill=document.createElement('span'); fill.className='ov-bar-f';
    fill.style.width=Math.round(langCount[l]/maxLang*100)+'%'; fill.style.background=hashColor(l);
    track.appendChild(fill);
    const c=document.createElement('span'); c.className='ov-bar-c'; c.textContent=langCount[l];
    row.appendChild(lab); row.appendChild(track); row.appendChild(c); body.appendChild(row);
  });
  const avg = nodes.length ? (edges.length*2/nodes.length).toFixed(1) : '0';
  const th=document.createElement('div'); th.className='ov-label'; th.style.marginTop='11px';
  th.textContent='Most connected · avg '+avg+'/file'; body.appendChild(th);
  nodes.slice().sort(function(a,b){ return (b.in_degree+b.out_degree)-(a.in_degree+a.out_degree); }).slice(0,5).forEach(function(n){
    const b2=document.createElement('button'); b2.className='rel-item';
    b2.textContent=n.label+'  ('+(n.in_degree+n.out_degree)+')'; b2.title=n.id;
    b2.addEventListener('click', function(){ if (nodeMap[n.id]!=null) selectNode(nodeMap[n.id]); });
    body.appendChild(b2);
  });
})();

// --- File explorer tree ----------------------------------------------------
(function(){
  const root = {};
  // Build a nested folder/file tree from node file paths (guard traversal).
  nodes.forEach(function(n){
    const path = String(n.id).replace(/\\/g,'/');
    if (path.indexOf('..') >= 0 || path.indexOf('\u0000') >= 0) return;
    const parts = path.replace(/^\/+/, '').split('/').filter(Boolean);
    let cur = root;
    for (let i=0;i<parts.length-1;i++){ const seg=parts[i]; cur.dirs=cur.dirs||{}; cur.dirs[seg]=cur.dirs[seg]||{}; cur=cur.dirs[seg]; }
    (cur.files = cur.files || []).push({ name: parts[parts.length-1], id: n.id });
  });
  const container = document.getElementById('filetree');
  function render(node, parentEl, depth) {
    const dirs = node.dirs ? Object.keys(node.dirs).sort() : [];
    dirs.forEach(function(d){
      const row=document.createElement('div'); row.className='ft-row ft-dir'; row.style.paddingLeft=(depth*10+4)+'px';
      const tw=document.createElement('span'); tw.className='ft-tw'; tw.textContent='▸';
      row.appendChild(tw); row.appendChild(document.createTextNode(d));
      const kids=document.createElement('div'); kids.className='ft-children';
      row.addEventListener('click', function(){ const open=kids.classList.toggle('open'); tw.textContent=open?'▾':'▸'; });
      parentEl.appendChild(row); parentEl.appendChild(kids);
      render(node.dirs[d], kids, depth+1);
    });
    (node.files||[]).sort(function(a,b){ return a.name<b.name?-1:1; }).forEach(function(f){
      const row=document.createElement('div'); row.className='ft-row ft-file'; row.style.paddingLeft=(depth*10+16)+'px';
      row.textContent=f.name; row.title=f.id;
      row.addEventListener('click', function(){ if (nodeMap[f.id]!=null) selectNode(nodeMap[f.id]); });
      parentEl.appendChild(row);
    });
  }
  render(root, container, 0);
})();

// --- Detail panel ----------------------------------------------------------
const detail = document.getElementById('detail');
function relList(container, ids, nlabel) {
  container.textContent = '';
  document.getElementById(nlabel).textContent = ids ? '(' + ids.length + ')' : '(0)';
  if (!ids || !ids.length) { const e = document.createElement('div'); e.className = 'rel-empty'; e.textContent = 'none'; container.appendChild(e); return; }
  ids.slice(0, 40).forEach(function(id){
    const b = document.createElement('button'); b.className = 'rel-item';
    b.textContent = id.split('/').pop(); b.title = id;
    b.addEventListener('click', function(){ if (nodeMap[id] != null) selectNode(nodeMap[id]); });
    container.appendChild(b);
  });
}
function renderDetail(n) {
  document.getElementById('d-path').textContent = n.id;
  document.getElementById('d-name').textContent = n.label;
  const badges = document.getElementById('d-badges'); badges.textContent = '';
  if (diffChanged[n.id]) { const b = document.createElement('span'); b.className = 'badge changed'; b.textContent = 'CHANGED'; badges.appendChild(b); }
  if (diffAffected[n.id]) { const b = document.createElement('span'); b.className = 'badge affected'; b.textContent = 'IN BLAST RADIUS'; badges.appendChild(b); }
  const m = document.getElementById('d-metrics'); m.textContent = '';
  [['PageRank', n.pagerank.toFixed(4)], ['Language', n.language],
   ['Callers in', String(n.in_degree)], ['Callees out', String(n.out_degree)],
   ['Layer', n.community != null ? (layerName[n.community] || ('#' + n.community)) : '—']
  ].forEach(function(p){
    const cell = document.createElement('div'); cell.className = 'metric';
    const l = document.createElement('div'); l.className = 'mlabel'; l.textContent = p[0];
    const v = document.createElement('div'); v.className = 'mval'; v.textContent = p[1];
    cell.appendChild(l); cell.appendChild(v); m.appendChild(cell);
  });
  relList(document.getElementById('d-callees'), callees[n.id], 'd-callee-n');
  relList(document.getElementById('d-callers'), callers[n.id], 'd-caller-n');
  detail.classList.add('open');
  document.getElementById('pf-set-from').disabled = false;
  document.getElementById('pf-set-to').disabled = false;
}
function selectNode(i) {
  if (i == null) { selected = null; detail.classList.remove('open'); return; }
  selected = i; renderDetail(nodes[i]); focusNodes([nodes[i].id], true);
}
document.getElementById('detail-close').addEventListener('click', function(){ selectNode(null); });

// --- View / camera ---------------------------------------------------------
function focusNodes(ids, keepScale) {
  const pts = ids.map(function(id){ return nodes[nodeMap[id]]; }).filter(Boolean);
  if (!pts.length) return;
  let minX=1e9,minY=1e9,maxX=-1e9,maxY=-1e9;
  pts.forEach(function(n){ minX=Math.min(minX,n.x); minY=Math.min(minY,n.y); maxX=Math.max(maxX,n.x); maxY=Math.max(maxY,n.y); });
  const cx=(minX+maxX)/2, cy=(minY+maxY)/2;
  if (!keepScale) {
    const span = Math.max(maxX-minX, maxY-minY, 120);
    scale = Math.min(2.2, Math.max(0.25, Math.min(W, H) / (span * 1.8)));
  }
  animateTo(W/2 - cx*scale, H/2 - cy*scale);
}
function animateTo(ntx, nty) {
  const stx=tx, sty=ty, t0=performance.now();
  (function step(t){
    const k = Math.min(1, (t-t0)/280); const e = 1-Math.pow(1-k,3);
    tx = stx+(ntx-stx)*e; ty = sty+(nty-sty)*e;
    if (k<1) requestAnimationFrame(step);
  })(t0);
}
function reheat() {
  if (viewMode === 'blocks') { layoutBlocks(); return; }
  alpha = Math.max(alpha, 0.7);
}
document.getElementById('reset-btn').addEventListener('click', function(){ scale=0.9; focusNodes(nodes.filter(function(n){return !n.hidden;}).map(function(n){return n.id;})); });

// --- Block view (module boxes) ---------------------------------------------
let savedPositions = null;
function layoutBlocks() {
  // Group visible nodes into module boxes by directory (top-2 path segments).
  const groups = {};
  nodes.forEach(function(n) { if (!n.hidden) (groups[n.dir] = groups[n.dir] || []).push(n); });
  const keys = Object.keys(groups).sort(function(a,b){ return groups[b].length - groups[a].length; });
  const PAD = 16, HEADER = 30, CELL = 20, GAP = 18;
  const sized = keys.map(function(k){
    const members = groups[k].slice().sort(function(a,b){ return b.pagerank - a.pagerank; });
    const cols = Math.max(1, Math.ceil(Math.sqrt(members.length)));
    const rows = Math.ceil(members.length / cols);
    // Box must be wide enough for its grid AND its header (name + count badge).
    const labelW = k.length * 7.6 + 56;
    const w = Math.max(PAD*2 + cols*CELL, labelW);
    return { name: k, members: members, cols: cols, w: w, h: HEADER + PAD + rows*CELL };
  });
  // Shelf-pack the boxes into rows under a target width.
  const area = sized.reduce(function(s,b){ return s + b.w*b.h; }, 0);
  const targetW = Math.max(900, Math.sqrt(area) * 1.7);
  let cx = 0, cy = 0, rowH = 0;
  blocks = [];
  sized.forEach(function(b){
    if (cx > 0 && cx + b.w > targetW) { cx = 0; cy += rowH + GAP; rowH = 0; }
    b.x = cx; b.y = cy; cx += b.w + GAP; rowH = Math.max(rowH, b.h);
    b.members.forEach(function(n, i){
      const col = i % b.cols, row = Math.floor(i / b.cols);
      n.x = b.x + PAD + col*CELL + CELL/2;
      n.y = b.y + HEADER + row*CELL + CELL/2;
      n.vx = 0; n.vy = 0;
    });
    blocks.push({ name: b.name, x: b.x, y: b.y, w: b.w, h: b.h, count: b.members.length });
  });
  // Re-center around the origin so the camera math stays stable.
  const ox = -targetW/2, oy = -(cy + rowH)/2;
  blocks.forEach(function(b){ b.x += ox; b.y += oy; b.cx = b.x + b.w/2; b.cy = b.y + b.h/2; });
  nodes.forEach(function(n){ if (!n.hidden) { n.x += ox; n.y += oy; } });

  // Aggregate file edges into one directed arrow per (sourceModule, targetModule).
  const byName = {};
  blocks.forEach(function(b){ byName[b.name] = b; });
  const agg = {};
  edges.forEach(function(e){
    const sn = nodes[nodeMap[e.source]], tn = nodes[nodeMap[e.target]];
    if (!sn || !tn || sn.hidden || tn.hidden) return;
    if (sn.dir === tn.dir) return;                 // skip intra-module edges
    if (!byName[sn.dir] || !byName[tn.dir]) return;
    const key = sn.dir.length + ':' + sn.dir + '\u0000' + tn.dir;
    (agg[key] = agg[key] || { s: sn.dir, t: tn.dir, count: 0 }).count++;
  });
  blockEdges = Object.keys(agg).map(function(k){
    const a = agg[k]; return { from: byName[a.s], to: byName[a.t], count: a.count };
  });
}
function setViewMode(mode) {
  if (mode === viewMode) return;
  if (mode === 'blocks') {
    savedPositions = nodes.map(function(n){ return [n.x, n.y]; });
    viewMode = 'blocks'; alpha = 0; layoutBlocks();
  } else {
    viewMode = 'graph'; blocks = [];
    if (savedPositions) nodes.forEach(function(n,i){ n.x = savedPositions[i][0]; n.y = savedPositions[i][1]; });
    alpha = Math.max(alpha, 0.6);
  }
  document.querySelectorAll('#viewmode button').forEach(function(b){ b.classList.toggle('on', b.dataset.mode === viewMode); });
  focusNodes(nodes.filter(function(n){ return !n.hidden; }).map(function(n){ return n.id; }));
}
document.querySelectorAll('#viewmode button').forEach(function(b){
  b.addEventListener('click', function(){ setViewMode(b.dataset.mode); });
});

// --- Drill-down: Overview <-> Layer detail ---------------------------------
let navLevel = 'overview', activeLayer = null, preDrillHidden = null;
function updateBreadcrumb() {
  const bc = document.getElementById('breadcrumb');
  if (navLevel !== 'layer-detail') { bc.style.display = 'none'; return; }
  bc.style.display = 'flex'; bc.textContent = '';
  const home = document.createElement('button'); home.className = 'bc-home'; home.textContent = 'Project'; home.addEventListener('click', exitDrill);
  const sep = document.createElement('span'); sep.className = 'bc-sep'; sep.textContent = '›';
  const cur = document.createElement('span'); cur.className = 'bc-cur'; cur.textContent = (layerName[activeLayer] || ('#' + activeLayer)) + '  ·  esc to exit';
  bc.appendChild(home); bc.appendChild(sep); bc.appendChild(cur);
}
function drillIntoLayer(layerId) {
  if (navLevel === 'layer-detail') { if (preDrillHidden) nodes.forEach(function(n,i){ n.hidden = preDrillHidden[i]; }); }
  preDrillHidden = nodes.map(function(n){ return n.hidden; });
  nodes.forEach(function(n){ n.hidden = (layerOf[n.id] !== layerId); });
  navLevel = 'layer-detail'; activeLayer = layerId; updateBreadcrumb();
  if (viewMode === 'blocks') layoutBlocks(); else alpha = Math.max(alpha, 0.8);
  focusNodes(nodes.filter(function(n){ return !n.hidden; }).map(function(n){ return n.id; }));
}
function exitDrill() {
  if (navLevel !== 'layer-detail') return;
  if (preDrillHidden) nodes.forEach(function(n,i){ n.hidden = preDrillHidden[i]; });
  navLevel = 'overview'; activeLayer = null; preDrillHidden = null; updateBreadcrumb();
  if (viewMode === 'blocks') layoutBlocks(); else alpha = Math.max(alpha, 0.6);
  focusNodes(nodes.filter(function(n){ return !n.hidden; }).map(function(n){ return n.id; }));
}

// --- Color-by --------------------------------------------------------------
document.getElementById('colorby').addEventListener('change', function(e){ colorMode = e.target.value; });

// --- Export (PNG / SVG / JSON) ---------------------------------------------
function downloadBlob(blob, name) {
  const url = URL.createObjectURL(blob);
  const a = document.createElement('a'); a.href = url; a.download = name;
  document.body.appendChild(a); a.click(); a.remove();
  setTimeout(function(){ URL.revokeObjectURL(url); }, 1000);
}
function escapeXml(s){ return String(s).replace(/[<>&'"]/g, function(c){ return {'<':'&lt;','>':'&gt;','&':'&amp;',"'":'&apos;','"':'&quot;'}[c]; }); }
function visibleNodes(){ return nodes.filter(function(n){ return !n.hidden; }); }
const projSlug = (DATA.project || 'graph').replace(/[^a-z0-9_-]+/gi, '-');
function exportPNG(){ canvas.toBlob(function(b){ if (b) downloadBlob(b, projSlug + '.png'); }, 'image/png'); }
function exportJSON(){
  const vis = {}; visibleNodes().forEach(function(n){ vis[n.id] = true; });
  const out = {
    project: DATA.project,
    nodes: visibleNodes().map(function(n){ return { id:n.id, label:n.label, dir:n.dir, language:n.language, pagerank:n.pagerank, community:n.community, in_degree:n.in_degree, out_degree:n.out_degree }; }),
    edges: edges.filter(function(e){ return vis[e.source] && vis[e.target]; }).map(function(e){ return { source:e.source, target:e.target, confidence:e.confidence }; })
  };
  downloadBlob(new Blob([JSON.stringify(out, null, 2)], { type:'application/json' }), projSlug + '.json');
}
function exportSVG(){
  const vis = visibleNodes(); if (!vis.length) return;
  let minX=1e9,minY=1e9,maxX=-1e9,maxY=-1e9;
  vis.forEach(function(n){ const r=nodeRadius(n)+24; minX=Math.min(minX,n.x-r); minY=Math.min(minY,n.y-r); maxX=Math.max(maxX,n.x+r); maxY=Math.max(maxY,n.y+r); });
  if (viewMode==='blocks') blocks.forEach(function(b){ minX=Math.min(minX,b.x); minY=Math.min(minY,b.y); maxX=Math.max(maxX,b.x+b.w); maxY=Math.max(maxY,b.y+b.h); });
  const w=Math.ceil(maxX-minX), h=Math.ceil(maxY-minY);
  const visSet={}; vis.forEach(function(n){ visSet[n.id]=true; });
  let s='<svg xmlns="http://www.w3.org/2000/svg" width="'+w+'" height="'+h+'" viewBox="0 0 '+w+' '+h+'"><rect width="'+w+'" height="'+h+'" fill="#0a0d13"/>';
  if (viewMode==='blocks') {
    blocks.forEach(function(b){ const c=hashColor(b.name);
      s+='<rect x="'+(b.x-minX).toFixed(1)+'" y="'+(b.y-minY).toFixed(1)+'" width="'+b.w.toFixed(1)+'" height="'+b.h.toFixed(1)+'" rx="10" fill="#ffffff" fill-opacity="0.02" stroke="'+c+'" stroke-opacity="0.5"/>';
      s+='<text x="'+(b.x-minX+13).toFixed(1)+'" y="'+(b.y-minY+19).toFixed(1)+'" fill="'+c+'" font-family="sans-serif" font-size="13">'+escapeXml(b.name)+'</text>';
    });
    blockEdges.forEach(function(be){ s+='<line x1="'+(be.from.cx-minX).toFixed(1)+'" y1="'+(be.from.cy-minY).toFixed(1)+'" x2="'+(be.to.cx-minX).toFixed(1)+'" y2="'+(be.to.cy-minY).toFixed(1)+'" stroke="#7d8694" stroke-opacity="0.45" stroke-width="'+Math.min(1+Math.log2(be.count+1),5).toFixed(1)+'"/>'; });
  } else {
    edges.forEach(function(e){ if(!visSet[e.source]||!visSet[e.target]) return; const a=nodes[nodeMap[e.source]], b=nodes[nodeMap[e.target]];
      s+='<line x1="'+(a.x-minX).toFixed(1)+'" y1="'+(a.y-minY).toFixed(1)+'" x2="'+(b.x-minX).toFixed(1)+'" y2="'+(b.y-minY).toFixed(1)+'" stroke="#7d8694" stroke-opacity="0.15" stroke-width="0.6"/>'; });
  }
  vis.forEach(function(n){ s+='<circle cx="'+(n.x-minX).toFixed(1)+'" cy="'+(n.y-minY).toFixed(1)+'" r="'+Math.max(2,nodeRadius(n)).toFixed(1)+'" fill="'+colorFor(n)+'"/>'; });
  s+='</svg>';
  downloadBlob(new Blob([s], { type:'image/svg+xml' }), projSlug + '.svg');
}
const exportMenu = document.getElementById('export-menu');
document.getElementById('export-btn').addEventListener('click', function(e){ e.stopPropagation(); exportMenu.style.display = exportMenu.style.display==='none' ? 'block' : 'none'; });
document.addEventListener('click', function(){ exportMenu.style.display='none'; });
exportMenu.querySelectorAll('button').forEach(function(b){
  b.addEventListener('click', function(e){ e.stopPropagation(); exportMenu.style.display='none';
    const f=b.dataset.fmt; if (f==='png') exportPNG(); else if (f==='svg') exportSVG(); else exportJSON();
  });
});

// --- Diff toggle -----------------------------------------------------------
if (DATA.diff) {
  const db = document.getElementById('diff-btn'); db.style.display = '';
  db.addEventListener('click', function(){
    diffMode = !diffMode; db.classList.toggle('on', diffMode);
    if (diffMode) { const ids = DATA.diff.changed.concat(DATA.diff.affected); focusNodes(ids); showToast(DATA.diff.changed.length + ' changed · ' + DATA.diff.affected.length + ' in blast radius'); }
  });
}

// --- Search ----------------------------------------------------------------
const searchbox = document.getElementById('searchbox');
searchbox.addEventListener('input', function(e){
  searchTerm = e.target.value.toLowerCase().trim();
  const hint = document.getElementById('search-hint');
  if (!searchTerm) { hint.textContent = ''; return; }
  const hits = nodes.filter(function(n){ return n.id.toLowerCase().indexOf(searchTerm) >= 0; });
  hint.textContent = hits.length + ' match' + (hits.length===1?'':'es');
  if (hits.length) focusNodes(hits.slice(0,30).map(function(n){return n.id;}));
});

// --- Tour ------------------------------------------------------------------
const tourSteps = DATA.tour || [];
document.getElementById('tour-count').textContent = tourSteps.length ? (tourSteps.length + ' steps') : '';
function renderTour() {
  const s = tourSteps[tour.idx];
  document.getElementById('tour-title').textContent = s ? s.title : 'Press Start to walk the architecture';
  document.getElementById('tour-desc').textContent = s ? s.description : '';
  document.getElementById('tour-prev').disabled = tour.idx <= 0;
  document.getElementById('tour-next').disabled = tour.idx >= tourSteps.length - 1;
  document.getElementById('tour-play').textContent = tour.active ? 'Stop' : 'Start';
  if (s) focusNodes(s.node_ids);
}
document.getElementById('tour-play').addEventListener('click', function(){
  if (!tourSteps.length) { showToast('No tour available'); return; }
  tour.active = !tour.active; tour.idx = tour.active ? 0 : -1; renderTour();
});
document.getElementById('tour-next').addEventListener('click', function(){ if (tour.idx < tourSteps.length-1){ tour.idx++; renderTour(); } });
document.getElementById('tour-prev').addEventListener('click', function(){ if (tour.idx > 0){ tour.idx--; renderTour(); } });

// --- Path finder -----------------------------------------------------------
function setPF(slot) {
  if (selected == null) return;
  pf[slot] = nodes[selected].id;
  const el = document.getElementById('pf-'+slot);
  el.textContent = nodes[selected].label; el.classList.remove('empty');
  document.getElementById('pf-find').disabled = !(pf.from && pf.to);
}
document.getElementById('pf-set-from').addEventListener('click', function(){ setPF('from'); });
document.getElementById('pf-set-to').addEventListener('click', function(){ setPF('to'); });
document.getElementById('pf-clear').addEventListener('click', function(){
  pf = { from: null, to: null, path: null, pathEdges: {} };
  ['from','to'].forEach(function(s){ const el=document.getElementById('pf-'+s); el.textContent='click a node → Set'; el.classList.add('empty'); });
  document.getElementById('pf-find').disabled = true;
});
function bfs(adj, from, to) {
  const q=[from], prev={}; prev[from]=null;
  while (q.length) {
    const cur=q.shift(); if (cur===to) break;
    (adj[cur]||[]).forEach(function(nx){ if (!(nx in prev)){ prev[nx]=cur; q.push(nx); } });
  }
  if (!(to in prev)) return null;
  const path=[]; let c=to; while (c!=null){ path.unshift(c); c=prev[c]; } return path;
}
document.getElementById('pf-find').addEventListener('click', function(){
  let path = bfs(callees, pf.from, pf.to); let dir = 'imports';
  if (!path) { path = bfs(callers, pf.from, pf.to); dir = 'reverse'; }
  if (!path) {
    const undirected = {};
    edges.forEach(function(e){ (undirected[e.source]=undirected[e.source]||[]).push(e.target); (undirected[e.target]=undirected[e.target]||[]).push(e.source); });
    path = bfs(undirected, pf.from, pf.to); dir = 'undirected';
  }
  if (!path) { showToast('No path between these files'); pf.path=null; pf.pathEdges={}; return; }
  pf.path = path; pf.pathEdges = {};
  for (let i=0;i<path.length-1;i++){ pf.pathEdges[path[i]+'|'+path[i+1]]=true; pf.pathEdges[path[i+1]+'|'+path[i]]=true; }
  focusNodes(path);
  showToast(path.length + ' hops (' + dir + '): ' + path.map(function(p){return p.split('/').pop();}).join(' → '));
});

// --- Sidebar toggle --------------------------------------------------------
document.getElementById('sidebar-toggle').addEventListener('click', function(){
  const sb = document.getElementById('sidebar'); sb.classList.toggle('collapsed');
  this.style.left = sb.classList.contains('collapsed') ? '0px' : '296px';
  this.textContent = sb.classList.contains('collapsed') ? '›' : '‹';
});

// --- Interaction -----------------------------------------------------------
canvas.addEventListener('wheel', function(e){
  e.preventDefault();
  const wc = screenToWorld(e.offsetX, e.offsetY);
  const factor = e.deltaY < 0 ? 1.12 : 0.89;
  scale = Math.min(6, Math.max(0.08, scale*factor));
  tx = e.offsetX - wc[0]*scale; ty = e.offsetY - wc[1]*scale;
}, { passive: false });

function pick(sx, sy) {
  const wc = screenToWorld(sx, sy);
  for (let i = nodes.length-1; i >= 0; i--) {
    const n = nodes[i]; if (n.hidden) continue;
    const r = nodeRadius(n) + 4;
    if ((n.x-wc[0])*(n.x-wc[0]) + (n.y-wc[1])*(n.y-wc[1]) < r*r) return i;
  }
  return null;
}
canvas.addEventListener('mousedown', function(e){
  downX = e.offsetX; downY = e.offsetY; dragMoved = false;
  const i = pick(e.offsetX, e.offsetY);
  if (i != null) { dragging = i; const wc = screenToWorld(e.offsetX, e.offsetY); nodes[i]._ox = nodes[i].x - wc[0]; nodes[i]._oy = nodes[i].y - wc[1]; }
  else { panning = true; panStartX = e.offsetX; panStartY = e.offsetY; panTx = tx; panTy = ty; }
});
canvas.addEventListener('mousemove', function(e){
  if (Math.abs(e.offsetX-downX)+Math.abs(e.offsetY-downY) > 4) dragMoved = true;
  if (dragging != null) { const wc = screenToWorld(e.offsetX, e.offsetY); const n = nodes[dragging]; n.x = wc[0]+n._ox; n.y = wc[1]+n._oy; n.vx=0; n.vy=0; reheat(); return; }
  if (panning) { tx = panTx + (e.offsetX-panStartX); ty = panTy + (e.offsetY-panStartY); return; }
  const i = pick(e.offsetX, e.offsetY); hoverNode = i;
  if (i != null) {
    const n = nodes[i];
    tooltip.style.display = 'block';
    tooltip.style.left = Math.min(e.clientX+14, window.innerWidth-360) + 'px';
    tooltip.style.top = (e.clientY+14) + 'px';
    tooltip.textContent = '';
    const nm = document.createElement('div'); nm.className='tt-name'; nm.textContent = n.label; tooltip.appendChild(nm);
    [n.id, 'PageRank ' + n.pagerank.toFixed(4) + ' · in ' + n.in_degree + ' · out ' + n.out_degree,
     (n.community!=null?('layer: '+(layerName[n.community]||('#'+n.community))):'') ].forEach(function(t){
      if (!t) return; const r = document.createElement('div'); r.className='tt-row'; r.textContent = t; tooltip.appendChild(r);
    });
    canvas.style.cursor = 'pointer';
  } else { tooltip.style.display='none'; canvas.style.cursor = 'default'; }
});
window.addEventListener('mouseup', function(){
  if (dragging != null && !dragMoved) selectNode(dragging);
  else if (panning && !dragMoved) selectNode(null);
  dragging = null; panning = false;
});

document.addEventListener('keydown', function(e){
  if (e.key === '/' && document.activeElement !== searchbox) { e.preventDefault(); searchbox.focus(); }
  else if (e.key === 'Escape') {
    if (navLevel === 'layer-detail') { exitDrill(); return; }
    searchbox.value=''; searchTerm=''; document.getElementById('search-hint').textContent=''; selectNode(null);
  }
  else if (e.key === 'ArrowRight' && tour.active) { document.getElementById('tour-next').click(); }
  else if (e.key === 'ArrowLeft' && tour.active) { document.getElementById('tour-prev').click(); }
});

// --- Force simulation (capped O(n²) with distance cutoff) ------------------
function simulate() {
  if (alpha < 0.01) return;
  const repulsion = 2600, attraction = 0.010, center = 0.009, damping = 0.86;
  const cutoff2 = 1100*1100;
  for (let i = 0; i < nodes.length; i++) {
    const n = nodes[i]; if (n.hidden) continue;
    n.vx += (0 - n.x) * center; n.vy += (0 - n.y) * center;
  }
  for (let i = 0; i < nodes.length; i++) {
    if (nodes[i].hidden) continue;
    for (let j = i+1; j < nodes.length; j++) {
      if (nodes[j].hidden) continue;
      let dx = nodes[j].x - nodes[i].x, dy = nodes[j].y - nodes[i].y;
      let d2 = dx*dx + dy*dy + 1; if (d2 > cutoff2) continue;
      const f = repulsion / d2;
      const fx = dx*f, fy = dy*f;
      nodes[i].vx -= fx; nodes[i].vy -= fy; nodes[j].vx += fx; nodes[j].vy += fy;
    }
  }
  for (let k = 0; k < edges.length; k++) {
    const si = nodeMap[edges[k].source], ti = nodeMap[edges[k].target];
    if (si === undefined || ti === undefined) continue;
    if (nodes[si].hidden || nodes[ti].hidden) continue;
    const dx = nodes[ti].x - nodes[si].x, dy = nodes[ti].y - nodes[si].y;
    const fx = dx*attraction, fy = dy*attraction;
    nodes[si].vx += fx; nodes[si].vy += fy; nodes[ti].vx -= fx; nodes[ti].vy -= fy;
  }
  for (let i = 0; i < nodes.length; i++) {
    const n = nodes[i]; if (n.hidden || i === dragging) continue;
    n.vx *= damping; n.vy *= damping; n.x += n.vx * alpha; n.y += n.vy * alpha;
  }
  alpha *= 0.992;
}

// --- Rendering -------------------------------------------------------------
function draw() {
  ctx.setTransform(devicePixelRatio, 0, 0, devicePixelRatio, 0, 0);
  ctx.clearRect(0, 0, W, H);
  ctx.save(); ctx.translate(tx, ty); ctx.scale(scale, scale);

  // Block view: draw module boxes behind everything else.
  if (viewMode === 'blocks') {
    ctx.textAlign = 'left';
    ctx.font = (13/scale) + "px ui-sans-serif, system-ui, sans-serif";
    blocks.forEach(function(b){
      const col = hashColor(b.name);
      ctx.fillStyle = 'rgba(255,255,255,0.018)';
      ctx.strokeStyle = col; ctx.globalAlpha = 0.45; ctx.lineWidth = 1.2/scale;
      const r = 10/scale;
      ctx.beginPath();
      if (ctx.roundRect) ctx.roundRect(b.x, b.y, b.w, b.h, r); else ctx.rect(b.x, b.y, b.w, b.h);
      ctx.fill(); ctx.stroke();
      ctx.globalAlpha = 1;
      ctx.fillStyle = col;
      ctx.fillText(b.name, b.x + 14/scale, b.y + 20/scale);
      ctx.fillStyle = 'rgba(125,134,148,0.9)';
      ctx.font = (11/scale) + "px ui-sans-serif, system-ui, sans-serif";
      const cnt = String(b.count);
      ctx.textAlign = 'right';
      ctx.fillText(cnt, b.x + b.w - 12/scale, b.y + 20/scale);
      ctx.textAlign = 'left';
      ctx.font = (13/scale) + "px ui-sans-serif, system-ui, sans-serif";
    });
    // Aggregated module->module dependency arrows (one per directed pair).
    ctx.textAlign = 'center';
    ctx.font = (10/scale) + "px ui-sans-serif, system-ui, sans-serif";
    blockEdges.forEach(function(be){
      const x1 = be.from.cx, y1 = be.from.cy, x2 = be.to.cx, y2 = be.to.cy;
      const ang = Math.atan2(y2 - y1, x2 - x1);
      // Back endpoints off to the box edges so arrows connect borders, not centers.
      const sx = x1 + Math.cos(ang) * (be.from.w/2 + 2/scale);
      const sy = y1 + Math.sin(ang) * (be.from.h/2 + 2/scale);
      const ex = x2 - Math.cos(ang) * (be.to.w/2 + 2/scale);
      const ey = y2 - Math.sin(ang) * (be.to.h/2 + 2/scale);
      ctx.strokeStyle = 'rgba(125,134,148,0.45)';
      ctx.fillStyle = 'rgba(125,134,148,0.45)';
      ctx.lineWidth = Math.min(1 + Math.log2(be.count + 1), 5) / scale;
      ctx.beginPath(); ctx.moveTo(sx, sy); ctx.lineTo(ex, ey); ctx.stroke();
      const ah = 9/scale;
      ctx.beginPath();
      ctx.moveTo(ex, ey);
      ctx.lineTo(ex - Math.cos(ang - 0.4)*ah, ey - Math.sin(ang - 0.4)*ah);
      ctx.lineTo(ex - Math.cos(ang + 0.4)*ah, ey - Math.sin(ang + 0.4)*ah);
      ctx.closePath(); ctx.fill();
      ctx.fillStyle = 'rgba(180,188,200,0.85)';
      ctx.fillText(String(be.count), (sx + ex)/2, (sy + ey)/2 - 2/scale);
    });
    ctx.textAlign = 'center';
  }

  const selId = selected != null ? nodes[selected].id : null;
  const neigh = {};
  if (selId) { neigh[selId] = true; (callees[selId]||[]).forEach(function(x){neigh[x]=true;}); (callers[selId]||[]).forEach(function(x){neigh[x]=true;}); }
  const hasPath = pf.path && pf.path.length > 1;

  // Edges
  for (let k = 0; k < edges.length; k++) {
    const e = edges[k];
    const si = nodeMap[e.source], ti = nodeMap[e.target];
    if (si === undefined || ti === undefined) continue;
    if (nodes[si].hidden || nodes[ti].hidden) continue;

    const onPath = hasPath && pf.pathEdges[e.source+'|'+e.target];
    const onSel = selId && (e.source===selId || e.target===selId);
    // In blocks mode the aggregated arrows carry the dependency story; only
    // draw raw file edges for the selected node or an active path.
    if (viewMode === 'blocks' && !onSel && !onPath) continue;
    const surprising = e.surprise > 0.55;

    let a = 0.06, lw = 0.5, col = '120,128,140';
    if (e.confidence === 'Verified') a = 0.12;
    else if (e.confidence === 'High') a = 0.085;
    else if (e.confidence === 'Low') a = 0.035;
    if (surprising) { col = '248,81,73'; a = 0.3; lw = 0.9; }
    if (onSel) { a = 0.9; lw = 1.5; col = '79,156,249'; }
    if (diffMode && (diffChanged[e.source]||diffChanged[e.target])) { col='240,136,62'; a=0.5; }
    if (onPath) { col = '227,179,65'; a = 0.95; lw = 2.4; }
    if (selId && !onSel && !onPath) a *= 0.12;
    if (searchTerm && !onPath) a *= 0.5;

    ctx.beginPath();
    ctx.strokeStyle = 'rgba('+col+','+a+')';
    ctx.lineWidth = lw / scale;
    if (e.confidence === 'High') ctx.setLineDash([6/scale, 3/scale]);
    else if (e.confidence === 'Medium') ctx.setLineDash([2/scale, 3/scale]);
    else ctx.setLineDash([]);
    ctx.moveTo(nodes[si].x, nodes[si].y); ctx.lineTo(nodes[ti].x, nodes[ti].y); ctx.stroke();
    ctx.setLineDash([]);
  }

  // Nodes
  for (let i = 0; i < nodes.length; i++) {
    const n = nodes[i]; if (n.hidden) continue;
    const r = nodeRadius(n);
    let a = 1.0, ring = null;

    const searchHit = searchTerm && n.id.toLowerCase().indexOf(searchTerm) >= 0;
    if (searchTerm && !searchHit) a = 0.15; else if (searchHit) ring = '#ffffff';
    if (selId && !neigh[n.id]) a *= 0.16;
    if (hasPath) { if (pf.path.indexOf(n.id) >= 0) ring = '#e3b341'; else a *= 0.18; }
    if (diffMode) { if (diffChanged[n.id]) ring = '#f0883e'; else if (diffAffected[n.id]) ring = '#f85149'; else a *= 0.2; }
    if (i === selected) ring = '#4f9cf9';

    ctx.globalAlpha = a;
    ctx.beginPath(); ctx.arc(n.x, n.y, r, 0, Math.PI*2);
    ctx.fillStyle = colorFor(n); ctx.fill();
    if (i === hoverNode) { ctx.shadowColor = colorFor(n); ctx.shadowBlur = 18; ctx.fill(); ctx.shadowBlur = 0; }
    if (ring) { ctx.strokeStyle = ring; ctx.lineWidth = 2.5/scale; ctx.stroke(); }
    ctx.globalAlpha = 1.0;
  }

  // Labels for the most important nodes (and selection/hover), zoom-gated.
  ctx.fillStyle = 'rgba(214,221,232,0.92)';
  ctx.font = (11/scale) + "px ui-sans-serif, system-ui, sans-serif";
  ctx.textAlign = 'center';
  // In blocks mode the box headers are the labels; only label selection/hover
  // (or when zoomed in far enough to read individual cells).
  const labelThreshold = viewMode === 'blocks'
    ? (scale > 2.2 ? 0 : 999)
    : (scale > 1.4 ? 0 : (scale > 0.7 ? 0.012 : 0.03));
  for (let i = 0; i < nodes.length; i++) {
    const n = nodes[i]; if (n.hidden) continue;
    const show = n.pagerank >= labelThreshold || i === selected || i === hoverNode;
    if (!show) continue;
    if (selId && !neigh[n.id] && i !== hoverNode) continue;
    ctx.fillText(n.label, n.x, n.y - nodeRadius(n) - 4/scale);
  }
  ctx.restore();
}

function loop() { simulate(); draw(); requestAnimationFrame(loop); }
resize(); window.addEventListener('resize', resize);
// On narrow screens, start with the sidebar collapsed so the graph is visible.
if (window.innerWidth <= 720) document.getElementById('sidebar-toggle').click();
// Let the layout settle before the first painted frame.
for (let i = 0; i < 120; i++) simulate();
focusNodes(nodes.map(function(n){ return n.id; }));
loop();
</script>
</body>
</html>"##;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::Language;

    #[test]
    fn export_creates_file() {
        let mut g = CodeGraph::new();
        g.add_edge(
            "src/a.rs",
            "src/b.rs",
            "crate::b",
            Language::Rust,
            Language::Rust,
        );

        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("test_graph.html");
        let opts = HtmlExportOptions {
            output_path: out.clone(),
            ..Default::default()
        };
        export_html(&g, &opts).unwrap();
        assert!(out.exists());

        let content = std::fs::read_to_string(&out).unwrap();
        assert!(content.contains("<!DOCTYPE html>"));
        assert!(content.contains("src/a.rs"));
        assert!(content.contains("src/b.rs"));
        // Placeholder must be fully substituted.
        assert!(!content.contains("__GRAPH_DATA_JSON__"));
        // No raw NUL bytes: they truncate the trigram index mid-file and break
        // text tooling. JS null-byte literals must use the `\u{0000}` escape.
        assert!(
            !content.contains('\u{0}'),
            "generated HTML contains a raw NUL byte"
        );
    }

    #[test]
    fn export_empty_graph() {
        let g = CodeGraph::new();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("empty.html");
        let opts = HtmlExportOptions {
            output_path: out.clone(),
            ..Default::default()
        };
        export_html(&g, &opts).unwrap();
        assert!(out.exists());
    }

    #[test]
    fn max_nodes_limits_output() {
        let mut g = CodeGraph::new();
        for i in 0..100 {
            g.get_or_insert_node(&format!("src/file_{i}.rs"), Language::Rust);
        }

        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("limited.html");
        let opts = HtmlExportOptions {
            max_nodes: 10,
            output_path: out.clone(),
            ..Default::default()
        };
        export_html(&g, &opts).unwrap();

        let content = std::fs::read_to_string(&out).unwrap();
        // Should contain at most 10 node entries.
        let node_count = content.matches("\"id\":").count();
        assert!(
            node_count <= 10,
            "expected at most 10 nodes, got {node_count}"
        );
    }

    #[test]
    fn embeds_layers_languages_and_tour() {
        let mut g = CodeGraph::new();
        g.add_edge(
            "crates/core/a.rs",
            "crates/core/b.rs",
            "b",
            Language::Rust,
            Language::Rust,
        );
        g.add_edge(
            "crates/cli/main.rs",
            "crates/core/a.rs",
            "a",
            Language::Rust,
            Language::Rust,
        );
        g.detect_communities();

        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("rich.html");
        let opts = HtmlExportOptions {
            output_path: out.clone(),
            project_name: "TestProj".to_string(),
            ..Default::default()
        };
        export_html(&g, &opts).unwrap();

        let content = std::fs::read_to_string(&out).unwrap();
        // Enriched schema fields are present.
        assert!(content.contains("\"layers\":"));
        assert!(content.contains("\"tour\":"));
        assert!(content.contains("\"language\":"));
        assert!(content.contains("\"label\":"));
        assert!(content.contains("TestProj"));
    }

    #[test]
    fn diff_overlay_included_when_requested() {
        let mut g = CodeGraph::new();
        g.add_edge("src/a.rs", "src/b.rs", "b", Language::Rust, Language::Rust);

        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("diff.html");
        let opts = HtmlExportOptions {
            output_path: out.clone(),
            changed_files: vec!["src/a.rs".to_string()],
            affected_files: vec!["src/b.rs".to_string()],
            ..Default::default()
        };
        export_html(&g, &opts).unwrap();

        let content = std::fs::read_to_string(&out).unwrap();
        assert!(content.contains("\"diff\":"));
        assert!(content.contains("\"changed\":"));
    }

    #[test]
    fn embeds_block_view() {
        let mut g = CodeGraph::new();
        g.add_edge(
            "crates/core/a.rs",
            "crates/core/b.rs",
            "b",
            Language::Rust,
            Language::Rust,
        );
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("bv.html");
        let opts = HtmlExportOptions {
            output_path: out.clone(),
            ..Default::default()
        };
        export_html(&g, &opts).unwrap();
        let content = std::fs::read_to_string(&out).unwrap();
        // The Graph/Blocks view toggle and its layout function must ship.
        assert!(content.contains("data-mode=\"blocks\""));
        assert!(content.contains("function layoutBlocks"));
    }

    #[test]
    fn embeds_dashboard_panels() {
        let mut g = CodeGraph::new();
        g.add_edge(
            "crates/core/a.rs",
            "crates/cli/b.rs",
            "b",
            Language::Rust,
            Language::Rust,
        );
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("panels.html");
        let opts = HtmlExportOptions {
            output_path: out.clone(),
            ..Default::default()
        };
        export_html(&g, &opts).unwrap();
        let content = std::fs::read_to_string(&out).unwrap();
        // Overview, file tree, export menu, breadcrumb, and edge aggregation all ship.
        assert!(content.contains("id=\"overview-body\""));
        assert!(content.contains("id=\"filetree\""));
        assert!(content.contains("id=\"export-menu\""));
        assert!(content.contains("id=\"breadcrumb\""));
        assert!(content.contains("function drillIntoLayer"));
        assert!(content.contains("blockEdges"));
        assert!(content.contains("function exportSVG"));
    }

    #[test]
    fn diff_files_kept_beyond_max_nodes_cap() {
        // Regression for the codex-flagged P2: a changed file ranking below the
        // `max_nodes` cap must still be force-included so the overlay can't
        // silently vanish on large repos.
        let mut g = CodeGraph::new();
        g.add_edge("b.rs", "a.rs", "a", Language::Rust, Language::Rust);
        g.add_edge("c.rs", "a.rs", "a", Language::Rust, Language::Rust);
        g.add_edge("z_changed.rs", "a.rs", "a", Language::Rust, Language::Rust);

        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("cap.html");
        let opts = HtmlExportOptions {
            output_path: out.clone(),
            max_nodes: 1, // only one node fits the PageRank cap
            changed_files: vec!["z_changed.rs".to_string()],
            ..Default::default()
        };
        export_html(&g, &opts).unwrap();

        let content = std::fs::read_to_string(&out).unwrap();
        assert!(
            content.contains("z_changed.rs"),
            "changed file was dropped past the max_nodes cap"
        );
        assert!(content.contains("\"diff\":"));
        assert!(content.contains("\"changed\":[\"z_changed.rs\"]"));
    }

    #[test]
    fn no_diff_overlay_by_default() {
        let mut g = CodeGraph::new();
        g.add_edge("src/a.rs", "src/b.rs", "b", Language::Rust, Language::Rust);
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("nodiff.html");
        let opts = HtmlExportOptions {
            output_path: out.clone(),
            ..Default::default()
        };
        export_html(&g, &opts).unwrap();
        let content = std::fs::read_to_string(&out).unwrap();
        assert!(content.contains("\"diff\":null"));
    }
}
