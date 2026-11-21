# Security

## Report a vulnerability

Use GitHub's [private vulnerability reporting](https://github.com/ddproxy/reiki/security/advisories/new) (the **Report a vulnerability** button on the repository's Security tab). Do not open a public issue for an unfixed vulnerability.

Reiki holds upstream API keys, so reports about credential exposure, header handling, request routing or the admin listener are especially welcome. See [security](docs/security.md) for the trust model and [security scanning](docs/security-scanning.md) for the checks this repository runs.

## Supported versions

Only the latest release receives security fixes. Update to the latest release before reporting; include the release version in your report.

## Response times

| Stage | Target |
| --- | --- |
| Acknowledgement | within 3 business days |
| Initial assessment | within 7 business days |
| Status update | at least every 7 days while triaging |
| Fix or mitigation decision | within 30 days of assessment |

Complex reports can take longer; you will get a status update before a target is missed.

## More

- [Published advisories](https://github.com/ddproxy/reiki/security/advisories)
- [Releases and verification](https://github.com/ddproxy/reiki/releases): see [security scanning](docs/security-scanning.md) for `gh attestation verify`
