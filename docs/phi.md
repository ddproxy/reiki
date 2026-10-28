# PHI and NPPI plugin

The `phi` plugin stops protected health information (PHI) and nonpublic personal information (NPPI) from leaving in an outbound request. It checks each request body for structured identifiers. In `block` mode a request that holds one is rejected before any key is used. In `report` mode the request goes through and the finding is logged.

It is a safeguard that catches common identifiers. It does not prove a request is free of protected data. Read [What it does not cover](#what-it-does-not-cover) before relying on it.

Compiled in by the default `plugin-phi` feature; inactive until a `[plugins.phi]` table exists.

## Configure

```toml
[plugins.phi]
mode = "report"                      # "block" or "report"; required
# kinds = ["ssn", "card", "email"]   # default: every kind below except email and phone
# upstreams = ["zen", "openrouter"]  # default: all upstreams
```

| Key | Default | Meaning |
| --- | --- | --- |
| `mode` | required | `block` rejects a request that holds an identifier. `report` logs it and lets the request through. |
| `kinds` | all but `email`, `phone` | The kinds to look for. |
| `upstreams` | all | Check only requests to these upstreams. A request to any other upstream is not checked. |

Start in `report`, read the log for false positives, then switch to `block`.

## Kinds

| Kind | Matches |
| --- | --- |
| `ssn` | `123-45-6789` with a valid area, group and serial. Nine digits in a row, or spaces instead of dashes, count only after a word such as `ssn` or `social security`. |
| `card` | 13 to 19 digits, grouped by single spaces or dashes, with a plausible first digit and a valid Luhn checksum. |
| `routing` | Nine digits with a valid ABA checksum after `routing` or `aba`. |
| `account` | Six to 17 digits after `account number`, `account no`, `account #` or `acct`. |
| `dob` | A date after `dob`, `date of birth`, `birth date` or `born on`. |
| `medical_id` | A token with at least four digits after `mrn`, `member id`, `subscriber id`, `policy number`, `patient id` or similar. |
| `npi` | A 10-digit National Provider Identifier with a valid checksum after `npi`. |
| `email` | An email address. Off by default, because source code and commit metadata carry many. |
| `phone` | A US phone number written with separators or parentheses. Off by default. |

## Behavior

- The plugin reads the request body as text. Matches never leave the plugin: the log and the rejection name each kind and its count, such as `ssn x1, card x2`, and never a value.
- A blocked request gets `422`. A body with a `Content-Encoding` cannot be read, so `block` mode rejects it with `415` and `report` mode logs that it was not checked.
- The plugin runs before `placeholders` in the default build, so it sees the original request.
- Only the request body is checked. Headers, the query string and responses are not.

## What it does not cover

- **Names and addresses.** Free-text names, street addresses and diagnoses have no fixed shape, so the plugin does not detect them. Add them as `terms` in the [placeholders](placeholders.md) plugin when you know them.
- **Traffic Reiki cannot read.** An opaque tunnel is not visible to any plugin. A tunnel allowlist such as `*` lets data leave without being checked. On a machine that handles protected data, allow only the hosts it needs and intercept the ones that carry prompts.
- **Encoded or split data.** An identifier that is base64-encoded, split across fields or written out in words is not found.
- **Images and files.** Pixel and file contents are not read.
