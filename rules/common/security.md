# Security Rules

## SEC-01: SQL/NoSQL/OS command injection (critical)
Use parameterized queries. Use array argument lists for commands.

## SEC-02: Hardcoded secrets (critical)
Use environment variables or secret managers. Add `.env` to `.gitignore`.

## SEC-03: XSS via unescaped user input (high)
Use DOMPurify or framework escaping. Never assign `innerHTML` directly.

## SEC-04: Missing auth/authz on API endpoints (high)
Add authentication middleware or guards.

## SEC-05: Dependencies with known CVEs (high)
Run audit commands and upgrade or replace.

## SEC-06: Weak cryptographic algorithms (high)
Replace MD5/SHA1 password hashing with bcrypt/argon2.

## SEC-07: Unvalidated file paths (medium)
Normalize paths and restrict to allowed base directories.

## SEC-08: Unrestricted SSRF (medium)
Add target address allowlists or network-layer restrictions.

## SEC-09: Insecure deserialization (medium)
Use `yaml.safe_load()`, avoid `pickle` on untrusted data.

## SEC-10: Sensitive data in logs (medium)
Redact passwords and tokens with `***`.

## SEC-11: Security logic invisible to agents (high)
Auth, authz, and input validation hidden in config files, decorators, or middleware causes AI agents to miss security checks when adding new routes/endpoints. Fix: security checks must be explicitly visible in business code (e.g., call `check_auth()` inside route handlers), not rely solely on framework implicit injection.

Source: Armin Ronacher (Flask author) agentic coding observations.

## SEC-12: MCP Docker config causes container leaks (medium)
MCP servers launched via `docker run -i` leave orphaned containers when Claude Code exits without sending SIGINT. Fix: prefer `uvx` (Python) or `npx` (Node.js) over `docker run`; periodically check `docker ps | grep mcp`.

## SEC-13: MCP tool poisoning and rug pulls (high)
MCP tool definitions can be maliciously replaced (Tool Poisoning) or mutate behavior after trust is established (Rug Pull). Fix: audit tool definitions and permission scope after installing new MCP servers; diff tool lists when updating MCP dependencies (`diff <(old) <(new)`); do not trust community MCP servers from unknown sources.

Source: ETDI (arXiv:2506.01333) — Enhanced Tool Definition Interface proposes cryptographic identity verification + immutable versioned definitions + OAuth 2.0 permission management.
