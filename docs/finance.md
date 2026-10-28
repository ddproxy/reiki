# Finance plugin

The `finance` plugin stops financial identifiers from leaving in an outbound request. It checks each request body for account, payment and securities identifiers. In `block` mode a request that holds one is rejected before any key is used. In `report` mode the request goes through and the finding is logged.

It is a safeguard that catches common identifiers. It does not prove a request is free of financial data. Read [What it does not cover](#what-it-does-not-cover) before relying on it.

Compiled in by the default `plugin-finance` feature; inactive until a `[plugins.finance]` table exists.

## Configure

```toml
[plugins.finance]
mode = "report"                        # "block" or "report"; required
# kinds = ["iban", "swift"]           # default: every kind below except account and sedol
# upstreams = ["zen", "openrouter"]   # default: all upstreams
```

| Key | Default | Meaning |
| --- | --- | --- |
| `mode` | required | `block` rejects a request that holds an identifier. `report` logs it and lets the request through. |
| `kinds` | all but `account`, `sedol` | The kinds to look for. |
| `upstreams` | all | Check only requests to these upstreams. A request to any other upstream is not checked. |

Start in `report`, read the log for false positives, then switch to `block`.

## Kinds

| Kind | Matches |
| --- | --- |
| `iban` | An IBAN with a registry country, the country's exact length and a valid mod-97 check. Printed spaces between groups are allowed; other punctuation is rejected. |
| `swift` | An 8- or 11-character BIC with an ISO country, after a `swift` or `bic` label, next to an IBAN, or under a `swift` or `bic` table header. A bare BIC is not reported. |
| `crypto` | A Bitcoin Base58Check address with a valid checksum, a Bitcoin bech32 or bech32m SegWit address, or an Ethereum `0x` address with 40 hex digits. Mixed-case Ethereum addresses must satisfy EIP-55; all-lowercase and all-uppercase ones are unchecksummed and accepted. |
| `cusip` | Nine characters with the CUSIP check digit, after `cusip` or in a `cusip` column. A bare token passes the check one time in ten, so it needs the label. |
| `isin` | Twelve characters with an ISO country and the Luhn check over the expanded value. |
| `pan_sad` | A card number with a valid Luhn checksum next to a `cvv`, `cvc`, `cid`, `cvv2`, `cvc2` or `cav2` label, or a card number at the start of `%B` track data. A bare card number is not reported. |
| `account` | Six to 17 digits after `account`, `acct`, `beneficiary` or a similar label, or under an `account` table header. Off by default; a bare account number is not reported. |
| `sedol` | Seven characters with the SEDOL check digit after a `sedol` label or under a `sedol` table header. Off by default. |

## Tables

A header names a column, so every cell under it is checked as that kind: `iban`, `swift`, `bic`, `cusip`, `isin`, `wallet`, `account`, `sedol` and their common synonyms. This covers delimited text with a header row, Markdown tables, SQL `INSERT INTO t (a, b) VALUES (...)` with a column list and SQL `COPY t (a, b) FROM stdin` blocks. A table without a recognised header, or an `INSERT` without a column list, is checked only by the rules above.

Run [`reiki scan`](plugins.md#tune-a-detector-with-reiki-scan) over sample data to see what this plugin flags before you choose a mode.

## Behavior

- The plugin reads the request body as text. Matches never leave the plugin: the log and the rejection name each kind and its count, such as `iban x1, crypto x2`, and never a value.
- A blocked request gets `422`. A body with a `Content-Encoding` cannot be read, so `block` mode rejects it with `415` and `report` mode logs that it was not checked.
- The plugin runs after `phi` and before `placeholders` in the default build, so it sees the original request.
- Only the request body is checked. Headers, the query string and responses are not.

## What it does not cover

- **Bare card numbers.** The `pan_sad` kind reports card numbers only next to CVV data or in track data. A PAN on its own is covered by the `card` kind in the [phi](phi.md) plugin.
- **Bare account numbers, routing numbers and sort codes.** None has a universal checksum, and they collide with order IDs, dates and phone numbers. `account` needs a label or header and is off by default.
- **SEDOL alone.** Seven characters collide too easily, so `sedol` needs a label or header and is off by default.
- **Ethereum checksums on uniform case.** An all-lowercase or all-uppercase address is accepted because EIP-55 applies only to mixed case.
- **Traffic Reiki cannot read.** An opaque tunnel is not visible to any plugin. A tunnel allowlist such as `*` lets data leave without being checked.
- **Encoded or split data.** An identifier that is base64-encoded, split across fields or written out in words is not found.
- **Images and files.** Pixel and file contents are not read.
