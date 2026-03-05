//! Shared test helpers for integration tests.
#![allow(dead_code)]

use std::fs;
use std::path::Path;

/// Set up a project with explicit import statements for graph testing.
///
/// Structure:
/// ```text
/// src/
///   main.rs     -- uses crate::engine and crate::parser
///   engine.rs   -- uses crate::parser
///   parser.rs   -- no imports
///   index.ts    -- imports ./foo
///   foo.ts      -- no imports
///   utils.py    -- from . import helpers
///   helpers.py  -- no imports
/// ```
pub fn setup_project_with_imports(root: &Path) {
    let src = root.join("src");
    fs::create_dir_all(&src).expect("failed to create src directory");

    fs::write(
        src.join("main.rs"),
        r#"use crate::engine::Engine;
use crate::parser::Parser;

fn main() {
    let _e = Engine::new();
}
"#,
    )
    .expect("failed to write main.rs");

    fs::write(
        src.join("engine.rs"),
        r#"use crate::parser::Parser;

pub struct Engine;

impl Engine {
    pub fn new() -> Self { Self }
}
"#,
    )
    .expect("failed to write engine.rs");

    fs::write(
        src.join("parser.rs"),
        r#"pub struct Parser;

impl Parser {
    pub fn new() -> Self { Self }
}
"#,
    )
    .expect("failed to write parser.rs");

    fs::write(
        src.join("index.ts"),
        r#"import { Foo } from "./foo";

export class App {
    run(): void {}
}
"#,
    )
    .expect("failed to write index.ts");

    fs::write(
        src.join("foo.ts"),
        r#"export class Foo {
    name: string = "foo";
}
"#,
    )
    .expect("failed to write foo.ts");

    fs::write(
        src.join("utils.py"),
        r#"from . import helpers

def run():
    pass
"#,
    )
    .expect("failed to write utils.py");

    fs::write(
        src.join("helpers.py"),
        r#"def help():
    return "help"
"#,
    )
    .expect("failed to write helpers.py");
}

/// Set up a multi-language project with Rust, Python, TypeScript, and Go files.
///
/// Creates the following structure under `root`:
/// ```text
/// src/
///   main.rs    -- main(), add(), Config struct
///   lib.rs     -- helper(), Processor trait
///   utils.py   -- parse_config(), Validator class
///   index.ts   -- App class, createApp()
///   server.go  -- Server struct, HandleRequest function
/// ```
pub fn setup_multi_language_project(root: &Path) {
    let src = root.join("src");
    fs::create_dir_all(&src).expect("failed to create src directory");

    // ---------- Rust: main.rs ----------
    fs::write(
        src.join("main.rs"),
        r#"/// Entry point for the application.
fn main() {
    println!("Hello, world!");
}

/// Add two numbers together.
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

/// Configuration for the application.
pub struct Config {
    pub verbose: bool,
    pub threads: usize,
}
"#,
    )
    .expect("failed to write main.rs");

    // ---------- Rust: lib.rs ----------
    fs::write(
        src.join("lib.rs"),
        r#"/// A helper function that returns a greeting.
pub fn helper() -> String {
    "help".to_string()
}

/// A processing trait for transforming input.
pub trait Processor {
    fn process(&self, input: &str) -> String;
}
"#,
    )
    .expect("failed to write lib.rs");

    // ---------- Python: utils.py ----------
    fs::write(
        src.join("utils.py"),
        r#""""Utility module for configuration parsing."""

def parse_config(path: str) -> dict:
    """Parse a configuration file and return a dict."""
    with open(path) as f:
        return {}

class Validator:
    """Validates input data against a schema."""

    def __init__(self, schema: dict):
        self.schema = schema

    def validate(self, data: dict) -> bool:
        """Check if data conforms to the schema."""
        return True
"#,
    )
    .expect("failed to write utils.py");

    // ---------- TypeScript: index.ts ----------
    fs::write(
        src.join("index.ts"),
        r#"/**
 * Main application class.
 */
export class App {
    private name: string;

    constructor(name: string) {
        this.name = name;
    }

    run(): void {
        console.log(`Running ${this.name}`);
    }
}

/**
 * Factory function to create an App instance.
 */
export function createApp(name: string): App {
    return new App(name);
}
"#,
    )
    .expect("failed to write index.ts");

    // ---------- Go: server.go ----------
    fs::write(
        src.join("server.go"),
        r#"package main

import "net/http"

// Server holds the HTTP server state.
type Server struct {
	Addr string
	Port int
}

// HandleRequest processes an incoming HTTP request.
func HandleRequest(w http.ResponseWriter, r *http.Request) {
	w.WriteHeader(http.StatusOK)
}
"#,
    )
    .expect("failed to write server.go");
}
