> **Status:** partial (`make mirai` / `make preflight-mirai` shipped; CI promotion pending after false-positive triage)

# MIRAI cargo integration

MIRAI is an abstract interpreter for Rust MIR. It can act as a linter for
unintentional panics and can also verify explicit correctness annotations. For
nanoguard, the practical first step is to use it as an optional deep
static-analysis pass alongside the existing `miri`, `audit`, and `preflight`
targets.

The maintained upstream is `https://github.com/endorlabs/MIRAI`. The upstream
installation guide currently recommends cloning that repository and installing
the cargo subcommand from its `checker` crate:

```bash
git clone https://github.com/endorlabs/MIRAI.git
cd MIRAI
cargo install --locked --path ./checker
```

After installation, run:

```bash
make mirai
```

This delegates to:

```bash
cargo mirai --tests
```

`cargo mirai --tests` is intentional because nanoguard has meaningful guard,
matcher, proxy, budget, and policy tests that expose more entry points than the
binary alone. MIRAI options can be supplied with `MIRAI_FLAGS`, for example:

```bash
MIRAI_FLAGS="--diag=verify --body_analysis_timeout 60" make mirai
```

## Adoption decision

Do not make MIRAI mandatory in default CI yet. The toolchain is heavier than
clippy/fmt/audit, and MIRAI may produce project-specific false positives that
should be triaged before it becomes a blocking check.

The initial integration is therefore:

- `make mirai` for explicit local runs.
- `make preflight-mirai` for security-sensitive branches that should run the
  normal preflight suite plus MIRAI.
- README documentation so contributors can install and run the same command.

If MIRAI reports are stable over a few security-sensitive changes, promote
`make mirai` into a non-blocking scheduled workflow first. Only make it a pull
request gate after the repo has annotations or code changes for recurring false
positives.
