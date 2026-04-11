//! Standalone HTML graph export with interactive visualization.
//!
//! Generates a self-contained HTML file with an inline force-directed graph
//! visualization. Uses a minimal canvas-based renderer (no external CDN).

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
    /// Whether to include __ext__ pseudo-nodes. Default: false.
    pub show_external: bool,
    /// Output file path.
    pub output_path: PathBuf,
}

impl Default for HtmlExportOptions {
    fn default() -> Self {
        Self {
            max_nodes: 2000,
            show_external: false,
            output_path: PathBuf::from("graph.html"),
        }
    }
}

/// JSON-serializable node for the HTML visualization.
#[derive(Debug, Serialize)]
struct HtmlNode {
    id: String,
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

/// JSON-serializable stats for the HTML visualization.
#[derive(Debug, Serialize)]
struct HtmlStats {
    node_count: usize,
    edge_count: usize,
    community_count: usize,
}

/// Full JSON data structure embedded in the HTML.
#[derive(Debug, Serialize)]
struct HtmlGraphData {
    nodes: Vec<HtmlNode>,
    edges: Vec<HtmlEdge>,
    stats: HtmlStats,
}

/// Export the dependency graph as a self-contained interactive HTML file.
pub fn export_html(graph: &CodeGraph, options: &HtmlExportOptions) -> Result<()> {
    // Collect nodes, filtering externals unless requested.
    let nodes: Vec<HtmlNode> = graph
        .nodes_by_pagerank()
        .into_iter()
        .filter(|n| options.show_external || !n.file_path.starts_with("__ext__:"))
        .take(options.max_nodes)
        .map(|n| HtmlNode {
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

    // Count communities.
    let community_count = {
        let mut seen = std::collections::HashSet::new();
        for n in &nodes {
            if let Some(c) = n.community {
                seen.insert(c);
            }
        }
        seen.len()
    };

    let data = HtmlGraphData {
        stats: HtmlStats {
            node_count: nodes.len(),
            edge_count: edges.len(),
            community_count,
        },
        nodes,
        edges,
    };

    let json_data = serde_json::to_string(&data)
        .map_err(|e| crate::error::CodixingError::Serialization(format!("HTML export: {e}")))?;

    let html = generate_html(&json_data);
    std::fs::write(&options.output_path, html)?;

    Ok(())
}

fn generate_html(json_data: &str) -> String {
    format!(
        r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>Codixing Dependency Graph</title>
<style>
* {{ margin: 0; padding: 0; box-sizing: border-box; }}
body {{ font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif; background: #1a1a2e; color: #e0e0e0; overflow: hidden; }}
#canvas {{ display: block; }}
#search {{ position: fixed; top: 12px; left: 12px; z-index: 10; padding: 8px 14px; border: 1px solid #444; border-radius: 6px; background: #16213e; color: #e0e0e0; font-size: 14px; width: 280px; outline: none; }}
#search:focus {{ border-color: #0f3460; }}
#legend {{ position: fixed; bottom: 12px; left: 12px; z-index: 10; background: rgba(22,33,62,0.95); border: 1px solid #333; border-radius: 8px; padding: 14px; font-size: 12px; max-width: 260px; }}
#legend h3 {{ margin-bottom: 8px; font-size: 14px; color: #e94560; }}
.legend-item {{ display: flex; align-items: center; margin: 4px 0; }}
.legend-swatch {{ width: 14px; height: 14px; border-radius: 3px; margin-right: 8px; flex-shrink: 0; }}
.legend-line {{ width: 28px; height: 0; margin-right: 8px; flex-shrink: 0; }}
#stats {{ position: fixed; top: 12px; right: 12px; z-index: 10; background: rgba(22,33,62,0.95); border: 1px solid #333; border-radius: 8px; padding: 14px; font-size: 12px; max-width: 300px; }}
#stats h3 {{ margin-bottom: 8px; font-size: 14px; color: #e94560; }}
#stats .stat-row {{ margin: 3px 0; }}
#tooltip {{ position: fixed; display: none; background: rgba(22,33,62,0.97); border: 1px solid #555; border-radius: 6px; padding: 10px 14px; font-size: 12px; z-index: 20; pointer-events: none; max-width: 350px; }}
#tooltip .tt-path {{ font-weight: bold; color: #e94560; margin-bottom: 4px; }}
#tooltip .tt-row {{ margin: 2px 0; color: #aaa; }}
</style>
</head>
<body>
<input id="search" type="text" placeholder="Search files..." />
<canvas id="canvas"></canvas>
<div id="tooltip"></div>
<div id="legend">
  <h3>Legend</h3>
  <div id="community-legend"></div>
  <div style="margin-top:8px; border-top:1px solid #333; padding-top:8px;">
    <div class="legend-item"><div class="legend-line" style="border-top:2px solid rgba(200,200,200,0.8);"></div> Verified</div>
    <div class="legend-item"><div class="legend-line" style="border-top:2px dashed rgba(200,200,200,0.6);"></div> High</div>
    <div class="legend-item"><div class="legend-line" style="border-top:2px dotted rgba(200,200,200,0.4);"></div> Medium</div>
    <div class="legend-item"><div class="legend-line" style="border-top:1px solid rgba(200,200,200,0.2);"></div> Low</div>
    <div class="legend-item"><div class="legend-line" style="border-top:3px solid #e94560;"></div> Surprising</div>
  </div>
</div>
<div id="stats"></div>
<script>
const DATA = {json_data};

const COLORS = [
  '#e94560','#0f3460','#16c79a','#f5a623','#8b5cf6',
  '#06b6d4','#ec4899','#84cc16','#f97316','#6366f1',
  '#14b8a6','#f43f5e','#a855f7','#22c55e','#eab308'
];

const canvas = document.getElementById('canvas');
const ctx = canvas.getContext('2d');
const searchInput = document.getElementById('search');
const tooltip = document.getElementById('tooltip');

let W, H;
function resize() {{
  W = window.innerWidth; H = window.innerHeight;
  canvas.width = W * devicePixelRatio;
  canvas.height = H * devicePixelRatio;
  canvas.style.width = W + 'px';
  canvas.style.height = H + 'px';
  ctx.setTransform(devicePixelRatio, 0, 0, devicePixelRatio, 0, 0);
}}
resize();
window.addEventListener('resize', resize);

// Initialize node positions randomly.
const nodes = DATA.nodes.map(function(n, i) {{ return Object.assign({{}}, n, {{
  x: W/2 + (Math.random()-0.5)*W*0.6,
  y: H/2 + (Math.random()-0.5)*H*0.6, vx: 0, vy: 0
}}); }});
const edges = DATA.edges;
const nodeMap = {{}};
nodes.forEach(function(n, i) {{ nodeMap[n.id] = i; }});

// Stats panel.
const statsDiv = document.getElementById('stats');
var statsHtml = '<h3>Graph Stats</h3>';
statsHtml += '<div class="stat-row">Nodes: ' + DATA.stats.node_count + '</div>';
statsHtml += '<div class="stat-row">Edges: ' + DATA.stats.edge_count + '</div>';
statsHtml += '<div class="stat-row">Communities: ' + DATA.stats.community_count + '</div>';
statsDiv.textContent = '';
var tmpDiv = document.createElement('div');
tmpDiv.textContent = '';
statsDiv.appendChild(tmpDiv);
// Use safe DOM construction for stats.
(function() {{
  var h = document.createElement('h3');
  h.textContent = 'Graph Stats';
  statsDiv.textContent = '';
  statsDiv.appendChild(h);
  var items = [
    ['Nodes', DATA.stats.node_count],
    ['Edges', DATA.stats.edge_count],
    ['Communities', DATA.stats.community_count]
  ];
  items.forEach(function(item) {{
    var d = document.createElement('div');
    d.className = 'stat-row';
    d.textContent = item[0] + ': ' + item[1];
    statsDiv.appendChild(d);
  }});
}})();

// Community legend (safe DOM construction).
var commLegend = document.getElementById('community-legend');
var comms = [];
var commSeen = {{}};
nodes.forEach(function(n) {{
  if (n.community != null && !commSeen[n.community]) {{
    commSeen[n.community] = true;
    comms.push(n.community);
  }}
}});
comms.sort(function(a,b) {{ return a - b; }});
comms.forEach(function(c) {{
  var div = document.createElement('div');
  div.className = 'legend-item';
  var swatch = document.createElement('div');
  swatch.className = 'legend-swatch';
  swatch.style.background = COLORS[c % COLORS.length];
  div.appendChild(swatch);
  var label = document.createTextNode(' Community ' + c);
  div.appendChild(label);
  commLegend.appendChild(div);
}});

// Zoom/pan state.
var scale = 1, tx = 0, ty = 0;
var dragging = null, dragOffX = 0, dragOffY = 0;
var panning = false, panStartX = 0, panStartY = 0, panTx = 0, panTy = 0;
var selectedNode = null, searchTerm = '';

function screenToWorld(sx, sy) {{
  return [(sx - tx) / scale, (sy - ty) / scale];
}}

canvas.addEventListener('wheel', function(e) {{
  e.preventDefault();
  var wc = screenToWorld(e.offsetX, e.offsetY);
  var factor = e.deltaY < 0 ? 1.1 : 0.9;
  scale *= factor;
  tx = e.offsetX - wc[0] * scale;
  ty = e.offsetY - wc[1] * scale;
}});

canvas.addEventListener('mousedown', function(e) {{
  var wc = screenToWorld(e.offsetX, e.offsetY);
  for (var i = 0; i < nodes.length; i++) {{
    var n = nodes[i];
    var r = nodeRadius(n);
    if ((n.x-wc[0])*(n.x-wc[0]) + (n.y-wc[1])*(n.y-wc[1]) < (r+4)*(r+4)) {{
      if (e.button === 0) {{
        dragging = i;
        dragOffX = n.x - wc[0];
        dragOffY = n.y - wc[1];
        selectedNode = i;
        return;
      }}
    }}
  }}
  selectedNode = null;
  if (e.button === 0) {{
    panning = true;
    panStartX = e.offsetX; panStartY = e.offsetY;
    panTx = tx; panTy = ty;
  }}
}});

canvas.addEventListener('mousemove', function(e) {{
  var wc = screenToWorld(e.offsetX, e.offsetY);
  if (dragging !== null) {{
    nodes[dragging].x = wc[0] + dragOffX;
    nodes[dragging].y = wc[1] + dragOffY;
    nodes[dragging].vx = 0;
    nodes[dragging].vy = 0;
    return;
  }}
  if (panning) {{
    tx = panTx + (e.offsetX - panStartX);
    ty = panTy + (e.offsetY - panStartY);
    return;
  }}
  // Tooltip (safe DOM construction).
  var found = false;
  for (var i = 0; i < nodes.length; i++) {{
    var n = nodes[i];
    var r = nodeRadius(n);
    if ((n.x-wc[0])*(n.x-wc[0]) + (n.y-wc[1])*(n.y-wc[1]) < (r+4)*(r+4)) {{
      tooltip.style.display = 'block';
      tooltip.style.left = (e.offsetX + 14) + 'px';
      tooltip.style.top = (e.offsetY + 14) + 'px';
      // Build tooltip content safely.
      tooltip.textContent = '';
      var pathDiv = document.createElement('div');
      pathDiv.className = 'tt-path';
      pathDiv.textContent = n.id;
      tooltip.appendChild(pathDiv);
      var rows = [
        'PageRank: ' + n.pagerank.toFixed(4),
        'Community: ' + (n.community != null ? n.community : 'N/A'),
        'In-degree: ' + n.in_degree + ' | Out-degree: ' + n.out_degree
      ];
      rows.forEach(function(txt) {{
        var rd = document.createElement('div');
        rd.className = 'tt-row';
        rd.textContent = txt;
        tooltip.appendChild(rd);
      }});
      found = true;
      break;
    }}
  }}
  if (!found) tooltip.style.display = 'none';
}});

canvas.addEventListener('mouseup', function() {{ dragging = null; panning = false; }});

searchInput.addEventListener('input', function(e) {{ searchTerm = e.target.value.toLowerCase(); }});

function nodeRadius(n) {{ return 3 + Math.sqrt(n.pagerank) * 12; }}
function nodeColor(n) {{
  if (n.community != null) return COLORS[n.community % COLORS.length];
  return '#888';
}}

// Simple force simulation.
function simulate() {{
  var alpha = 0.3;
  var repulsion = 800;
  var attraction = 0.005;
  var centerForce = 0.01;
  var damping = 0.85;

  for (var i = 0; i < nodes.length; i++) {{
    nodes[i].vx += (W/2 - nodes[i].x) * centerForce;
    nodes[i].vy += (H/2 - nodes[i].y) * centerForce;
  }}

  for (var i = 0; i < nodes.length; i++) {{
    for (var j = i+1; j < nodes.length; j++) {{
      var dx = nodes[j].x - nodes[i].x;
      var dy = nodes[j].y - nodes[i].y;
      var d2 = dx*dx + dy*dy + 1;
      if (d2 > 500*500) continue;
      var f = repulsion / d2;
      var fx = dx * f, fy = dy * f;
      nodes[i].vx -= fx; nodes[i].vy -= fy;
      nodes[j].vx += fx; nodes[j].vy += fy;
    }}
  }}

  for (var k = 0; k < edges.length; k++) {{
    var e = edges[k];
    var si = nodeMap[e.source], ti = nodeMap[e.target];
    if (si === undefined || ti === undefined) continue;
    var dx = nodes[ti].x - nodes[si].x;
    var dy = nodes[ti].y - nodes[si].y;
    var fx = dx * attraction, fy = dy * attraction;
    nodes[si].vx += fx; nodes[si].vy += fy;
    nodes[ti].vx -= fx; nodes[ti].vy -= fy;
  }}

  for (var i = 0; i < nodes.length; i++) {{
    var n = nodes[i];
    if (dragging !== null && i === dragging) continue;
    n.vx *= damping; n.vy *= damping;
    n.x += n.vx * alpha; n.y += n.vy * alpha;
  }}
}}

function draw() {{
  ctx.save();
  ctx.setTransform(devicePixelRatio, 0, 0, devicePixelRatio, 0, 0);
  ctx.clearRect(0, 0, W, H);
  ctx.save();
  ctx.translate(tx, ty);
  ctx.scale(scale, scale);

  var highlightSet = {{}};
  if (selectedNode !== null) {{
    highlightSet[nodes[selectedNode].id] = true;
    for (var k = 0; k < edges.length; k++) {{
      var e = edges[k];
      if (e.source === nodes[selectedNode].id) highlightSet[e.target] = true;
      if (e.target === nodes[selectedNode].id) highlightSet[e.source] = true;
    }}
  }}

  for (var k = 0; k < edges.length; k++) {{
    var e = edges[k];
    var si = nodeMap[e.source], ti = nodeMap[e.target];
    if (si === undefined || ti === undefined) continue;

    var isSurprising = e.surprise > 0.3;
    var isHighlighted = selectedNode !== null &&
      (e.source === nodes[selectedNode].id || e.target === nodes[selectedNode].id);

    var alpha = 0.15;
    var lineWidth = 0.5;
    var color = '200,200,200';

    if (isSurprising) {{ color = '233,69,96'; alpha = 0.7; lineWidth = 2; }}
    if (isHighlighted) {{ alpha = 0.8; lineWidth = 1.5; }}

    switch(e.confidence) {{
      case 'Verified': alpha = Math.max(alpha, 0.3); break;
      case 'High': alpha = Math.max(alpha, 0.2); break;
      case 'Medium': alpha *= 0.8; break;
      case 'Low': alpha *= 0.5; break;
    }}

    if (selectedNode !== null && !isHighlighted) alpha *= 0.1;

    ctx.beginPath();
    ctx.strokeStyle = 'rgba(' + color + ',' + alpha + ')';
    ctx.lineWidth = lineWidth / scale;

    if (e.confidence === 'High') {{
      ctx.setLineDash([6/scale, 3/scale]);
    }} else if (e.confidence === 'Medium') {{
      ctx.setLineDash([2/scale, 2/scale]);
    }} else {{
      ctx.setLineDash([]);
    }}

    ctx.moveTo(nodes[si].x, nodes[si].y);
    ctx.lineTo(nodes[ti].x, nodes[ti].y);
    ctx.stroke();
    ctx.setLineDash([]);
  }}

  for (var i = 0; i < nodes.length; i++) {{
    var n = nodes[i];
    var r = nodeRadius(n);

    var nodeAlpha = 1.0;
    var strokeColor = null;

    if (searchTerm && n.id.toLowerCase().indexOf(searchTerm) >= 0) {{
      strokeColor = '#fff';
    }} else if (searchTerm) {{
      nodeAlpha = 0.2;
    }}

    if (selectedNode !== null && !highlightSet[n.id]) {{
      nodeAlpha *= 0.15;
    }}

    ctx.beginPath();
    ctx.arc(n.x, n.y, r, 0, Math.PI * 2);
    ctx.fillStyle = nodeColor(n);
    ctx.globalAlpha = nodeAlpha;
    ctx.fill();

    if (strokeColor) {{
      ctx.strokeStyle = strokeColor;
      ctx.lineWidth = 2 / scale;
      ctx.stroke();
    }}

    ctx.globalAlpha = 1.0;
  }}

  ctx.restore();
  ctx.restore();
}}

function animLoop() {{
  simulate();
  draw();
  requestAnimationFrame(animLoop);
}}
animLoop();
</script>
</body>
</html>"##,
        json_data = json_data
    )
}

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
}
