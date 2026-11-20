# Security Policy

## Supported versions

Only the latest release and the `master` branch receive security fixes.

| Version | Supported |
|---|---|
| V2 (`latest` release) / `master` | ✅ |
| V1 and older releases | ❌ |

## Reporting a vulnerability

**Please do not open a public issue for security problems.**

Use GitHub's private reporting channel:

1. Go to <https://github.com/vamtic/makerr/security/advisories/new>.
2. Describe the vulnerability, the affected version, and steps to reproduce.
3. Include any known impact (e.g. path traversal, credential exposure,
   arbitrary code execution) and a suggested fix if you have one.

You should receive an acknowledgement within a few days. Once a fix is
available, a release and a security advisory will be published, and you will be
credited unless you prefer to remain anonymous.

## Scope notes

TimelapseMaker downloads tiles from third-party canvas servers and invokes
`ffmpeg` on local files. Reports about malicious server responses, unsafe path
handling for output/template files, or unsafe `ffmpeg` argument construction are
especially welcome.
