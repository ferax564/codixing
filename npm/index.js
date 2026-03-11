#!/usr/bin/env node
"use strict";

const { spawn } = require("child_process");
const { existsSync, mkdirSync, createWriteStream, chmodSync, unlinkSync } = require("fs");
const { join } = require("path");
const https = require("https");

const VERSION = require("./package.json").version;
const BIN_DIR = join(__dirname, "bin");
const BINARY_NAME = "codixing-mcp";
const BINARY_PATH = join(BIN_DIR, BINARY_NAME);

const PLATFORM_MAP = {
  "darwin-arm64": "codixing-mcp-macos-aarch64",
  "darwin-x64": "codixing-mcp-macos-x86_64",
  "linux-x64": "codixing-mcp-linux-x86_64",
  "linux-arm64": "codixing-mcp-linux-aarch64",
};

function getDownloadUrl() {
  const key = `${process.platform}-${process.arch}`;
  const artifact = PLATFORM_MAP[key];
  if (!artifact) {
    console.error(`codixing-mcp: unsupported platform ${key}`);
    console.error(`Supported: ${Object.keys(PLATFORM_MAP).join(", ")}`);
    process.exit(1);
  }
  return `https://github.com/ferax564/codixing/releases/download/v${VERSION}/${artifact}`;
}

function download(url, dest) {
  return new Promise((resolve, reject) => {
    const follow = (url, redirects) => {
      if (redirects > 5) return reject(new Error("Too many redirects"));
      https.get(url, { headers: { "User-Agent": "codixing-mcp-npm" } }, (res) => {
        if (res.statusCode === 302 || res.statusCode === 301) {
          return follow(res.headers.location, redirects + 1);
        }
        if (res.statusCode !== 200) {
          return reject(new Error(`HTTP ${res.statusCode} downloading ${url}`));
        }
        const file = createWriteStream(dest);
        res.pipe(file);
        file.on("finish", () => { file.close(resolve); });
        file.on("error", (err) => { unlinkSync(dest); reject(err); });
      }).on("error", reject);
    };
    follow(url, 0);
  });
}

async function install() {
  if (existsSync(BINARY_PATH)) {
    return;
  }
  mkdirSync(BIN_DIR, { recursive: true });
  const url = getDownloadUrl();
  process.stderr.write(`codixing-mcp: downloading v${VERSION} from ${url}\n`);
  try {
    await download(url, BINARY_PATH);
    chmodSync(BINARY_PATH, 0o755);
    process.stderr.write(`codixing-mcp: installed successfully\n`);
  } catch (err) {
    process.stderr.write(`codixing-mcp: download failed: ${err.message}\n`);
    process.stderr.write(`codixing-mcp: you can manually download from https://github.com/ferax564/codixing/releases\n`);
    if (existsSync(BINARY_PATH)) unlinkSync(BINARY_PATH);
    process.exit(1);
  }
}

async function main() {
  if (process.argv.includes("--install")) {
    await install();
    return;
  }

  if (!existsSync(BINARY_PATH)) {
    await install();
  }

  const child = spawn(BINARY_PATH, process.argv.slice(2), {
    stdio: "inherit",
    env: process.env,
  });
  child.on("exit", (code) => process.exit(code ?? 0));
  child.on("error", (err) => {
    console.error(`codixing-mcp: failed to start: ${err.message}`);
    process.exit(1);
  });
}

main();
