# Redaction reference

How Reiki removes an upstream key when an upstream echoes it back. Keys are never sent to the client on purpose; this is the safeguard for an upstream that repeats a credential in a header or body. For the trust boundary, see [security](security.md). Request-log masking is a separate mechanism; see [request log](requestlog.md#what-is-kept-out-of-the-log).

## Where each mechanism applies

| Where | Function | Replacement | Length |
| --- | --- | --- | --- |
| Headers relayed to the client | `scrub` in `src/proxy.rs` | `[redacted]` | Changes |
| Upstream error body before it is cut to 64 KiB | `scrub` in `src/proxy.rs` | `[redacted]` | Changes |
| Successful response body, streamed | `StreamScrubber` in `src/scrub.rs` | A same-length `[***…]` marker | Kept |
| Failure-ring snippet and `Retry-After` note | `scrub_text` in `src/scrub.rs` | A same-length `[***…]` marker | Kept |

A successful body keeps its length, so a `content-length` stays correct. A streamed body holds back only the suffix that could still become the start of a key, so an event is not delayed when no key is pending. Scrubbing runs before plugins see the response. `tests/scrub_success.rs` covers the successful path and `tests/scrub_failures.rs` covers ring snippets.

## Forms generated per key

`forms()` in `src/scrub.rs` builds these byte strings for each configured key. The shapes below are schematic, not captured output.

| Form | What is matched | Source |
| --- | --- | --- |
| Raw and formatted credential | The exact key and the whole `Bearer <key>` value | `src/scrub.rs`, `forms()` |
| JSON escapes | `\uXXXX` escapes of ASCII letters and digits: every alphanumeric escaped, one at each position, or one per distinct character; lowercase and uppercase hex; minimal escapes and `\/` | `json_forms()` |
| Percent encoding | Every byte encoded, only unreserved bytes encoded, one unreserved byte at each position; lowercase and uppercase hex, `+` for a space | `percent_*_forms()` |
| Base64 | Standard and URL-safe alphabets, padded and unpadded, plus complete characters for a key starting 1 or 2 bytes into a larger value, with the edge characters trimmed | `base64()`, `base64_at_offset()` |
| Split | One ASCII whitespace or control byte at each boundary and one between every character; space-separated forms come first | `split_forms()` |

`forms()` sorts longest first and removes duplicates, so the formatted credential is blanked before the raw key.

## Budgets and minimum length

| Limit | Value | Effect |
| --- | --- | --- |
| Forms per key | 4096 | The list is cut at this size. |
| Reserved split slots | 2048 | The raw form and split forms take these first, so encoding variants cannot crowd them out. A long key with a space at any boundary is still caught. |
| Minimum configured key | 8 bytes | `load_keys` refuses a shorter key; an echo that short would not be scrubbed. |

## Limits and fail-open cases

- Other encodings, compressed bodies, and base64 embedded more than two bytes into a larger value are not covered by `forms()`.
- A response that still carries a content coding Reiki did not decode is refused with `502` when a key is in play. The body is not relayed unscrubbed. Codings Reiki decodes are handled before this check; bodyless responses keep their headers.
- A failure-ring snippet is scrubbed the same way as a relayed body. If a compacted copy still contains key material after ASCII whitespace and control bytes are removed, the whole snippet becomes `[body withheld: contains key material]` (`src/proxy.rs`, `WITHHELD_NOTE`).
- A key split across chunk boundaries is caught: tests cover every cut point and three chunks. The scrubber holds only a possible key prefix.
- Scrubbing is pattern matching against known key values. It is not a guarantee that every echo of a key is removed for every possible upstream.
