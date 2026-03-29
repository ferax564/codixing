# Security Policy

## Supported Versions

| Version | Supported |
|---------|-----------|
| latest `main` | Yes |
| older releases | Best effort |

## Reporting a Vulnerability

**Do not open a public GitHub issue for security vulnerabilities.**

Report security issues by emailing **security@codixing.com**.

Include:

- Description of the vulnerability and its potential impact
- Steps to reproduce or a minimal proof of concept
- Affected versions (if known)
- Any suggested mitigations

### What to expect

- Acknowledgement within **48 hours**
- Status update within **7 days**
- Coordinated disclosure: we will work with you to agree on a disclosure timeline before publishing a fix. We aim for a **90-day** disclosure window from initial report.

---

## Threat Model

Codixing runs locally and indexes files on the local filesystem. It does not:

- Transmit source code to any external server (all embedding inference runs locally via ONNX)
- Accept inbound network connections by default (the REST server and daemon Unix socket require explicit opt-in)
- Execute indexed code

Potential attack surfaces:

- **Malicious tree-sitter input**: crafted source files could trigger parser bugs in the C-level tree-sitter grammars. We vendor grammars from upstream — report to upstream grammar maintainers and to us.
- **Path traversal in file reads**: `read_file` MCP tool validates paths against the indexed root. Report any bypass.
- **Unix socket permissions**: the daemon socket at `.codixing/daemon.sock` inherits directory permissions. Ensure `.codixing/` is not world-writable on shared machines.
- **Qdrant backend** (optional, `--features qdrant`): if `QDRANT_URL` points to a shared Qdrant instance, standard Qdrant security considerations apply.

---

## Dependency Updates

We pin dependencies in `Cargo.lock`. Security patches in dependencies are picked up by running `cargo update` and opening a PR. `cargo audit` is the recommended tool for scanning advisories:

```bash
cargo install cargo-audit
cargo audit
```
