# Jev solver

A Rust library and CLI for reCAPTCHA v2 image grids, with a Chrome browser workflow.
Jev judges each tile; Rust controls image preparation, clicks, replacement detection,
verification, and stopping conditions.

The default is **`jev-1.14.0`**, the newest documented release on October 15, 2026.
TypeSafe currently maps `jev-latest` to 1.13.0 and `jev-preview` to 1.14.0, so the
version is pinned deliberately. Override it with `--model` or `TYPESAFE_MODEL`.
See the [model reference](https://docs.typesafe.ai/models).

TypeSafe's [image API](https://docs.typesafe.ai/images) is experimental. This
project implements a working integration and browser loop; model judgments can
be wrong and there is no guaranteed reCAPTCHA success rate. It supports English
v2 checkbox challenges with 3×3 or 4×4 image grids. Audio challenges, invisible
widgets, v3 risk scoring, and widgets nested inside other iframes are unsupported.
The browser reports access blocks and rejected selections as failures.

## Setup

Install Rust 1.88+ and Chrome. The browser command downloads and manages the
matching ChromeDriver on first use. An existing WebDriver server is also supported.

```sh
cargo build --release --locked
export TYPESAFE_API_KEY='your-typesafe-key'
```

Alternatively, copy `secrets.example.yml` to `secrets.yml` and fill in the key.
The CLI loads the `Official` entry from `system-one` by default. Environment
credentials take priority. `--secrets` and `--provider` select another file or
provider, and `--endpoint` / `TYPESAFE_ENDPOINT` override the full evaluation URL.
Providers must implement the TypeSafe HTTP API, including multipart image assets;
OpenRouter chat-completions endpoints use a different contract and are not supported.
Provider `model-name` fields are ignored: choose the model explicitly with `--model`.

Credential files and `artifacts/` are ignored by Git. The CLI never prints API
keys or raw provider error bodies. `.env` files are not loaded automatically.

## Solve a saved grid

Crop the image to the grid, excluding the instruction header. Copy the exact
instruction into `--prompt`. PNG, JPEG, and WebP input are accepted.

```sh
cargo run --release -- solve \
  --image challenge.png \
  --prompt 'Select all squares with bicycles' \
  --rows 4 --cols 4 \
  --output solution.json \
  --annotate selected.png
```

JSON goes to stdout; diagnostics go to stderr. `selected` and `uncertain` contain
**one-based, row-major** tile indices:

```text
3×3              4×4
1 2 3            1  2  3  4
4 5 6            5  6  7  8
7 8 9            9 10 11 12
                13 14 15 16
```

Each tile includes its probability, decision, row, column, and pixel bounds in
the upright input image. Green annotation borders mark selections; amber borders
mark uncertain tiles. An empty `selected` list is valid.

Generate a simple input to check your account's image API access:

```sh
cargo run --example sample_grid
cargo run -- solve --image artifacts/sample-grid.png \
  --prompt 'Select all images with red squares' \
  --annotate artifacts/sample-selected.png
```

The red squares occupy tiles 1, 5, and 9. This is an API smoke test, not a measure
of accuracy on real reCAPTCHA photographs.

## Browser automation

```sh
cargo run --release -- browser \
  --url 'https://www.google.com/recaptcha/api2/demo' \
  --token-output artifacts/google-demo-token.txt
```

Optional flags:

```sh
cargo run --release -- browser \
  --url 'https://www.google.com/recaptcha/api2/demo' \
  --headless --max-rounds 40 --max-reloads 4 --wait-seconds 30

# Use a driver you already run, instead of managing one automatically:
chromedriver --port=9515
cargo run --release -- browser --url 'https://example.com/form' \
  --webdriver-url 'http://localhost:9515'
```

`--chrome-binary` chooses a Chrome executable. `--widget-index` selects a
zero-based checkbox widget when a page has several. The command creates a fresh
browser session, clicks the checkbox, and handles:

- Static 3×3 and 4×4 grids, followed by Verify/Next/Skip.
- Dynamic grids: wait for clicked images to be replaced, then evaluate again.
- Image changes during inference: discard stale decisions before any clicks.
- Wait for grid animations to finish and its position to stay stable for 450 ms.
- Detect closed/offscreen challenge frames and reopen expired checkboxes.
- Uncertain decisions: re-check enlarged tiles with neighboring context, then
  request a fresh challenge if doubt remains. Stop when the refresh budget is used.
- Already checked widgets, access blocks, verification rejection, timeouts, and Ctrl-C.

Every run creates a unique directory under `artifacts/` containing screenshots,
annotated grids, observed image identities and fade states, per-round probabilities,
and `report.json`. The report records the page URL, resolved models, token usage,
refresh count, and every refinement judgment. `--max-reloads` limits refreshes of
unresolved or rejected challenges (default 4). Access blocks stop the run.

A successful report requires a nonempty `token`. If the checkbox completes
before its callback runs, the solver waits for the response textarea or the
first widget's public `grecaptcha.getResponse()` result. A checked checkbox with
no exposed response becomes a failure, not a successful null-token report.
`--token-output` additionally writes the raw token to a text file. The browser
session closes at the end. The host application still needs its normal server-side
verification. Google documents a two-minute validity period and one verification
per token; a saved token is evidence of a run, not a reusable credential.
See [Google's verification documentation](https://developers.google.com/recaptcha/docs/verify).
Response tokens and screenshots in the run artifacts may be sensitive.

## Judgment and uncertainty

The [HTTP integration](https://docs.typesafe.ai/api) uses `POST /v1/systemone`,
multipart PNG assets, unique UUID references, and the required
`TypeSafe-Danger-Zone-Images: true` header. Images are rotated upright, resized,
or padded to fit Jev's documented dimensions. The full grid appears once in
state; each question has its own enlarged crop, location, and the exact target.

One independent [Noul question](https://docs.typesafe.ai/primitives/noul) per tile
asks whether it contains any visible part of the requested target. All questions
are sent in one request. Rust validates answer IDs, answer types, and probabilities
before it takes action. Noul returns a yes-probability, not a separate confidence.

Default decisions are `select` at probability ≥ 0.7, `skip` at ≤ 0.3, and `review`
between them. These are starting values to evaluate on your own images, not
validated accuracy guarantees. Tune with `--select-threshold` and
`--reject-threshold`; thresholds must straddle 0.5. Each report keeps the raw
probabilities so you can inspect and recalibrate them.

Before returning `review`, the solver makes up to two targeted refinement calls
(`--refinement-passes`, range 0–3). Only unresolved tiles are re-examined. Each
question receives a larger raw crop and a neighboring crop with a pink outline
outside the target tile; the outline preserves object pixels inside the tile.
The latest probability drives the decision using the same thresholds. Upscaling
does not add source detail, and repeated judgments are not independent evidence:
they are not multiplied into an artificial confidence. `initial_probability`
and `refinements` preserve the original judgment and each follow-up. CLI image
mode returns exit code 2 if refinement still leaves doubt; browser mode can
refresh the challenge within its budget.

`--timeout-seconds` bounds an API operation including retries (default 60).
`--retries` defaults to 2. Explicit rate-limit, overload, and transient service
responses use bounded exponential backoff and honor `Retry-After`; transport
failures and authentication errors are not retried.

Exit codes: `0` completed, `2` needs human review, `1` failed or stopped.

## Rust library

```rust,no_run
use std::time::Duration;
use jev_solver::{
    client::JevClient,
    config::{Credentials, DEFAULT_ENDPOINT, DEFAULT_MODEL},
    solver::{Grid, Solver, Thresholds},
};

# async fn example() -> anyhow::Result<()> {
let credentials = Credentials::new(DEFAULT_ENDPOINT, std::env::var("TYPESAFE_API_KEY")?)?;
let client = JevClient::new(credentials, Duration::from_secs(60), 2)?;
let solver = Solver::new(client, DEFAULT_MODEL.to_owned(), Thresholds::default())?;
let bytes = std::fs::read("challenge.png")?;
let result = solver.solve(&bytes, "Select all squares with bicycles", Grid::new(4, 4)?).await?;
println!("{:?}", result.selected);
# Ok(())
# }
```

Use `browser::run` with `BrowserOptions` to drive the same solver through Chrome.

## Validation

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked

# Real Chrome against local deterministic fixtures; no TypeSafe key required:
cargo test --locked --test integration browser_ -- --ignored --test-threads=1
```

The browser fixtures test actual iframe navigation, screenshots, clicks, static
grids, delayed replacements, moving grids, stale inference, closed dialogs,
targeted refinement, bounded refreshes, rejected selections, delayed response
callbacks, missing tokens, access blocks, unsupported challenges, and round limits. Their
TypeSafe responses are mocked, so these checks validate the workflow rather
than Jev's visual accuracy. Set `TEST_WEBDRIVER_URL` or `TEST_CHROME_BINARY` to
override browser setup. The default test suite does not call external services.