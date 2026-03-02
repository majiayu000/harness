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
