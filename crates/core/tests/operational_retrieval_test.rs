//! Eval cases for operational/security concept queries (issue #104).
//!
//! These queries describe the *task* (auditing security headers, verifying
//! installer checksums, finding the webhook secret) rather than the file
//! name. They are the realistic shape of agent queries during a launch
//! readiness review on a mixed Go/shell repo, and are exactly the queries
//! the issue calls out as currently weak.
//!
//! The acceptance bar from the issue: each expected file must appear in
//! the top **3** results under the default search strategy (BM25 here so
//! the test stays self-contained — no ONNX, no embeddings, no network).
//!
//! When this test fails today, it should NOT be deleted. Mark new gaps
//! with `#[ignore = "tracked: ..."]` so the eval surface keeps growing
//! and the v0.42 boost work has a measurable target.

use std::collections::HashSet;

use tempfile::TempDir;

use codixing_core::config::EmbeddingConfig;
use codixing_core::{Engine, IndexConfig, SearchQuery, Strategy};

/// Build a synthetic mixed Go/shell repo that mirrors the EZKeel-shaped
/// project from issue #104: Go HTTP server entrypoint, install script,
/// webhook handler, SSH helper, rate-limiting middleware.
fn build_operational_index() -> (Engine, TempDir) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    let files: &[(&str, &str)] = &[
        // -------- HTTP server entrypoint with security headers --------
        (
            "cmd/ezkeel-web/main.go",
            r#"
package main

// Wraps the HTTP server with browser security headers: HSTS, CSP,
// X-Frame-Options, X-Content-Type-Options. Run from `go run ./cmd/ezkeel-web`.
func wrapWithSecurityHeaders(next http.Handler) http.Handler {
    return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
        w.Header().Set("Strict-Transport-Security", "max-age=63072000")
        w.Header().Set("Content-Security-Policy", "default-src 'self'")
        w.Header().Set("X-Frame-Options", "DENY")
        w.Header().Set("X-Content-Type-Options", "nosniff")
        next.ServeHTTP(w, r)
    })
}

func main() {
    server := &http.Server{Addr: ":8080", Handler: wrapWithSecurityHeaders(router)}
    server.ListenAndServe()
}
"#,
        ),
        // -------- Installer with sha256 checksum verification --------
        (
            "cli/install.sh",
            r#"#!/bin/sh
# EZKeel installer.
#
# Downloads the release tarball and verifies its sha256 checksum against
# the provenance file before extracting. Aborts on signature mismatch.
set -eu

VERSION="${VERSION:-latest}"
URL="https://example.com/releases/ezkeel-${VERSION}.tar.gz"
SUM_URL="https://example.com/releases/ezkeel-${VERSION}.tar.gz.sha256"

curl -fsSL "$URL"     -o ezkeel.tar.gz
curl -fsSL "$SUM_URL" -o ezkeel.tar.gz.sha256

# Verify the release sha256 checksum.
echo "$(cat ezkeel.tar.gz.sha256)  ezkeel.tar.gz" | sha256sum -c -

tar -xzf ezkeel.tar.gz -C /usr/local/bin/
"#,
        ),
        // -------- Stripe webhook secret startup wiring --------
        (
            "internal/billing/webhook.go",
            r#"
package billing

// HandleStripeWebhook validates the Stripe-Signature header against
// the configured webhook endpoint secret before processing events.
func HandleStripeWebhook(secret string) http.HandlerFunc {
    return func(w http.ResponseWriter, r *http.Request) {
        sig := r.Header.Get("Stripe-Signature")
        if !verifyWebhookSignature(r.Body, sig, secret) {
            http.Error(w, "invalid stripe webhook signature", 401)
            return
        }
        // ...
    }
}
"#,
        ),
        // -------- SSH host key pinning --------
        (
            "internal/sshx/hostkeys.go",
            r#"
package sshx

// pinnedHostKeyCallback returns an ssh.HostKeyCallback that pins the
// remote host key fingerprint. Connections fail closed if the presented
// key does not match the pinned value.
func pinnedHostKeyCallback(expected string) ssh.HostKeyCallback {
    return func(hostname string, remote net.Addr, key ssh.PublicKey) error {
        actual := ssh.FingerprintSHA256(key)
        if actual != expected {
            return fmt.Errorf("host key pin mismatch: got %s, want %s", actual, expected)
        }
        return nil
    }
}
"#,
        ),
        // -------- Rate limiter middleware --------
        (
            "internal/web/middleware/ratelimit.go",
            r#"
package middleware

// rateLimit returns an HTTP middleware that throttles requests per
// remote address using a token bucket (burst=10, refill=1/s).
func rateLimit(next http.Handler) http.Handler {
    limiter := newTokenBucket(10, time.Second)
    return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
        if !limiter.Allow(r.RemoteAddr) {
            http.Error(w, "rate limit exceeded", 429)
            return
        }
        next.ServeHTTP(w, r)
    })
}
"#,
        ),
        // -------- Distractor: unrelated auth/session file that
        // pre-fix tended to dominate the security-headers query --------
        (
            "internal/auth/jwt.go",
            r#"
package auth

// JWT helpers. Has no opinion on browser security headers.
func ParseJWT(token string) (*Claims, error) {
    return jwt.Parse(token, keyFn)
}
"#,
        ),
    ];

    for (rel, content) in files {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir");
        std::fs::write(&path, content).expect("write file");
    }

    let mut config = IndexConfig::new(root);
    config.embedding = EmbeddingConfig {
        enabled: false,
        ..Default::default()
    };
    let engine = Engine::init(root, config).expect("engine init");
    (engine, tmp)
}

fn assert_recall_at_k(engine: &Engine, query: &str, expected_file: &str, k: usize) {
    let results = engine
        .search(SearchQuery {
            query: query.to_string(),
            limit: k,
            file_filter: None,
            strategy: Strategy::Instant,
            token_budget: None,
            queries: None,
            doc_filter: None,
        })
        .unwrap_or_default();
    let found: HashSet<&str> = results.iter().map(|r| r.file_path.as_str()).collect();
    assert!(
        found.contains(expected_file),
        "Recall@{k} FAIL: query={query:?} expected={expected_file} got={:?}",
        results
            .iter()
            .map(|r| r.file_path.as_str())
            .collect::<Vec<_>>()
    );
}

#[test]
fn operational_security_headers_finds_server_entrypoint() {
    let (engine, _tmp) = build_operational_index();
    assert_recall_at_k(
        &engine,
        "browser security headers HSTS CSP X-Frame-Options",
        "cmd/ezkeel-web/main.go",
        3,
    );
}

#[test]
fn operational_installer_checksum_finds_install_script() {
    let (engine, _tmp) = build_operational_index();
    assert_recall_at_k(
        &engine,
        "installer verify release checksum sha256",
        "cli/install.sh",
        3,
    );
}

#[test]
fn operational_stripe_webhook_finds_billing_handler() {
    let (engine, _tmp) = build_operational_index();
    assert_recall_at_k(
        &engine,
        "stripe webhook endpoint secret",
        "internal/billing/webhook.go",
        3,
    );
}

#[test]
fn operational_host_key_pinning_finds_ssh_helper() {
    let (engine, _tmp) = build_operational_index();
    assert_recall_at_k(
        &engine,
        "ssh host key pinning fingerprint",
        "internal/sshx/hostkeys.go",
        3,
    );
}

#[test]
fn operational_rate_limiting_finds_middleware() {
    let (engine, _tmp) = build_operational_index();
    assert_recall_at_k(
        &engine,
        "rate limiting middleware token bucket",
        "internal/web/middleware/ratelimit.go",
        3,
    );
}
