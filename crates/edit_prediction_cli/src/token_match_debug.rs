use crate::{example::read_example_files, metrics};
use anyhow::Context as _;
use clap::Args;
use std::fmt::Write as _;
use std::path::PathBuf;

#[derive(Args, Debug, Clone)]
#[command(
    about = "Generate token-match debug HTML for expected vs predicted patches",
    after_help = r#"EXAMPLES:
  # Debug all examples from a jsonl dataset
  ep token-match-debug examples.jsonl

  # Write HTML files to a specific directory
  ep token-match-debug examples.jsonl --output-dir out/token-debug

  # Keep only the best expected patch per prediction
  ep token-match-debug examples.jsonl --best-only

  # Limit generated files
  ep token-match-debug examples.jsonl --limit 50
"#
)]
pub struct TokenMatchDebugArgs {
    /// Directory where HTML reports are written.
    #[arg(long, default_value = "token-match-debug")]
    pub output_dir: PathBuf,

    /// Only emit one report per prediction (best matching expected patch).
    #[arg(long, default_value_t = false)]
    pub best_only: bool,

    /// Maximum number of reports to write.
    #[arg(long)]
    pub limit: Option<usize>,
}

pub fn run_token_match_debug(args: &TokenMatchDebugArgs, inputs: &[PathBuf]) -> anyhow::Result<()> {
    let stdin_path = PathBuf::from("-");
    let inputs = if inputs.is_empty() {
        std::slice::from_ref(&stdin_path)
    } else {
        inputs
    };

    let examples = read_example_files(inputs);
    std::fs::create_dir_all(&args.output_dir).with_context(|| {
        format!(
            "failed to create output directory '{}'",
            args.output_dir.display()
        )
    })?;

    let mut written = 0usize;
    for example in &examples {
        let expected_patches = example.spec.expected_patches_with_cursor_positions();
        if expected_patches.is_empty() || example.predictions.is_empty() {
            continue;
        }

        for (prediction_index, prediction) in example.predictions.iter().enumerate() {
            let Some(actual_patch) = prediction.actual_patch.as_deref() else {
                continue;
            };
            if actual_patch.trim().is_empty() {
                continue;
            }

            if args.best_only {
                if let Some((expected_index, report)) =
                    best_expected_patch_report(&expected_patches, actual_patch)
                {
                    let html = render_report_html(
                        &example.spec.name,
                        prediction_index,
                        expected_index,
                        &expected_patches[expected_index].0,
                        actual_patch,
                        &report,
                    );

                    let path = args.output_dir.join(report_filename(
                        &example.spec.filename(),
                        prediction_index,
                        expected_index,
                    ));
                    std::fs::write(&path, html)
                        .with_context(|| format!("failed to write report '{}'", path.display()))?;
                    written += 1;
                    if args.limit.is_some_and(|limit| written >= limit) {
                        eprintln!(
                            "Wrote {} report(s) to {}",
                            written,
                            args.output_dir.display()
                        );
                        return Ok(());
                    }
                }
                continue;
            }

            for (expected_index, (expected_patch, _)) in expected_patches.iter().enumerate() {
                let report = metrics::token_match_debug_report(expected_patch, actual_patch);
                let html = render_report_html(
                    &example.spec.name,
                    prediction_index,
                    expected_index,
                    expected_patch,
                    actual_patch,
                    &report,
                );
                let path = args.output_dir.join(report_filename(
                    &example.spec.filename(),
                    prediction_index,
                    expected_index,
                ));

                std::fs::write(&path, html)
                    .with_context(|| format!("failed to write report '{}'", path.display()))?;
                written += 1;

                if args.limit.is_some_and(|limit| written >= limit) {
                    eprintln!(
                        "Wrote {} report(s) to {}",
                        written,
                        args.output_dir.display()
                    );
                    return Ok(());
                }
            }
        }
    }

    eprintln!(
        "Wrote {} report(s) to {}",
        written,
        args.output_dir.display()
    );
    Ok(())
}

fn best_expected_patch_report(
    expected_patches: &[(String, Option<usize>)],
    actual_patch: &str,
) -> Option<(usize, metrics::TokenMatchDebugReport)> {
    let mut best: Option<(usize, metrics::TokenMatchDebugReport)> = None;
    for (index, (expected_patch, _)) in expected_patches.iter().enumerate() {
        let report = metrics::token_match_debug_report(expected_patch, actual_patch);
        match &best {
            Some((_, current)) => {
                if metrics::compare_classification_metrics(&report.metrics, &current.metrics)
                    .is_gt()
                {
                    best = Some((index, report));
                }
            }
            None => best = Some((index, report)),
        }
    }
    best
}

fn report_filename(example_name: &str, prediction_index: usize, expected_index: usize) -> String {
    format!(
        "{}__prediction-{}__expected-{}.html",
        example_name, prediction_index, expected_index
    )
}

fn render_report_html(
    example_name: &str,
    prediction_index: usize,
    expected_index: usize,
    expected_patch: &str,
    actual_patch: &str,
    report: &metrics::TokenMatchDebugReport,
) -> String {
    let mut html = String::new();

    let precision = report.metrics.precision() * 100.0;
    let recall = report.metrics.recall() * 100.0;
    let f1 = report.metrics.f1() * 100.0;

    let _ = write!(
        html,
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width, initial-scale=1" />
<title>Token Match Debug</title>
<style>
:root {{
  color-scheme: light dark;
  --bg: #0f1115;
  --panel: #161a22;
  --muted: #9ca3af;
  --text: #e5e7eb;
  --tp: #22c55e33;
  --fp: #ef444433;
  --fn: #f59e0b33;
  --border: #2a3140;
}}
* {{ box-sizing: border-box; }}
body {{
  margin: 0;
  font-family: ui-sans-serif, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
  background: var(--bg);
  color: var(--text);
}}
main {{
  max-width: 1400px;
  margin: 0 auto;
  padding: 20px;
}}
h1, h2, h3 {{
  margin: 0 0 10px;
}}
.meta {{
  color: var(--muted);
  margin-bottom: 16px;
}}
.grid {{
  display: grid;
  gap: 16px;
}}
.grid.two {{
  grid-template-columns: repeat(2, minmax(0, 1fr));
}}
.panel {{
  border: 1px solid var(--border);
  background: var(--panel);
  border-radius: 10px;
  padding: 12px;
}}
.metrics {{
  display: flex;
  gap: 18px;
  flex-wrap: wrap;
}}
.metric {{
  min-width: 160px;
}}
.metric .label {{
  color: var(--muted);
  font-size: 12px;
}}
.metric .value {{
  font-weight: 700;
  font-size: 20px;
}}
pre {{
  white-space: pre-wrap;
  word-break: break-word;
  margin: 0;
  font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
  font-size: 12px;
  line-height: 1.5;
}}
.legend {{
  display: flex;
  gap: 12px;
  font-size: 12px;
  color: var(--muted);
  flex-wrap: wrap;
}}
.swatch {{
  display: inline-block;
  width: 12px;
  height: 12px;
  border-radius: 2px;
  margin-right: 4px;
  vertical-align: middle;
}}
.tp {{ background: var(--tp); }}
.fp {{ background: var(--fp); }}
.fn {{ background: var(--fn); }}
.token.tp {{ background: var(--tp); }}
.token.fp {{ background: var(--fp); }}
.token.fn {{ background: var(--fn); }}
.token {{
  border-radius: 3px;
}}
.section-title {{
  margin-bottom: 8px;
  color: var(--muted);
  font-size: 12px;
  text-transform: uppercase;
  letter-spacing: 0.05em;
}}
</style>
</head>
<body>
<main>
  <h1>Token Match Debug</h1>
  <p class="meta">Example: {example_name} · Prediction #{prediction_index} · Expected Patch #{expected_index}</p>

  <section class="panel">
    <div class="metrics">
      <div class="metric"><div class="label">Precision</div><div class="value">{precision:.1}%</div></div>
      <div class="metric"><div class="label">Recall</div><div class="value">{recall:.1}%</div></div>
      <div class="metric"><div class="label">F1</div><div class="value">{f1:.1}%</div></div>
      <div class="metric"><div class="label">TP</div><div class="value">{tp}</div></div>
      <div class="metric"><div class="label">FP</div><div class="value">{fp}</div></div>
      <div class="metric"><div class="label">FN</div><div class="value">{fn}</div></div>
    </div>
    <div class="legend" style="margin-top: 10px;">
      <span><span class="swatch tp"></span>True Positive</span>
      <span><span class="swatch fp"></span>False Positive</span>
      <span><span class="swatch fn"></span>False Negative</span>
    </div>
  </section>

  <div class="grid two" style="margin-top: 16px;">
    <section class="panel">
      <div class="section-title">Expected patch</div>
      <pre>{expected_patch}</pre>
    </section>
    <section class="panel">
      <div class="section-title">Actual patch</div>
      <pre>{actual_patch}</pre>
    </section>
  </div>

  <div class="grid two" style="margin-top: 16px;">
    <section class="panel">
      <h3>Deleted-side token alignment</h3>
      <div class="section-title">Expected deleted text</div>
      <pre>{expected_deleted_text}</pre>
      <div class="section-title" style="margin-top: 10px;">Actual deleted text</div>
      <pre>{actual_deleted_text}</pre>
      <div class="section-title" style="margin-top: 10px;">Expected deleted tokens (FN highlighted)</div>
      <pre>{deleted_expected_tokens}</pre>
      <div class="section-title" style="margin-top: 10px;">Actual deleted tokens (FP highlighted)</div>
      <pre>{deleted_actual_tokens}</pre>
    </section>

    <section class="panel">
      <h3>Inserted-side token alignment</h3>
      <div class="section-title">Expected inserted text</div>
      <pre>{expected_inserted_text}</pre>
      <div class="section-title" style="margin-top: 10px;">Actual inserted text</div>
      <pre>{actual_inserted_text}</pre>
      <div class="section-title" style="margin-top: 10px;">Expected inserted tokens (FN highlighted)</div>
      <pre>{inserted_expected_tokens}</pre>
      <div class="section-title" style="margin-top: 10px;">Actual inserted tokens (FP highlighted)</div>
      <pre>{inserted_actual_tokens}</pre>
    </section>
  </div>
</main>
</body>
</html>"#,
        example_name = escape_html(example_name),
        prediction_index = prediction_index,
        expected_index = expected_index,
        precision = precision,
        recall = recall,
        f1 = f1,
        tp = report.metrics.true_positives,
        fp = report.metrics.false_positives,
        fn = report.metrics.false_negatives,
        expected_patch = escape_html(expected_patch),
        actual_patch = escape_html(actual_patch),
        expected_deleted_text = escape_html(&report.expected_deleted_text),
        actual_deleted_text = escape_html(&report.actual_deleted_text),
        expected_inserted_text = escape_html(&report.expected_inserted_text),
        actual_inserted_text = escape_html(&report.actual_inserted_text),
        deleted_expected_tokens = render_classified_tokens(&report.deleted.expected_tokens),
        deleted_actual_tokens = render_classified_tokens(&report.deleted.actual_tokens),
        inserted_expected_tokens = render_classified_tokens(&report.inserted.expected_tokens),
        inserted_actual_tokens = render_classified_tokens(&report.inserted.actual_tokens),
    );

    html
}

fn render_classified_tokens(tokens: &[metrics::ClassifiedToken]) -> String {
    let mut result = String::new();
    for token in tokens {
        let class = match token.class {
            metrics::TokenClass::TruePositive => "tp",
            metrics::TokenClass::FalsePositive => "fp",
            metrics::TokenClass::FalseNegative => "fn",
        };
        let escaped = escape_html(&token.token);
        let _ = write!(result, r#"<span class="token {class}">{escaped}</span>"#);
    }
    result
}

fn escape_html(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    for character in input.chars() {
        match character {
            '&' => result.push_str("&amp;"),
            '<' => result.push_str("&lt;"),
            '>' => result.push_str("&gt;"),
            '"' => result.push_str("&quot;"),
            '\'' => result.push_str("&#39;"),
            _ => result.push(character),
        }
    }
    result
}
