#!/usr/bin/env node
"use strict";

const { spawn } = require("child_process");
const { createHash, randomBytes } = require("crypto");
const {
  createReadStream,
  createWriteStream,
  promises: fs,
} = require("fs");
const http = require("http");
const https = require("https");
const { dirname, join } = require("path");
const { pipeline } = require("stream/promises");
const { Transform } = require("stream");

const VERSION = require("./package.json").version;
const BIN_DIR = join(__dirname, "bin");
const IS_WINDOWS = process.platform === "win32";
const BINARY_NAME = IS_WINDOWS ? "codixing-mcp.exe" : "codixing-mcp";
const BINARY_PATH = join(BIN_DIR, BINARY_NAME);
const METADATA_PATH = `${BINARY_PATH}.metadata.json`;
const RELEASE_BASE_URL = `https://github.com/ferax564/codixing/releases/download/v${VERSION}`;
// Release executables can be tens or hundreds of MiB. Keep the transfer
// bounded without requiring a >50-Mbit/s connection merely to finish before
// the default deadline.
const REQUEST_TIMEOUT_MS = 120_000;
const MAX_REDIRECTS = 5;
const MAX_MANIFEST_BYTES = 1024 * 1024;
const MAX_BINARY_BYTES = 256 * 1024 * 1024;
const MAX_METADATA_BYTES = 16 * 1024;
const METADATA_SCHEMA_VERSION = 2;
const INTEGRITY_RECHECK_MS = 7 * 24 * 60 * 60 * 1000;

// Keep this list exactly aligned with the release-build matrix in ci.yml.
const PLATFORM_MAP = Object.freeze({
  "darwin-arm64": "codixing-mcp-macos-aarch64",
  "linux-x64": "codixing-mcp-linux-x86_64",
  "win32-x64": "codixing-mcp-windows-x86_64.exe",
});

function getPlatformArtifact(platform = process.platform, arch = process.arch) {
  const key = `${platform}-${arch}`;
  const artifact = PLATFORM_MAP[key];
  if (!artifact) {
    throw new Error(
      `unsupported platform ${key}. Supported: ${Object.keys(PLATFORM_MAP).join(", ")}`,
    );
  }
  return artifact;
}

function defaultRequest(url, options, callback) {
  const protocol = new URL(url).protocol;
  if (protocol === "https:") {
    return https.get(url, options, callback);
  }
  if (protocol === "http:") {
    return http.get(url, options, callback);
  }
  throw new Error(`unsupported download protocol ${protocol}`);
}

function abortError(message = "download aborted") {
  const error = new Error(message);
  error.name = "AbortError";
  return error;
}

function requestResponse(
  url,
  {
    request = defaultRequest,
    timeoutMs = REQUEST_TIMEOUT_MS,
    maxRedirects = MAX_REDIRECTS,
    signal,
    deadline = Date.now() + timeoutMs,
  } = {},
  redirects = 0,
) {
  return new Promise((resolve, reject) => {
    if (signal?.aborted) {
      reject(abortError());
      return;
    }

    let req;
    let response;
    let settled = false;
    let deadlineTimeout;

    const cleanup = () => {
      clearTimeout(deadlineTimeout);
      signal?.removeEventListener("abort", onAbort);
    };
    const fail = (error) => {
      if (settled) return;
      settled = true;
      cleanup();
      reject(error);
    };
    const onAbort = () => {
      const error = abortError();
      if (response) response.destroy(error);
      if (req) req.destroy(error);
      fail(error);
    };

    signal?.addEventListener("abort", onAbort, { once: true });
    const remainingMs = deadline - Date.now();
    if (remainingMs <= 0) {
      fail(new Error(`download timed out after ${timeoutMs}ms`));
      return;
    }
    deadlineTimeout = setTimeout(() => {
      const error = new Error(`download timed out after ${timeoutMs}ms`);
      if (response) response.destroy(error);
      else if (req) req.destroy(error);
      fail(error);
    }, remainingMs);
    deadlineTimeout.unref?.();

    try {
      req = request(
        url,
        { headers: { "User-Agent": "codixing-mcp-npm" } },
        (res) => {
          response = res;
          const status = res.statusCode ?? 0;

          if ([301, 302, 303, 307, 308].includes(status)) {
            const location = res.headers.location;
            res.resume();
            if (!location) {
              fail(new Error(`HTTP ${status} redirect without a Location header`));
              return;
            }
            if (redirects >= maxRedirects) {
              fail(new Error(`too many redirects downloading ${url}`));
              return;
            }

            const nextUrl = new URL(location, url);
            const currentUrl = new URL(url);
            if (currentUrl.protocol === "https:" && nextUrl.protocol !== "https:") {
              fail(new Error("refusing to follow an HTTPS redirect to an insecure URL"));
              return;
            }

            settled = true;
            cleanup();
            resolve(
              requestResponse(
                nextUrl.toString(),
                { request, timeoutMs, maxRedirects, signal, deadline },
                redirects + 1,
              ),
            );
            return;
          }

          if (status !== 200) {
            res.resume();
            fail(new Error(`HTTP ${status} downloading ${url}`));
            return;
          }

          res.once("close", cleanup);
          settled = true;
          resolve(res);
        },
      );
    } catch (error) {
      fail(error);
      return;
    }

    req.once("error", fail);
  });
}

async function downloadText(url, options = {}) {
  const response = await requestResponse(url, options);
  const chunks = [];
  let size = 0;

  for await (const chunk of response) {
    size += chunk.length;
    if (size > MAX_MANIFEST_BYTES) {
      response.destroy();
      throw new Error(`checksum manifest exceeds ${MAX_MANIFEST_BYTES} bytes`);
    }
    chunks.push(chunk);
  }

  return Buffer.concat(chunks).toString("utf8");
}

function parseChecksumManifest(manifest, artifact) {
  for (const rawLine of manifest.split(/\r?\n/)) {
    const match = rawLine.match(/^([a-fA-F0-9]{64})\s+[*]?(.+)$/);
    if (match && match[2].trim() === artifact) {
      return match[1].toLowerCase();
    }
  }
  throw new Error(`SHA256SUMS does not contain ${artifact}`);
}

function uniqueSibling(path, label) {
  return `${path}.${label}-${process.pid}-${randomBytes(8).toString("hex")}`;
}

async function sha256File(path, maxBytes = Number.POSITIVE_INFINITY) {
  const hash = createHash("sha256");
  let bytes = 0;
  for await (const chunk of createReadStream(path)) {
    bytes += chunk.length;
    if (bytes > maxBytes) {
      throw new Error(`file exceeds ${maxBytes} byte verification limit`);
    }
    hash.update(chunk);
  }
  return hash.digest("hex");
}

async function readTextFileBounded(path, maxBytes) {
  const handle = await fs.open(path, "r");
  try {
    const stat = await handle.stat();
    if (!stat.isFile() || stat.size <= 0 || stat.size > maxBytes) {
      throw new Error(`metadata exceeds ${maxBytes} byte limit or is not a regular file`);
    }
    const buffer = Buffer.alloc(stat.size + 1);
    const { bytesRead } = await handle.read(buffer, 0, buffer.length, 0);
    if (bytesRead > maxBytes) {
      throw new Error(`metadata exceeds ${maxBytes} byte limit`);
    }
    return buffer.subarray(0, bytesRead).toString("utf8");
  } finally {
    await handle.close();
  }
}

async function validateExistingBinary({
  binaryPath,
  metadataPath,
  version,
  artifact,
  isWindows,
  maxBinaryBytes = MAX_BINARY_BYTES,
  maxMetadataBytes = MAX_METADATA_BYTES,
  integrityRecheckMs = INTEGRITY_RECHECK_MS,
  nowMs = Date.now(),
  hashFile = sha256File,
}) {
  try {
    const [stat, metadataText] = await Promise.all([
      fs.stat(binaryPath),
      readTextFileBounded(metadataPath, maxMetadataBytes),
    ]);
    if (
      !stat.isFile() ||
      stat.size === 0 ||
      stat.size > maxBinaryBytes
    ) {
      return false;
    }

    const metadata = JSON.parse(metadataText);
    if (
      metadata === null ||
      typeof metadata !== "object" ||
      metadata.version !== version ||
      metadata.artifact !== artifact ||
      !/^[a-f0-9]{64}$/.test(metadata.sha256)
    ) {
      return false;
    }

    const executable = isWindows || Boolean(stat.mode & 0o111);
    const statMatches =
      metadata.schemaVersion === METADATA_SCHEMA_VERSION &&
      metadata.size === stat.size &&
      metadata.mtimeMs === stat.mtimeMs &&
      metadata.ctimeMs === stat.ctimeMs;
    const verificationAge = nowMs - metadata.verifiedAtMs;
    const verifiedRecently =
      Number.isFinite(metadata.verifiedAtMs) &&
      verificationAge >= -(5 * 60 * 1000) &&
      verificationAge < integrityRecheckMs;

    // Metadata and the executable live in the same package directory and are
    // published atomically. Matching immutable file identity fields let normal
    // MCP starts avoid re-reading a potentially hundreds-of-megabytes binary.
    // A full bounded hash still runs after the recheck interval, after any stat
    // change, and when migrating metadata written by older package versions.
    if (statMatches && executable && verifiedRecently) return true;

    const actual = await hashFile(binaryPath, maxBinaryBytes);
    if (actual !== metadata.sha256) return false;

    const afterHash = await fs.stat(binaryPath);
    if (
      !afterHash.isFile() ||
      afterHash.size !== stat.size ||
      afterHash.mtimeMs !== stat.mtimeMs ||
      afterHash.ctimeMs !== stat.ctimeMs
    ) {
      return false;
    }
    if (!isWindows && !(afterHash.mode & 0o111)) {
      await fs.chmod(binaryPath, 0o755);
    }
    const finalStat = await fs.stat(binaryPath);
    await writeMetadataAtomically(
      metadataPath,
      {
        schemaVersion: METADATA_SCHEMA_VERSION,
        version,
        artifact,
        sha256: actual,
        size: finalStat.size,
        mtimeMs: finalStat.mtimeMs,
        ctimeMs: finalStat.ctimeMs,
        verifiedAtMs: nowMs,
      },
      isWindows,
    ).catch(() => {
      // The binary is verified and executable. If metadata refresh is blocked,
      // keep serving it and retry the bounded hash on the next invocation.
    });
    return true;
  } catch {
    return false;
  }
}

async function replaceFileAtomically(
  tempPath,
  destination,
  isWindows,
  fileSystem = fs,
) {
  try {
    await fileSystem.rename(tempPath, destination);
    return;
  } catch (error) {
    if (!isWindows || !["EEXIST", "EPERM", "EACCES"].includes(error.code)) {
      throw error;
    }
  }

  const backupPath = uniqueSibling(destination, "old");
  let movedExisting = false;
  try {
    await fileSystem.rename(destination, backupPath);
    movedExisting = true;
  } catch (error) {
    if (error.code !== "ENOENT") throw error;
  }

  try {
    await fileSystem.rename(tempPath, destination);
  } catch (error) {
    if (movedExisting) {
      await fileSystem.rename(backupPath, destination).catch(() => {});
    }
    throw error;
  }

  if (movedExisting) {
    // Publication already succeeded. Antivirus/indexer races can temporarily
    // hold the old executable on Windows; failing the install here would leave
    // a verified new binary paired with stale metadata and trigger redownloads
    // forever. The uniquely named old file is safe to remove best-effort.
    await fileSystem.rm(backupPath, { force: true }).catch(() => {});
  }
}

async function writeMetadataAtomically(metadataPath, metadata, isWindows) {
  const tempPath = uniqueSibling(metadataPath, "tmp");
  try {
    await fs.writeFile(tempPath, `${JSON.stringify(metadata)}\n`, {
      encoding: "utf8",
      flag: "wx",
      mode: 0o644,
    });
    await replaceFileAtomically(tempPath, metadataPath, isWindows);
  } finally {
    await fs.rm(tempPath, { force: true }).catch(() => {});
  }
}

async function downloadVerifiedBinary(
  url,
  destination,
  expectedSha256,
  options,
  maxBytes = MAX_BINARY_BYTES,
) {
  const tempPath = uniqueSibling(destination, "tmp");
  const hash = createHash("sha256");
  let downloadedBytes = 0;

  try {
    const response = await requestResponse(url, options);
    const contentLengthHeader = response.headers["content-length"];
    const contentLength = contentLengthHeader
      ? Number.parseInt(contentLengthHeader, 10)
      : null;
    if (Number.isFinite(contentLength) && contentLength > maxBytes) {
      response.destroy();
      throw new Error(
        `binary exceeds ${maxBytes} byte download limit (Content-Length: ${contentLength})`,
      );
    }
    const hasher = new Transform({
      transform(chunk, _encoding, callback) {
        downloadedBytes += chunk.length;
        if (downloadedBytes > maxBytes) {
          callback(
            new Error(
              `binary exceeds ${maxBytes} byte download limit while streaming`,
            ),
          );
          return;
        }
        hash.update(chunk);
        callback(null, chunk);
      },
    });

    await pipeline(
      response,
      hasher,
      createWriteStream(tempPath, { flags: "wx", mode: 0o600 }),
    );

    if (Number.isFinite(contentLength) && downloadedBytes !== contentLength) {
      throw new Error(
        `incomplete download: expected ${contentLength} bytes, received ${downloadedBytes}`,
      );
    }
    if (downloadedBytes === 0) {
      throw new Error("binary download was empty");
    }

    const actualSha256 = hash.digest("hex");
    if (actualSha256 !== expectedSha256) {
      throw new Error(
        `checksum mismatch for ${new URL(url).pathname.split("/").pop()}: expected ${expectedSha256}, received ${actualSha256}`,
      );
    }

    return { tempPath, sha256: actualSha256 };
  } catch (error) {
    await fs.rm(tempPath, { force: true }).catch(() => {});
    throw error;
  }
}

async function install({
  platform = process.platform,
  arch = process.arch,
  version = VERSION,
  binaryPath = BINARY_PATH,
  metadataPath = METADATA_PATH,
  releaseBaseUrl = RELEASE_BASE_URL,
  request = defaultRequest,
  timeoutMs = REQUEST_TIMEOUT_MS,
  maxRedirects = MAX_REDIRECTS,
  maxBinaryBytes = MAX_BINARY_BYTES,
  signal,
  isWindows = platform === "win32",
  log = (message) => process.stderr.write(`${message}\n`),
} = {}) {
  const artifact = getPlatformArtifact(platform, arch);
  if (
    await validateExistingBinary({
      binaryPath,
      metadataPath,
      version,
      artifact,
      isWindows,
      maxBinaryBytes,
    })
  ) {
    return { installed: false, artifact };
  }

  await fs.mkdir(dirname(binaryPath), { recursive: true });
  const requestOptions = { request, timeoutMs, maxRedirects, signal };
  const baseUrl = releaseBaseUrl.replace(/\/$/, "");
  const manifestUrl = `${baseUrl}/SHA256SUMS`;
  const binaryUrl = `${baseUrl}/${artifact}`;

  log(`codixing-mcp: downloading v${version} from ${binaryUrl}`);
  const manifest = await downloadText(manifestUrl, requestOptions);
  const expectedSha256 = parseChecksumManifest(manifest, artifact);
  const { tempPath, sha256 } = await downloadVerifiedBinary(
    binaryUrl,
    binaryPath,
    expectedSha256,
    requestOptions,
    maxBinaryBytes,
  );

  try {
    if (!isWindows) await fs.chmod(tempPath, 0o755);
    await replaceFileAtomically(tempPath, binaryPath, isWindows);
    const publishedStat = await fs.stat(binaryPath);
    await writeMetadataAtomically(
      metadataPath,
      {
        schemaVersion: METADATA_SCHEMA_VERSION,
        version,
        artifact,
        sha256,
        size: publishedStat.size,
        mtimeMs: publishedStat.mtimeMs,
        ctimeMs: publishedStat.ctimeMs,
        verifiedAtMs: Date.now(),
      },
      isWindows,
    );
  } finally {
    await fs.rm(tempPath, { force: true }).catch(() => {});
  }

  log("codixing-mcp: installed successfully");
  return { installed: true, artifact };
}

function wireChildProcess(child, processRef = process, log = console.error) {
  let finished = false;
  const handlers = new Map();
  const signals = ["SIGINT", "SIGTERM", "SIGHUP"];

  const cleanup = () => {
    for (const [signal, handler] of handlers) {
      processRef.removeListener(signal, handler);
    }
    handlers.clear();
  };

  for (const signal of signals) {
    const handler = () => {
      if (!child.killed) child.kill(signal);
    };
    try {
      processRef.on(signal, handler);
      handlers.set(signal, handler);
    } catch {
      // Some platforms do not support every POSIX signal.
    }
  }

  child.once("error", (error) => {
    if (finished) return;
    finished = true;
    cleanup();
    log(`codixing-mcp: failed to start: ${error.message}`);
    processRef.exit(1);
  });
  child.once("exit", (code, signal) => {
    if (finished) return;
    finished = true;
    cleanup();
    if (signal) {
      processRef.kill(processRef.pid, signal);
      return;
    }
    processRef.exit(Number.isInteger(code) ? code : 1);
  });

  return cleanup;
}

async function installBeforeSpawn({
  installFn = install,
  processRef = process,
} = {}) {
  const controller = new AbortController();
  const handlers = new Map();
  let interruptedSignal;

  const cleanup = () => {
    for (const [signal, handler] of handlers) {
      processRef.removeListener(signal, handler);
    }
    handlers.clear();
  };

  for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"]) {
    const handler = () => {
      if (interruptedSignal) return;
      interruptedSignal = signal;
      controller.abort(abortError(`download aborted by ${signal}`));
    };
    try {
      processRef.on(signal, handler);
      handlers.set(signal, handler);
    } catch {
      // Some platforms do not support every POSIX signal.
    }
  }

  try {
    const result = await installFn({ signal: controller.signal });
    return { result, interruptedSignal };
  } catch (error) {
    if (interruptedSignal) return { interruptedSignal };
    throw error;
  } finally {
    cleanup();
  }
}

async function main({
  installFn = install,
  spawnFn = spawn,
  processRef = process,
  binaryPath = BINARY_PATH,
} = {}) {
  const { interruptedSignal } = await installBeforeSpawn({
    installFn,
    processRef,
  });
  if (interruptedSignal) {
    processRef.kill(processRef.pid, interruptedSignal);
    return;
  }
  if (processRef.argv.includes("--install")) return;

  const child = spawnFn(binaryPath, processRef.argv.slice(2), {
    stdio: "inherit",
    env: processRef.env,
  });
  wireChildProcess(child, processRef);
}

if (require.main === module) {
  main().catch((error) => {
    process.stderr.write(`codixing-mcp: ${error.message}\n`);
    process.stderr.write(
      "codixing-mcp: manually download a release from https://github.com/ferax564/codixing/releases\n",
    );
    process.exitCode = 1;
  });
}

module.exports = {
  INTEGRITY_RECHECK_MS,
  MAX_BINARY_BYTES,
  MAX_METADATA_BYTES,
  MAX_REDIRECTS,
  PLATFORM_MAP,
  downloadText,
  getPlatformArtifact,
  install,
  installBeforeSpawn,
  main,
  parseChecksumManifest,
  replaceFileAtomically,
  requestResponse,
  sha256File,
  validateExistingBinary,
  wireChildProcess,
};
