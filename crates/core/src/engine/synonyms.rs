//! Vocabulary bridge: synonym expansion and code-pattern (HyDE) reformulations.
//!
//! Two public-crate functions:
//!
//! - [`expand_synonyms`] — appends domain synonyms to a query so that users
//!   searching for "rate limiting" also match "throttle", "burst", "leaky bucket".
//! - [`reformulate_to_code`] — maps conceptual queries to code tokens that
//!   would appear in an implementation (lightweight HyDE).
//!
//! Both are called from `generate_reformulations_with_synonyms` in `search.rs`
//! and produce *extra reformulation strings*, never polluting the main BM25 query.

/// Lightweight HyDE: map programming concepts to code tokens.
///
/// Returns a list of code-pattern strings that would appear in an implementation
/// of the described concept. The caller takes `top_k` (typically 3) and joins
/// them into a single reformulation query.
///
/// Example: "rate limiting" → `["RateLimiter", "throttle", "burst_capacity"]`
pub(crate) fn reformulate_to_code(query: &str) -> Vec<String> {
    let query_lower = query.to_lowercase();
    let mut patterns = Vec::new();

    let concept_map: &[(&[&str], &[&str])] = &[
        // Sorting
        (
            &["sort", "order", "arrange"],
            &["fn sort", ".sort(", "sort_by", "Ord", "cmp"],
        ),
        // Searching
        (
            &["search", "find", "lookup", "locate"],
            &["fn search", "fn find", ".find(", "filter", "contains"],
        ),
        // Iteration
        (
            &["iterate", "loop", "traverse", "walk"],
            &["for ", ".iter()", ".map(", "while ", "Iterator"],
        ),
        // Error handling
        (
            &["error", "exception", "handle error", "failure"],
            &["Result<", "Err(", "unwrap", "anyhow", "?;"],
        ),
        // Parsing
        (
            &["parse", "parsing", "tokenize", "lex"],
            &["fn parse", "Parser", "Token", "from_str"],
        ),
        // Serialization
        (
            &["serialize", "deserialize", "json", "encode", "decode"],
            &["Serialize", "Deserialize", "serde", "to_string", "from_str"],
        ),
        // Concurrency
        (
            &["concurrent", "parallel", "thread", "async", "mutex"],
            &["Arc<", "Mutex<", "async fn", "tokio", "rayon", "RwLock"],
        ),
        // Testing
        (
            &["test", "assert", "verify", "check"],
            &["#[test]", "assert!", "assert_eq!", "fn test_"],
        ),
        // Caching
        (
            &["cache", "memoize", "store"],
            &["HashMap", "cache", "LruCache", "memo"],
        ),
        // Configuration
        (
            &["config", "setting", "option", "preference"],
            &["Config", "Settings", "Options", "Default"],
        ),
        // Networking/HTTP
        (
            &["http", "request", "endpoint", "api", "rest"],
            &["fn get", "fn post", "Handler", "Router", "axum"],
        ),
        // File I/O
        (
            &["file", "read file", "write file", "io"],
            &["File::open", "read_to_string", "BufReader", "std::fs"],
        ),
        // Graph/tree
        (
            &["graph", "tree", "node", "edge"],
            &["Graph", "Node", "Edge", "petgraph", "DiGraph"],
        ),
        // Database
        (
            &["database", "query", "sql", "store"],
            &["Connection", "execute", "query", "INSERT", "SELECT"],
        ),
        // Authentication
        (
            &["auth", "login", "password", "token", "jwt"],
            &["authenticate", "verify_token", "Bearer", "Session"],
        ),
        // Hashing
        (
            &["hash", "digest", "checksum"],
            &["Hash", "Hasher", "sha256", "xxh3", "digest"],
        ),
        // Embedding/vector
        (
            &["embed", "vector", "similarity", "cosine"],
            &["embed", "Vec<f32>", "cosine_similarity", "dot_product"],
        ),
        // Indexing
        (
            &["index", "inverted", "full text", "bm25"],
            &["Index", "Tantivy", "BM25", "tokenizer"],
        ),
        // Rate limiting / throttling
        (
            &["rate limit", "throttle", "throttling", "burst"],
            &["RateLimiter", "throttle", "burst_capacity", "leaky_bucket"],
        ),
        // Retry / backoff
        (
            &["retry", "backoff", "exponential", "jitter"],
            &["retry", "backoff", "max_attempts", "ExponentialBackoff"],
        ),
        // Scheduling / cron
        (
            &["schedule", "cron", "job", "periodic", "interval"],
            &["Scheduler", "cron", "interval", "spawn_task", "tokio::time"],
        ),
        // Logging / tracing
        (
            &["log", "logging", "trace", "tracing", "metrics"],
            &["tracing", "log!", "info!", "warn!", "span"],
        ),
        // Middleware
        (
            &["middleware", "interceptor", "filter", "layer"],
            &["Middleware", "Layer", "tower::Service", "from_fn"],
        ),
        // Routing
        (
            &["route", "routing", "path", "url"],
            &["Router", "route", "get", "post", "axum::Router"],
        ),
        // WebSocket
        (
            &["websocket", "ws", "real-time", "socket"],
            &["WebSocket", "ws", "tungstenite", "on_message"],
        ),
        // Queue / message bus
        (
            &["queue", "message", "publish", "subscribe", "event bus"],
            &["Queue", "publish", "subscribe", "channel", "mpsc"],
        ),
        // Encryption / crypto
        (
            &["encrypt", "decrypt", "crypto", "cipher", "aes"],
            &["encrypt", "decrypt", "AES", "cipher", "ring"],
        ),
        // Compression
        (
            &["compress", "decompress", "gzip", "zlib", "zstd"],
            &["compress", "decompress", "flate2", "zstd", "GzEncoder"],
        ),
        // Pagination
        (
            &["paginate", "pagination", "page", "cursor", "offset"],
            &["paginate", "offset", "limit", "cursor", "next_page"],
        ),
        // Filtering
        (
            &["filter", "where", "predicate", "condition"],
            &[".filter(", "predicate", "where_clause", "QueryBuilder"],
        ),
        // Template / rendering
        (
            &["template", "render", "html", "view"],
            &["render", "template", "Tera", "Handlebars", "minijinja"],
        ),
        // Dependency injection
        (
            &["dependency injection", "di", "inject", "container"],
            &["inject", "Container", "Provider", "register"],
        ),
        // Event / observer
        (
            &["event", "observer", "emit", "dispatch", "pub/sub"],
            &["emit", "on_event", "EventBus", "observer", "dispatch"],
        ),
        // State machine
        (
            &["state machine", "fsm", "transition", "state"],
            &["State", "Transition", "StateMachine", "on_enter"],
        ),
        // Migration / schema
        (
            &["migration", "migrate", "schema", "alter table"],
            &["Migration", "migrate", "schema", "up", "down"],
        ),
        // Webhook
        (
            &["webhook", "callback url", "notify"],
            &["webhook", "on_event", "post", "hmac_verify"],
        ),
        // Session / cookie
        (
            &["session", "cookie", "csrf", "flash"],
            &["Session", "Cookie", "csrf_token", "flash_message"],
        ),
        // CORS
        (
            &["cors", "cross-origin", "access-control"],
            &["CorsLayer", "allow_origin", "Access-Control", "cors"],
        ),
        // CLI
        (
            &["cli", "command line", "argument", "flag", "subcommand"],
            &["clap", "Arg", "SubCommand", "ArgMatches", "parse"],
        ),
        // Plugin / extension
        (
            &["plugin", "extension", "hook", "addon"],
            &["Plugin", "Extension", "register_hook", "dyn Trait"],
        ),
        // Redaction / masking
        (
            &["redact", "mask", "sanitize", "scrub", "pii"],
            &["redact", "mask", "sanitize", "scrub_pii", "Redactor"],
        ),
        // Prompt / AI / LLM
        (
            &["prompt", "llm", "completion", "generate", "chat"],
            &["prompt", "completion", "ChatMessage", "generate", "token"],
        ),
        // Monitoring / alerting
        (
            &["monitor", "alert", "health check", "probe"],
            &["health_check", "probe", "alert", "Gauge", "Counter"],
        ),
        // Sorting / ranking (duplicate-safe: different keywords)
        (
            &["rank", "relevance", "score"],
            &["rank", "score", "relevance", "pagerank", "BM25"],
        ),
        // Memory / allocation
        (
            &["memory", "heap", "allocation", "leak"],
            &["Box<", "Vec::with_capacity", "allocate", "Arena"],
        ),
        // Graceful shutdown
        (
            &["shutdown", "graceful", "drain", "stop"],
            &[
                "shutdown",
                "graceful_shutdown",
                "drain",
                "CancellationToken",
            ],
        ),
        // Feature flags
        (
            &["feature flag", "toggle", "rollout", "a/b"],
            &["feature_flag", "toggle", "rollout", "FeatureStore"],
        ),
    ];

    for (keywords, code_patterns) in concept_map {
        if keywords.iter().any(|kw| query_lower.contains(kw)) {
            patterns.extend(code_patterns.iter().map(|p| p.to_string()));
        }
    }

    patterns
}

/// Expand a query with domain-specific code synonyms.
///
/// Appends extra terms that bridge vocabulary gaps between how users describe
/// a concept and how it appears in source code.  Returns `None` when no synonyms
/// match (so the caller can cheaply skip adding a redundant reformulation).
///
/// Example: `"dead code"` → `Some("dead code orphan zero in-degree find_orphans")`
pub(crate) fn expand_synonyms(query: &str) -> Option<String> {
    let query_lower = query.to_lowercase();
    let mut extra_terms = Vec::new();

    let synonym_map: &[(&[&str], &[&str])] = &[
        // Dead code / unused detection
        (
            &["dead code", "unused", "unreachable"],
            &["orphan", "zero in-degree", "find_orphans"],
        ),
        // Error handling
        (
            &["error handling", "exception"],
            &["Result", "Error", "anyhow"],
        ),
        // Dependency / import
        (
            &["dependency", "dependencies"],
            &["import", "require", "use"],
        ),
        // Callback / handler
        (
            &["callback", "handler", "listener"],
            &["on_", "handle_", "hook"],
        ),
        // Cache / memoize
        (
            &["cache", "caching", "memoize"],
            &["LruCache", "HashMap", "memo"],
        ),
        // Refactor / rename
        (
            &["refactor", "restructure"],
            &["rename", "extract", "inline"],
        ),
        // Performance / optimization
        (
            &["performance", "optimize", "speed"],
            &["benchmark", "perf", "fast"],
        ),
        // Authentication / authorization
        (
            &["authentication", "authorization", "auth"],
            &["login", "token", "session", "jwt"],
        ),
        // Serialization
        (
            &["serialize", "marshal"],
            &["serde", "Serialize", "Deserialize", "json"],
        ),
        // Similarity / matching
        (
            &["similar", "duplicate", "clone detection"],
            &["cosine", "similarity", "find_similar"],
        ),
        // Ranking / scoring
        (
            &["ranking", "scoring", "relevance"],
            &["pagerank", "boost", "score", "BM25"],
        ),
        // Documentation
        (
            &["documentation", "docs", "docstring"],
            &["doc comment", "///", "enrich_docs"],
        ),
        // Coverage / testing
        (
            &["coverage", "test coverage"],
            &["find_tests", "test_mapping", "#[test]"],
        ),
        // Complexity
        (
            &["complexity", "complex", "complicated"],
            &["cyclomatic", "get_complexity", "McCabe"],
        ),
        // Rate limiting / throttling
        (
            &["rate limit", "throttle", "throttling", "burst", "quota"],
            &[
                "RateLimiter",
                "throttle",
                "burst_capacity",
                "leaky_bucket",
                "TokenBucket",
            ],
        ),
        // Retry / backoff
        (
            &["retry", "backoff", "exponential backoff", "jitter"],
            &["max_attempts", "ExponentialBackoff", "retry_policy"],
        ),
        // Scheduling / cron
        (
            &["schedule", "cron", "periodic task", "job scheduler"],
            &["Scheduler", "cron_expression", "interval", "spawn_task"],
        ),
        // Logging / observability
        (
            &["logging", "log level", "structured log"],
            &["tracing", "log!", "info!", "warn!", "debug!", "span"],
        ),
        // HTTP client / networking
        (
            &["http client", "rest client", "network request"],
            &["reqwest", "hyper", "Client", "send", "Response"],
        ),
        // HTTP server
        (
            &["http server", "web server", "web framework"],
            &["axum", "actix", "warp", "Router", "listen"],
        ),
        // Validation / input checking
        (
            &["validate", "validation", "sanitize input", "check input"],
            &["validate", "validator", "is_valid", "ValidationError"],
        ),
        // Database / ORM
        (
            &["orm", "active record", "query builder"],
            &["diesel", "sqlx", "sea_orm", "QueryBuilder", "Model"],
        ),
        // Migration
        (
            &["migration", "schema migration", "alter table"],
            &["Migration", "up", "down", "refinery", "diesel_migrations"],
        ),
        // Encryption / crypto
        (
            &["encryption", "cryptography", "cipher", "encrypt", "decrypt"],
            &["ring", "AES", "RSA", "cipher", "nonce", "KeyPair"],
        ),
        // Hashing / integrity
        (
            &["hash", "checksum", "digest", "integrity"],
            &["sha256", "blake3", "xxhash", "Hash", "digest"],
        ),
        // Compression
        (
            &["compress", "compression", "gzip", "zstd"],
            &["flate2", "zstd", "GzEncoder", "compress", "decompress"],
        ),
        // Pagination / cursor
        (
            &["pagination", "paginate", "cursor pagination"],
            &["paginate", "cursor", "offset", "limit", "next_page"],
        ),
        // Filtering / query predicate
        (
            &["filter", "predicate", "where clause"],
            &[".filter(", "predicate", "where_clause", "QueryFilter"],
        ),
        // Middleware / interceptor
        (
            &["middleware", "interceptor", "layer"],
            &["Middleware", "Layer", "tower::Service", "from_fn"],
        ),
        // Routing
        (
            &["routing", "url dispatch", "path matching"],
            &["Router", "route", "match_path", "RouteTable"],
        ),
        // WebSocket
        (
            &["websocket", "ws connection", "real-time push"],
            &["WebSocket", "tungstenite", "on_message", "ws_upgrade"],
        ),
        // Queue / message broker
        (
            &["message queue", "job queue", "task queue"],
            &["Queue", "enqueue", "dequeue", "channel", "mpsc::Sender"],
        ),
        // Pub/sub / event bus
        (
            &["pub/sub", "event bus", "publish subscribe"],
            &["publish", "subscribe", "EventBus", "broadcast"],
        ),
        // Session / cookie
        (
            &["session management", "cookie", "csrf"],
            &["Session", "Cookie", "csrf_token", "store_session"],
        ),
        // CORS
        (
            &["cors", "cross-origin", "access control"],
            &["CorsLayer", "allow_origin", "Access-Control-Allow-Origin"],
        ),
        // Security / authorization policy
        (
            &["rbac", "acl", "permission", "policy", "role"],
            &["Role", "Permission", "Policy", "authorize", "check_access"],
        ),
        // CLI / argument parsing
        (
            &["cli", "command line", "argument parsing", "flags"],
            &["clap", "Arg", "ArgMatches", "subcommand", "parse_args"],
        ),
        // Plugin / extension system
        (
            &["plugin system", "extension point", "addon"],
            &["Plugin", "Extension", "register_hook", "PluginManager"],
        ),
        // Redaction / PII
        (
            &["redact", "masking", "pii", "anonymize"],
            &["redact", "mask", "scrub_pii", "Redactor", "anonymize"],
        ),
        // Prompt / LLM / AI
        (
            &["prompt", "llm", "language model", "completion", "chat"],
            &["ChatMessage", "prompt_template", "completion", "tokens"],
        ),
        // Monitoring / health check
        (
            &["monitoring", "health", "probe", "liveness"],
            &[
                "health_check",
                "liveness_probe",
                "metrics",
                "Counter",
                "Gauge",
            ],
        ),
        // Graceful shutdown / drain
        (
            &["graceful shutdown", "drain", "stop server"],
            &[
                "graceful_shutdown",
                "drain",
                "CancellationToken",
                "shutdown_signal",
            ],
        ),
        // Webhook
        (
            &["webhook", "callback url", "outbound notification"],
            &["webhook", "hmac_verify", "on_event", "WebhookPayload"],
        ),
        // Feature flags / toggles
        (
            &["feature flag", "feature toggle", "rollout", "a/b test"],
            &[
                "feature_flag",
                "toggle",
                "rollout_percentage",
                "FeatureStore",
            ],
        ),
        // Dependency injection
        (
            &[
                "dependency injection",
                "di container",
                "inversion of control",
            ],
            &["inject", "Container", "Provider", "register", "resolve"],
        ),
        // Event sourcing / CQRS
        (
            &["event sourcing", "cqrs", "command", "aggregate"],
            &["Event", "Command", "Aggregate", "event_store", "apply"],
        ),
        // State machine / FSM
        (
            &["state machine", "fsm", "state transition"],
            &["State", "Transition", "StateMachine", "on_enter", "on_exit"],
        ),
        // Template engine
        (
            &["template engine", "render template", "html template"],
            &[
                "Tera",
                "Handlebars",
                "minijinja",
                "render",
                "template_engine",
            ],
        ),
        // Memory management
        (
            &["memory leak", "memory management", "heap allocation"],
            &["Box<", "Arc<", "Vec::with_capacity", "allocate", "Arena"],
        ),
        // Concurrency primitives
        (
            &["mutex", "rwlock", "semaphore", "barrier"],
            &["Mutex", "RwLock", "Semaphore", "Arc<Mutex<", "parking_lot"],
        ),
        // Channel / actor
        (
            &["actor", "message passing", "channel"],
            &["mpsc", "oneshot", "broadcast", "Actor", "Mailbox"],
        ),
        // Sorting algorithms
        (
            &["sort algorithm", "merge sort", "quicksort"],
            &["sort_unstable", "merge_sort", "quicksort", "sort_by_key"],
        ),
        // Graph algorithms
        (
            &["bfs", "dfs", "shortest path", "dijkstra"],
            &["BFS", "DFS", "dijkstra", "shortest_path", "petgraph"],
        ),
        // Cold start / lazy init
        (
            &["lazy", "lazy init", "cold start", "on demand"],
            &["OnceLock", "Lazy", "lazy_static", "get_or_init"],
        ),
    ];

    for (triggers, expansions) in synonym_map {
        if triggers.iter().any(|t| query_lower.contains(t)) {
            for exp in expansions.iter() {
                if !query_lower.contains(&exp.to_lowercase()) {
                    extra_terms.push(exp.to_string());
                }
            }
        }
    }

    if extra_terms.is_empty() {
        None
    } else {
        Some(format!("{} {}", query, extra_terms.join(" ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_synonyms_rate_limiting() {
        let result = expand_synonyms("rate limit and throttling");
        assert!(result.is_some());
        let expanded = result.unwrap();
        assert!(expanded.contains("RateLimiter") || expanded.contains("throttle"));
    }

    #[test]
    fn expand_synonyms_no_match() {
        // A query with no synonym triggers should return None.
        let result = expand_synonyms("FiberNode abort callers");
        assert!(result.is_none());
    }

    #[test]
    fn reformulate_to_code_rate_limiting() {
        let patterns = reformulate_to_code("rate limit and throttling");
        assert!(!patterns.is_empty());
        assert!(
            patterns
                .iter()
                .any(|p| p.contains("RateLimiter") || p.contains("throttle"))
        );
    }

    #[test]
    fn reformulate_to_code_sorting() {
        let patterns = reformulate_to_code("how to sort a list efficiently");
        assert!(patterns.iter().any(|p| p.contains("sort")));
    }
}
