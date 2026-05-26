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

#[derive(Parser, Debug)]
#[command(name = "cc-stats", about = "Claude Code usage analytics")]
struct Args {
    /// Path to Claude projects dir
    #[arg(long, default_value = "~/.claude/projects")]
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

#[derive(Debug, Default, Serialize, Clone, Copy)]
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

// pricing per million tokens (USD)
fn pricing(model: &str) -> (f64, f64, f64, f64, f64) {
    // (input, output, cache_write_5m, cache_write_1h, cache_read)
    let m = model.to_lowercase();
    if m.contains("opus") {
        (15.0, 75.0, 18.75, 30.0, 1.50)
    } else if m.contains("haiku") {
        (1.0, 5.0, 1.25, 2.0, 0.10)
    } else if m.contains("sonnet") {
        (3.0, 15.0, 3.75, 6.0, 0.30)
    } else {
        (3.0, 15.0, 3.75, 6.0, 0.30)
    }
}

fn cost_usd(t: &Tokens, model: &str) -> f64 {
    let (i, o, c5, c1, cr) = pricing(model);
    (t.input as f64 * i
        + t.output as f64 * o
        + t.cache_create_5m as f64 * c5
        + t.cache_create_1h as f64 * c1
        + t.cache_read as f64 * cr)
        / 1_000_000.0
}

#[derive(Debug)]
struct Event {
    dt_utc: DateTime<Utc>,
    model: Option<String>,
    skill: Option<String>,
    plugin: Option<String>,
    git_branch: Option<String>,
    stop_reason: Option<String>,
    content_types: Vec<String>, // assistant content block types: thinking/text/tool_use
    tools: Vec<String>,         // tool_use names within this assistant turn
    tokens: Tokens,
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

#[derive(Debug, Serialize, Clone)]
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

fn parse_event(v: &Value) -> Option<Event> {
    let ts = v.get("timestamp")?.as_str()?;
    let dt = DateTime::parse_from_rfc3339(ts).ok()?.with_timezone(&Utc);
    let typ = v.get("type")?.as_str()?;

    let mut tokens = Tokens::default();
    let mut model = None;
    let mut stop_reason = None;
    let mut content_types: Vec<String> = Vec::new();
    let mut tools: Vec<String> = Vec::new();
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
                            }
                        }
                    }
                }
            }
            if let Some(u) = msg.get("usage") {
                tokens.input = u.get("input_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
                tokens.output = u.get("output_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
                tokens.cache_read = u
                    .get("cache_read_input_tokens")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0);
                // split cache creation into 5m / 1h if available
                if let Some(cc) = u.get("cache_creation") {
                    tokens.cache_create_5m = cc
                        .get("ephemeral_5m_input_tokens")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(0);
                    tokens.cache_create_1h = cc
                        .get("ephemeral_1h_input_tokens")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(0);
                } else {
                    tokens.cache_create_5m = u
                        .get("cache_creation_input_tokens")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(0);
                }
                // iterations[] = number of model calls within this assistant turn
                tokens.iterations = u
                    .get("iterations")
                    .and_then(|x| x.as_array())
                    .map(|a| a.len() as u64)
                    .unwrap_or(1);
            }
        }
    }

    Some(Event {
        dt_utc: dt,
        model,
        skill,
        plugin,
        git_branch,
        stop_reason,
        content_types,
        tools,
        tokens,
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
    for s in &mut r.sessions {
        s.file = String::new();
        if s.title.is_some() {
            title_counter += 1;
            s.title = Some(format!("Session {:04}", title_counter));
        }
    }
    // project.cwd (raw cwd not affected by --merge)
    for p in &mut r.projects {
        p.cwd = None;
    }
    // branches: keep universal names (main/master/develop/HEAD), anonymize the rest
    let universal: std::collections::HashSet<&str> =
        ["main", "master", "develop", "HEAD", ""].iter().copied().collect();
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

    // Per-project aggregates
    let mut projects: BTreeMap<String, ProjectStat> = BTreeMap::new();
    let mut sessions: Vec<SessionStat> = Vec::new();
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
    // collect (start, end) intervals per day for union calculation
    let mut intervals_by_day: BTreeMap<String, Vec<(DateTime<Utc>, DateTime<Utc>)>> =
        BTreeMap::new();
    // per (project, day) intervals for per-project union
    let mut intervals_by_proj_day: BTreeMap<(String, String), Vec<(DateTime<Utc>, DateTime<Utc>)>> =
        BTreeMap::new();
    let mut earliest: Option<DateTime<Utc>> = None;
    let mut latest: Option<DateTime<Utc>> = None;

    let weekday_names = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

    for entry in fs::read_dir(&projects_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let folder = entry.file_name().to_string_lossy().to_string();
        let mut project_name = apply_merge(&decode_project_folder(&folder), &merges);
        let mut project_cwd: Option<String> = None;

        for jsonl in WalkDir::new(&path)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map(|x| x == "jsonl").unwrap_or(false))
        {
            let jsonl_path = jsonl.path();
            let (sess_meta, events) = match process_jsonl(jsonl_path) {
                Some(x) => x,
                None => continue,
            };
            let cwd = &sess_meta.cwd;
            if events.is_empty() {
                continue;
            }
            // prefer real cwd as project name
            if project_cwd.is_none() && !cwd.is_empty() {
                project_cwd = Some(cwd.clone());
                project_name = apply_merge(cwd, &merges);
            }
            // date filter on session start
            let sess_start_date = date_key(events.first().unwrap().dt_utc);
            if !within_range(&sess_start_date, &from, &to) {
                continue;
            }
            // project filter
            if !matches_project(&project_name, cwd, &args.project) {
                continue;
            }

            let start = events.first().unwrap().dt_utc;
            let end = events.last().unwrap().dt_utc;
            let total = (end - start).num_seconds().max(0) as u64;
            let mut active = 0u64;
            let mut sess_tokens = Tokens::default();
            let mut sess_cost = 0.0;
            let mut sess_models: BTreeMap<String, ()> = BTreeMap::new();
            let mut sess_msgs = 0u64;
            let mut sess_by_hour = [0u64; 24];
            let mut sess_by_weekday = [0u64; 7];
            let mut sess_by_skill: BTreeMap<String, u64> = BTreeMap::new();
            let mut sess_by_tool: BTreeMap<String, u64> = BTreeMap::new();
            let mut sess_by_branch: BTreeMap<String, (u64, u64, Tokens, f64)> = BTreeMap::new();

            for (a, b) in events.iter().zip(events.iter().skip(1)) {
                let gap = (b.dt_utc - a.dt_utc).num_seconds().max(0) as u64;
                if gap <= gap_sec {
                    active += gap;
                    // bucket per day / hour / weekday by start ts
                    let dk = date_key(a.dt_utc);
                    let day = daily.entry(dk.clone()).or_insert_with(|| DailyStat {
                        date: dk.clone(),
                        ..Default::default()
                    });
                    day.active_sec += gap;
                    *day.by_project.entry(project_name.clone()).or_insert(0) += gap;
                    intervals_by_day
                        .entry(dk.clone())
                        .or_default()
                        .push((a.dt_utc, b.dt_utc));
                    intervals_by_proj_day
                        .entry((project_name.clone(), dk.clone()))
                        .or_default()
                        .push((a.dt_utc, b.dt_utc));

                    let local = a.dt_utc.with_timezone(tz());
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
                    if let Some(br) = &a.git_branch {
                        let entry = sess_by_branch
                            .entry(br.clone())
                            .or_insert_with(|| (0u64, 0u64, Tokens::default(), 0.0));
                        entry.0 += gap;
                    }
                }
            }

            // per-event aggregates (messages, tokens, attribution)
            for e in &events {
                sess_msgs += 1;
                let c = cost_usd(&e.tokens, e.model.as_deref().unwrap_or(""));
                sess_tokens.add(&e.tokens);
                sess_cost += c;

                if let Some(m) = &e.model {
                    sess_models.insert(m.clone(), ());
                    let ms = by_model.entry(m.clone()).or_insert_with(|| ModelStat {
                        name: m.clone(),
                        ..Default::default()
                    });
                    ms.messages += 1;
                    ms.tokens.add(&e.tokens);
                    ms.cost_usd += c;
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
                            *by_mcp_server.entry(server).or_insert(0) += 1;
                        }
                    }
                }
                for ct in &e.content_types {
                    *by_content.entry(ct.clone()).or_insert(0) += 1;
                }
                if let Some(sr) = &e.stop_reason {
                    *by_stop_reason.entry(sr.clone()).or_insert(0) += 1;
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
            });

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

            // daily sessions count
            let day = daily.entry(sess_start_date).or_insert_with(|| DailyStat {
                ..Default::default()
            });
            day.sessions += 1;

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
    };

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
        let html = REPORT_HTML_TEMPLATE.replace(
            "/*__CC_STATS_DATA__*/null",
            &serde_json::to_string(&report)?,
        );
        let p = args.out.join("report.html");
        fs::write(&p, &html)?;
        println!("wrote {}", p.display());
        if args.open {
            let _ = std::process::Command::new("open").arg(&p).status();
        }
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
