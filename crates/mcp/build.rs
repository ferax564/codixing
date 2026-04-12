//! Build script for codixing-mcp: generates tool definitions and dispatch code
//! from TOML files in `tool_defs/`.
//!
//! This eliminates 450+ lines of hand-written JSON (CC=59) in `tools/mod.rs`
//! and replaces it with data-driven TOML definitions.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

// ---------------------------------------------------------------------------
// TOML data model
// ---------------------------------------------------------------------------

/// Represents a single TOML tool definition file (e.g. `search.toml`).
#[derive(Debug)]
struct ToolFile {
    /// If true, these tools are federation-only (not in main tool_definitions).
    federation_only: bool,
    tools: Vec<ToolDef>,
}

#[derive(Debug)]
struct ToolDef {
    name: String,
    description: String,
    handler: String,
    calling_convention: String,
    read_only: bool,
    medium: bool,
    meta: bool,
    requires_federation: bool,
    params: Vec<ParamDef>,
}

#[derive(Debug)]
struct ParamDef {
    name: String,
    param_type: String,
    description: String,
    required: bool,
    enum_values: Vec<String>,
    items_type: Option<String>,
}

// ---------------------------------------------------------------------------
// TOML parsing (manual, no serde dependency in build script)
// ---------------------------------------------------------------------------

fn parse_toml_file(path: &Path) -> ToolFile {
    let content =
        fs::read_to_string(path).unwrap_or_else(|e| panic!("Cannot read {}: {e}", path.display()));

    let mut federation_only = false;
    let mut tools = Vec::new();

    // Check for file-level federation_only flag
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed == "federation_only = true" {
            federation_only = true;
            break;
        }
        // Stop at first [[tools]] header
        if trimmed == "[[tools]]" {
            break;
        }
    }

    // Split by [[tools]] sections
    let sections: Vec<&str> = content.split("[[tools]]").collect();

    for section in sections.iter().skip(1) {
        let tool = parse_tool_section(section);
        tools.push(tool);
    }

    ToolFile {
        federation_only,
        tools,
    }
}

fn parse_tool_section(section: &str) -> ToolDef {
    let mut name = String::new();
    let mut description = String::new();
    let mut handler = String::new();
    let mut calling_convention = String::new();
    let mut read_only = true;
    let mut medium = false;
    let mut meta = false;
    let mut requires_federation = false;
    let mut params: BTreeMap<String, ParamDef> = BTreeMap::new();

    let mut current_param: Option<String> = None;

    for line in section.lines() {
        let trimmed = line.trim();

        // Skip comments and empty lines
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        // Detect param section headers: [tools.params.query]
        if trimmed.starts_with("[tools.params.") && trimmed.ends_with(']') {
            let param_name = trimmed
                .strip_prefix("[tools.params.")
                .unwrap()
                .strip_suffix(']')
                .unwrap()
                .to_string();
            params
                .entry(param_name.clone())
                .or_insert_with(|| ParamDef {
                    name: param_name.clone(),
                    param_type: String::new(),
                    description: String::new(),
                    required: false,
                    enum_values: Vec::new(),
                    items_type: None,
                });
            current_param = Some(param_name);
            continue;
        }

        // Any other section header resets current param
        if trimmed.starts_with('[') {
            current_param = None;
            continue;
        }

        // Key = value parsing
        if let Some((key, value)) = parse_kv(trimmed) {
            if let Some(ref param_name) = current_param {
                // We're inside a param section
                if let Some(param) = params.get_mut(param_name) {
                    match key {
                        "type" => param.param_type = value,
                        "description" => param.description = value,
                        "required" => param.required = value == "true",
                        "enum" => param.enum_values = parse_string_array(&value),
                        "items_type" => param.items_type = Some(value),
                        _ => {}
                    }
                }
            } else {
                // Top-level tool fields
                match key {
                    "name" => name = value,
                    "description" => description = value,
                    "handler" => handler = value,
                    "calling_convention" => calling_convention = value,
                    "read_only" => read_only = value == "true",
                    "medium" => medium = value == "true",
                    "meta" => meta = value == "true",
                    "requires_federation" => requires_federation = value == "true",
                    _ => {}
                }
            }
        }
    }

    // Collect params in insertion order (BTreeMap sorts alphabetically,
    // but we want definition order — re-parse to get order)
    let ordered_params = reorder_params(section, params);

    ToolDef {
        name,
        description,
        handler,
        calling_convention,
        read_only,
        medium,
        meta,
        requires_federation,
        params: ordered_params,
    }
}

/// Re-scan section to get param definition order (BTreeMap loses it).
fn reorder_params(section: &str, mut map: BTreeMap<String, ParamDef>) -> Vec<ParamDef> {
    let mut ordered = Vec::new();
    for line in section.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("[tools.params.") && trimmed.ends_with(']') {
            let param_name = trimmed
                .strip_prefix("[tools.params.")
                .unwrap()
                .strip_suffix(']')
                .unwrap();
            if let Some(p) = map.remove(param_name) {
                ordered.push(p);
            }
        }
    }
    // Any remaining (shouldn't happen)
    for (_, p) in map {
        ordered.push(p);
    }
    ordered
}

fn parse_kv(line: &str) -> Option<(&str, String)> {
    let eq_pos = line.find('=')?;
    let key = line[..eq_pos].trim();
    let raw_value = line[eq_pos + 1..].trim();

    // Strip quotes from string values
    let value = if raw_value.starts_with('"') && raw_value.ends_with('"') && raw_value.len() >= 2 {
        raw_value[1..raw_value.len() - 1].to_string()
    } else if raw_value.starts_with('[') {
        // Array value — return as-is for parse_string_array
        raw_value.to_string()
    } else {
        raw_value.to_string()
    };

    Some((key, value))
}

fn parse_string_array(s: &str) -> Vec<String> {
    let trimmed = s.trim();
    if !trimmed.starts_with('[') || !trimmed.ends_with(']') {
        return Vec::new();
    }
    let inner = &trimmed[1..trimmed.len() - 1];
    inner
        .split(',')
        .map(|item| {
            let t = item.trim();
            if t.starts_with('"') && t.ends_with('"') && t.len() >= 2 {
                t[1..t.len() - 1].to_string()
            } else {
                t.to_string()
            }
        })
        .filter(|s| !s.is_empty())
        .collect()
}

// ---------------------------------------------------------------------------
// Code generation
// ---------------------------------------------------------------------------

fn generate_input_schema(tool: &ToolDef) -> String {
    if tool.params.is_empty() {
        return r#"json!({"type": "object", "properties": {}, "required": []})"#.to_string();
    }

    let mut props = Vec::new();
    let mut required = Vec::new();

    for param in &tool.params {
        let mut prop_parts = Vec::new();

        // Escape the description for Rust string literal
        let desc_escaped = param.description.replace('\\', "\\\\").replace('"', "\\\"");

        if param.param_type == "array" {
            prop_parts.push(r#""type": "array""#.to_string());
            if let Some(ref items) = param.items_type {
                prop_parts.push(format!(r#""items": {{"type": "{}"}}"#, items));
            }
            prop_parts.push(format!(r#""description": "{}""#, desc_escaped));
        } else {
            prop_parts.push(format!(r#""type": "{}""#, param.param_type));
            if !param.enum_values.is_empty() {
                let enum_str = param
                    .enum_values
                    .iter()
                    .map(|v| format!(r#""{}""#, v))
                    .collect::<Vec<_>>()
                    .join(", ");
                prop_parts.push(format!(r#""enum": [{}]"#, enum_str));
            }
            prop_parts.push(format!(r#""description": "{}""#, desc_escaped));
        }

        props.push(format!(
            r#"                    "{}": {{ {} }}"#,
            param.name,
            prop_parts.join(", ")
        ));

        if param.required {
            required.push(format!(r#""{}""#, param.name));
        }
    }

    let props_str = props.join(",\n");
    let required_str = required.join(", ");

    format!(
        r#"json!({{
                "type": "object",
                "properties": {{
{}
                }},
                "required": [{}]
            }})"#,
        props_str, required_str
    )
}

fn generate_tool_json_expr(tool: &ToolDef) -> String {
    let desc_escaped = tool.description.replace('\\', "\\\\").replace('"', "\\\"");
    let schema = generate_input_schema(tool);

    format!(
        r#"        json!({{
            "name": "{}",
            "description": "{}",
            "inputSchema": {}
        }})"#,
        tool.name, desc_escaped, schema,
    )
}

fn main() {
    let tool_defs_dir = Path::new("tool_defs");
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let out_path = Path::new(&out_dir).join("tool_definitions_generated.rs");

    // Collect all TOML files
    let mut toml_files: Vec<_> = fs::read_dir(tool_defs_dir)
        .expect("Cannot read tool_defs/ directory")
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("toml") {
                Some(path)
            } else {
                None
            }
        })
        .collect();
    toml_files.sort();

    // Tell cargo to re-run if any TOML changes
    println!("cargo:rerun-if-changed=tool_defs/");
    for f in &toml_files {
        println!("cargo:rerun-if-changed={}", f.display());
    }

    let mut main_tools: Vec<ToolDef> = Vec::new();
    let mut federation_tools: Vec<ToolDef> = Vec::new();

    for path in &toml_files {
        let tool_file = parse_toml_file(path);
        for tool in tool_file.tools {
            if tool_file.federation_only {
                federation_tools.push(tool);
            } else {
                main_tools.push(tool);
            }
        }
    }

    // ---------------------------------------------------------------------------
    // Generate tool_definitions()
    // ---------------------------------------------------------------------------
    let mut code = String::new();

    code.push_str("// AUTO-GENERATED by build.rs from tool_defs/*.toml — DO NOT EDIT\n");
    code.push_str("// \n");
    code.push_str("// To add a new tool, create or edit a TOML file in crates/mcp/tool_defs/.\n\n");
    code.push_str("use serde_json::{Value, json};\n\n");

    // tool_definitions() -> Value
    code.push_str("/// Return the JSON-Schema definitions for all MCP tools.\n");
    code.push_str("///\n");
    code.push_str("/// Generated from TOML files in `tool_defs/`.\n");
    code.push_str("pub fn tool_definitions() -> Value {\n");
    code.push_str("    json!([\n");
    for (i, tool) in main_tools.iter().enumerate() {
        code.push_str(&generate_tool_json_expr(tool));
        if i + 1 < main_tools.len() {
            code.push(',');
        }
        code.push('\n');
    }
    code.push_str("    ])\n");
    code.push_str("}\n\n");

    // federation_tool_definitions() -> Vec<Value>
    let federation_no_requires: Vec<&ToolDef> = federation_tools
        .iter()
        .filter(|t| !t.requires_federation)
        .collect();
    code.push_str("/// JSON-Schema definitions for federation management tools.\n");
    code.push_str("pub fn federation_tool_definitions() -> Vec<Value> {\n");
    code.push_str("    vec![\n");
    for (i, tool) in federation_no_requires.iter().enumerate() {
        code.push_str(&generate_tool_json_expr(tool));
        if i + 1 < federation_no_requires.len() {
            code.push(',');
        }
        code.push('\n');
    }
    code.push_str("    ]\n");
    code.push_str("}\n\n");

    // list_projects_tool_definition() -> Value
    if let Some(lp) = federation_tools.iter().find(|t| t.name == "list_projects") {
        code.push_str("/// JSON-Schema definition for the `list_projects` tool (deprecated).\n");
        code.push_str("pub fn list_projects_tool_definition() -> Value {\n");
        code.push_str(&generate_tool_json_expr(lp));
        code.push('\n');
        code.push_str("}\n\n");
    }

    // MEDIUM_TOOLS constant
    let medium_names: Vec<&str> = main_tools
        .iter()
        .filter(|t| t.medium)
        .map(|t| t.name.as_str())
        .collect();

    code.push_str("/// Curated set of tool names exposed in `--medium` mode.\n");
    code.push_str("pub const MEDIUM_TOOLS: &[&str] = &[\n");
    for name in &medium_names {
        code.push_str(&format!("    \"{}\",\n", name));
    }
    code.push_str("];\n\n");

    // medium_tool_definitions() -> Value
    code.push_str("/// Return the medium tool list for `--medium` mode.\n");
    code.push_str("pub fn medium_tool_definitions() -> Value {\n");
    code.push_str("    let defs = tool_definitions();\n");
    code.push_str("    let empty = vec![];\n");
    code.push_str("    let all_tools = defs.as_array().unwrap_or(&empty);\n");
    code.push_str("    let subset: Vec<&Value> = all_tools\n");
    code.push_str("        .iter()\n");
    code.push_str("        .filter(|t| {\n");
    code.push_str("            t.get(\"name\")\n");
    code.push_str("                .and_then(|v| v.as_str())\n");
    code.push_str("                .is_some_and(|name| MEDIUM_TOOLS.contains(&name))\n");
    code.push_str("        })\n");
    code.push_str("        .collect();\n");
    code.push_str("    json!(subset)\n");
    code.push_str("}\n\n");

    // is_read_only_tool()
    let all_tools_combined: Vec<&ToolDef> =
        main_tools.iter().chain(federation_tools.iter()).collect();
    let read_only_names: Vec<&&ToolDef> =
        all_tools_combined.iter().filter(|t| t.read_only).collect();

    code.push_str("/// Returns true if the tool only needs read access to the engine.\n");
    code.push_str("pub fn is_read_only_tool(name: &str) -> bool {\n");
    code.push_str("    matches!(\n");
    code.push_str("        name,\n");
    for (i, tool) in read_only_names.iter().enumerate() {
        if i == 0 {
            code.push_str(&format!("        \"{}\"", tool.name));
        } else {
            code.push_str(&format!("\n            | \"{}\"", tool.name));
        }
    }
    code.push('\n');
    code.push_str("    )\n");
    code.push_str("}\n\n");

    // is_meta_tool() — kept available for future dynamic-discovery use cases.
    code.push_str(
        "/// Returns true if the tool is a meta-tool (used for dynamic tool discovery).\n",
    );
    code.push_str("#[allow(dead_code)]\n");
    code.push_str("pub fn is_meta_tool(name: &str) -> bool {\n");
    code.push_str("    matches!(name, ");
    let meta_names: Vec<String> = main_tools
        .iter()
        .chain(federation_tools.iter())
        .filter(|t| t.meta)
        .map(|t| format!("\"{}\"", t.name))
        .collect();
    code.push_str(&meta_names.join(" | "));
    code.push_str(")\n");
    code.push_str("}\n\n");

    // ---------------------------------------------------------------------------
    // Generate dispatch functions
    // ---------------------------------------------------------------------------

    // dispatch_read_only_match — generates the match arms for read-only dispatch
    code.push_str("/// Generated dispatch match arms for read-only tools.\n");
    code.push_str("///\n");
    code.push_str(
        "/// Returns `Some((output, is_error))` if the tool name matched, `None` otherwise.\n",
    );
    code.push_str("#[allow(unused_variables)]\n");
    code.push_str("pub fn dispatch_read_only_match(\n");
    code.push_str("    engine: &codixing_core::Engine,\n");
    code.push_str("    name: &str,\n");
    code.push_str("    args: &Value,\n");
    code.push_str("    federation: Option<&codixing_core::FederatedEngine>,\n");
    code.push_str("    progress: Option<&super::common::ProgressReporter>,\n");
    code.push_str(") -> Option<(String, bool)> {\n");
    code.push_str("    let result = match name {\n");

    // All read-only tools from main + federation
    for tool in main_tools.iter().chain(federation_tools.iter()) {
        if !tool.read_only {
            continue;
        }
        let handler = format_handler_call(&tool.handler);
        let call = match tool.calling_convention.as_str() {
            "engine_args_progress" => format!("{handler}(engine, args, progress)"),
            "engine_args" => format!("{handler}(engine, args)"),
            "engine_only" => format!("{handler}(engine)"),
            "args_only" => format!("{handler}(args)"),
            "federation_only" => format!("{handler}(federation)"),
            "args_federation" => format!("{handler}(args, federation)"),
            other => panic!(
                "Unknown calling convention for read-only tool '{}': {}",
                tool.name, other
            ),
        };
        code.push_str(&format!("        \"{}\" => {},\n", tool.name, call));
    }

    code.push_str("        _ => return None,\n");
    code.push_str("    };\n");
    code.push_str("    Some(result)\n");
    code.push_str("}\n\n");

    // dispatch_write_match — generates the match arms for write (mut) tools
    code.push_str("/// Generated dispatch match arms for write (mutable) tools.\n");
    code.push_str("///\n");
    code.push_str(
        "/// Returns `Some((output, is_error))` if the tool name matched, `None` otherwise.\n",
    );
    code.push_str("#[allow(unused_variables)]\n");
    code.push_str("pub fn dispatch_write_match(\n");
    code.push_str("    engine: &mut codixing_core::Engine,\n");
    code.push_str("    name: &str,\n");
    code.push_str("    args: &Value,\n");
    code.push_str(") -> Option<(String, bool)> {\n");
    code.push_str("    let result = match name {\n");

    for tool in main_tools.iter().chain(federation_tools.iter()) {
        if tool.read_only {
            continue;
        }
        let handler = format_handler_call(&tool.handler);
        let call = match tool.calling_convention.as_str() {
            "engine_mut_args" => format!("{handler}(engine, args)"),
            "engine_mut_only" => format!("{handler}(engine)"),
            other => panic!(
                "Unknown calling convention for write tool '{}': {}",
                tool.name, other
            ),
        };
        code.push_str(&format!("        \"{}\" => {},\n", tool.name, call));
    }

    code.push_str("        _ => return None,\n");
    code.push_str("    };\n");
    code.push_str("    Some(result)\n");
    code.push_str("}\n");

    fs::write(&out_path, code).expect("Failed to write generated code");

    // Sanity check: print tool counts
    let main_count = main_tools.len();
    let fed_count = federation_tools.len();
    eprintln!(
        "codegen: generated {main_count} main tools + {fed_count} federation tools = {} total",
        main_count + fed_count
    );
}

/// Format a handler reference for use in the generated `mod generated` submodule.
///
/// - `"search::call_code_search"` -> `"super::search::call_code_search"`
/// - `"self::call_search_tools"` -> `"super::call_search_tools"` (defined in mod.rs itself)
fn format_handler_call(handler: &str) -> String {
    let (module, func) = handler
        .split_once("::")
        .expect("handler must be in 'module::function' format");

    if module == "self" {
        format!("super::{func}")
    } else {
        format!("super::{module}::{func}")
    }
}
