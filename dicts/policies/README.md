# Industry policy packs

Opt-in keyword bundles for regulated industries. None are loaded by default — add the ones you want to your config:

```toml
[input.keyword]
dict_paths = [
  "dicts/policies/healthcare.txt",
  "dicts/policies/finance.txt",
  "dicts/policies/legal.txt",
]
```

| Pack | Scope | Notes |
|---|---|---|
| `healthcare.txt` | HIPAA-aware PHI keywords | Medical record numbers, sensitive conditions (HIV, mental health, substance abuse), JP equivalents (カルテ, 保険証 …) |
| `finance.txt` | PCI-DSS / GLBA / MNPI | Payment card details, insider information, investment advice surface, JP equivalents (暗証番号, インサイダー情報 …) |
| `legal.txt` | Attorney-client privilege | Privilege markers, active-matter signals, sealed documents, JP equivalents (弁護士秘匿特権, 営業秘密 …) |

These packs are advisory starting points, not certified compliance bundles. Tune the entries and weights for your deployment, and combine with the regex-based detectors in `dicts/pii-regex.txt` for structured identifiers.

## Format reminder

Tab-separated:

```
keyword<TAB>key<TAB>weight
```

- `key`: `0` = BLOCK, `1` = ALERT, `2` = FLAG
- `weight`: optional (default 1.0); used by the iword-rs engine for accumulation thresholds
- Lines starting with `#` are comments
- Regex rules go in a separate file and use `/pattern/<TAB>key<TAB>weight` syntax
