//! MCP tool definitions and engine dispatch helpers.
//!
//! Tool schemas, `is_read_only_tool`, `is_meta_tool`, and the dispatch match
//! arms are **generated at build time** from TOML files in
//! `crates/mcp/tool_defs/`.  See `build.rs` for the codegen logic.
//!
//! To add a new tool:
//! 1. Add a `[[tools]]` entry to the appropriate TOML file in `tool_defs/`.
//! 2. Implement the handler function in the corresponding submodule.
//! 3. Run `cargo build` — the rest is automatic.

mod analysis;
mod common;
mod context;
mod feature_hub;
pub mod federation;
mod files;
mod focus;
mod freshness;
mod graph;
mod memory;
mod orphans;
mod search;
mod temporal;

#[cfg(test)]
mod tests;

use serde::de::{DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value, json};

use codixing_core::{Engine, FederatedEngine};

pub use common::ProgressReporter;

/// Default context envelope for every MCP tool response.
pub(crate) const DEFAULT_TOOL_TOKEN_BUDGET: usize = 4_000;

/// Absolute context envelope for every MCP tool response, including tools
/// whose own schema accepts a larger budget.
pub(crate) const MAX_TOOL_TOKEN_BUDGET: usize = 12_000;

/// Maximum caller-controlled result count accepted by read-only MCP tools.
pub(crate) const MAX_TOOL_RESULT_COUNT: usize = 100;

/// Maximum graph traversal depth accepted at the MCP ingress boundary.
pub(crate) const MAX_TOOL_TRAVERSAL_DEPTH: usize = 8;

/// Maximum before/after context accepted by text-search tools.
pub(crate) const MAX_TOOL_CONTEXT_LINES: usize = 5;

/// Maximum number of caller-provided items accepted by array parameters.
pub(crate) const MAX_TOOL_ARRAY_ITEMS: usize = 64;

/// Maximum Unicode scalar count for ordinary read-only tool text inputs.
pub(crate) const MAX_TOOL_INPUT_CHARS: usize = 1_024;

/// Diffs legitimately need more room than a query, but still need a hard bound
/// before handlers split them into files, hunks, and symbols.
pub(crate) const MAX_TOOL_PATCH_CHARS: usize = 1_048_576;

/// Line coordinates are serialized as u64 by MCP but core file APIs do not
/// need values beyond the 32-bit range.
pub(crate) const MAX_TOOL_LINE_NUMBER: usize = u32::MAX as usize;

/// Avoid nonsensical git/freshness scans spanning more than one millennium.
pub(crate) const MAX_TOOL_TIME_WINDOW_DAYS: usize = 365_000;

/// Complete-reference pagination is bounded by the core's deterministic scan
/// cap, preventing arbitrary offsets from triggering surprising work.
pub(crate) const MAX_TOOL_PAGINATION_OFFSET: usize = 100_000;

const TOOL_OUTPUT_TRUNCATION_MARKER: &str =
    "\n\n<!-- truncated: MCP tool output token budget reached -->\n";

// ---------------------------------------------------------------------------
// Generated code: tool schemas, classification, and dispatch match arms
// ---------------------------------------------------------------------------

/// Submodule containing build-time generated code from `tool_defs/*.toml`.
///
/// Re-exported items: `tool_definitions`, `federation_tool_definitions`,
/// `list_projects_tool_definition`, `is_read_only_tool`, `is_meta_tool`.
mod generated {
    include!(concat!(env!("OUT_DIR"), "/tool_definitions_generated.rs"));
}

// Re-export generated public API so callers see the same interface as before.
pub use generated::{
    federation_tool_definitions, is_known_tool, is_read_only_tool, list_projects_tool_definition,
    tool_definitions,
};

// ---------------------------------------------------------------------------
// Tool definitions with federation
// ---------------------------------------------------------------------------

/// Return tool definitions, optionally including federation-only tools.
pub fn tool_definitions_with_federation(has_federation: bool) -> Value {
    let mut defs = tool_definitions();
    if let Some(arr) = defs.as_array_mut() {
        // Federation management tools are always listed so users can manage
        // configs even without a live FederatedEngine.
        arr.extend(federation_tool_definitions());
        if has_federation {
            arr.push(list_projects_tool_definition());
        }
    }
    defs
}

// ---------------------------------------------------------------------------
// Dynamic tool discovery helpers
// ---------------------------------------------------------------------------

/// Return a compact list of `(name, description)` tuples from the full tool
/// definitions (including federation tools). Used by `search_tools` to return
/// lightweight summaries.
pub fn tool_summaries() -> Vec<(String, String)> {
    let defs = tool_definitions_with_federation(true);
    defs.as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|tool| {
            let name = tool.get("name")?.as_str()?.to_string();
            let desc = tool.get("description")?.as_str()?.to_string();
            Some((name, desc))
        })
        .collect()
}

/// Handle the `search_tools` meta-tool: substring-match `query` against tool
/// names and descriptions, returning a compact list.
pub(crate) fn call_search_tools(args: &Value) -> (String, bool) {
    let query = args
        .get("query")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();

    let summaries = tool_summaries();
    let matches: Vec<&(String, String)> = if query.is_empty() {
        summaries.iter().collect()
    } else {
        summaries
            .iter()
            .filter(|(name, desc)| {
                name.to_lowercase().contains(&query) || desc.to_lowercase().contains(&query)
            })
            .collect()
    };

    if matches.is_empty() {
        return (
            format!("No tools match query '{query}'. Try a broader keyword."),
            false,
        );
    }

    let mut out = format!("## Matching tools ({} results)\n\n", matches.len());
    for (name, desc) in &matches {
        // Truncate description to first sentence for compact output.
        let short_desc = desc.split(". ").next().unwrap_or(desc);
        out.push_str(&format!("- **{name}**: {short_desc}.\n"));
    }
    out.push_str("\nUse `get_tool_schema` with the tool name(s) to get full parameter details.");

    (out, false)
}

/// Handle the `get_tool_schema` meta-tool: return full JSON schemas for the
/// requested tool name(s).
pub(crate) fn call_get_tool_schema(args: &Value) -> (String, bool) {
    let names: Vec<&str> = match args.get("names").and_then(|v| v.as_array()) {
        Some(arr) => arr
            .iter()
            .filter_map(|v| v.as_str())
            .take(MAX_TOOL_ARRAY_ITEMS)
            .collect(),
        None => {
            return (
                "Missing required parameter 'names' (array of tool name strings).".to_string(),
                true,
            );
        }
    };

    if names.is_empty() {
        return (
            "Parameter 'names' must contain at least one tool name.".to_string(),
            true,
        );
    }

    let defs = tool_definitions_with_federation(true);
    let empty = vec![];
    let all_tools = defs.as_array().unwrap_or(&empty);

    let mut results: Vec<Value> = Vec::new();
    let mut not_found: Vec<&str> = Vec::new();

    for name in &names {
        let found = all_tools
            .iter()
            .find(|t| t.get("name").and_then(|v| v.as_str()) == Some(name));
        match found {
            Some(tool) => results.push(tool.clone()),
            None => not_found.push(name),
        }
    }

    if results.is_empty() {
        return (
            format!(
                "Unknown tool(s): {}. Use search_tools to discover available tools.",
                not_found.join(", ")
            ),
            true,
        );
    }

    let output_json = if not_found.is_empty() {
        json!(results)
    } else {
        json!({
            "tools": results,
            "unknown_tools": not_found,
        })
    };
    (
        serde_json::to_string_pretty(&output_json).unwrap_or_else(|_| "[]".to_string()),
        false,
    )
}

/// Profile management tools are handled in the JSON-RPC layer because they
/// mutate per-connection dispatch state rather than the repository engine.
pub(crate) fn call_get_mcp_profile_placeholder(_args: &Value) -> (String, bool) {
    (
        "Internal error: get_mcp_profile must be handled by the JSON-RPC dispatcher.".to_string(),
        true,
    )
}

/// Profile management tools are handled in the JSON-RPC layer because they
/// mutate per-connection dispatch state rather than the repository engine.
pub(crate) fn call_set_mcp_profile_placeholder(_args: &Value) -> (String, bool) {
    (
        "Internal error: set_mcp_profile must be handled by the JSON-RPC dispatcher.".to_string(),
        true,
    )
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Dispatch a read-only `tools/call` invocation.
///
/// Takes `&Engine` (shared reference) so multiple read-only calls can run
/// concurrently under a `RwLock::read()` guard.
///
/// The optional `federation` parameter provides access to the federated engine
/// for cross-repo tools like `list_projects`.
///
/// Returns `(text_output, is_error)`.
/// Convenience wrapper for `dispatch_tool_ref_with_progress` without progress.
///
/// Used by unit tests in `tools/tests.rs` which don't need progress reporting.
#[allow(dead_code)]
pub fn dispatch_tool_ref(
    engine: &Engine,
    name: &str,
    args: &Value,
    federation: Option<&FederatedEngine>,
) -> (String, bool) {
    dispatch_tool_ref_with_progress(engine, name, args, federation, None)
}

/// Dispatch a read-only `tools/call` invocation, optionally with progress
/// reporting for long-running operations.
pub fn dispatch_tool_ref_with_progress(
    engine: &Engine,
    name: &str,
    args: &Value,
    federation: Option<&FederatedEngine>,
    progress: Option<&ProgressReporter>,
) -> (String, bool) {
    let bounded_args = match bounded_read_only_args(args) {
        Ok(args) => args,
        Err(error) => return (error, true),
    };
    let (output, is_error) = match generated::dispatch_read_only_match(
        engine,
        name,
        &bounded_args,
        federation,
        progress,
    ) {
        Some(result) => result,
        None => (format!("Unknown read-only tool: {name}"), true),
    };
    let output_is_json = serde_json::from_str::<Value>(&output).is_ok();
    let filtered_output = if output_is_json {
        output
    } else if args
        .get("compact")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        compact_output(&output)
    } else {
        engine.filter_output(&output, name).output
    };
    let final_output = enforce_tool_output_budget(&filtered_output, args);
    (final_output, is_error)
}

/// Dispatch a `tools/call` invocation to the appropriate engine method.
///
/// Takes `&mut Engine` so that write tools (write_file, edit_file, delete_file,
/// etc.) can mutate the index inline.
///
/// Returns `(text_output, is_error)`.
/// Convenience wrapper for `dispatch_tool_with_progress` without progress.
///
/// Used by unit tests in `tools/tests.rs` which don't need progress reporting.
#[allow(dead_code)]
pub fn dispatch_tool(
    engine: &mut Engine,
    name: &str,
    args: &Value,
    federation: Option<&FederatedEngine>,
) -> (String, bool) {
    dispatch_tool_with_progress(engine, name, args, federation, None)
}

/// Dispatch a `tools/call` invocation to the appropriate engine method,
/// optionally with progress reporting.
pub fn dispatch_tool_with_progress(
    engine: &mut Engine,
    name: &str,
    args: &Value,
    federation: Option<&FederatedEngine>,
    progress: Option<&ProgressReporter>,
) -> (String, bool) {
    let (output, is_error) = match generated::dispatch_write_match(engine, name, args) {
        Some(result) => result,
        // Fallback: if a read-only tool is accidentally dispatched through the
        // write path, handle it rather than returning an error.
        None => {
            let bounded_args = match bounded_read_only_args(args) {
                Ok(args) => args,
                Err(error) => return (error, true),
            };
            match generated::dispatch_read_only_match(
                engine,
                name,
                &bounded_args,
                federation,
                progress,
            ) {
                Some(result) => result,
                None => (format!("Unknown tool: {name}"), true),
            }
        }
    };
    let output_is_json = serde_json::from_str::<Value>(&output).is_ok();
    let filtered_output = if output_is_json {
        output
    } else if args
        .get("compact")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        compact_output(&output)
    } else {
        engine.filter_output(&output, name).output
    };
    let final_output = enforce_tool_output_budget(&filtered_output, args);
    (final_output, is_error)
}

/// Resolve the response budget shared by handlers and the final envelope.
/// Values above the hard cap are clamped; zero is normalized to one token so
/// truncation can still return an explicit ellipsis.
pub(crate) fn requested_tool_token_budget(args: &Value) -> usize {
    args.get("token_budget")
        .and_then(|value| value.as_u64())
        .map(|value| value.min(MAX_TOOL_TOKEN_BUDGET as u64) as usize)
        .unwrap_or(DEFAULT_TOOL_TOKEN_BUDGET)
        .max(1)
}

pub(crate) fn requested_result_count(args: &Value, field: &str, default: usize) -> usize {
    requested_bounded_usize(args, field, default, 1, MAX_TOOL_RESULT_COUNT)
}

pub(crate) fn requested_traversal_depth(args: &Value, field: &str, default: usize) -> usize {
    requested_bounded_usize(args, field, default, 1, MAX_TOOL_TRAVERSAL_DEPTH)
}

pub(crate) fn requested_context_lines(args: &Value, field: &str, default: usize) -> usize {
    requested_bounded_usize(args, field, default, 0, MAX_TOOL_CONTEXT_LINES)
}

pub(crate) fn requested_bounded_usize(
    args: &Value,
    field: &str,
    default: usize,
    minimum: usize,
    maximum: usize,
) -> usize {
    args.get(field)
        .and_then(Value::as_u64)
        .unwrap_or(default as u64)
        .clamp(minimum as u64, maximum as u64) as usize
}

/// Reserve space for pretty-printing and the JSON container itself. Core
/// assemblers budget payload content, while MCP returns the serialized object.
pub(crate) fn requested_structured_tool_token_budget(args: &Value) -> usize {
    (requested_tool_token_budget(args).saturating_mul(3) / 4).max(1)
}

/// Sanitize read-only arguments before generated dispatch reaches a handler.
///
/// This is deliberately centralized: every current and future read-only tool
/// gets the same bounded ingress even if its handler forgets to clamp a raw
/// JSON integer or array. Strings are counted as Unicode scalar values so a
/// multi-byte UTF-8 query is not rejected based on byte length.
pub(crate) fn bounded_read_only_args(args: &Value) -> Result<Value, String> {
    let Some(object) = args.as_object() else {
        return Ok(json!({ "token_budget": requested_tool_token_budget(args) }));
    };

    let mut bounded = Map::with_capacity(object.len() + 1);
    for (key, value) in object {
        let value = match key.as_str() {
            "token_budget" => json!(requested_tool_token_budget(args)),
            "limit" | "max_files" => bounded_integer(value, 1, MAX_TOOL_RESULT_COUNT),
            "offset" => bounded_integer(value, 0, MAX_TOOL_PAGINATION_OFFSET),
            "depth" | "callee_depth" => bounded_integer(value, 1, MAX_TOOL_TRAVERSAL_DEPTH),
            "context_lines" | "before_context" | "after_context" => {
                bounded_integer(value, 0, MAX_TOOL_CONTEXT_LINES)
            }
            "line" | "line_start" | "line_end" => bounded_integer(value, 0, MAX_TOOL_LINE_NUMBER),
            "days" | "threshold_days" => bounded_integer(value, 1, MAX_TOOL_TIME_WINDOW_DAYS),
            "min_complexity" => bounded_integer(value, 1, 1_000_000),
            "patch" => bounded_text_value(value, key, MAX_TOOL_PATCH_CHARS)?,
            "query" | "task" | "pattern" => bounded_text_value(value, key, MAX_TOOL_INPUT_CHARS)?,
            _ if value.is_array() => bounded_string_array(value, key)?,
            _ if value.is_string() => bounded_text_value(value, key, MAX_TOOL_INPUT_CHARS)?,
            _ => value.clone(),
        };
        bounded.insert(key.clone(), value);
    }

    // Ensure handlers that use a larger historical default never perform more
    // work than the response envelope can return.
    bounded
        .entry("token_budget".to_string())
        .or_insert_with(|| json!(requested_tool_token_budget(args)));
    Ok(Value::Object(bounded))
}

fn bounded_integer(value: &Value, minimum: usize, maximum: usize) -> Value {
    match value.as_u64() {
        Some(value) => json!(value.clamp(minimum as u64, maximum as u64)),
        None => value.clone(),
    }
}

fn bounded_text_value(value: &Value, field: &str, maximum: usize) -> Result<Value, String> {
    let Some(text) = value.as_str() else {
        return Ok(value.clone());
    };
    validate_text_length(text, field, maximum)?;
    Ok(Value::String(text.to_string()))
}

fn bounded_string_array(value: &Value, field: &str) -> Result<Value, String> {
    let Some(values) = value.as_array() else {
        return Ok(value.clone());
    };
    let mut bounded = Vec::with_capacity(values.len().min(MAX_TOOL_ARRAY_ITEMS));
    for text in values
        .iter()
        .filter_map(Value::as_str)
        .take(MAX_TOOL_ARRAY_ITEMS)
    {
        validate_text_length(text, field, MAX_TOOL_INPUT_CHARS)?;
        bounded.push(Value::String(text.to_string()));
    }
    Ok(Value::Array(bounded))
}

fn validate_text_length(text: &str, field: &str, maximum: usize) -> Result<(), String> {
    if text.chars().count() > maximum {
        return Err(format!(
            "Argument '{field}' is too long (maximum: {maximum} characters)"
        ));
    }
    Ok(())
}

fn enforce_tool_output_budget(output: &str, args: &Value) -> String {
    let token_budget = requested_tool_token_budget(args);
    // Every tokenizer token consumes at least one source byte, so this is a
    // zero-allocation proof that the output fits. For moderately sized output
    // retain exact behavior, but never ask cl100k to allocate a token Vec for
    // an arbitrarily large producer response.
    if output.len() <= token_budget
        || (output.len() <= bounded_token_count_bytes(token_budget)
            && codixing_core::formatter::count_tokens(output) <= token_budget)
    {
        return output.to_string();
    }

    // Byte/token slicing a serialized value produces invalid JSON. Preserve a
    // progressively reduced preview instead of replacing every result with an
    // empty omission marker. This keeps the first results and scalar metadata
    // useful to an agent while still enforcing the exact token ceiling.
    if matches!(
        output.trim_start().as_bytes().first(),
        Some(b'{') | Some(b'[')
    ) && let Some(preview) = bounded_json_preview(output, token_budget)
    {
        return preview;
    }

    let bounded_input = bounded_text_prefix(output, token_budget);
    codixing_core::formatter::truncate_to_token_budget(
        bounded_input,
        token_budget,
        TOOL_OUTPUT_TRUNCATION_MARKER,
    )
}

/// Maximum source bytes handed to the allocating tokenizer for one response.
/// The cap scales with the requested result size but stays small enough that a
/// runaway producer cannot turn a 100 MiB response into a larger token vector.
fn bounded_token_count_bytes(token_budget: usize) -> usize {
    token_budget.saturating_mul(8).min(1024 * 1024)
}

/// Keep a generous byte prefix for free-form text before exact token truncation.
/// JSON uses the structured streaming path above. A 64× allowance preserves
/// highly-compressible whitespace/repetition while bounding tokenizer memory.
fn bounded_text_prefix(output: &str, token_budget: usize) -> &str {
    let cap = token_budget.saturating_mul(64).min(1024 * 1024);
    if output.len() <= cap {
        return output;
    }
    let mut end = cap;
    while end > 0 && !output.is_char_boundary(end) {
        end -= 1;
    }
    &output[..end]
}

/// Wrap structured output in a valid truncation envelope and retain the largest
/// serialized prefix that fits the exact token budget.
fn bounded_json_preview(output: &str, token_budget: usize) -> Option<String> {
    // Four source bytes per token is a useful first estimate for code-shaped
    // JSON. A single scaled retry handles escape-heavy payloads; unlike the old
    // DOM + binary-search path, peak memory is O(requested budget), not O(output).
    let mut byte_budget = output
        .len()
        .min(token_budget.saturating_mul(4).saturating_sub(48));
    for _ in 0..2 {
        let partial = match stream_json_preview(output, byte_budget) {
            Ok(Some(partial)) => partial,
            Ok(None) => break,
            Err(_) => return None,
        };
        let serialized = json!({"truncated": true, "partial": partial}).to_string();
        let tokens = codixing_core::formatter::count_tokens(&serialized);
        if tokens <= token_budget {
            return Some(serialized);
        }
        byte_budget = byte_budget
            .saturating_mul(token_budget)
            .checked_div(tokens.max(1))
            .unwrap_or(0)
            .saturating_mul(9)
            / 10;
    }

    let omission = json!({"truncated": true}).to_string();
    Some(
        if codixing_core::formatter::count_tokens(&omission) <= token_budget {
            omission
        } else {
            "{}".to_string()
        },
    )
}

/// Deserialize and validate the whole JSON document while retaining only a
/// byte-bounded prefix tree. Omitted values are consumed as `IgnoredAny`, so a
/// multi-megabyte result never becomes a multi-megabyte `Value` allocation.
fn stream_json_preview(
    output: &str,
    byte_budget: usize,
) -> Result<Option<Value>, serde_json::Error> {
    let mut remaining = byte_budget;
    let mut deserializer = serde_json::Deserializer::from_str(output);
    let value = BoundedJsonSeed {
        remaining: &mut remaining,
    }
    .deserialize(&mut deserializer)?;
    deserializer.end()?;
    Ok(value)
}

struct BoundedJsonSeed<'a> {
    remaining: &'a mut usize,
}

impl<'de> DeserializeSeed<'de> for BoundedJsonSeed<'_> {
    type Value = Option<Value>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(BoundedJsonVisitor {
            remaining: self.remaining,
        })
    }
}

struct BoundedJsonVisitor<'a> {
    remaining: &'a mut usize,
}

impl BoundedJsonVisitor<'_> {
    fn scalar(&mut self, value: Value) -> Option<Value> {
        let bytes = value.to_string().len();
        if bytes > *self.remaining {
            return None;
        }
        *self.remaining -= bytes;
        Some(value)
    }
}

impl<'de> Visitor<'de> for BoundedJsonVisitor<'_> {
    type Value = Option<Value>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("any JSON value")
    }

    fn visit_unit<E>(mut self) -> Result<Self::Value, E> {
        Ok(self.scalar(Value::Null))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.visit_unit()
    }

    fn visit_bool<E>(mut self, value: bool) -> Result<Self::Value, E> {
        Ok(self.scalar(Value::Bool(value)))
    }

    fn visit_i64<E>(mut self, value: i64) -> Result<Self::Value, E> {
        Ok(self.scalar(Value::Number(value.into())))
    }

    fn visit_u64<E>(mut self, value: u64) -> Result<Self::Value, E> {
        Ok(self.scalar(Value::Number(value.into())))
    }

    fn visit_f64<E>(mut self, value: f64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        let number = serde_json::Number::from_f64(value)
            .ok_or_else(|| E::custom("non-finite JSON number"))?;
        Ok(self.scalar(Value::Number(number)))
    }

    fn visit_borrowed_str<E>(self, value: &'de str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.visit_str(value)
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(bounded_json_string(value, self.remaining).map(Value::String))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.visit_str(&value)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        if *self.remaining < 2 {
            while sequence.next_element::<IgnoredAny>()?.is_some() {}
            return Ok(None);
        }
        *self.remaining -= 2;
        let mut output = Vec::new();
        loop {
            let separator = usize::from(!output.is_empty());
            if *self.remaining <= separator {
                while sequence.next_element::<IgnoredAny>()?.is_some() {}
                break;
            }
            let before = *self.remaining;
            *self.remaining -= separator;
            match sequence.next_element_seed(BoundedJsonSeed {
                remaining: self.remaining,
            })? {
                Some(Some(value)) => output.push(value),
                Some(None) => {
                    *self.remaining = before;
                    while sequence.next_element::<IgnoredAny>()?.is_some() {}
                    break;
                }
                None => {
                    *self.remaining = before;
                    break;
                }
            }
        }
        Ok(Some(Value::Array(output)))
    }

    fn visit_map<A>(self, mut object: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        if *self.remaining < 2 {
            while object.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
            return Ok(None);
        }
        *self.remaining -= 2;
        let mut output = Map::new();
        while let Some(key) = object.next_key::<String>()? {
            let overhead = json_string_byte_len(&key)
                .saturating_add(1)
                .saturating_add(usize::from(!output.is_empty()));
            if *self.remaining <= overhead {
                object.next_value::<IgnoredAny>()?;
                continue;
            }

            let before = *self.remaining;
            *self.remaining -= overhead;
            // Preserve a little space for scalar metadata that follows a bulky
            // collection (serde_json maps are commonly key-sorted).
            let reserve = (*self.remaining / 8).min(64);
            let mut child_remaining = self.remaining.saturating_sub(reserve);
            let retained = object.next_value_seed(BoundedJsonSeed {
                remaining: &mut child_remaining,
            })?;
            if let Some(value) = retained {
                let consumed = self
                    .remaining
                    .saturating_sub(reserve)
                    .saturating_sub(child_remaining);
                *self.remaining = before.saturating_sub(overhead + consumed);
                output.insert(key, value);
            } else {
                *self.remaining = before;
            }
        }
        Ok(Some(Value::Object(output)))
    }
}

fn bounded_json_string(value: &str, remaining: &mut usize) -> Option<String> {
    if *remaining < 2 {
        return None;
    }
    let content_budget = remaining.saturating_sub(2);
    let mut output = String::new();
    let mut encoded = 0usize;
    let mut truncated = false;
    for character in value.chars() {
        let width = json_char_byte_len(character);
        if encoded.saturating_add(width) > content_budget {
            truncated = true;
            break;
        }
        output.push(character);
        encoded += width;
    }
    if truncated && content_budget >= '…'.len_utf8() {
        while encoded.saturating_add('…'.len_utf8()) > content_budget {
            let Some(character) = output.pop() else {
                break;
            };
            encoded = encoded.saturating_sub(json_char_byte_len(character));
        }
        output.push('…');
        encoded += '…'.len_utf8();
    }
    *remaining = remaining.saturating_sub(encoded + 2);
    Some(output)
}

fn json_string_byte_len(value: &str) -> usize {
    2 + value.chars().map(json_char_byte_len).sum::<usize>()
}

fn json_char_byte_len(character: char) -> usize {
    match character {
        '"' | '\\' | '\u{08}' | '\u{0c}' | '\n' | '\r' | '\t' => 2,
        '\u{00}'..='\u{1f}' => 6,
        _ => character.len_utf8(),
    }
}

// ---------------------------------------------------------------------------
// Compact output post-processing
// ---------------------------------------------------------------------------

/// Compress tool output for token-constrained AI agents:
/// - Remove fenced code blocks, keep only `// <file>` headers and signatures
/// - Truncate lines longer than 120 chars
/// - Limit total output to ~2000 chars
/// - Preserve structural elements (headers, file paths, line numbers)
fn compact_output(output: &str) -> String {
    let mut result = String::with_capacity(output.len().min(2200));
    let mut in_code_block = false;
    let mut code_block_lines = 0u32;

    for line in output.lines() {
        let trimmed = line.trim();

        // Track fenced code blocks.
        if trimmed.starts_with("```") {
            if in_code_block {
                // Closing fence — emit summary if we skipped lines.
                if code_block_lines > 2 {
                    result.push_str(&format!("  ... ({code_block_lines} lines)\n"));
                }
                in_code_block = false;
                code_block_lines = 0;
            } else {
                in_code_block = true;
                code_block_lines = 0;
            }
            continue;
        }

        if in_code_block {
            code_block_lines += 1;
            // Keep only the first 2 lines of each code block (signature / key info).
            if code_block_lines <= 2 {
                let truncated = truncate_line(line, 120);
                result.push_str(truncated);
                result.push('\n');
            }
            continue;
        }

        // Outside code blocks: keep headers, file paths, bullet points.
        let truncated = truncate_line(line, 120);
        result.push_str(truncated);
        result.push('\n');

        // Hard limit on total output.
        if result.len() > 2000 {
            result.push_str("\n... (output compacted)\n");
            break;
        }
    }

    result
}

/// Return a `&str` slice of at most `max_len` characters.
fn truncate_line(line: &str, max_len: usize) -> &str {
    if line.len() <= max_len {
        line
    } else {
        // Find a safe char boundary.
        let mut end = max_len;
        while end > 0 && !line.is_char_boundary(end) {
            end -= 1;
        }
        &line[..end]
    }
}

// ---------------------------------------------------------------------------
// Session helpers (called by generated dispatch via `super::`)
// ---------------------------------------------------------------------------

pub(crate) fn call_get_session_summary(engine: &Engine, args: &Value) -> (String, bool) {
    let token_budget = requested_tool_token_budget(args);

    let summary = engine.session().summary(token_budget);
    (summary, false)
}

pub(crate) fn call_session_status(engine: &Engine, args: &Value) -> (String, bool) {
    let limit = requested_result_count(args, "limit", 10);

    let shared = engine.shared_session();
    let agents = shared.active_agents();
    let hot_files = shared.get_hot_files(limit);
    let event_count = shared.event_count();

    let mut out = String::from("## Shared Session Status\n\n");

    out.push_str(&format!("**Total events:** {event_count}\n"));
    out.push_str(&format!(
        "**Active agents:** {}\n",
        if agents.is_empty() {
            "none".to_string()
        } else {
            format!("{} ({})", agents.len(), agents.join(", "))
        }
    ));
    out.push_str(&format!(
        "**Current agent:** {}\n\n",
        engine.session().session_id()
    ));

    if hot_files.is_empty() {
        out.push_str("No recently active files.\n");
    } else {
        out.push_str("### Hot files (cross-agent activity)\n\n");
        for (i, (file, score)) in hot_files.iter().enumerate() {
            out.push_str(&format!("  {}. `{}` (score: {:.3})\n", i + 1, file, score));
        }
    }

    if !engine.embeddings_ready() {
        let (done, total) = engine.embedding_progress();
        out.push_str("\n## Embedding Progress\n\n");
        out.push_str(&format!(
            "  {done}/{total} chunks ({:.0}%)\n",
            if total > 0 {
                done as f64 / total as f64 * 100.0
            } else {
                100.0
            }
        ));
    } else if engine.embedding_progress().1 > 0 {
        out.push_str("\n## Embedding Progress\n\n  Complete\n");
    }

    (out, false)
}

pub(crate) fn call_session_reset_focus(engine: &Engine) -> (String, bool) {
    engine.session().reset_focus();
    (
        "Progressive focus cleared. Search results will no longer be narrowed to a specific directory.".to_string(),
        false,
    )
}

// ---------------------------------------------------------------------------
// Federation helpers (called by generated dispatch via `super::`)
// ---------------------------------------------------------------------------

pub(crate) fn call_list_projects(federation: Option<&FederatedEngine>) -> (String, bool) {
    let fed = match federation {
        Some(f) => f,
        None => {
            return (
                "Federation is not enabled. Start the server with --federation <config.json> to use cross-repo features.".to_string(),
                true,
            );
        }
    };

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

    (out, false)
}
