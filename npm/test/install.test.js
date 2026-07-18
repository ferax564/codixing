"use strict";

const assert = require("node:assert/strict");
const { createHash } = require("node:crypto");
const { EventEmitter } = require("node:events");
const { promises: fs, readFileSync } = require("node:fs");
const http = require("node:http");
const os = require("node:os");
const path = require("node:path");
const test = require("node:test");

const {
  INTEGRITY_RECHECK_MS,
  MAX_BINARY_BYTES,
  MAX_METADATA_BYTES,
  PLATFORM_MAP,
  install,
  main,
  replaceFileAtomically,
  sha256File,
  validateExistingBinary,
  wireChildProcess,
} = require("../index.js");

const ARTIFACT = "codixing-mcp-linux-x86_64";
const VERSION = "9.9.9-test";

function sha256(value) {
  return createHash("sha256").update(value).digest("hex");
}

function nodeHttpRequest(url, options, callback) {
  return http.get(url, options, callback);
}

async function withServer(handler, run) {
  const server = http.createServer(handler);
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  const baseUrl = `http://127.0.0.1:${address.port}`;
  try {
    return await run(baseUrl);
  } finally {
    server.closeAllConnections?.();
    await new Promise((resolve) => server.close(resolve));
  }
}

async function withInstallPaths(run) {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "codixing-npm-test-"));
  const binaryPath = path.join(root, "bin", "codixing-mcp");
  const metadataPath = `${binaryPath}.metadata.json`;
  try {
    return await run({ root, binaryPath, metadataPath });
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
}

function installOptions(baseUrl, paths, overrides = {}) {
  return {
    platform: "linux",
    arch: "x64",
    version: VERSION,
    releaseBaseUrl: baseUrl,
    request: nodeHttpRequest,
    timeoutMs: 250,
    log: () => {},
    ...paths,
    ...overrides,
  };
}

test("advertises only platforms produced by the release matrix", () => {
  assert.equal(MAX_BINARY_BYTES, 256 * 1024 * 1024);
  assert.deepEqual(Object.keys(PLATFORM_MAP).sort(), [
    "darwin-arm64",
    "linux-x64",
    "win32-x64",
  ]);

  const workflow = readFileSync(
    path.join(__dirname, "..", "..", ".github", "workflows", "ci.yml"),
    "utf8",
  );
  const releaseStart = workflow.indexOf("\n  release-build:\n");
  assert.notEqual(releaseStart, -1, "release-build job is missing");
  const remaining = workflow.slice(releaseStart + 1);
  const nextJob = remaining.slice(1).search(/\n  [a-zA-Z0-9_-]+:\n/);
  const releaseJob = nextJob === -1 ? remaining : remaining.slice(0, nextJob + 1);
  const releaseSuffixes = [...releaseJob.matchAll(/artifact_suffix:\s*(\S+)/g)]
    .map((match) => match[1])
    .sort();
  const advertisedSuffixes = Object.values(PLATFORM_MAP)
    .map((artifact) => artifact.replace(/^codixing-mcp-/, "").replace(/\.exe$/, ""))
    .sort();
  assert.deepEqual(advertisedSuffixes, releaseSuffixes);
});

test("downloads a checksum-verified binary and records its version", async () => {
  const binary = Buffer.from("verified codixing binary");
  await withInstallPaths(async (paths) => {
    await withServer((req, res) => {
      if (req.url === "/SHA256SUMS") {
        res.end(`${sha256(binary)}  ${ARTIFACT}\n`);
      } else if (req.url === `/${ARTIFACT}`) {
        res.setHeader("Content-Length", binary.length);
        res.end(binary);
      } else {
        res.writeHead(404).end();
      }
    }, async (baseUrl) => {
      const result = await install(installOptions(baseUrl, paths));
      assert.equal(result.installed, true);
      assert.deepEqual(await fs.readFile(paths.binaryPath), binary);
      assert.equal(await sha256File(paths.binaryPath), sha256(binary));
      const metadata = JSON.parse(await fs.readFile(paths.metadataPath, "utf8"));
      assert.equal(metadata.schemaVersion, 2);
      assert.equal(metadata.version, VERSION);
      assert.equal(metadata.artifact, ARTIFACT);
      assert.equal(metadata.sha256, sha256(binary));
      assert.equal(metadata.size, binary.length);
      assert.ok(Number.isFinite(metadata.mtimeMs));
      assert.ok(Number.isFinite(metadata.ctimeMs));
      assert.ok(Number.isFinite(metadata.verifiedAtMs));
      assert.ok((await fs.stat(paths.binaryPath)).mode & 0o111);

      let hashCalls = 0;
      assert.equal(
        await validateExistingBinary({
          ...paths,
          version: VERSION,
          artifact: ARTIFACT,
          isWindows: false,
          nowMs: metadata.verifiedAtMs + 1,
          hashFile: async () => {
            hashCalls += 1;
            throw new Error("fresh stat metadata must avoid a full binary hash");
          },
        }),
        true,
      );
      assert.equal(hashCalls, 0);

      assert.equal(
        await validateExistingBinary({
          ...paths,
          version: VERSION,
          artifact: ARTIFACT,
          isWindows: false,
          nowMs: metadata.verifiedAtMs + INTEGRITY_RECHECK_MS + 1,
          hashFile: async (file, maxBytes) => {
            hashCalls += 1;
            return sha256File(file, maxBytes);
          },
        }),
        true,
      );
      assert.equal(hashCalls, 1, "periodic validation must rehash the binary");
    });
  });
});

test("a valid version-bound binary starts without network access", async () => {
  const binary = Buffer.from("already installed");
  await withInstallPaths(async (paths) => {
    await fs.mkdir(path.dirname(paths.binaryPath), { recursive: true });
    await fs.writeFile(paths.binaryPath, binary);
    await fs.writeFile(
      paths.metadataPath,
      JSON.stringify({
        version: VERSION,
        artifact: ARTIFACT,
        sha256: sha256(binary),
      }),
    );

    const result = await install(
      installOptions("http://network-must-not-be-used.invalid", paths, {
        request: () => {
          throw new Error("unexpected network request");
        },
      }),
    );
    assert.equal(result.installed, false);
  });
});

test("rejects oversized cache metadata before hashing the executable", async () => {
  await withInstallPaths(async (paths) => {
    await fs.mkdir(path.dirname(paths.binaryPath), { recursive: true });
    await fs.writeFile(paths.binaryPath, "cached executable");
    await fs.writeFile(paths.metadataPath, "x".repeat(MAX_METADATA_BYTES + 1));
    let hashCalls = 0;
    const valid = await validateExistingBinary({
      ...paths,
      version: VERSION,
      artifact: ARTIFACT,
      isWindows: true,
      hashFile: async () => {
        hashCalls += 1;
        return sha256("cached executable");
      },
    });
    assert.equal(valid, false);
    assert.equal(hashCalls, 0);
  });
});

test("follows relative redirects for release assets", async () => {
  const binary = Buffer.from("redirected binary");
  await withInstallPaths(async (paths) => {
    await withServer((req, res) => {
      if (req.url === "/SHA256SUMS") {
        res.writeHead(302, { Location: "/checksums" }).end();
      } else if (req.url === "/checksums") {
        res.end(`${sha256(binary)} *${ARTIFACT}\n`);
      } else if (req.url === `/${ARTIFACT}`) {
        res.writeHead(307, { Location: "/binary" }).end();
      } else if (req.url === "/binary") {
        res.end(binary);
      } else {
        res.writeHead(404).end();
      }
    }, async (baseUrl) => {
      await install(installOptions(baseUrl, paths));
      assert.deepEqual(await fs.readFile(paths.binaryPath), binary);
    });
  });
});

test("checksum failures preserve the previous binary and remove temp files", async () => {
  const oldBinary = Buffer.from("previous binary");
  const downloaded = Buffer.from("corrupt download");
  await withInstallPaths(async (paths) => {
    await fs.mkdir(path.dirname(paths.binaryPath), { recursive: true });
    await fs.writeFile(paths.binaryPath, oldBinary);

    await withServer((req, res) => {
      if (req.url === "/SHA256SUMS") {
        res.end(`${sha256("expected binary")}  ${ARTIFACT}\n`);
      } else if (req.url === `/${ARTIFACT}`) {
        res.end(downloaded);
      } else {
        res.writeHead(404).end();
      }
    }, async (baseUrl) => {
      await assert.rejects(
        install(installOptions(baseUrl, paths)),
        /checksum mismatch/,
      );
    });

    assert.deepEqual(await fs.readFile(paths.binaryPath), oldBinary);
    const entries = await fs.readdir(path.dirname(paths.binaryPath));
    assert.equal(entries.some((entry) => entry.includes(".tmp-")), false);
  });
});

test("rejects a checksum-valid empty artifact before publication", async () => {
  const oldBinary = Buffer.from("previous binary");
  await withInstallPaths(async (paths) => {
    await fs.mkdir(path.dirname(paths.binaryPath), { recursive: true });
    await fs.writeFile(paths.binaryPath, oldBinary);

    await withServer((req, res) => {
      if (req.url === "/SHA256SUMS") {
        res.end(`${sha256(Buffer.alloc(0))}  ${ARTIFACT}\n`);
      } else if (req.url === `/${ARTIFACT}`) {
        res.setHeader("Content-Length", 0);
        res.end();
      } else {
        res.writeHead(404).end();
      }
    }, async (baseUrl) => {
      await assert.rejects(
        install(installOptions(baseUrl, paths)),
        /binary download was empty/,
      );
    });

    assert.deepEqual(await fs.readFile(paths.binaryPath), oldBinary);
    const entries = await fs.readdir(path.dirname(paths.binaryPath));
    assert.equal(entries.some((entry) => entry.includes(".tmp-")), false);
  });
});

test("Windows replacement restores the previous binary when publication fails", async () => {
  let destinationPresent = true;
  let backupPresent = false;
  let publishAttempts = 0;
  let backupMoves = 0;

  const fakeFs = {
    async rename(from, to) {
      if (from === "temp" && to === "destination") {
        publishAttempts += 1;
        const error = new Error(
          publishAttempts === 1 ? "destination exists" : "publication failed",
        );
        error.code = publishAttempts === 1 ? "EEXIST" : "EIO";
        throw error;
      }
      if (from === "destination" && to.includes(".old-")) {
        backupMoves += 1;
        assert.equal(destinationPresent, true);
        destinationPresent = false;
        backupPresent = true;
        return;
      }
      if (from.includes(".old-") && to === "destination") {
        assert.equal(backupPresent, true);
        backupPresent = false;
        destinationPresent = true;
        return;
      }
      assert.fail(`unexpected rename: ${from} -> ${to}`);
    },
    async rm() {
      assert.fail("backup must not be removed after failed publication");
    },
  };

  await assert.rejects(
    replaceFileAtomically("temp", "destination", true, fakeFs),
    /publication failed/,
  );
  assert.equal(backupMoves, 1, "the existing binary should move only once");
  assert.equal(destinationPresent, true, "the previous binary must be restored");
  assert.equal(backupPresent, false);
});

test("Windows backup cleanup failure does not invalidate a successful publication", async () => {
  let destination = "old";
  let backup;
  let publishAttempts = 0;
  let cleanupAttempts = 0;

  const fakeFs = {
    async rename(from, to) {
      if (from === "temp" && to === "destination") {
        publishAttempts += 1;
        if (publishAttempts === 1) {
          const error = new Error("destination exists");
          error.code = "EEXIST";
          throw error;
        }
        destination = "new";
        return;
      }
      if (from === "destination" && to.includes(".old-")) {
        backup = destination;
        destination = undefined;
        return;
      }
      assert.fail(`unexpected rename: ${from} -> ${to}`);
    },
    async rm() {
      cleanupAttempts += 1;
      const error = new Error("antivirus still holds the old executable");
      error.code = "EBUSY";
      throw error;
    },
  };

  await replaceFileAtomically("temp", "destination", true, fakeFs);
  assert.equal(destination, "new");
  assert.equal(backup, "old");
  assert.equal(cleanupAttempts, 1);
});

test("rejects oversized Content-Length before writing a temporary file", async () => {
  const binary = Buffer.from("too large");
  await withInstallPaths(async (paths) => {
    await withServer((req, res) => {
      if (req.url === "/SHA256SUMS") {
        res.end(`${sha256(binary)}  ${ARTIFACT}\n`);
      } else if (req.url === `/${ARTIFACT}`) {
        res.writeHead(200, { "Content-Length": 1024 });
        res.end(binary);
      } else {
        res.writeHead(404).end();
      }
    }, async (baseUrl) => {
      await assert.rejects(
        install(installOptions(baseUrl, paths, { maxBinaryBytes: 16 })),
        /exceeds 16 byte download limit.*Content-Length: 1024/,
      );
    });

    const entries = await fs.readdir(path.dirname(paths.binaryPath));
    assert.equal(entries.some((entry) => entry.includes(".tmp-")), false);
  });
});

test("aborts an oversized stream without Content-Length and preserves the old binary", async () => {
  const oldBinary = Buffer.from("previous binary");
  const downloaded = Buffer.alloc(64, "x");
  await withInstallPaths(async (paths) => {
    await fs.mkdir(path.dirname(paths.binaryPath), { recursive: true });
    await fs.writeFile(paths.binaryPath, oldBinary);

    await withServer((req, res) => {
      if (req.url === "/SHA256SUMS") {
        res.end(`${sha256(downloaded)}  ${ARTIFACT}\n`);
      } else if (req.url === `/${ARTIFACT}`) {
        res.writeHead(200, { "Transfer-Encoding": "chunked" });
        res.write(downloaded.subarray(0, 32));
        res.end(downloaded.subarray(32));
      } else {
        res.writeHead(404).end();
      }
    }, async (baseUrl) => {
      await assert.rejects(
        install(installOptions(baseUrl, paths, { maxBinaryBytes: 16 })),
        /exceeds 16 byte download limit while streaming/,
      );
    });

    assert.deepEqual(await fs.readFile(paths.binaryPath), oldBinary);
    const entries = await fs.readdir(path.dirname(paths.binaryPath));
    assert.equal(entries.some((entry) => entry.includes(".tmp-")), false);
  });
});

test("aborted downloads clean up unique temporary files", async () => {
  const expected = Buffer.from("complete binary");
  const controller = new AbortController();
  await withInstallPaths(async (paths) => {
    await withServer((req, res) => {
      if (req.url === "/SHA256SUMS") {
        res.end(`${sha256(expected)}  ${ARTIFACT}\n`);
      } else if (req.url === `/${ARTIFACT}`) {
        res.write("partial");
      } else {
        res.writeHead(404).end();
      }
    }, async (baseUrl) => {
      const pending = install(
        installOptions(baseUrl, paths, { signal: controller.signal }),
      );
      setTimeout(() => controller.abort(), 25);
      await assert.rejects(pending, /aborted/);
    });

    const entries = await fs.readdir(path.dirname(paths.binaryPath));
    assert.equal(entries.some((entry) => entry.includes(".tmp-")), false);
  });
});

test("stalled downloads time out and clean up temporary files", async () => {
  const expected = Buffer.from("complete binary");
  await withInstallPaths(async (paths) => {
    await withServer((req, res) => {
      if (req.url === "/SHA256SUMS") {
        res.end(`${sha256(expected)}  ${ARTIFACT}\n`);
      } else if (req.url === `/${ARTIFACT}`) {
        res.write("partial");
      } else {
        res.writeHead(404).end();
      }
    }, async (baseUrl) => {
      await assert.rejects(
        install(installOptions(baseUrl, paths, { timeoutMs: 25 })),
        /timed out/,
      );
    });

    const entries = await fs.readdir(path.dirname(paths.binaryPath));
    assert.equal(entries.some((entry) => entry.includes(".tmp-")), false);
  });
});

test("one timeout budget covers redirects and a slow-drip response body", async () => {
  const binary = Buffer.from(
    "a body that keeps making progress but never finishes in time",
  );
  await withInstallPaths(async (paths) => {
    await withServer((req, res) => {
      if (req.url === "/SHA256SUMS") {
        res.end(`${sha256(binary)}  ${ARTIFACT}\n`);
        return;
      }
      if (req.url === `/${ARTIFACT}`) {
        setTimeout(() => {
          if (!res.destroyed) {
            res.writeHead(302, { Location: "/slow-binary" }).end();
          }
        }, 15);
        return;
      }
      if (req.url === "/slow-binary") {
        setTimeout(() => {
          if (res.destroyed) return;
          res.writeHead(200, { "Transfer-Encoding": "chunked" });
          let offset = 0;
          const timer = setInterval(() => {
            if (res.destroyed) {
              clearInterval(timer);
              return;
            }
            res.write(binary.subarray(offset, offset + 1));
            offset += 1;
            if (offset === binary.length) {
              clearInterval(timer);
              res.end();
            }
          }, 10);
          res.once("close", () => clearInterval(timer));
        }, 15);
        return;
      }
      res.writeHead(404).end();
    }, async (baseUrl) => {
      const startedAt = Date.now();
      await assert.rejects(
        install(installOptions(baseUrl, paths, { timeoutMs: 60 })),
        /timed out after 60ms/,
      );
      assert.ok(
        Date.now() - startedAt < 250,
        "slow progress must not reset the end-to-end timeout",
      );
    });

    const entries = await fs.readdir(path.dirname(paths.binaryPath));
    assert.equal(entries.some((entry) => entry.includes(".tmp-")), false);
  });
});

test("requests time out even before response headers arrive", async () => {
  const expected = Buffer.from("complete binary");
  await withInstallPaths(async (paths) => {
    await withServer((req, res) => {
      if (req.url === "/SHA256SUMS") {
        res.end(`${sha256(expected)}  ${ARTIFACT}\n`);
      } else if (req.url !== `/${ARTIFACT}`) {
        res.writeHead(404).end();
      }
      // Intentionally leave the artifact request unanswered.
    }, async (baseUrl) => {
      await assert.rejects(
        install(installOptions(baseUrl, paths, { timeoutMs: 25 })),
        /timed out/,
      );
    });
  });
});

test("rejects redirect loops at the configured limit", async () => {
  await withInstallPaths(async (paths) => {
    await withServer((_req, res) => {
      res.writeHead(302, { Location: "/again" }).end();
    }, async (baseUrl) => {
      await assert.rejects(
        install(installOptions(baseUrl, paths, { maxRedirects: 2 })),
        /too many redirects/,
      );
    });
  });
});

test("forwards parent signals and preserves child signal termination", () => {
  const child = new EventEmitter();
  child.killed = false;
  const childSignals = [];
  child.kill = (signal) => {
    childSignals.push(signal);
    return true;
  };

  const processRef = new EventEmitter();
  processRef.pid = 4242;
  const selfSignals = [];
  const exitCodes = [];
  processRef.kill = (pid, signal) => selfSignals.push([pid, signal]);
  processRef.exit = (code) => exitCodes.push(code);

  wireChildProcess(child, processRef, () => {});
  processRef.emit("SIGTERM");
  assert.deepEqual(childSignals, ["SIGTERM"]);

  child.emit("exit", null, "SIGTERM");
  assert.deepEqual(selfSignals, [[4242, "SIGTERM"]]);
  assert.deepEqual(exitCodes, []);
  assert.equal(processRef.listenerCount("SIGTERM"), 0);
});

test("signals abort installation cleanly before a child is spawned", async () => {
  const processRef = new EventEmitter();
  processRef.pid = 4242;
  processRef.argv = ["node", "index.js"];
  processRef.env = {};
  const selfSignals = [];
  processRef.kill = (pid, signal) => selfSignals.push([pid, signal]);

  let downloadAborted = false;
  let spawnCalls = 0;
  const pending = main({
    processRef,
    installFn: ({ signal }) =>
      new Promise((_resolve, reject) => {
        signal.addEventListener(
          "abort",
          () => {
            downloadAborted = true;
            reject(signal.reason);
          },
          { once: true },
        );
      }),
    spawnFn: () => {
      spawnCalls += 1;
      throw new Error("child must not be spawned after an install signal");
    },
  });

  assert.equal(processRef.listenerCount("SIGTERM"), 1);
  processRef.emit("SIGTERM");
  await pending;

  assert.equal(downloadAborted, true);
  assert.equal(spawnCalls, 0);
  assert.deepEqual(selfSignals, [[4242, "SIGTERM"]]);
  assert.equal(processRef.listenerCount("SIGINT"), 0);
  assert.equal(processRef.listenerCount("SIGTERM"), 0);
  assert.equal(processRef.listenerCount("SIGHUP"), 0);
});
