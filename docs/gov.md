# Government plugin

The `gov` plugin stops government markings and identifier values from leaving in an outbound request. It checks each request body for banner markings and for identifiers next to their own label. In `block` mode a request that holds one is rejected before any key is used. In `report` mode the request goes through and the finding is logged.

It is a safeguard that catches common markings. It does not prove a request is free of controlled data. Read [What it does not cover](#what-it-does-not-cover) before relying on it.

Compiled in by the default `plugin-gov` feature; inactive until a `[plugins.gov]` table exists.

## Configure

```toml
[plugins.gov]
mode = "report"                      # "block" or "report"; required
# kinds = ["cui", "ein"]            # default: cui, classification, dissemination
# upstreams = ["zen", "openrouter"]  # default: all upstreams
```

| Key | Default | Meaning |
| --- | --- | --- |
| `mode` | required | `block` rejects a request that holds a marking or identifier. `report` logs it and lets the request through. |
| `kinds` | `cui`, `classification`, `dissemination` | The kinds to look for. |
| `upstreams` | all | Check only requests to these upstreams. A request to any other upstream is not checked. |

Start in `report`, read the log for false positives, then switch to `block`.

## Kinds

| Kind | Default | Matches |
| --- | --- | --- |
| `cui` | on | A stand-alone upper-case `CUI` banner, or a `CUI//SP-` marker with a 2- to 8-character category code such as `CUI//SP-CTI`. |
| `classification` | on | `TOP SECRET`, `SECRET` or `CONFIDENTIAL` followed immediately by a `//` caveat chain, such as `TOP SECRET//SI//NOFORN` or `SECRET//REL TO USA, GBR`. A bare level is not reported. |
| `dissemination` | on | A stand-alone upper-case `NOFORN` or `ORCON` token. |
| `ein` | off | Two digits, a hyphen and seven digits after `ein` or `employer identification number`. No checksum; the label is the only gate. |
| `itin` | off | `9XX-XX-XXXX` after `itin` or `individual taxpayer identification number`, with the historical group range 70-88, 90-92 or 94-99. |
| `cage` | off | Five characters from the CAGE alphabet (no `I`, `O`, `Q` or `S`) after `cage` or `cage code`. |
| `uei` | off | Twelve characters from the UEI alphabet (no `I`, `O`, `Q` or `S`) after `uei` or `unique entity id`. |
| `edipi` | off | Ten digits, not starting with zero, after `edipi`, `dod id` or `dod_id`. |
| `eccn` | off | A five-character export control number such as `5A002` after `eccn`, with an optional dot suffix. |

The keyword-bound kinds are off by default because their shapes alone collide with ordinary numbers and codes. Turn them on for a corpus that carries contracts or export paperwork.

## Behavior

- The plugin reads the request body as text. Matches never leave the plugin: the log and the rejection name each kind and its count, such as `cui x1, dissemination x1`, and never a value.
- A blocked request gets `422`. A body with a `Content-Encoding` cannot be read, so `block` mode rejects it with `415` and `report` mode logs that it was not checked.
- The plugin runs after `phi` and `finance` and before `placeholders` in the default build, so it sees the original request.
- Only the request body is checked. Headers, the query string and responses are not.

## What it does not cover

- **Bare classification and control words.** `SECRET`, `CONFIDENTIAL`, `EAR`, `ITAR`, `FCI`, `CTI`, `CMMC` and `FOUO` are not detected on their own, because they are ordinary prose and code. A classification level is reported only with a `//` caveat chain.
- **Passports and driver licences.** Formats vary by issuer and have no reliable public checksum, so they are not detected.
- **Identifiers without their label.** `ein`, `itin`, `cage`, `uei`, `edipi` and `eccn` are reported only next to their own label. A bare number or code is not reported, and a label that appears after the value is not enough.
- **Contract numbers.** FAR and DFARS numbers vary too widely to detect without a false-positive rate that would make `block` mode unusable.
- **Traffic Reiki cannot read.** An opaque tunnel is not visible to any plugin. A tunnel allowlist such as `*` lets data leave without being checked.
- **Encoded or split data.** A marking or identifier that is base64-encoded, split across fields or written out in words is not found.
- **Images and files.** Pixel and file contents are not read.
