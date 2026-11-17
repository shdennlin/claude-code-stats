use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, FixedOffset, Local, NaiveDate, TimeZone, Timelike, Utc};
use clap::Parser;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use walkdir::WalkDir;

static TZ: OnceLock<FixedOffset> = OnceLock::new();
fn tz() -> &'static FixedOffset {
    TZ.get().expect("tz not initialized")
}

const REPORT_HTML_TEMPLATE: &str = include_str!("../templates/report.html");

/// JSON for embedding inside a `<script>` element. Names in the data come from transcripts (project,
/// branch, command and skill names, titles), and `serde_json` leaves `<` alone, so a name containing
/// `</script>` would end the element and let the rest run as markup. These characters only ever occur
/// inside JSON string literals, where `\uXXXX` is the same character, so nothing is lost. U+2028 and
/// U+2029 are escaped as well: they were line terminators inside JS string literals before ES2019.
fn json_for_script<T: Serialize>(value: &T) -> Result<String> {
    Ok(serde_json::to_string(value)?
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029"))
}

#[derive(Parser, Debug)]
#[command(name = "cc-stats", about = "Claude Code usage analytics")]
struct Args {
    /// Path to Claude projects dir (e.g. a rsync backup of ~/.claude/projects)
    #[arg(long, visible_alias = "source", default_value = "~/.claude/projects")]
    dir: String,

    /// Output dir
    #[arg(short, long, default_value = "./out")]
    out: PathBuf,

    /// Gap threshold (minutes) for active time. 5 = dense typing only; 15 ≈ includes
    /// think/read time; 30+ ≈ includes long agent runs.
    #[arg(short = 'g', long, default_value_t = 15)]
    gap: u64,

    /// Filter from date (YYYY-MM-DD)
    #[arg(long)]
    from: Option<String>,

    /// Filter to date (YYYY-MM-DD)
    #[arg(long)]
    to: Option<String>,

    /// Filter by project substring (repeatable)
    #[arg(short, long)]
    project: Vec<String>,

    /// Output formats: json, csv, html, all
    #[arg(short, long, default_value = "all")]
    format: String,

    /// Open report.html after generating
    #[arg(long)]
    open: bool,

    /// Timezone: "local" (default), "UTC", or "+08:00" / "-05:00"
    #[arg(long, default_value = "local")]
    tz: String,

    /// Merge projects matching substring into one label. Repeatable.
    /// Forms: "myproject" (label=myproject) or "workspace/myproject=Project-A"
    #[arg(long, value_name = "PATTERN[=LABEL]")]
    merge: Vec<String>,

    /// HTML viewer default filter window (days from latest). 0 = full range.
    #[arg(long, default_value_t = 90)]
    default_window_days: i64,

    /// CLI scan window (days back from today). Overridden by --from.
    #[arg(long, default_value_t = 365)]
    days: i64,

    /// Scan ALL history (disables --days default).
    #[arg(long)]
    all: bool,

    /// Slash command to leave out of the runs view (repeatable, case-insensitive), on top of the
    /// built-in local commands such as /compact and /model. E.g. --run-exclude my-notes
    #[arg(long, value_name = "COMMAND")]
    run_exclude: Vec<String>,

    /// Strip PII for public demo: clears session.file / project.cwd; replaces
    /// session titles with "Session NNNN"; replaces non-universal git branches
    /// with "branch-NN" (keeps main/master/develop/HEAD).
    #[arg(long)]
    anonymize: bool,
}

#[derive(Debug, Clone)]
struct MergeRule {
    pattern: String,
    label: String,
}

fn parse_merges(args: &[String]) -> Vec<MergeRule> {
    args.iter()
        .map(|s| match s.split_once('=') {
            Some((p, l)) => MergeRule {
                pattern: p.trim().to_string(),
                label: l.trim().to_string(),
            },
            None => MergeRule {
                pattern: s.trim().to_string(),
                label: s.trim().to_string(),
            },
        })
        .collect()
}

fn apply_merge(name: &str, rules: &[MergeRule]) -> String {
    let lower = name.to_lowercase();
    for r in rules {
        if !r.pattern.is_empty() && lower.contains(&r.pattern.to_lowercase()) {
            return r.label.clone();
        }
    }
    name.to_string()
}

fn parse_tz(s: &str) -> Result<FixedOffset> {
    let s = s.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("local") {
        return Ok(*Local::now().offset());
    }
    if s.eq_ignore_ascii_case("utc") || s == "Z" {
        return Ok(FixedOffset::east_opt(0).unwrap());
    }
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        anyhow::bail!("empty tz");
    }
    let sign = match bytes[0] {
        b'+' => 1i32,
        b'-' => -1i32,
        _ => anyhow::bail!("tz must start with + or -, got {s}"),
    };
    let rest = &s[1..];
    let (h, m) = if let Some((h, m)) = rest.split_once(':') {
        (h.parse::<i32>()?, m.parse::<i32>()?)
    } else if rest.len() == 4 {
        (rest[0..2].parse::<i32>()?, rest[2..4].parse::<i32>()?)
    } else {
        (rest.parse::<i32>()?, 0)
    };
    FixedOffset::east_opt(sign * (h * 3600 + m * 60)).context("invalid tz offset (range)")
}

#[derive(Debug, Default, Serialize, Clone, Copy, PartialEq, Eq)]
struct Tokens {
    input: u64,
    output: u64,
    cache_create_5m: u64,
    cache_create_1h: u64,
    cache_read: u64,
    iterations: u64,
}

impl Tokens {
    fn total(&self) -> u64 {
        self.input + self.output + self.cache_create_5m + self.cache_create_1h + self.cache_read
    }
    fn add(&mut self, o: &Tokens) {
        self.input += o.input;
        self.output += o.output;
        self.cache_create_5m += o.cache_create_5m;
        self.cache_create_1h += o.cache_create_1h;
        self.cache_read += o.cache_read;
        self.iterations += o.iterations;
    }
}

// Standard first-party API prices, USD per million tokens, checked 2026-10-09.
// https://platform.claude.com/docs/en/about-claude/pricing
// Keep versions explicit: future models must not silently inherit old prices.
fn model_id(model: &str) -> String {
    let m = model
        .to_lowercase()
        .replace("claude-fabel-", "claude-fable-");
    // Accept dated snapshots and the documented latest aliases only.
    let mut id = m.as_str();
    if let Some(base) = id.strip_suffix("-latest") {
        id = base;
    }
    if let Some((base, date)) = id.rsplit_once('-') {
        if date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()) {
            id = base;
        }
    }
    id.to_string()
}

fn pricing(model: &str, prompt_tokens: u64) -> Option<(f64, f64, f64, f64, f64)> {
    let (input, output, read) = match model_id(model).as_str() {
        "claude-fable-5-2" | "claude-mythos-5-2" => (8.0, 40.0, 0.20),
        "claude-fable-5-1" | "claude-mythos-5-1" => (10.0, 50.0, 0.25),
        "claude-fable-5" | "claude-mythos-5" => (10.0, 50.0, 1.0),
        "claude-opus-5-5" => (4.0, 20.0, 0.20),
        "claude-opus-5" | "claude-opus-4-8" | "claude-opus-4-7" | "claude-opus-4-6"
        | "claude-opus-4-5" => (5.0, 25.0, 0.50),
        "claude-opus-4-1" | "claude-opus-4" => (15.0, 75.0, 1.50),
        "claude-sonnet-5-5" => (2.0, 10.0, 0.10),
        "claude-sonnet-5" => (2.0, 10.0, 0.20),
        "claude-sonnet-4-6" | "claude-sonnet-4-5" | "claude-sonnet-4" => (3.0, 15.0, 0.30),
        "claude-haiku-5-5" if prompt_tokens > 100_000 => (0.50, 2.50, 0.05),
        "claude-haiku-5-5" => (0.10, 0.50, 0.01),
        "claude-haiku-4-5" => (1.0, 5.0, 0.10),
        "claude-3-5-haiku" => (0.80, 4.0, 0.08),
        _ => return None,
    };
    Some((input, output, input * 1.25, input * 2.0, read))
}

fn cost_usd(t: &Tokens, model: &str) -> f64 {
    let prompt_tokens = t.input + t.cache_create_5m + t.cache_create_1h + t.cache_read;
    let Some((i, o, c5, c1, cr)) = pricing(model, prompt_tokens) else {
        return 0.0;
    };
    (t.input as f64 * i
        + t.output as f64 * o
        + t.cache_create_5m as f64 * c5
        + t.cache_create_1h as f64 * c1
        + t.cache_read as f64 * cr)
        / 1_000_000.0
}

fn usage_tokens(u: &Value) -> Tokens {
    let mut tokens = Tokens {
        input: u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0),
        output: u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0),
        cache_read: u
            .get("cache_read_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        ..Default::default()
    };
    // Total writes are authoritative when present; derive the 5m remainder
    // from the 1h split, including logs that omit the explicit 5m field.
    let total_write = u.get("cache_creation_input_tokens").and_then(Value::as_u64);
    let cc = u.get("cache_creation");
    let one_hour = cc
        .and_then(|v| v.get("ephemeral_1h_input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if let Some(total) = total_write {
        tokens.cache_create_1h = one_hour.min(total);
        tokens.cache_create_5m = total - tokens.cache_create_1h;
    } else {
        tokens.cache_create_1h = one_hour;
        tokens.cache_create_5m = cc
            .and_then(|v| v.get("ephemeral_5m_input_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
    }
    tokens
}

/// One advisor call inside an assistant turn (`usage.iterations[]` with `type: "advisor_message"`).
/// Claude Code leaves it out of the turn's top-level usage and prices it on the advisor's own model,
/// so it is carried separately and charged to that model.
#[derive(Debug, Clone)]
struct AdvisorUse {
    model: String,
    tokens: Tokens,
    cost_usd: f64,
}

fn is_advisor_iteration(iteration: &Value) -> bool {
    iteration.get("type").and_then(Value::as_str) == Some("advisor_message")
}

fn advisor_uses(u: &Value) -> Vec<AdvisorUse> {
    let Some(iterations) = u.get("iterations").and_then(Value::as_array) else {
        return Vec::new();
    };
    iterations
        .iter()
        .filter(|it| is_advisor_iteration(it))
        .map(|it| {
            let model = it
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            let tokens = usage_tokens(it);
            // No speed / inference_geo modifier here: those describe the host call, and an advisor
            // iteration does not report its own.
            let cost_usd = cost_usd(&tokens, &model);
            AdvisorUse {
                model,
                tokens,
                cost_usd,
            }
        })
        .collect()
}

fn usage_cost_usd(u: &Value, model: &str, tokens: &Tokens) -> f64 {
    // Iterations are individual API calls; Haiku's tier applies per call.
    // Only use the breakdown if it reconciles with the recorded totals.
    let mut total = *tokens;
    total.iterations = 0;
    let mut cost = cost_usd(tokens, model);
    if let Some(iterations) = u.get("iterations").and_then(Value::as_array) {
        let mut sum = Tokens::default();
        let mut iteration_cost = 0.0;
        // Advisor iterations are billed on their own model (see advisor_uses) and are not part of the
        // top-level totals, so only the host's own calls can reconcile with them.
        for iteration in iterations.iter().filter(|it| !is_advisor_iteration(it)) {
            let t = usage_tokens(iteration);
            sum.add(&t);
            iteration_cost += cost_usd(&t, model);
        }
        if iterations.iter().any(|it| !is_advisor_iteration(it)) && sum == total {
            cost = iteration_cost;
        }
    }
    // Fast mode doubles all token rates on the supported Opus versions.
    if u.get("speed").and_then(Value::as_str) == Some("fast")
        && matches!(
            model_id(model).as_str(),
            "claude-opus-5-5" | "claude-opus-5" | "claude-opus-4-8"
        )
    {
        cost *= 2.0;
    }
    if u.get("inference_geo").and_then(Value::as_str) == Some("us") {
        cost *= 1.1;
    }
    cost
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn costs_use_version_specific_rates_for_each_token_category() {
        let cases = [
            ("claude-opus-5", [5.0, 25.0, 6.25, 10.0, 0.5]),
            ("claude-opus-5-5", [4.0, 20.0, 5.0, 8.0, 0.2]),
            ("claude-opus-4-8", [5.0, 25.0, 6.25, 10.0, 0.5]),
            ("claude-opus-4-7", [5.0, 25.0, 6.25, 10.0, 0.5]),
            ("claude-opus-4-6", [5.0, 25.0, 6.25, 10.0, 0.5]),
            ("claude-opus-4-5-20251101", [5.0, 25.0, 6.25, 10.0, 0.5]),
            ("claude-opus-4-1-20250805", [15.0, 75.0, 18.75, 30.0, 1.5]),
            ("claude-opus-4-20250514", [15.0, 75.0, 18.75, 30.0, 1.5]),
            ("claude-sonnet-5", [2.0, 10.0, 2.5, 4.0, 0.2]),
            ("claude-sonnet-5-5", [2.0, 10.0, 2.5, 4.0, 0.1]),
            ("claude-sonnet-4-6", [3.0, 15.0, 3.75, 6.0, 0.3]),
            ("claude-sonnet-4-5-20250929", [3.0, 15.0, 3.75, 6.0, 0.3]),
            ("claude-sonnet-4-20250514", [3.0, 15.0, 3.75, 6.0, 0.3]),
            ("claude-fable-5", [10.0, 50.0, 12.5, 20.0, 1.0]),
            ("claude-fable-5-1", [10.0, 50.0, 12.5, 20.0, 0.25]),
            ("claude-fable-5-2", [8.0, 40.0, 10.0, 16.0, 0.2]),
            ("claude-mythos-5-2", [8.0, 40.0, 10.0, 16.0, 0.2]),
            ("claude-mythos-5", [10.0, 50.0, 12.5, 20.0, 1.0]),
            ("claude-mythos-5-1", [10.0, 50.0, 12.5, 20.0, 0.25]),
            ("claude-haiku-4-5-20251001", [1.0, 5.0, 1.25, 2.0, 0.1]),
            ("claude-3-5-haiku-20241022", [0.8, 4.0, 1.0, 1.6, 0.08]),
        ];
        for (model, rates) in cases {
            for (category, rate) in rates.into_iter().enumerate() {
                let mut t = Tokens::default();
                match category {
                    0 => t.input = 1,
                    1 => t.output = 1,
                    2 => t.cache_create_5m = 1,
                    3 => t.cache_create_1h = 1,
                    _ => t.cache_read = 1,
                }
                assert!(
                    (cost_usd(&t, model) - rate / 1_000_000.0).abs() < 1e-12,
                    "{model} category {category}"
                );
            }
        }
    }

    #[test]
    fn advisor_iterations_are_priced_on_their_own_model_and_exclude_host_calls() {
        let u = serde_json::json!({"iterations": [
            {"type": "message", "input_tokens": 5, "output_tokens": 7},
            {"type": "advisor_message", "model": "claude-fable-5-2",
             "input_tokens": 200000, "output_tokens": 9000,
             "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0},
            {"type": "advisor_message", "model": "claude-opus-5-5",
             "input_tokens": 1000, "output_tokens": 100},
        ]});
        let a = advisor_uses(&u);
        assert_eq!(a.len(), 2, "message iterations are not advisor calls");
        assert_eq!(a[0].model, "claude-fable-5-2");
        assert_eq!((a[0].tokens.input, a[0].tokens.output), (200000, 9000));
        assert!((a[0].cost_usd - (200000.0 * 8.0 + 9000.0 * 40.0) / 1e6).abs() < 1e-12);
        assert!((a[1].cost_usd - (1000.0 * 4.0 + 100.0 * 20.0) / 1e6).abs() < 1e-12);
        assert!(advisor_uses(&serde_json::json!({"input_tokens": 1})).is_empty());
    }

    #[test]
    fn advisor_iteration_does_not_stop_host_iterations_reconciling_with_the_totals() {
        // Two host calls of 60k each: Haiku's tier is per call, so each is cheap even though the
        // turn's top-level prompt (120k) alone would fall in the expensive tier. The advisor call is
        // not part of the top-level totals and must not make the reconciliation fail.
        let u = serde_json::json!({"input_tokens": 120000, "output_tokens": 0, "iterations": [
            {"type": "message", "input_tokens": 60000, "output_tokens": 0},
            {"type": "advisor_message", "model": "claude-fable-5-2", "input_tokens": 300000},
            {"type": "message", "input_tokens": 60000, "output_tokens": 0},
        ]});
        let tokens = usage_tokens(&u);
        let cost = usage_cost_usd(&u, "claude-haiku-5-5", &tokens);
        assert!((cost - 120000.0 * 0.10 / 1e6).abs() < 1e-12, "got {cost}");
    }

    #[test]
    fn haiku_prompt_threshold_counts_both_cache_write_types_and_reads() {
        let mut t = Tokens {
            input: 1,
            output: 10,
            cache_create_5m: 10_000,
            cache_create_1h: 20_000,
            cache_read: 69_999,
            ..Default::default()
        };
        let low = (0.1 + 5.0 + 1250.0 + 4000.0 + 699.99) / 1_000_000.0;
        assert!((cost_usd(&t, "claude-haiku-5-5") - low).abs() < 1e-12);
        t.cache_read += 1;
        let high = (0.5 + 25.0 + 6250.0 + 20000.0 + 3500.0) / 1_000_000.0;
        assert!((cost_usd(&t, "claude-haiku-5-5") - high).abs() < 1e-12);
    }

    #[test]
    fn unknown_models_never_inherit_a_known_models_price() {
        let t = Tokens {
            input: 1_000_000,
            ..Default::default()
        };
        for model in [
            "",
            "<synthetic>",
            "claude-opus-6",
            "claude-opus-5-9",
            "not-sonnet",
        ] {
            assert_eq!(cost_usd(&t, model), 0.0, "{model}");
        }
    }

    #[test]
    fn haiku_iterations_are_priced_as_separate_requests() {
        let v = serde_json::json!({
            "timestamp": "2026-10-09T00:00:00Z", "type": "assistant",
            "message": {"model": "claude-haiku-5-5", "usage": {
                "input_tokens": 120000, "output_tokens": 20,
                "iterations": [
                    {"input_tokens": 60000, "output_tokens": 10},
                    {"input_tokens": 60000, "output_tokens": 10}
                ]
            }}
        });
        let e = parse_event(&v).unwrap();
        assert!((e.cost_usd - 0.01201).abs() < 1e-12);
    }

    #[test]
    fn recorded_fast_and_us_inference_modifiers_stack() {
        let v = serde_json::json!({
            "timestamp": "2026-10-09T00:00:00Z", "type": "assistant",
            "message": {"model": "claude-opus-5-5", "usage": {
                "input_tokens": 1000000, "output_tokens": 1000000,
                "cache_read_input_tokens": 1000000,
                "speed": "fast", "inference_geo": "us"
            }}
        });
        let e = parse_event(&v).unwrap();
        assert!((e.cost_usd - 53.24).abs() < 1e-12);
    }

    #[test]
    fn unsupported_fast_mode_does_not_raise_opus_47_price() {
        let u = serde_json::json!({"input_tokens": 1000000, "speed": "fast"});
        assert_eq!(
            usage_cost_usd(&u, "claude-opus-4-7", &usage_tokens(&u)),
            5.0
        );
    }

    #[test]
    fn incomplete_iteration_breakdown_keeps_top_level_usage() {
        let u = serde_json::json!({"input_tokens": 120000, "iterations": [
            {"input_tokens": 60000}
        ]});
        assert_eq!(
            usage_cost_usd(&u, "claude-haiku-5-5", &usage_tokens(&u)),
            0.06
        );
    }

    #[test]
    fn model_aliases_and_snapshots_keep_their_versions() {
        assert_eq!(
            pricing("CLAUDE-OPUS-5-5-20261001", 0),
            pricing("claude-opus-5-5", 0)
        );
        assert_eq!(
            pricing("claude-sonnet-4-5-latest", 0),
            pricing("claude-sonnet-4-5", 0)
        );
        assert_eq!(pricing("claude-opus-5-50", 0), None);
    }

    #[test]
    fn pricing_recognizes_fable_models() {
        assert_eq!(
            pricing("claude-fable-5", 0),
            Some((10.0, 50.0, 12.5, 20.0, 1.0))
        );
        assert_eq!(
            pricing("claude-fabel-5", 0),
            Some((10.0, 50.0, 12.5, 20.0, 1.0))
        );
    }
}

#[derive(Debug)]
struct Event {
    billing_key: Option<String>,
    usage: Option<Value>,
    cost_usd: f64,
    dt_utc: DateTime<Utc>,
    model: Option<String>,
    skill: Option<String>,
    plugin: Option<String>,
    git_branch: Option<String>,
    stop_reason: Option<String>,
    content_types: Vec<String>, // assistant content block types: thinking/text/tool_use
    tools: Vec<String>,         // tool_use names within this assistant turn
    // `tokens` / `cost_usd` are the host call's usage until `deduplicate_usage` has run; afterwards they
    // also include `advisor`, with the host-only figures kept in `host_*` for per-model attribution.
    tokens: Tokens,
    advisor: Vec<AdvisorUse>,
    host_tokens: Tokens,
    host_cost_usd: f64,
    // Run reconstruction (see `build_runs`): what this transcript line is, not what it cost.
    role: Role,
    command: Option<String>, // slash command typed in a human prompt, e.g. "spectra-apply"
    skills: Vec<String>,     // Skill tool_use inputs in this assistant message
    interrupted: bool,       // "[Request interrupted by user...]" marker
    sidechain: bool,         // subagent traffic: never part of a main-session run
    uuid: Option<String>,
}

/// Role of a transcript line for run reconstruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Other,       // attachments, system events, metadata: no bearing on a run
    Assistant,   // the agent working
    HumanPrompt, // starts a run
    UserOther,   // tool results, interrupt markers, task notifications: the run continues
}

impl Event {
    /// Rows for per-model attribution: (model, messages, tokens, cost). The host model gets its own usage
    /// and one message; each advisor model gets the advisor call's usage and no message.
    fn model_usage(&self) -> Vec<(Option<String>, u64, Tokens, f64)> {
        let mut rows = vec![(self.model.clone(), 1, self.host_tokens, self.host_cost_usd)];
        rows.extend(
            self.advisor
                .iter()
                .map(|a| (Some(a.model.clone()), 0, a.tokens, a.cost_usd)),
        );
        rows
    }
}

#[derive(Debug, Default, Serialize, Clone)]
struct ProjectStat {
    name: String,
    cwd: Option<String>,
    active_sec: u64,
    active_sec_union: u64,
    total_sec: u64,
    sessions: u64,
    messages: u64,
    tokens: Tokens,
    cost_usd: f64,
    first: Option<String>,
    last: Option<String>,
    by_hour: Vec<u64>,               // length 24, active_sec per scan-tz hour
    by_weekday: Vec<u64>,            // length 7, Mon..Sun
    by_skill: BTreeMap<String, u64>, // skill -> message count
    by_tool: BTreeMap<String, u64>,  // tool name -> count
}

#[derive(Debug, Default, Serialize, Clone)]
struct SessionModelAttr {
    messages: u64,
    tokens: Tokens,
    cost_usd: f64,
}

#[derive(Debug, Default, Serialize, Clone)]
struct SessionBranchAttr {
    active_sec: u64,
    messages: u64,
    tokens: Tokens,
    cost_usd: f64,
}

#[derive(Debug, Default, Serialize, Clone)]
struct SessionStat {
    project: String,
    session_id: String,
    title: Option<String>,
    file: String,
    start: String,
    end: String,
    active_sec: u64,
    total_sec: u64,
    messages: u64,
    tokens: Tokens,
    cost_usd: f64,
    models: Vec<String>,
    by_hour: Vec<u64>,                              // 24, active_sec
    by_hour_output: Vec<u64>, // 24, output tokens per scan-tz hour of the event
    by_hour_cost: Vec<f64>,   // 24, estimated cost per scan-tz hour of the event
    by_weekday: Vec<u64>,     // 7, active_sec
    by_skill: BTreeMap<String, u64>, // skill -> messages
    by_tool: BTreeMap<String, u64>, // tool -> uses
    by_mcp_server: BTreeMap<String, u64>, // mcp server -> uses
    by_content_type: BTreeMap<String, u64>, // type -> count
    by_stop_reason: BTreeMap<String, u64>, // reason -> count
    by_branch: BTreeMap<String, SessionBranchAttr>, // branch -> {active_sec, msgs, tokens, cost}
    by_model: BTreeMap<String, SessionModelAttr>, // model -> {msgs, tokens, cost}
}

#[derive(Debug, Serialize, Clone)]
struct SessionDay {
    date: String,
    #[serde(flatten)]
    session: SessionStat,
}

// Retain every event for activity/tool metrics, but charge API usage only once.
// Syrtis/tokscale-core uses message.id + requestId and per-field maxima.
fn deduplicate_usage(sessions: &mut [(PathBuf, SessionMeta, Vec<Event>)]) {
    let mut snapshots: BTreeMap<String, (usize, usize, Tokens, Value)> = BTreeMap::new();
    for (si, (_, _, events)) in sessions.iter().enumerate() {
        for (ei, e) in events.iter().enumerate() {
            let (Some(key), Some(usage)) = (&e.billing_key, &e.usage) else {
                continue;
            };
            let snapshot = snapshots
                .entry(key.clone())
                .or_insert_with(|| (si, ei, e.tokens, usage.clone()));
            let first = &sessions[snapshot.0].2[snapshot.1];
            if e.dt_utc < first.dt_utc {
                snapshot.0 = si;
                snapshot.1 = ei;
            }
            // Billing modifiers can arrive without increasing token counts.
            // Preserve known values when a more complete usage omits them.
            let modifiers: Vec<_> = ["speed", "inference_geo"]
                .into_iter()
                .filter_map(|key| {
                    let valid = |value: &Value| {
                        value
                            .as_str()
                            .filter(|v| match key {
                                "speed" => matches!(*v, "fast" | "standard"),
                                _ => matches!(*v, "us" | "global"),
                            })
                            .map(str::to_string)
                    };
                    usage
                        .get(key)
                        .and_then(valid)
                        .or_else(|| snapshot.3.get(key).and_then(valid))
                        .map(|value| (key, value))
                })
                .collect();
            if e.tokens.output > snapshot.2.output {
                snapshot.3 = usage.clone();
            }
            if let Some(object) = snapshot.3.as_object_mut() {
                for (key, value) in modifiers {
                    object.insert(key.to_string(), Value::String(value));
                }
            }
            let t = &mut snapshot.2;
            t.input = t.input.max(e.tokens.input);
            t.output = t.output.max(e.tokens.output);
            // Later snapshots can add TTL details without adding cache writes.
            let cache_write_total = (t.cache_create_5m + t.cache_create_1h)
                .max(e.tokens.cache_create_5m + e.tokens.cache_create_1h);
            t.cache_create_1h = t
                .cache_create_1h
                .max(e.tokens.cache_create_1h)
                .min(cache_write_total);
            t.cache_create_5m = cache_write_total - t.cache_create_1h;
            t.cache_read = t.cache_read.max(e.tokens.cache_read);
            t.iterations = t.iterations.max(e.tokens.iterations);
        }
    }
    for (_, _, events) in sessions.iter_mut() {
        for e in events {
            if e.billing_key.is_some() && e.usage.is_some() {
                e.tokens = Tokens::default();
                e.cost_usd = 0.0;
                e.advisor.clear();
            }
        }
    }
    for (_, (si, ei, tokens, usage)) in snapshots {
        let e = &mut sessions[si].2[ei];
        e.tokens = tokens;
        e.cost_usd = usage_cost_usd(&usage, e.model.as_deref().unwrap_or(""), &tokens);
        e.advisor = advisor_uses(&usage);
    }
    // Charge the advisor calls once, on top of the host call: totals (session, day, hour, branch, ...)
    // include them, while `host_*` keeps the host-only usage for the per-model views.
    for (_, _, events) in sessions.iter_mut() {
        for e in events {
            e.host_tokens = e.tokens;
            e.host_cost_usd = e.cost_usd;
            for a in &e.advisor {
                e.tokens.add(&a.tokens);
                e.cost_usd += a.cost_usd;
            }
        }
    }
}

// Split active intervals at hour boundaries, including midnight in scan TZ.
fn active_intervals(
    events: &[Event],
    gap_sec: u64,
) -> Vec<(DateTime<Utc>, DateTime<Utc>, Option<String>)> {
    let mut intervals = Vec::new();
    for pair in events.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        let gap = (b.dt_utc - a.dt_utc).num_seconds();
        if gap <= 0 || gap as u64 > gap_sec {
            continue;
        }
        // Work in whole seconds, matching the existing gap metric. Quantize
        // once before splitting so two half-seconds across midnight don't vanish.
        let mut start = Utc.timestamp_opt(a.dt_utc.timestamp(), 0).single().unwrap();
        let interval_end = start + chrono::Duration::seconds(gap);
        while start < interval_end {
            let local = start.with_timezone(tz());
            let hour_start = local
                .with_minute(0)
                .unwrap()
                .with_second(0)
                .unwrap()
                .with_nanosecond(0)
                .unwrap();
            let next_hour = (hour_start + chrono::Duration::hours(1)).with_timezone(&Utc);
            let end = interval_end.min(next_hour);
            intervals.push((start, end, a.git_branch.clone()));
            start = end;
        }
    }
    intervals
}

fn build_session_days(
    session: &SessionStat,
    events: &[Event],
    intervals: &[(DateTime<Utc>, DateTime<Utc>, Option<String>)],
) -> Vec<SessionDay> {
    let mut days: BTreeMap<String, SessionStat> = BTreeMap::new();
    let blank = || SessionStat {
        project: session.project.clone(),
        session_id: session.session_id.clone(),
        title: session.title.clone(),
        file: session.file.clone(),
        by_hour: vec![0; 24],
        by_hour_output: vec![0; 24],
        by_hour_cost: vec![0.0; 24],
        by_weekday: vec![0; 7],
        ..Default::default()
    };
    for e in events {
        let day = days.entry(date_key(e.dt_utc)).or_insert_with(&blank);
        let event_hour = e.dt_utc.with_timezone(tz()).hour() as usize;
        day.by_hour_output[event_hour] += e.tokens.output;
        day.by_hour_cost[event_hour] += e.cost_usd;
        let ts = fmt_dt(e.dt_utc);
        if day.start.is_empty() || ts < day.start {
            day.start = ts.clone();
        }
        if day.end.is_empty() || ts > day.end {
            day.end = ts;
        }
        day.messages += 1;
        day.tokens.add(&e.tokens);
        day.cost_usd += e.cost_usd;
        for (model, messages, tokens, cost) in e.model_usage() {
            let Some(model) = model else { continue };
            if !day.models.contains(&model) {
                day.models.push(model.clone());
            }
            let attr = day.by_model.entry(model).or_default();
            attr.messages += messages;
            attr.tokens.add(&tokens);
            attr.cost_usd += cost;
        }
        if let Some(skill) = &e.skill {
            *day.by_skill.entry(skill.clone()).or_default() += 1;
        }
        for tool in &e.tools {
            *day.by_tool.entry(tool.clone()).or_default() += 1;
            if let Some(rest) = tool.strip_prefix("mcp__") {
                let server = rest.split("__").next().unwrap_or("");
                if !server.is_empty() {
                    *day.by_mcp_server.entry(server.to_string()).or_default() += 1;
                }
            }
        }
        for ct in &e.content_types {
            *day.by_content_type.entry(ct.clone()).or_default() += 1;
        }
        if let Some(stop) = &e.stop_reason {
            *day.by_stop_reason.entry(stop.clone()).or_default() += 1;
        }
        if let Some(branch) = &e.git_branch {
            let attr = day.by_branch.entry(branch.clone()).or_default();
            attr.messages += 1;
            attr.tokens.add(&e.tokens);
            attr.cost_usd += e.cost_usd;
        }
    }
    for (start, end, branch) in intervals {
        let day = days.entry(date_key(*start)).or_insert_with(&blank);
        let secs = (*end - *start).num_seconds().max(0) as u64;
        day.active_sec += secs;
        let local = start.with_timezone(tz());
        day.by_hour[local.hour() as usize] += secs;
        day.by_weekday[local.weekday().num_days_from_monday() as usize] += secs;
        if let Some(branch) = branch {
            day.by_branch.entry(branch.clone()).or_default().active_sec += secs;
        }
        let ts = fmt_dt(*start);
        let te = fmt_dt(*end);
        if day.start.is_empty() || ts < day.start {
            day.start = ts;
        }
        if day.end.is_empty() || te > day.end {
            day.end = te;
        }
    }
    // Split elapsed session wall time by calendar day, including idle periods.
    let start = DateTime::parse_from_rfc3339(&session.start)
        .unwrap()
        .with_timezone(&Utc);
    let end = DateTime::parse_from_rfc3339(&session.end)
        .unwrap()
        .with_timezone(&Utc);
    let mut cursor = start;
    while cursor < end {
        let local = cursor.with_timezone(tz());
        let next = local
            .date_naive()
            .succ_opt()
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        let boundary = tz()
            .from_local_datetime(&next)
            .single()
            .unwrap()
            .with_timezone(&Utc);
        let stop = end.min(boundary);
        let day = days.entry(date_key(cursor)).or_insert_with(&blank);
        day.total_sec += (stop - cursor).num_seconds().max(0) as u64;
        if day.start.is_empty() {
            day.start = fmt_dt(cursor);
            day.end = fmt_dt(stop);
        }
        cursor = stop;
    }
    days.into_iter()
        .map(|(date, session)| SessionDay { date, session })
        .collect()
}

#[derive(Debug, Default, Serialize, Clone)]
struct DailyStat {
    date: String,
    active_sec: u64,       // sum across sessions (overlaps double-counted)
    active_sec_union: u64, // wall-clock union (overlapping sessions deduped)
    sessions: u64,
    messages: u64,
    tokens: Tokens,
    cost_usd: f64,
    by_project: BTreeMap<String, u64>, // project -> active_sec (sum)
    union_by_project: BTreeMap<String, u64>, // project -> active_sec (union within day)
}

#[derive(Debug, Default, Serialize, Clone)]
struct NamedStat {
    name: String,
    messages: u64,
    tokens: Tokens,
    cost_usd: f64,
}

#[derive(Debug, Default, Serialize, Clone)]
struct ModelStat {
    name: String,
    messages: u64,
    tokens: Tokens,
    cost_usd: f64,
}

#[derive(Debug, Default, Serialize, Clone)]
struct HourStat {
    hour: u8, // 0-23 local
    messages: u64,
    active_sec: u64,
    output_tokens: u64, // deduplicated assistant output tokens
    cost_usd: f64,      // deduplicated API-equivalent cost estimate
}

#[derive(Debug, Default, Serialize, Clone)]
struct WeekdayStat {
    weekday: u8, // 0=Mon..6=Sun
    name: &'static str,
    messages: u64,
    active_sec: u64,
}

#[derive(Debug, Default, Serialize)]
struct Summary {
    generated_at: String,
    gap_threshold_min: u64,
    tz_offset: String,
    default_window_days: i64,
    earliest: Option<String>,
    latest: Option<String>,
    span_days: f64,
    projects: u64,
    sessions: u64,
    messages: u64,
    active_sec: u64,
    active_sec_union: u64,
    total_sec: u64,
    active_total_ratio: f64,
    avg_active_per_day_h: f64,
    tokens: Tokens,
    cost_usd: f64,
    cache_hit_rate: f64,
}

#[derive(Debug, Default, Serialize, Clone)]
struct NamedCount {
    name: String,
    count: u64,
}

#[derive(Debug, Default, Serialize, Clone)]
struct BranchStat {
    name: String,
    active_sec: u64,
    sessions: u64,
    messages: u64,
    tokens: Tokens,
    cost_usd: f64,
}

#[derive(Debug, Serialize)]
struct Report {
    summary: Summary,
    projects: Vec<ProjectStat>,
    sessions: Vec<SessionStat>,
    session_days: Vec<SessionDay>,
    daily: Vec<DailyStat>,
    by_model: Vec<ModelStat>,
    by_skill: Vec<NamedStat>,
    by_plugin: Vec<NamedStat>,
    by_tool: Vec<NamedCount>, // tool name -> messages-containing-this-tool count
    by_mcp_server: Vec<NamedCount>, // mcp__<server>__* grouped
    by_content_type: Vec<NamedCount>, // thinking / text / tool_use
    by_stop_reason: Vec<NamedCount>, // end_turn / tool_use / stop_sequence / ...
    by_branch: Vec<BranchStat>,
    by_hour: Vec<HourStat>,
    by_weekday: Vec<WeekdayStat>,
    runs: Vec<Run>,                  // one per human prompt that the agent answered
    by_trigger: Vec<TriggerStat>,    // runs grouped by slash command / first skill, auto-discovered
    runs_weekly: Vec<WeeklyTrigger>, // week x trigger
    runs_weekly_total: Vec<WeeklyTotal>, // week: agent hours, wall-clock hours, parallelism
}

fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = dirs_home() {
            return home.join(rest);
        }
    }
    PathBuf::from(p)
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn decode_project_folder(name: &str) -> String {
    // Claude Code encodes cwd by replacing '/' with '-' in the folder name.
    // We crudely reverse it; dashes inside the original path component cannot
    // be distinguished from path separators, so we prefer the real cwd from
    // the first message in the JSONL when available.
    name.replace('-', "/")
}

/// Text of a user message: the string itself, or the text blocks joined.
fn user_text(content: &Value) -> String {
    match content {
        Value::String(t) => t.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// `<command-name>/spectra-apply</command-name>` -> "spectra-apply".
fn command_name(text: &str) -> Option<String> {
    let rest = &text[text.find("<command-name>")? + "<command-name>".len()..];
    let name = rest[..rest.find("</command-name>")?]
        .trim()
        .trim_start_matches('/');
    (!name.is_empty()).then(|| name.to_string())
}

/// Classify a user line. Returns (role, interrupted).
fn classify_user(v: &Value) -> (Role, bool) {
    let content = v.pointer("/message/content").unwrap_or(&Value::Null);
    let text = user_text(content);
    let interrupted = text.trim_start().starts_with("[Request interrupted");
    let meta = v.get("isMeta").and_then(Value::as_bool).unwrap_or(false);
    if meta || interrupted {
        return (Role::UserOther, interrupted);
    }
    match content {
        Value::String(t) => {
            let machine = t.starts_with("<task-notification") || t.starts_with("<local-command");
            (
                if machine {
                    Role::UserOther
                } else {
                    Role::HumanPrompt
                },
                false,
            )
        }
        Value::Array(blocks) => {
            let kind = |k: &str| {
                blocks
                    .iter()
                    .any(|b| b.get("type").and_then(Value::as_str) == Some(k))
            };
            if kind("tool_result") {
                return (Role::UserOther, false);
            }
            let human_origin = v.pointer("/origin/kind").and_then(Value::as_str) == Some("human");
            let typed = human_origin || kind("text") || kind("image");
            (
                if typed {
                    Role::HumanPrompt
                } else {
                    Role::UserOther
                },
                false,
            )
        }
        _ => (Role::UserOther, false),
    }
}

fn parse_event(v: &Value) -> Option<Event> {
    let ts = v.get("timestamp")?.as_str()?;
    let dt = DateTime::parse_from_rfc3339(ts).ok()?.with_timezone(&Utc);
    let typ = v.get("type")?.as_str()?;

    let mut tokens = Tokens::default();
    let mut event_cost = 0.0;
    let mut model = None;
    let mut stop_reason = None;
    let mut content_types: Vec<String> = Vec::new();
    let mut tools: Vec<String> = Vec::new();
    let mut skills: Vec<String> = Vec::new();
    let skill = v
        .get("attributionSkill")
        .and_then(|x| x.as_str())
        .map(String::from);
    let plugin = v
        .get("attributionPlugin")
        .and_then(|x| x.as_str())
        .map(String::from);
    let git_branch = v
        .get("gitBranch")
        .and_then(|x| x.as_str())
        .map(String::from);

    if typ == "assistant" {
        if let Some(msg) = v.get("message") {
            model = msg.get("model").and_then(|x| x.as_str()).map(String::from);
            stop_reason = msg
                .get("stop_reason")
                .and_then(|x| x.as_str())
                .map(String::from);
            if let Some(content) = msg.get("content").and_then(|x| x.as_array()) {
                for c in content {
                    if let Some(ct) = c.get("type").and_then(|x| x.as_str()) {
                        content_types.push(ct.to_string());
                        if ct == "tool_use" {
                            if let Some(name) = c.get("name").and_then(|x| x.as_str()) {
                                tools.push(name.to_string());
                                if name == "Skill" {
                                    if let Some(sk) =
                                        c.pointer("/input/skill").and_then(Value::as_str)
                                    {
                                        skills.push(sk.to_string());
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if let Some(u) = msg.get("usage") {
                tokens = usage_tokens(u);
                event_cost = usage_cost_usd(u, model.as_deref().unwrap_or(""), &tokens);
                // iterations[] = number of model calls within this assistant turn
                tokens.iterations = u
                    .get("iterations")
                    .and_then(|x| x.as_array())
                    .map(|a| a.len() as u64)
                    .unwrap_or(1);
            }
        }
    }

    let billing_key = if typ == "assistant" {
        let msg_id = v.pointer("/message/id").and_then(Value::as_str);
        match (msg_id, v.get("requestId").and_then(Value::as_str)) {
            (Some(m), Some(r)) => Some(format!("message:{m}:request:{r}")),
            (Some(m), None) => Some(format!("message:{m}")),
            _ => v
                .get("uuid")
                .and_then(Value::as_str)
                .map(|id| format!("uuid:{id}")),
        }
    } else {
        None
    };
    let sidechain = v
        .get("isSidechain")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let (role, interrupted, command) = match typ {
        "assistant" => (Role::Assistant, false, None),
        "user" => {
            let (role, interrupted) = classify_user(v);
            let command = (role == Role::HumanPrompt)
                .then(|| {
                    v.pointer("/message/content")
                        .and_then(Value::as_str)
                        .and_then(command_name)
                })
                .flatten();
            (role, interrupted, command)
        }
        _ => (Role::Other, false, None),
    };
    Some(Event {
        role,
        command,
        skills,
        interrupted,
        sidechain,
        uuid: v.get("uuid").and_then(Value::as_str).map(String::from),
        billing_key,
        usage: v.pointer("/message/usage").cloned(),
        cost_usd: event_cost,
        dt_utc: dt,
        model,
        skill,
        plugin,
        git_branch,
        stop_reason,
        content_types,
        tools,
        tokens,
        advisor: v
            .pointer("/message/usage")
            .map(advisor_uses)
            .unwrap_or_default(),
        host_tokens: tokens,
        host_cost_usd: event_cost,
    })
}

struct SessionMeta {
    cwd: String,
    ai_title: Option<String>,
    custom_title: Option<String>,
}

fn process_jsonl(path: &Path) -> Option<(SessionMeta, Vec<Event>)> {
    let file = File::open(path).ok()?;
    let reader = BufReader::new(file);
    let mut events = Vec::new();
    let mut cwd: Option<String> = None;
    let mut ai_title: Option<String> = None;
    let mut custom_title: Option<String> = None;
    for line in reader.lines().flatten() {
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if cwd.is_none() {
            if let Some(c) = v.get("cwd").and_then(|x| x.as_str()) {
                cwd = Some(c.to_string());
            }
        }
        let typ = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
        if typ == "ai-title" {
            if let Some(t) = v.get("aiTitle").and_then(|x| x.as_str()) {
                ai_title = Some(t.to_string());
            }
        } else if typ == "custom-title" {
            if let Some(t) = v.get("customTitle").and_then(|x| x.as_str()) {
                custom_title = Some(t.to_string());
            }
        }
        if let Some(e) = parse_event(&v) {
            events.push(e);
        }
    }
    events.sort_by_key(|e| e.dt_utc);
    Some((
        SessionMeta {
            cwd: cwd.unwrap_or_default(),
            ai_title,
            custom_title,
        },
        events,
    ))
}

fn fmt_dt(dt: DateTime<Utc>) -> String {
    // RFC3339 with the configured tz offset; viewer can re-format client-side.
    dt.with_timezone(tz()).to_rfc3339()
}

fn date_key(dt: DateTime<Utc>) -> String {
    dt.with_timezone(tz()).format("%Y-%m-%d").to_string()
}

fn within_range(d: &str, from: &Option<NaiveDate>, to: &Option<NaiveDate>) -> bool {
    let nd = match NaiveDate::parse_from_str(d, "%Y-%m-%d") {
        Ok(x) => x,
        Err(_) => return true,
    };
    if let Some(f) = from {
        if nd < *f {
            return false;
        }
    }
    if let Some(t) = to {
        if nd > *t {
            return false;
        }
    }
    true
}

fn anonymize_report(r: &mut Report) {
    // session.file (full filesystem path) + session.title (potentially identifying)
    let mut title_counter = 0u64;
    let mut titles = BTreeMap::new();
    for session in &mut r.sessions {
        if session.title.is_some() {
            title_counter += 1;
            session.title = Some(format!("Session {:04}", title_counter));
        }
        titles.insert(session.file.clone(), session.title.clone());
        session.file.clear();
    }
    for day in &mut r.session_days {
        day.session.title = titles.get(&day.session.file).cloned().flatten();
        day.session.file.clear();
    }
    // project.cwd (raw cwd not affected by --merge)
    for p in &mut r.projects {
        p.cwd = None;
    }
    // branches: keep universal names (main/master/develop/HEAD), anonymize the rest
    let universal: std::collections::HashSet<&str> = ["main", "master", "develop", "HEAD", ""]
        .iter()
        .copied()
        .collect();
    let mut branch_counter = 0u64;
    for b in &mut r.by_branch {
        if !universal.contains(b.name.as_str()) {
            branch_counter += 1;
            b.name = format!("branch-{:02}", branch_counter);
        }
    }
}

fn compute_union(mut ivs: Vec<(DateTime<Utc>, DateTime<Utc>)>) -> u64 {
    if ivs.is_empty() {
        return 0;
    }
    ivs.sort_by_key(|x| x.0);
    let mut union = 0i64;
    let mut cur: Option<(DateTime<Utc>, DateTime<Utc>)> = None;
    for (s, e) in ivs {
        cur = Some(match cur {
            None => (s, e),
            Some((cs, ce)) if s <= ce => (cs, ce.max(e)),
            Some((cs, ce)) => {
                union += (ce - cs).num_seconds();
                (s, e)
            }
        });
    }
    if let Some((cs, ce)) = cur {
        union += (ce - cs).num_seconds();
    }
    union.max(0) as u64
}

fn matches_project(name: &str, cwd: &str, filters: &[String]) -> bool {
    if filters.is_empty() {
        return true;
    }
    let name_l = name.to_lowercase();
    let cwd_l = cwd.to_lowercase();
    filters.iter().any(|f| {
        let fl = f.to_lowercase();
        name_l.contains(&fl) || cwd_l.contains(&fl)
    })
}

// ---------------------------------------------------------------------------------------------
// Runs. One run is one stretch of agent work: a human prompt until the next human prompt.
// The trigger (typed slash command, else the first Skill the agent invoked, else a plain prompt)
// is discovered from the transcript itself, so a new skill shows up without any configuration.
// ---------------------------------------------------------------------------------------------

const PLAIN_PROMPT: &str = "(plain prompt)";

/// Built-in commands the harness answers locally. They are not agent work, so they never form a
/// run. Extendable with `--run-exclude`. (`goal`, `spectra-apply` and friends are agent work.)
const LOCAL_COMMANDS: &[&str] = &[
    "compact",
    "clear",
    "model",
    "plugin",
    "reload-plugins",
    "mcp",
    "config",
    "help",
    "status",
    "resume",
    "effort",
    "advisor",
    "agents",
    "hooks",
    "permissions",
    "login",
    "logout",
    "cost",
    "context",
    "memory",
    "doctor",
    "exit",
    "ide",
    "theme",
    "vim",
    "usage",
    "rewind",
    "export",
    "add-dir",
    "color",
    "rename",
    "fast",
    "stats",
];

#[derive(Debug, Clone, Serialize)]
struct Run {
    date: String, // scan-tz date of the prompt
    start: i64,   // prompt time, unix seconds
    dur_sec: u64, // active time: every gap between consecutive events is capped at `--gap`
    trigger: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    skills: Vec<String>, // every Skill the agent invoked during the run
    interrupts: u32, // "[Request interrupted by user]" markers inside the run
    ended_by_interrupt: bool,
    project: String,
    #[serde(skip)]
    uuid: Option<String>,
    #[serde(skip)]
    events: usize,
}

struct RunBuild {
    run: Run,
    segs: Vec<(i64, i64)>, // active segments in unix seconds, for concurrency
    session: String,       // "<project folder>/<session id>": ties subagent work to its parent run
}

/// Active time of one subagent transcript, tied to the main session that spawned it.
struct SubWork {
    session: String,
    segs: Vec<(i64, i64)>,
}

#[derive(Default)]
struct RunAcc {
    uuid: Option<String>,
    command: Option<String>,
    times: Vec<i64>,
    skills: Vec<String>,
    interrupts: u32,
    ended_by_interrupt: bool,
    assistants: usize,
}

/// Rebuild the runs of one transcript. Pure: no timezone, no globals.
fn build_runs(events: &[Event], gap_sec: i64, exclude: &[String]) -> Vec<RunBuild> {
    let mut out = Vec::new();
    let mut cur: Option<RunAcc> = None;
    let finish = |acc: Option<RunAcc>, out: &mut Vec<RunBuild>| {
        let Some(a) = acc else { return };
        if a.assistants == 0 {
            return; // nothing answered: a local command, or a prompt that was never run
        }
        let command = a.command.filter(|c| !c.is_empty());
        if let Some(c) = &command {
            let c = c.to_lowercase();
            if LOCAL_COMMANDS.contains(&c.as_str()) || exclude.iter().any(|x| x.to_lowercase() == c)
            {
                return;
            }
        }
        let trigger = command
            .clone()
            .or_else(|| a.skills.first().cloned())
            .unwrap_or_else(|| PLAIN_PROMPT.to_string());
        let mut dur = 0u64;
        let mut segs = Vec::new();
        for w in a.times.windows(2) {
            let step = (w[1] - w[0]).clamp(0, gap_sec);
            dur += step as u64;
            segs.push((w[0], w[0] + step));
        }
        let mut skills: Vec<String> = Vec::new();
        for s in a.skills {
            if s != trigger && !skills.contains(&s) {
                skills.push(s);
            }
        }
        let start = a.times[0];
        out.push(RunBuild {
            run: Run {
                date: String::new(),
                start,
                dur_sec: dur,
                trigger,
                skills,
                interrupts: a.interrupts,
                ended_by_interrupt: a.ended_by_interrupt,
                project: String::new(),
                uuid: a.uuid,
                events: a.times.len(),
            },
            segs,
            session: String::new(),
        });
    };
    for e in events {
        if e.sidechain || e.role == Role::Other {
            continue;
        }
        let t = e.dt_utc.timestamp();
        if e.role == Role::HumanPrompt {
            finish(cur.take(), &mut out);
            cur = Some(RunAcc {
                uuid: e.uuid.clone(),
                command: e.command.clone(),
                times: vec![t],
                ..Default::default()
            });
            continue;
        }
        let Some(a) = cur.as_mut() else { continue };
        a.times.push(t);
        a.ended_by_interrupt = e.interrupted;
        if e.interrupted {
            a.interrupts += 1;
        }
        if e.role == Role::Assistant {
            a.assistants += 1;
            a.skills.extend(e.skills.iter().cloned());
        }
    }
    finish(cur.take(), &mut out);
    out
}

/// A resumed session copies its history into a new file, so the same prompt can appear twice.
/// Keep the copy that saw more of the run.
fn dedupe_runs(runs: Vec<RunBuild>) -> Vec<RunBuild> {
    let mut by_uuid: BTreeMap<String, usize> = BTreeMap::new();
    let mut out: Vec<RunBuild> = Vec::new();
    for r in runs {
        match r.run.uuid.clone() {
            Some(u) => match by_uuid.get(&u) {
                Some(&i) => {
                    if r.run.events > out[i].run.events {
                        out[i] = r;
                    }
                }
                None => {
                    by_uuid.insert(u, out.len());
                    out.push(r);
                }
            },
            None => out.push(r),
        }
    }
    out
}

/// Nearest-rank percentile on an ascending slice. The viewer uses the same rule.
fn pct(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    sorted[((sorted.len() as f64 * p) as usize).min(sorted.len() - 1)]
}

fn week_start(date: &str) -> String {
    NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map(|d| {
            (d - chrono::Duration::days(d.weekday().num_days_from_monday() as i64)).to_string()
        })
        .unwrap_or_default()
}

#[derive(Debug, Serialize)]
struct TriggerStat {
    name: String,
    runs: u64,
    median_sec: u64,
    p90_sec: u64,
    p99_sec: u64,
    max_sec: u64,
    hours: f64,
    over_10m_pct: f64,
    interrupts: u64,
    interrupts_per_hour: f64,
    first_seen: String,
    last_seen: String,
    is_new: bool, // first seen in the last 14 days of the data
    runner: bool, // looks like "let it run": >= 5 runs and >= 30% of them over 10 minutes
}

#[derive(Debug, Serialize)]
struct WeeklyTrigger {
    week: String,
    trigger: String,
    runs: u64,
    median_sec: u64,
    p90_sec: u64,
    hours: f64,
    interrupts: u64,
}

#[derive(Debug, Serialize)]
struct WeeklyTotal {
    week: String,
    runs: u64,
    hours: f64,
    union_hours: f64, // wall-clock hours with at least one agent working
    avg_parallel: f64,
    peak_parallel: u32,
    // The same, counting subagents. `*_all` counts threads that are working: while a main session
    // has a subagent working, its own waiting time is not counted, so one subagent running alone is
    // 1, not 2.
    subagent_hours: f64,
    peak_subagents: u32,
    hours_all: f64,
    union_hours_all: f64,
    avg_parallel_all: f64,
    peak_parallel_all: u32,
}

fn summarize_triggers(runs: &[Run]) -> Vec<TriggerStat> {
    let latest = runs.iter().map(|r| r.date.as_str()).max().unwrap_or("");
    let mut groups: BTreeMap<&str, Vec<&Run>> = BTreeMap::new();
    for r in runs {
        groups.entry(r.trigger.as_str()).or_default().push(r);
    }
    let mut out: Vec<TriggerStat> = groups
        .into_iter()
        .map(|(name, rs)| {
            let mut d: Vec<u64> = rs.iter().map(|r| r.dur_sec).collect();
            d.sort_unstable();
            let hours = d.iter().sum::<u64>() as f64 / 3600.0;
            let interrupts: u64 = rs.iter().map(|r| r.interrupts as u64).sum();
            let over = d.iter().filter(|&&x| x > 600).count() as f64 * 100.0 / d.len() as f64;
            let first = rs
                .iter()
                .map(|r| r.date.as_str())
                .min()
                .unwrap_or("")
                .to_string();
            let last = rs
                .iter()
                .map(|r| r.date.as_str())
                .max()
                .unwrap_or("")
                .to_string();
            let is_new = match (
                NaiveDate::parse_from_str(&first, "%Y-%m-%d"),
                NaiveDate::parse_from_str(latest, "%Y-%m-%d"),
            ) {
                (Ok(f), Ok(l)) => (l - f).num_days() <= 14,
                _ => false,
            };
            TriggerStat {
                name: name.to_string(),
                runs: d.len() as u64,
                median_sec: pct(&d, 0.5),
                p90_sec: pct(&d, 0.9),
                p99_sec: pct(&d, 0.99),
                max_sec: *d.last().unwrap_or(&0),
                hours,
                over_10m_pct: over,
                interrupts,
                interrupts_per_hour: if hours > 0.0 {
                    interrupts as f64 / hours
                } else {
                    0.0
                },
                first_seen: first,
                last_seen: last,
                is_new,
                runner: d.len() >= 5 && over >= 30.0,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        b.hours
            .partial_cmp(&a.hours)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

fn weekly_triggers(runs: &[Run]) -> Vec<WeeklyTrigger> {
    let mut groups: BTreeMap<(String, &str), Vec<&Run>> = BTreeMap::new();
    for r in runs {
        groups
            .entry((week_start(&r.date), r.trigger.as_str()))
            .or_default()
            .push(r);
    }
    groups
        .into_iter()
        .map(|((week, trigger), rs)| {
            let mut d: Vec<u64> = rs.iter().map(|r| r.dur_sec).collect();
            d.sort_unstable();
            WeeklyTrigger {
                week,
                trigger: trigger.to_string(),
                runs: d.len() as u64,
                median_sec: pct(&d, 0.5),
                p90_sec: pct(&d, 0.9),
                hours: d.iter().sum::<u64>() as f64 / 3600.0,
                interrupts: rs.iter().map(|r| r.interrupts as u64).sum(),
            }
        })
        .collect()
}

/// Gap-capped active segments of one transcript, from every user/assistant line (a subagent's own
/// lines are all sidechain, so unlike `build_runs` nothing is skipped on that account).
fn agent_segments(events: &[Event], gap_sec: i64) -> Vec<(i64, i64)> {
    let times: Vec<i64> = events
        .iter()
        .filter(|e| e.role != Role::Other)
        .map(|e| e.dt_utc.timestamp())
        .collect();
    times
        .windows(2)
        .map(|w| (w[0], w[0] + (w[1] - w[0]).clamp(0, gap_sec)))
        .filter(|(a, z)| z > a)
        .collect()
}

/// `<project>/<session id>/subagents/agent-x.jsonl` -> the session id of the main transcript.
fn subagent_session(path: &Path) -> Option<String> {
    let parts: Vec<_> = path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();
    let i = parts.iter().position(|p| p == "subagents")?;
    (i > 0).then(|| parts[i - 1].clone())
}

/// Sorted, non-overlapping union of intervals.
fn merge_intervals(mut v: Vec<(i64, i64)>) -> Vec<(i64, i64)> {
    v.sort_unstable();
    let mut out: Vec<(i64, i64)> = Vec::new();
    for (a, z) in v {
        match out.last_mut() {
            Some(last) if a <= last.1 => last.1 = last.1.max(z),
            _ => out.push((a, z)),
        }
    }
    out
}

/// `seg` with every part covered by `cut` (sorted, non-overlapping) removed.
fn subtract_intervals(seg: (i64, i64), cut: &[(i64, i64)]) -> Vec<(i64, i64)> {
    let mut out = Vec::new();
    let mut from = seg.0;
    for &(a, z) in cut {
        if z <= from {
            continue;
        }
        if a >= seg.1 {
            break;
        }
        if a > from {
            out.push((from, a));
        }
        from = from.max(z);
    }
    if from < seg.1 {
        out.push((from, seg.1));
    }
    out
}

/// (active seconds summed over segments, seconds with at least one open, most open at once)
fn sweep(segs: &[(i64, i64)]) -> (i64, i64, i32) {
    let mut points: Vec<(i64, i32)> = Vec::new();
    let mut total = 0i64;
    for &(a, z) in segs {
        if z > a {
            points.push((a, 1));
            points.push((z, -1));
            total += z - a;
        }
    }
    // an end sorts before a start at the same instant, so back-to-back segments do not overlap
    points.sort_unstable();
    let (mut open, mut peak, mut union, mut last) = (0i32, 0i32, 0i64, 0i64);
    for (t, d) in points {
        if open > 0 {
            union += t - last;
        }
        open += d;
        peak = peak.max(open);
        last = t;
    }
    (total, union, peak.max(0))
}

fn week_of_epoch(t: i64, tz: &FixedOffset) -> String {
    DateTime::from_timestamp(t, 0)
        .map(|d| week_start(&d.with_timezone(tz).format("%Y-%m-%d").to_string()))
        .unwrap_or_default()
}

#[derive(Default)]
struct WeekSegs {
    runs: u64,
    main: Vec<(i64, i64)>, // main-session runs
    sub: Vec<(i64, i64)>,  // subagents
    all: Vec<(i64, i64)>, // threads actually working: main time is dropped while its own subagents work
}

/// Per week: agent hours, wall-clock hours with any agent working, and how many ran at once, for the
/// main sessions alone and again counting subagents. A segment belongs to the week it starts in.
fn weekly_totals(builds: &[RunBuild], subs: &[SubWork], tz: &FixedOffset) -> Vec<WeeklyTotal> {
    let mut cut: BTreeMap<&str, Vec<(i64, i64)>> = BTreeMap::new();
    for s in subs {
        cut.entry(s.session.as_str())
            .or_default()
            .extend(s.segs.iter().copied());
    }
    let cut: BTreeMap<&str, Vec<(i64, i64)>> = cut
        .into_iter()
        .map(|(k, v)| (k, merge_intervals(v)))
        .collect();
    let mut weeks: BTreeMap<String, WeekSegs> = BTreeMap::new();
    for b in builds {
        weeks.entry(week_start(&b.run.date)).or_default().runs += 1;
        let parent_cut = cut.get(b.session.as_str());
        for &(a, z) in b.segs.iter().filter(|(a, z)| z > a) {
            let w = weeks.entry(week_of_epoch(a, tz)).or_default();
            w.main.push((a, z));
            match parent_cut {
                Some(c) => w.all.extend(subtract_intervals((a, z), c)),
                None => w.all.push((a, z)),
            }
        }
    }
    for s in subs {
        for &(a, z) in s.segs.iter().filter(|(a, z)| z > a) {
            let w = weeks.entry(week_of_epoch(a, tz)).or_default();
            w.sub.push((a, z));
            w.all.push((a, z));
        }
    }
    let hours = |secs: i64| secs as f64 / 3600.0;
    let avg = |total: i64, union: i64| {
        if union > 0 {
            total as f64 / union as f64
        } else {
            0.0
        }
    };
    weeks
        .into_iter()
        .map(|(week, w)| {
            let (m_tot, m_uni, m_peak) = sweep(&w.main);
            let (a_tot, a_uni, a_peak) = sweep(&w.all);
            let (s_tot, _, s_peak) = sweep(&w.sub);
            WeeklyTotal {
                week,
                runs: w.runs,
                hours: hours(m_tot),
                union_hours: hours(m_uni),
                avg_parallel: avg(m_tot, m_uni),
                peak_parallel: m_peak as u32,
                subagent_hours: hours(s_tot),
                peak_subagents: s_peak as u32,
                hours_all: hours(a_tot),
                union_hours_all: hours(a_uni),
                avg_parallel_all: avg(a_tot, a_uni),
                peak_parallel_all: a_peak as u32,
            }
        })
        .collect()
}

fn run() -> Result<()> {
    let args = Args::parse();
    let parsed_tz = parse_tz(&args.tz).context("--tz parse")?;
    TZ.set(parsed_tz).expect("TZ init");
    let merges = parse_merges(&args.merge);

    let projects_dir = expand_tilde(&args.dir);
    if !projects_dir.is_dir() {
        anyhow::bail!("Projects dir not found: {}", projects_dir.display());
    }
    fs::create_dir_all(&args.out)?;
    let gap_sec = args.gap * 60;

    let mut from = args
        .from
        .as_deref()
        .map(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d"))
        .transpose()
        .context("invalid --from")?;
    let to = args
        .to
        .as_deref()
        .map(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d"))
        .transpose()
        .context("invalid --to")?;

    // default scan window: last N days unless --from set or --all
    if from.is_none() && !args.all {
        let today = Utc::now().with_timezone(tz()).date_naive();
        from = today.checked_sub_signed(chrono::Duration::days(args.days));
    }

    let mut loaded = Vec::new();
    for jsonl in WalkDir::new(&projects_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
    {
        if let Some((meta, events)) = process_jsonl(jsonl.path()) {
            loaded.push((jsonl.into_path(), meta, events));
        }
    }
    loaded.sort_by(|a, b| a.0.cmp(&b.0));
    deduplicate_usage(&mut loaded);

    // Per-project aggregates
    let mut projects: BTreeMap<String, ProjectStat> = BTreeMap::new();
    let mut sessions: Vec<SessionStat> = Vec::new();
    let mut session_days = Vec::new();
    let mut daily: BTreeMap<String, DailyStat> = BTreeMap::new();
    let mut by_model: BTreeMap<String, ModelStat> = BTreeMap::new();
    let mut by_skill: BTreeMap<String, NamedStat> = BTreeMap::new();
    let mut by_plugin: BTreeMap<String, NamedStat> = BTreeMap::new();
    let mut by_hour: BTreeMap<u8, HourStat> = BTreeMap::new();
    let mut by_weekday: BTreeMap<u8, WeekdayStat> = BTreeMap::new();
    let mut by_tool: BTreeMap<String, u64> = BTreeMap::new();
    let mut by_mcp_server: BTreeMap<String, u64> = BTreeMap::new();
    let mut by_content: BTreeMap<String, u64> = BTreeMap::new();
    let mut by_stop_reason: BTreeMap<String, u64> = BTreeMap::new();
    let mut by_branch: BTreeMap<String, BranchStat> = BTreeMap::new();

    let mut grand_active = 0u64;
    let mut grand_total = 0u64;
    let mut grand_msgs = 0u64;
    let mut grand_tokens = Tokens::default();
    let mut grand_cost = 0.0;
    let mut unpriced_models: BTreeMap<String, u64> = BTreeMap::new();
    // collect (start, end) intervals per day for union calculation
    let mut intervals_by_day: BTreeMap<String, Vec<(DateTime<Utc>, DateTime<Utc>)>> =
        BTreeMap::new();
    // per (project, day) intervals for per-project union
    let mut intervals_by_proj_day: BTreeMap<(String, String), Vec<(DateTime<Utc>, DateTime<Utc>)>> =
        BTreeMap::new();
    let mut earliest: Option<DateTime<Utc>> = None;
    let mut latest: Option<DateTime<Utc>> = None;

    let weekday_names = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

    let folder_key = |path: &Path| {
        path.strip_prefix(&projects_dir)
            .ok()
            .and_then(|p| p.components().next())
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .unwrap_or_default()
    };
    let mut folder_projects: BTreeMap<String, (String, Option<String>)> = BTreeMap::new();
    for (path, meta, events) in &loaded {
        if events.is_empty() {
            continue;
        }
        let folder = folder_key(path);
        let entry = folder_projects
            .entry(folder.clone())
            .or_insert_with(|| (apply_merge(&decode_project_folder(&folder), &merges), None));
        if entry.1.is_none() && !meta.cwd.is_empty() {
            *entry = (apply_merge(&meta.cwd, &merges), Some(meta.cwd.clone()));
        }
    }
    // Runs come from main-session transcripts only (a subagent's work is part of its parent's run),
    // filtered by the same project and date window as the sessions.
    let mut run_builds: Vec<RunBuild> = Vec::new();
    for (path, meta, events) in &loaded {
        if events.is_empty() || path.components().any(|c| c.as_os_str() == "subagents") {
            continue;
        }
        let (project_name, _) = folder_projects.get(&folder_key(path)).unwrap().clone();
        if !matches_project(&project_name, &meta.cwd, &args.project) {
            continue;
        }
        for mut b in build_runs(events, gap_sec as i64, &args.run_exclude) {
            let Some(start) = DateTime::from_timestamp(b.run.start, 0) else {
                continue;
            };
            b.run.date = date_key(start);
            if !within_range(&b.run.date, &from, &to) {
                continue;
            }
            b.run.project = project_name.clone();
            b.session = format!(
                "{}/{}",
                folder_key(path),
                path.file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default()
            );
            run_builds.push(b);
        }
    }
    // Subagent transcripts: same project and date filters, tied to the session that spawned them.
    let mut sub_work: Vec<SubWork> = Vec::new();
    for (path, meta, events) in &loaded {
        let Some(parent) = subagent_session(path) else {
            continue;
        };
        if events.is_empty() {
            continue;
        }
        let (project_name, _) = folder_projects.get(&folder_key(path)).unwrap().clone();
        if !matches_project(&project_name, &meta.cwd, &args.project) {
            continue;
        }
        let segs: Vec<(i64, i64)> = agent_segments(events, gap_sec as i64)
            .into_iter()
            .filter(|&(a, _)| {
                DateTime::from_timestamp(a, 0)
                    .is_some_and(|d| within_range(&date_key(d), &from, &to))
            })
            .collect();
        if !segs.is_empty() {
            sub_work.push(SubWork {
                session: format!("{}/{}", folder_key(path), parent),
                segs,
            });
        }
    }
    let mut run_builds = dedupe_runs(run_builds);
    run_builds.sort_by_key(|b| b.run.start);

    for (jsonl_path, sess_meta, mut events) in loaded {
        let folder = folder_key(&jsonl_path);
        let cwd = &sess_meta.cwd;
        if events.is_empty() {
            continue;
        }
        let (project_name, project_cwd) = folder_projects.get(&folder).unwrap().clone();
        if !matches_project(&project_name, cwd, &args.project) {
            continue;
        }
        let mut intervals = active_intervals(&events, gap_sec);
        intervals.retain(|(start, _, _)| within_range(&date_key(*start), &from, &to));
        events.retain(|e| within_range(&date_key(e.dt_utc), &from, &to));
        if events.is_empty() && intervals.is_empty() {
            continue;
        }
        let start = events
            .first()
            .map(|e| e.dt_utc)
            .into_iter()
            .chain(intervals.first().map(|i| i.0))
            .min()
            .unwrap();
        let end = events
            .last()
            .map(|e| e.dt_utc)
            .into_iter()
            .chain(intervals.last().map(|i| i.1))
            .max()
            .unwrap();
        let total = (end - start).num_seconds().max(0) as u64;
        let mut active = 0u64;
        let mut sess_tokens = Tokens::default();
        let mut sess_cost = 0.0;
        let mut sess_models: BTreeMap<String, ()> = BTreeMap::new();
        let mut sess_msgs = 0u64;
        let mut sess_by_hour = [0u64; 24];
        let mut sess_by_hour_output = [0u64; 24];
        let mut sess_by_hour_cost = [0f64; 24];
        let mut sess_by_weekday = [0u64; 7];
        let mut sess_by_skill: BTreeMap<String, u64> = BTreeMap::new();
        let mut sess_by_tool: BTreeMap<String, u64> = BTreeMap::new();
        let mut sess_by_branch: BTreeMap<String, (u64, u64, Tokens, f64)> = BTreeMap::new();
        let mut sess_by_mcp: BTreeMap<String, u64> = BTreeMap::new();
        let mut sess_by_content: BTreeMap<String, u64> = BTreeMap::new();
        let mut sess_by_stop: BTreeMap<String, u64> = BTreeMap::new();
        let mut sess_by_model_attr: BTreeMap<String, SessionModelAttr> = BTreeMap::new();

        for (seg_start, seg_end, branch) in &intervals {
            let gap = (*seg_end - *seg_start).num_seconds().max(0) as u64;
            active += gap;
            // bucket per day / hour / weekday by start ts
            let dk = date_key(*seg_start);
            let day = daily.entry(dk.clone()).or_insert_with(|| DailyStat {
                date: dk.clone(),
                ..Default::default()
            });
            day.active_sec += gap;
            *day.by_project.entry(project_name.clone()).or_insert(0) += gap;
            intervals_by_day
                .entry(dk.clone())
                .or_default()
                .push((*seg_start, *seg_end));
            intervals_by_proj_day
                .entry((project_name.clone(), dk.clone()))
                .or_default()
                .push((*seg_start, *seg_end));

            let local = seg_start.with_timezone(tz());
            let hour = local.hour() as u8;
            let h = by_hour.entry(hour).or_insert(HourStat {
                hour,
                ..Default::default()
            });
            h.active_sec += gap;
            sess_by_hour[hour as usize] += gap;

            let wd = local.weekday().num_days_from_monday() as u8;
            let w = by_weekday.entry(wd).or_insert(WeekdayStat {
                weekday: wd,
                name: weekday_names[wd as usize],
                ..Default::default()
            });
            w.active_sec += gap;
            sess_by_weekday[wd as usize] += gap;
            // attribute gap to the start event's git branch
            if let Some(br) = branch {
                let entry = sess_by_branch
                    .entry(br.clone())
                    .or_insert_with(|| (0u64, 0u64, Tokens::default(), 0.0));
                entry.0 += gap;
            }
        }

        // per-event aggregates (messages, tokens, attribution)
        for e in &events {
            sess_msgs += 1;
            for (m, _, t, _) in e.model_usage() {
                let m = m.unwrap_or_default();
                if t.total() > 0 && pricing(&m, 0).is_none() {
                    *unpriced_models.entry(m).or_default() += t.total();
                }
            }
            let c = e.cost_usd;
            sess_tokens.add(&e.tokens);
            sess_cost += c;

            for (m, messages, tokens, cost) in e.model_usage() {
                let Some(m) = m else { continue };
                sess_models.insert(m.clone(), ());
                let ms = by_model.entry(m.clone()).or_insert_with(|| ModelStat {
                    name: m.clone(),
                    ..Default::default()
                });
                ms.messages += messages;
                ms.tokens.add(&tokens);
                ms.cost_usd += cost;
                let sm = sess_by_model_attr.entry(m).or_default();
                sm.messages += messages;
                sm.tokens.add(&tokens);
                sm.cost_usd += cost;
            }
            if let Some(s) = &e.skill {
                let ns = by_skill.entry(s.clone()).or_insert_with(|| NamedStat {
                    name: s.clone(),
                    ..Default::default()
                });
                ns.messages += 1;
                ns.tokens.add(&e.tokens);
                ns.cost_usd += c;
                *sess_by_skill.entry(s.clone()).or_insert(0) += 1;
            }
            if let Some(p) = &e.plugin {
                let np = by_plugin.entry(p.clone()).or_insert_with(|| NamedStat {
                    name: p.clone(),
                    ..Default::default()
                });
                np.messages += 1;
                np.tokens.add(&e.tokens);
                np.cost_usd += c;
            }
            // tools, content types, stop reason, branch (from this event)
            for tool in &e.tools {
                *by_tool.entry(tool.clone()).or_insert(0) += 1;
                *sess_by_tool.entry(tool.clone()).or_insert(0) += 1;
                if let Some(rest) = tool.strip_prefix("mcp__") {
                    let server = rest.split("__").next().unwrap_or("").to_string();
                    if !server.is_empty() {
                        *by_mcp_server.entry(server.clone()).or_insert(0) += 1;
                        *sess_by_mcp.entry(server).or_insert(0) += 1;
                    }
                }
            }
            for ct in &e.content_types {
                *by_content.entry(ct.clone()).or_insert(0) += 1;
                *sess_by_content.entry(ct.clone()).or_insert(0) += 1;
            }
            if let Some(sr) = &e.stop_reason {
                *by_stop_reason.entry(sr.clone()).or_insert(0) += 1;
                *sess_by_stop.entry(sr.clone()).or_insert(0) += 1;
            }
            if let Some(br) = &e.git_branch {
                let entry = sess_by_branch
                    .entry(br.clone())
                    .or_insert_with(|| (0u64, 0u64, Tokens::default(), 0.0));
                entry.1 += 1; // messages
                entry.2.add(&e.tokens);
                entry.3 += c;
            }

            let local = e.dt_utc.with_timezone(tz());
            let hour = local.hour() as u8;
            let h = by_hour.entry(hour).or_insert(HourStat {
                hour,
                ..Default::default()
            });
            h.messages += 1;
            h.output_tokens += e.tokens.output;
            h.cost_usd += c;
            sess_by_hour_output[hour as usize] += e.tokens.output;
            sess_by_hour_cost[hour as usize] += c;
            let wd = local.weekday().num_days_from_monday() as u8;
            let w = by_weekday.entry(wd).or_insert(WeekdayStat {
                weekday: wd,
                name: weekday_names[wd as usize],
                ..Default::default()
            });
            w.messages += 1;

            let dk = date_key(e.dt_utc);
            let day = daily.entry(dk.clone()).or_insert_with(|| DailyStat {
                date: dk,
                ..Default::default()
            });
            day.messages += 1;
            day.tokens.add(&e.tokens);
            day.cost_usd += c;
        }

        // session record
        let session_id = jsonl_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        let title = sess_meta
            .custom_title
            .clone()
            .or_else(|| sess_meta.ai_title.clone());
        let sess_by_branch_serial: BTreeMap<String, SessionBranchAttr> = sess_by_branch
            .iter()
            .map(|(k, (a_sec, msgs, tks, cst))| {
                (
                    k.clone(),
                    SessionBranchAttr {
                        active_sec: *a_sec,
                        messages: *msgs,
                        tokens: tks.clone(),
                        cost_usd: *cst,
                    },
                )
            })
            .collect();
        sessions.push(SessionStat {
            project: project_name.clone(),
            session_id,
            title,
            file: jsonl_path.display().to_string(),
            start: fmt_dt(start),
            end: fmt_dt(end),
            active_sec: active,
            total_sec: total,
            messages: sess_msgs,
            tokens: sess_tokens,
            cost_usd: sess_cost,
            models: sess_models.into_keys().collect(),
            by_hour: sess_by_hour.to_vec(),
            by_hour_output: sess_by_hour_output.to_vec(),
            by_hour_cost: sess_by_hour_cost.to_vec(),
            by_weekday: sess_by_weekday.to_vec(),
            by_skill: sess_by_skill.clone(),
            by_tool: sess_by_tool.clone(),
            by_mcp_server: sess_by_mcp,
            by_content_type: sess_by_content,
            by_stop_reason: sess_by_stop,
            by_branch: sess_by_branch_serial,
            by_model: sess_by_model_attr,
        });

        session_days.extend(build_session_days(
            sessions.last().unwrap(),
            &events,
            &intervals,
        ));

        // project aggregate
        let p = projects
            .entry(project_name.clone())
            .or_insert_with(|| ProjectStat {
                name: project_name.clone(),
                cwd: project_cwd.clone(),
                by_hour: vec![0u64; 24],
                by_weekday: vec![0u64; 7],
                ..Default::default()
            });
        p.active_sec += active;
        p.total_sec += total;
        p.sessions += 1;
        p.messages += sess_msgs;
        p.tokens.add(&sess_tokens);
        p.cost_usd += sess_cost;
        for i in 0..24 {
            p.by_hour[i] += sess_by_hour[i];
        }
        for i in 0..7 {
            p.by_weekday[i] += sess_by_weekday[i];
        }
        for (k, v) in sess_by_skill {
            *p.by_skill.entry(k).or_insert(0) += v;
        }
        for (k, v) in sess_by_tool {
            *p.by_tool.entry(k).or_insert(0) += v;
        }
        // merge sess_by_branch into global by_branch (one session counted per branch touched)
        for (br, (a_sec, msgs, tks, cst)) in &sess_by_branch {
            let bs = by_branch.entry(br.clone()).or_insert_with(|| BranchStat {
                name: br.clone(),
                ..Default::default()
            });
            bs.active_sec += a_sec;
            bs.messages += msgs;
            bs.tokens.add(tks);
            bs.cost_usd += cst;
            bs.sessions += 1;
        }
        if p.first.is_none()
            || start
                < DateTime::parse_from_rfc3339(p.first.as_ref().unwrap())
                    .unwrap_or_else(|_| Utc::now().into())
                    .with_timezone(&Utc)
        {
            p.first = Some(start.to_rfc3339());
        }
        if p.last.is_none()
            || end
                > DateTime::parse_from_rfc3339(p.last.as_ref().unwrap())
                    .unwrap_or_else(|_| Utc.timestamp_opt(0, 0).unwrap().into())
                    .with_timezone(&Utc)
        {
            p.last = Some(end.to_rfc3339());
        }

        // One session contribution per calendar day, including cross-day activity.
        for slice in session_days
            .iter()
            .rev()
            .take_while(|d| d.session.file == jsonl_path.to_string_lossy())
        {
            let day = daily
                .entry(slice.date.clone())
                .or_insert_with(|| DailyStat {
                    date: slice.date.clone(),
                    ..Default::default()
                });
            day.sessions += 1;
        }

        // grand totals
        grand_active += active;
        grand_total += total;
        grand_msgs += sess_msgs;
        grand_tokens.add(&sess_tokens);
        grand_cost += sess_cost;
        if earliest.is_none() || start < earliest.unwrap() {
            earliest = Some(start);
        }
        if latest.is_none() || end > latest.unwrap() {
            latest = Some(end);
        }
    }

    // compute per-day wall-clock UNION across all sessions (dedupes overlap)
    let mut grand_union = 0u64;
    for (dk, ivs) in intervals_by_day {
        let u = compute_union(ivs);
        grand_union += u;
        if let Some(d) = daily.get_mut(&dk) {
            d.active_sec_union = u;
        }
    }
    // per-(project, day) union -> attach to daily.union_by_project AND ProjectStat.active_sec_union
    for ((proj, dk), ivs) in intervals_by_proj_day {
        let u = compute_union(ivs);
        if let Some(d) = daily.get_mut(&dk) {
            d.union_by_project.insert(proj.clone(), u);
        }
        if let Some(p) = projects.get_mut(&proj) {
            p.active_sec_union += u;
        }
    }

    // finalize
    let span_days = match (earliest, latest) {
        (Some(a), Some(b)) => (b - a).num_seconds() as f64 / 86400.0,
        _ => 0.0,
    };
    let cache_hit_rate = {
        let read = grand_tokens.cache_read as f64;
        let denom = read
            + grand_tokens.cache_create_5m as f64
            + grand_tokens.cache_create_1h as f64
            + grand_tokens.input as f64;
        if denom > 0.0 {
            read / denom
        } else {
            0.0
        }
    };

    let summary = Summary {
        generated_at: Utc::now()
            .with_timezone(tz())
            .format("%Y-%m-%d %H:%M:%S")
            .to_string(),
        gap_threshold_min: args.gap,
        tz_offset: tz().to_string(),
        default_window_days: args.default_window_days,
        earliest: earliest.map(|d| d.with_timezone(tz()).format("%Y-%m-%d").to_string()),
        latest: latest.map(|d| d.with_timezone(tz()).format("%Y-%m-%d").to_string()),
        span_days,
        projects: projects.len() as u64,
        sessions: sessions.len() as u64,
        messages: grand_msgs,
        active_sec: grand_active,
        active_sec_union: grand_union,
        total_sec: grand_total,
        active_total_ratio: if grand_total > 0 {
            grand_active as f64 / grand_total as f64
        } else {
            0.0
        },
        avg_active_per_day_h: if span_days > 0.0 {
            (grand_active as f64 / 3600.0) / span_days
        } else {
            0.0
        },
        tokens: grand_tokens,
        cost_usd: grand_cost,
        cache_hit_rate,
    };

    // build sorted vectors
    let mut projects_v: Vec<ProjectStat> = projects.into_values().collect();
    projects_v.sort_by(|a, b| b.active_sec.cmp(&a.active_sec));

    let mut daily_v: Vec<DailyStat> = daily.into_values().collect();
    daily_v.sort_by(|a, b| a.date.cmp(&b.date));

    let mut by_model_v: Vec<ModelStat> = by_model.into_values().collect();
    by_model_v.sort_by(|a, b| b.cost_usd.partial_cmp(&a.cost_usd).unwrap());

    let mut by_skill_v: Vec<NamedStat> = by_skill.into_values().collect();
    by_skill_v.sort_by(|a, b| b.messages.cmp(&a.messages));

    let mut by_plugin_v: Vec<NamedStat> = by_plugin.into_values().collect();
    by_plugin_v.sort_by(|a, b| b.messages.cmp(&a.messages));

    // fill hour/weekday gaps
    let by_hour_v: Vec<HourStat> = (0..24u8)
        .map(|h| {
            by_hour.get(&h).cloned().unwrap_or(HourStat {
                hour: h,
                ..Default::default()
            })
        })
        .collect();
    let by_weekday_v: Vec<WeekdayStat> = (0..7u8)
        .map(|w| {
            by_weekday.get(&w).cloned().unwrap_or(WeekdayStat {
                weekday: w,
                name: weekday_names[w as usize],
                ..Default::default()
            })
        })
        .collect();

    sessions.sort_by(|a, b| b.active_sec.cmp(&a.active_sec));

    let to_named_count = |m: BTreeMap<String, u64>| -> Vec<NamedCount> {
        let mut v: Vec<NamedCount> = m
            .into_iter()
            .map(|(k, c)| NamedCount { name: k, count: c })
            .collect();
        v.sort_by(|a, b| b.count.cmp(&a.count));
        v
    };
    let by_tool_v = to_named_count(by_tool);
    let by_mcp_server_v = to_named_count(by_mcp_server);
    let by_content_v = to_named_count(by_content);
    let by_stop_reason_v = to_named_count(by_stop_reason);

    let mut by_branch_v: Vec<BranchStat> = by_branch.into_values().collect();
    by_branch_v.sort_by(|a, b| b.active_sec.cmp(&a.active_sec));

    let mut report = Report {
        summary,
        projects: projects_v,
        sessions,
        session_days,
        daily: daily_v,
        by_model: by_model_v,
        by_skill: by_skill_v,
        by_plugin: by_plugin_v,
        by_tool: by_tool_v,
        by_mcp_server: by_mcp_server_v,
        by_content_type: by_content_v,
        by_stop_reason: by_stop_reason_v,
        by_branch: by_branch_v,
        by_hour: by_hour_v,
        by_weekday: by_weekday_v,
        by_trigger: Vec::new(),
        runs_weekly: Vec::new(),
        runs_weekly_total: weekly_totals(&run_builds, &sub_work, tz()),
        runs: Vec::new(),
    };
    report.runs = run_builds.into_iter().map(|b| b.run).collect();
    report.by_trigger = summarize_triggers(&report.runs);
    report.runs_weekly = weekly_triggers(&report.runs);

    if args.anonymize {
        anonymize_report(&mut report);
    }

    let fmt = args.format.to_lowercase();
    let want_all = fmt == "all";
    let want_json = want_all || fmt == "json";
    let want_csv = want_all || fmt == "csv";
    let want_html = want_all || fmt == "html";

    let json_str = serde_json::to_string_pretty(&report)?;
    if want_json {
        let p = args.out.join("data.json");
        fs::write(&p, &json_str)?;
        println!("wrote {}", p.display());
    }

    if want_csv {
        write_csv(&args.out, &report)?;
    }

    if want_html {
        let html =
            REPORT_HTML_TEMPLATE.replace("/*__CC_STATS_DATA__*/null", &json_for_script(&report)?);
        let p = args.out.join("report.html");
        fs::write(&p, &html)?;
        println!("wrote {}", p.display());
        if args.open {
            let _ = std::process::Command::new("open").arg(&p).status();
        }
    }

    for (model, tokens) in &unpriced_models {
        eprintln!("warning: no verified price for model {model:?}; {tokens} tokens excluded from cost estimates");
    }

    // CLI summary print
    print_summary(&report);

    Ok(())
}

fn write_csv(out: &Path, r: &Report) -> Result<()> {
    let mut f = File::create(out.join("daily.csv"))?;
    writeln!(f, "date,active_sec,messages,sessions,cost_usd,tokens_total")?;
    for d in &r.daily {
        writeln!(
            f,
            "{},{},{},{},{:.4},{}",
            d.date,
            d.active_sec,
            d.messages,
            d.sessions,
            d.cost_usd,
            d.tokens.total()
        )?;
    }

    let mut f = File::create(out.join("projects.csv"))?;
    writeln!(
        f,
        "project,active_sec,total_sec,sessions,messages,cost_usd,tokens_total"
    )?;
    for p in &r.projects {
        writeln!(
            f,
            "{:?},{},{},{},{},{:.4},{}",
            p.name,
            p.active_sec,
            p.total_sec,
            p.sessions,
            p.messages,
            p.cost_usd,
            p.tokens.total()
        )?;
    }

    let mut f = File::create(out.join("sessions.csv"))?;
    writeln!(
        f,
        "project,session_id,start,end,active_sec,total_sec,messages,cost_usd,tokens_total"
    )?;
    for s in &r.sessions {
        writeln!(
            f,
            "{:?},{},{},{},{},{},{},{:.4},{}",
            s.project,
            s.session_id,
            s.start,
            s.end,
            s.active_sec,
            s.total_sec,
            s.messages,
            s.cost_usd,
            s.tokens.total()
        )?;
    }
    println!(
        "wrote {}/daily.csv projects.csv sessions.csv",
        out.display()
    );
    Ok(())
}

fn fmt_hms(sec: u64) -> String {
    let h = sec / 3600;
    let m = (sec % 3600) / 60;
    let s = sec % 60;
    format!("{:>4}h {:02}m {:02}s", h, m, s)
}

fn print_summary(r: &Report) {
    let s = &r.summary;
    println!();
    println!("{}", "=".repeat(78));
    println!(
        "Claude Code Stats   gap={}m   generated {}",
        s.gap_threshold_min, s.generated_at
    );
    println!("{}", "=".repeat(78));
    println!(
        "Range     : {}  →  {}   ({:.1} days)",
        s.earliest.as_deref().unwrap_or("-"),
        s.latest.as_deref().unwrap_or("-"),
        s.span_days
    );
    println!(
        "Projects/Sessions/Messages : {} / {} / {}",
        s.projects, s.sessions, s.messages
    );
    println!(
        "Active sum : {}   ({:.1} h)   per-session sum; parallel sessions double-counted",
        fmt_hms(s.active_sec),
        s.active_sec as f64 / 3600.0
    );
    println!(
        "Active union: {}   ({:.1} h)   wall-clock union; real attention time",
        fmt_hms(s.active_sec_union),
        s.active_sec_union as f64 / 3600.0
    );
    println!(
        "Total      : {}   ({:.1} h)",
        fmt_hms(s.total_sec),
        s.total_sec as f64 / 3600.0
    );
    println!("Active/Total: {:.1}%", s.active_total_ratio * 100.0);
    println!("Avg active/day: {:.2} h", s.avg_active_per_day_h);
    println!();
    println!(
        "Tokens     : in={}  out={}  cache_write={}+{}  cache_read={}   total={}",
        s.tokens.input,
        s.tokens.output,
        s.tokens.cache_create_5m,
        s.tokens.cache_create_1h,
        s.tokens.cache_read,
        s.tokens.total()
    );
    println!(
        "Iterations : {} model calls   (avg {:.2} per assistant turn)",
        s.tokens.iterations,
        if s.messages > 0 {
            s.tokens.iterations as f64 / s.messages as f64
        } else {
            0.0
        }
    );
    println!("TZ         : {}", s.tz_offset);
    println!(
        "Cache hit  : {:.1}%   (cache_read / [input + cache_write + cache_read])",
        s.cache_hit_rate * 100.0
    );
    println!("Cost (est.): ${:.2} USD", s.cost_usd);
    println!();
    println!("Top projects by active time:");
    for (i, p) in r.projects.iter().take(10).enumerate() {
        println!(
            "  {:>2}. {}  {}  msgs={}  ${:.2}",
            i + 1,
            fmt_hms(p.active_sec),
            p.name,
            p.messages,
            p.cost_usd
        );
    }
    if !r.by_model.is_empty() {
        println!();
        println!("By model:");
        for m in &r.by_model {
            println!(
                "  {:<24}  msgs={:>6}  ${:.2}",
                m.name, m.messages, m.cost_usd
            );
        }
    }
    if !r.by_skill.is_empty() {
        println!();
        println!("Top skills:");
        for sk in r.by_skill.iter().take(10) {
            println!("  {:<40}  msgs={}", sk.name, sk.messages);
        }
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod run_tests {
    use super::*;
    use serde_json::json;

    fn ev(role: Role, t: i64) -> Event {
        Event {
            billing_key: None,
            usage: None,
            cost_usd: 0.0,
            dt_utc: DateTime::from_timestamp(t, 0).unwrap(),
            model: None,
            skill: None,
            plugin: None,
            git_branch: None,
            stop_reason: None,
            content_types: vec![],
            tools: vec![],
            tokens: Tokens::default(),
            advisor: vec![],
            host_tokens: Tokens::default(),
            host_cost_usd: 0.0,
            role,
            command: None,
            skills: vec![],
            interrupted: false,
            sidechain: false,
            uuid: None,
        }
    }
    fn prompt(t: i64, uuid: &str) -> Event {
        Event {
            uuid: Some(uuid.into()),
            ..ev(Role::HumanPrompt, t)
        }
    }
    fn typed(t: i64, cmd: &str) -> Event {
        Event {
            command: Some(cmd.into()),
            uuid: Some(format!("u{t}")),
            ..ev(Role::HumanPrompt, t)
        }
    }
    fn assistant(t: i64, skills: &[&str]) -> Event {
        Event {
            skills: skills.iter().map(|s| s.to_string()).collect(),
            ..ev(Role::Assistant, t)
        }
    }
    fn interrupt(t: i64) -> Event {
        Event {
            interrupted: true,
            ..ev(Role::UserOther, t)
        }
    }
    const CAP: i64 = 900;

    #[test]
    fn a_run_spans_prompt_to_last_event_before_the_next_prompt() {
        let evs = vec![
            prompt(0, "a"),
            assistant(10, &[]),
            ev(Role::UserOther, 20),
            assistant(30, &[]),
            prompt(100, "b"),
            assistant(110, &[]),
        ];
        let r = build_runs(&evs, CAP, &[]);
        assert_eq!(r.len(), 2);
        assert_eq!(
            (r[0].run.dur_sec, r[0].run.trigger.as_str()),
            (30, PLAIN_PROMPT)
        );
        assert_eq!(r[1].run.dur_sec, 10);
    }

    #[test]
    fn idle_gaps_are_capped_not_counted_in_full() {
        let evs = vec![
            prompt(0, "a"),
            assistant(10, &[]),
            assistant(10 + 36_000, &[]),
        ];
        assert_eq!(build_runs(&evs, CAP, &[])[0].run.dur_sec, 10 + 900);
    }

    #[test]
    fn typed_command_wins_then_first_skill_then_plain() {
        let evs = vec![
            typed(0, "spectra-apply"),
            assistant(5, &["spectra-verify", "spectra-verify"]),
            prompt(100, "p"),
            assistant(105, &["spectra-review", "spectra-apply"]),
            prompt(200, "q"),
            assistant(205, &[]),
        ];
        let r = build_runs(&evs, CAP, &[]);
        assert_eq!(r[0].run.trigger, "spectra-apply");
        assert_eq!(
            r[0].run.skills,
            vec!["spectra-verify"],
            "deduped, trigger excluded"
        );
        assert_eq!(r[1].run.trigger, "spectra-review");
        assert_eq!(r[1].run.skills, vec!["spectra-apply"]);
        assert_eq!(r[2].run.trigger, PLAIN_PROMPT);
    }

    #[test]
    fn local_commands_and_unanswered_prompts_are_not_runs() {
        let evs = vec![
            typed(0, "compact"),
            assistant(5, &[]),
            typed(100, "goal"),
            assistant(105, &[]),
            typed(200, "Model"),
            ev(Role::UserOther, 201),
            prompt(300, "x"),
            ev(Role::UserOther, 301), // never answered
            typed(400, "my-own"),
            assistant(405, &[]),
        ];
        let names: Vec<_> = build_runs(&evs, CAP, &["my-own".into()])
            .iter()
            .map(|b| b.run.trigger.clone())
            .collect();
        assert_eq!(
            names,
            vec!["goal"],
            "/goal is agent work; --run-exclude drops my-own"
        );
    }

    #[test]
    fn interrupts_are_counted_and_the_last_one_marks_how_the_run_ended() {
        let evs = vec![
            prompt(0, "a"),
            assistant(10, &[]),
            interrupt(11),
            assistant(20, &[]),
            interrupt(21),
            prompt(100, "b"),
            assistant(110, &[]),
            interrupt(111),
            assistant(120, &[]),
        ];
        let r = build_runs(&evs, CAP, &[]);
        assert_eq!(
            (r[0].run.interrupts, r[0].run.ended_by_interrupt),
            (2, true)
        );
        assert_eq!(
            (r[1].run.interrupts, r[1].run.ended_by_interrupt),
            (1, false),
            "work resumed after it"
        );
    }

    #[test]
    fn subagent_and_metadata_lines_never_extend_a_run() {
        let evs = vec![
            prompt(0, "a"),
            assistant(10, &[]),
            Event {
                sidechain: true,
                ..assistant(5000, &[])
            },
            ev(Role::Other, 9000),
        ];
        assert_eq!(build_runs(&evs, CAP, &[])[0].run.dur_sec, 10);
    }

    #[test]
    fn a_resumed_copy_of_a_run_is_counted_once_keeping_the_longer_one() {
        let short = build_runs(&[prompt(0, "same"), assistant(10, &[])], CAP, &[]);
        let long = build_runs(
            &[prompt(0, "same"), assistant(10, &[]), assistant(50, &[])],
            CAP,
            &[],
        );
        let d = dedupe_runs(short.into_iter().chain(long).collect());
        assert_eq!((d.len(), d[0].run.dur_sec), (1, 50));
    }

    #[test]
    fn nearest_rank_percentiles_match_the_viewer() {
        let v: Vec<u64> = (1..=10).collect();
        assert_eq!(
            (pct(&v, 0.5), pct(&v, 0.9), pct(&v, 0.99), pct(&[], 0.5)),
            (6, 10, 10, 0)
        );
    }

    // Mon 2026-11-09 08:00:00 UTC, so weeks and run dates agree with the epochs used below.
    fn t0() -> i64 {
        NaiveDate::from_ymd_opt(2026, 11, 9)
            .unwrap()
            .and_hms_opt(8, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp()
    }
    fn utc() -> FixedOffset {
        FixedOffset::east_opt(0).unwrap()
    }
    fn main_run(session: &str, a: i64, b: i64) -> RunBuild {
        let mut r = build_runs(
            &[prompt(a, &format!("{session}{a}")), assistant(b, &[])],
            CAP,
            &[],
        );
        r[0].run.date = "2026-11-09".into();
        r[0].session = session.into();
        r.remove(0)
    }
    fn sub(session: &str, a: i64, b: i64) -> SubWork {
        SubWork {
            session: session.into(),
            segs: vec![(a, b)],
        }
    }

    #[test]
    fn overlapping_runs_raise_parallelism_and_back_to_back_ones_do_not() {
        let base = t0();
        let runs = [
            main_run("s1", base, base + 600),
            main_run("s2", base + 300, base + 900),
            main_run("s3", base + 1000, base + 1100),
            main_run("s4", base + 1100, base + 1200),
        ];
        let t = weekly_totals(&runs, &[], &utc());
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].peak_parallel, 2);
        // agent = 600+600+100+100 = 1400 s; union = [0,900] + [1000,1200] = 1100 s
        assert!(
            (t[0].hours * 3600.0 - 1400.0).abs() < 1e-6
                && (t[0].union_hours * 3600.0 - 1100.0).abs() < 1e-6
        );
        assert_eq!(
            t[0].peak_parallel_all, 2,
            "without subagents the two views agree"
        );
        assert!((t[0].avg_parallel_all - t[0].avg_parallel).abs() < 1e-9);
    }

    #[test]
    fn a_main_agent_waiting_on_one_subagent_counts_once_not_twice() {
        let base = t0();
        let t = weekly_totals(
            &[main_run("s1", base, base + 600)],
            &[sub("s1", base, base + 600)],
            &utc(),
        );
        assert_eq!(
            (
                t[0].peak_parallel,
                t[0].peak_parallel_all,
                t[0].peak_subagents
            ),
            (1, 1, 1)
        );
        assert!(
            (t[0].avg_parallel_all - 1.0).abs() < 1e-9,
            "one thread of work, however it is delegated"
        );
        assert!((t[0].subagent_hours * 3600.0 - 600.0).abs() < 1e-6);
    }

    #[test]
    fn parallel_subagents_show_up_and_the_main_agents_waiting_time_is_dropped() {
        let base = t0();
        // main works 0..600; subagent A 60..540 and B 120..480 overlap each other
        let t = weekly_totals(
            &[main_run("s1", base, base + 600)],
            &[
                sub("s1", base + 60, base + 540),
                sub("s1", base + 120, base + 480),
            ],
            &utc(),
        );
        let w = &t[0];
        assert_eq!(
            (w.peak_parallel, w.peak_parallel_all, w.peak_subagents),
            (1, 2, 2)
        );
        // main only: 600 s. main time left after the subagents' span 60..540 is removed: 120 s; plus 480 + 360
        assert!((w.hours * 3600.0 - 600.0).abs() < 1e-6);
        assert!(
            (w.hours_all * 3600.0 - 960.0).abs() < 1e-6,
            "{}",
            w.hours_all * 3600.0
        );
        assert!((w.union_hours_all * 3600.0 - 600.0).abs() < 1e-6);
        assert!((w.avg_parallel_all - 1.6).abs() < 1e-9);
        assert!((w.subagent_hours * 3600.0 - 840.0).abs() < 1e-6);
    }

    #[test]
    fn subagents_of_another_session_do_not_cancel_this_sessions_main_time() {
        let base = t0();
        let t = weekly_totals(
            &[main_run("s1", base, base + 600)],
            &[sub("other", base, base + 600)],
            &utc(),
        );
        assert_eq!(
            t[0].peak_parallel_all, 2,
            "two unrelated sessions working at once"
        );
    }

    #[test]
    fn work_counts_in_the_week_it_starts_in() {
        let sunday_late = t0() - 8 * 3600 + 6 * 86400 + 23 * 3600 + 50 * 60; // Sun 2026-11-15 23:50 UTC
        let t = weekly_totals(&[], &[sub("s1", sunday_late, sunday_late + 1200)], &utc());
        assert_eq!((t.len(), t[0].week.as_str()), (1, "2026-11-09"));
        // the same instant is already Monday in +08:00
        let t8 = weekly_totals(
            &[],
            &[sub("s1", sunday_late, sunday_late + 1200)],
            &FixedOffset::east_opt(8 * 3600).unwrap(),
        );
        assert_eq!(t8[0].week, "2026-11-16");
    }

    #[test]
    fn interval_helpers_merge_and_subtract() {
        assert_eq!(
            merge_intervals(vec![(5, 9), (0, 3), (2, 4), (9, 10)]),
            vec![(0, 4), (5, 10)]
        );
        assert_eq!(
            subtract_intervals((0, 10), &[(2, 4), (6, 7)]),
            vec![(0, 2), (4, 6), (7, 10)]
        );
        assert_eq!(subtract_intervals((0, 10), &[(0, 10)]), vec![]);
        assert_eq!(subtract_intervals((0, 10), &[(20, 30)]), vec![(0, 10)]);
        assert_eq!(subtract_intervals((5, 10), &[(0, 7)]), vec![(7, 10)]);
    }

    #[test]
    fn a_subagent_file_names_its_parent_session() {
        let p = Path::new("/home/u/.claude/projects/-proj/abc-123/subagents/agent-x.jsonl");
        assert_eq!(subagent_session(p).as_deref(), Some("abc-123"));
        assert_eq!(
            subagent_session(Path::new("/home/u/.claude/projects/-proj/abc-123.jsonl")),
            None
        );
    }

    #[test]
    fn subagent_segments_use_every_line_and_cap_the_gaps() {
        let evs = vec![
            Event {
                sidechain: true,
                ..ev(Role::UserOther, 0)
            },
            Event {
                sidechain: true,
                ..assistant(100, &[])
            },
            Event {
                sidechain: true,
                ..ev(Role::Other, 150)
            },
            Event {
                sidechain: true,
                ..assistant(100 + 9000, &[])
            },
        ];
        assert_eq!(agent_segments(&evs, CAP), vec![(0, 100), (100, 1000)]);
    }

    #[test]
    fn json_embedded_in_a_script_element_cannot_end_it() {
        let v = json!({"name": "</script><!--&\u{2028}\u{2029}>", "n": 1});
        let out = json_for_script(&v).unwrap();
        assert!(
            !out.contains(['<', '>', '&', '\u{2028}', '\u{2029}']),
            "{out}"
        );
        assert_eq!(
            serde_json::from_str::<Value>(&out).unwrap(),
            v,
            "escapes are plain JSON, nothing is lost"
        );
    }

    #[test]
    fn weeks_start_on_monday() {
        assert_eq!(
            (
                week_start("2026-11-09").as_str(),
                week_start("2026-11-15").as_str()
            ),
            ("2026-11-09", "2026-11-09")
        );
        assert_eq!(week_start("2026-11-16"), "2026-11-16");
    }

    #[test]
    fn user_lines_are_classified_like_the_transcript_marks_them() {
        let c = |v: Value| classify_user(&v);
        assert_eq!(
            c(json!({"message":{"content":"fix the bug"}})).0,
            Role::HumanPrompt
        );
        assert_eq!(c(json!({"message":{"content":[{"type":"text","text":"hi"}]},"origin":{"kind":"human"}})).0, Role::HumanPrompt);
        assert_eq!(
            c(json!({"message":{"content":[{"type":"tool_result","content":"x"}]}})).0,
            Role::UserOther
        );
        assert_eq!(
            c(json!({"message":{"content":"<task-notification>x"}})).0,
            Role::UserOther
        );
        assert_eq!(
            c(json!({"message":{"content":"<local-command-stdout>x"}})).0,
            Role::UserOther
        );
        assert_eq!(
            c(json!({"message":{"content":"hi"},"isMeta":true})).0,
            Role::UserOther
        );
        assert_eq!(
            c(
                json!({"message":{"content":[{"type":"text","text":"[Request interrupted by user]"}]}})
            ),
            (Role::UserOther, true)
        );
        assert_eq!(
            c(json!({"message":{"content":"[Request interrupted by user for tool use]"}})),
            (Role::UserOther, true)
        );
    }

    #[test]
    fn command_names_are_read_from_the_slash_command_markup() {
        assert_eq!(
            command_name(
                "<command-message>x</command-message>\n<command-name>/spectra-apply</command-name>"
            )
            .as_deref(),
            Some("spectra-apply")
        );
        assert_eq!(
            command_name("<command-name>reviewer:result</command-name>").as_deref(),
            Some("reviewer:result")
        );
        assert_eq!(command_name("no markup"), None);
    }

    #[test]
    fn triggers_are_discovered_ranked_and_flagged_without_configuration() {
        let mk = |trigger: &str, dur: u64, date: &str| Run {
            date: date.into(),
            start: 0,
            dur_sec: dur,
            trigger: trigger.into(),
            skills: vec![],
            interrupts: 1,
            ended_by_interrupt: false,
            project: "p".into(),
            uuid: None,
            events: 2,
        };
        let mut runs: Vec<Run> = (0..5)
            .map(|i| mk("big", 1200, &format!("2026-10-0{}", i + 1)))
            .collect();
        runs.push(mk("brand-new-skill", 30, "2026-11-10"));
        runs.push(mk("quick", 30, "2026-11-10"));
        let s = summarize_triggers(&runs);
        assert_eq!(s[0].name, "big", "ranked by hours");
        assert!(s[0].runner && !s[0].is_new);
        let n = s.iter().find(|t| t.name == "brand-new-skill").unwrap();
        assert!(n.is_new && !n.runner, "seen only in the last 14 days");
        assert_eq!(s[0].interrupts_per_hour, 5.0 / (6000.0 / 3600.0));
    }
}
