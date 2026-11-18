//! What actually landed, read from git: merged PRs, other merges into the default branch, and commits.
//!
//! Merges are the outcome that matters here. A commit says something was written; a merge says it
//! was accepted. Everything comes from local git history, with no network, so it only sees repos
//! whose working directory (the transcript's `cwd`) still exists on this machine.

use chrono::{DateTime, FixedOffset, NaiveDate};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Landings and commits counted in one week.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WeekOutcome {
    pub commits: u64,
    pub merged_prs: u64,
    pub other_merges: u64,
}

#[derive(Debug, Default, Serialize)]
pub struct GitSummary {
    pub enabled: bool,
    pub authors: Vec<String>,
    pub repos: usize,
    pub projects_total: usize,
    pub projects_tracked: usize,
}

#[derive(Default)]
pub struct Outcomes {
    pub weekly: BTreeMap<String, WeekOutcome>,
    /// Projects whose working directory is inside a git repo: the agent hours that can be compared
    /// with the counts above.
    pub tracked: BTreeSet<String>,
    pub summary: GitSummary,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Landing {
    Pr(u64),
    Merge,
}

/// The PR number in a landing commit's subject: a squash/rebase merge ends in `(#123)`, a merge
/// commit reads `Merge pull request #123 from ...`.
pub fn pr_number(subject: &str) -> Option<u64> {
    let digits = |s: &str| -> Option<u64> {
        let n: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
        (!n.is_empty()).then(|| n.parse().ok()).flatten()
    };
    if let Some(rest) = subject.strip_prefix("Merge pull request #") {
        return digits(rest);
    }
    let s = subject.trim_end();
    let inner = s.strip_suffix(')')?;
    let at = inner.rfind("(#")?;
    let n = &inner[at + 2..];
    (!n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
        .then(|| n.parse().ok())
        .flatten()
}

/// Branches work is integrated into. A merge whose source is one of these is housekeeping (a back-merge
/// of main into develop, a release merge of develop into main): the work it carries landed already.
const INTEGRATION_BRANCHES: [&str; 6] =
    ["main", "master", "develop", "dev", "development", "trunk"];

/// Merges that move history between branches without landing new work: `git pull` syncing the remote,
/// and an integration branch merged into another.
pub fn is_sync_merge(subject: &str) -> bool {
    if subject.starts_with("Merge remote-tracking branch 'origin/")
        || (subject.starts_with("Merge branch '") && subject.contains("' of "))
    {
        return true;
    }
    subject
        .strip_prefix("Merge branch '")
        .and_then(|rest| rest.split('\'').next())
        .is_some_and(|source| INTEGRATION_BRANCHES.contains(&source))
}

/// What a first-parent commit on an integration branch is: a PR landing, another merge, or neither.
pub fn landing(subject: &str, parents: usize) -> Option<Landing> {
    if let Some(n) = pr_number(subject) {
        return Some(Landing::Pr(n));
    }
    (parents >= 2 && !is_sync_merge(subject)).then_some(Landing::Merge)
}

/// Week (Monday, in `tz`) of an RFC 3339 timestamp, or None if it is unreadable or outside the window.
fn week_of(
    ts: &str,
    tz: &FixedOffset,
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
) -> Option<String> {
    let day = DateTime::parse_from_rfc3339(ts)
        .ok()?
        .with_timezone(tz)
        .date_naive();
    if from.is_some_and(|f| day < f) || to.is_some_and(|t| day > t) {
        return None;
    }
    Some(super::week_start(&day.format("%Y-%m-%d").to_string()))
}

/// Add the non-merge commits in `log` (`sha US email US author-date` per line) written by `authors`.
/// A commit already in `seen` (another clone or worktree of the same repo) is not counted twice.
pub fn tally_commits(
    log: &str,
    authors: &BTreeSet<String>,
    seen: &mut HashSet<String>,
    window: (Option<NaiveDate>, Option<NaiveDate>),
    tz: &FixedOffset,
    weekly: &mut BTreeMap<String, WeekOutcome>,
) {
    for line in log.lines() {
        let mut f = line.split('\u{1f}');
        let (Some(sha), Some(email), Some(date)) = (f.next(), f.next(), f.next()) else {
            continue;
        };
        if !authors.contains(&email.to_lowercase()) || !seen.insert(sha.to_string()) {
            continue;
        }
        if let Some(w) = week_of(date, tz, window.0, window.1) {
            weekly.entry(w).or_default().commits += 1;
        }
    }
}

/// Add the landings in `log` (`sha US parents US email US committer-date US subject` per line, first
/// parent of the default branch). `repo_key` identifies the repo across clones, so one PR seen from two
/// clones counts once.
pub fn tally_landings(
    log: &str,
    repo_key: &str,
    authors: &BTreeSet<String>,
    seen: &mut HashSet<String>,
    window: (Option<NaiveDate>, Option<NaiveDate>),
    tz: &FixedOffset,
    weekly: &mut BTreeMap<String, WeekOutcome>,
) {
    for line in log.lines() {
        let mut f = line.split('\u{1f}');
        let (Some(sha), Some(parents), Some(email), Some(date), Some(subject)) =
            (f.next(), f.next(), f.next(), f.next(), f.next())
        else {
            continue;
        };
        if !authors.contains(&email.to_lowercase()) {
            continue;
        }
        let Some(kind) = landing(subject, parents.split_whitespace().count()) else {
            continue;
        };
        // the same commit seen again (another clone, or reachable from two integration branches) counts
        // once; so does one PR number landing as different commits (a cherry-pick into a release branch)
        let sha_key = format!("sha:{sha}");
        if seen.contains(&sha_key) {
            continue;
        }
        if let Landing::Pr(n) = kind {
            if !seen.insert(format!("pr:{repo_key}#{n}")) {
                continue;
            }
        }
        seen.insert(sha_key);
        if let Some(w) = week_of(date, tz, window.0, window.1) {
            let e = weekly.entry(w).or_default();
            match kind {
                Landing::Pr(_) => e.merged_prs += 1,
                Landing::Merge => e.other_merges += 1,
            }
        }
    }
}

/// Run git in `dir`. Paths come from transcripts, so keep git from doing anything but reading history:
/// no prompts, no optional locks, no fsmonitor hook.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(["-c", "core.fsmonitor=false", "-c", "core.pager=cat", "-C"])
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C")
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).to_string())
}

struct Repo {
    /// Shared by every worktree of the repo.
    key: String,
    /// A clone of the same remote is the same repo for counting PRs.
    identity: String,
    dir: PathBuf,
}

fn repo_of(cwd: &Path) -> Option<Repo> {
    if !cwd.is_dir() {
        return None;
    }
    let common = git(
        cwd,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let key = common.trim().to_string();
    let identity = git(cwd, &["config", "--get", "remote.origin.url"])
        .map(|u| u.trim().trim_end_matches(".git").to_string())
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| key.clone());
    Some(Repo {
        key,
        identity,
        dir: cwd.to_path_buf(),
    })
}

/// The branches landings are read from: origin/HEAD's target plus every integration branch that exists,
/// preferring the remote copy (it is what was pushed). Many repos integrate on `develop` while
/// origin/HEAD still points at `main`, so reading only the default branch would miss almost everything.
fn integration_refs(dir: &Path) -> Vec<String> {
    let exists = |r: &str| git(dir, &["rev-parse", "--verify", "-q", r]).is_some();
    let mut refs: Vec<String> = Vec::new();
    if let Some(r) = git(
        dir,
        &["symbolic-ref", "-q", "--short", "refs/remotes/origin/HEAD"],
    ) {
        let r = r.trim().to_string();
        if !r.is_empty() && exists(&r) {
            refs.push(r);
        }
    }
    for name in INTEGRATION_BRANCHES {
        let remote = format!("origin/{name}");
        let pick = if exists(&remote) {
            Some(remote)
        } else if exists(name) {
            Some(name.to_string())
        } else {
            None
        };
        if let Some(r) = pick {
            if !refs.contains(&r) {
                refs.push(r);
            }
        }
    }
    refs
}

/// Read git for the repos behind `projects` (project name, working directory).
/// `authors` limits whose work counts; empty means the git `user.email` of each repo.
pub fn collect(
    projects: &[(String, PathBuf)],
    authors: &[String],
    window: (Option<NaiveDate>, Option<NaiveDate>),
    tz: &FixedOffset,
) -> Outcomes {
    let mut out = Outcomes::default();
    let mut repos: BTreeMap<String, Repo> = BTreeMap::new();
    let mut who: BTreeSet<String> = authors.iter().map(|a| a.to_lowercase()).collect();
    for (name, cwd) in projects {
        let Some(repo) = repo_of(cwd) else { continue };
        out.tracked.insert(name.clone());
        if authors.is_empty() {
            if let Some(e) = git(cwd, &["config", "user.email"]) {
                who.insert(e.trim().to_lowercase());
            }
        }
        repos.entry(repo.key.clone()).or_insert(repo);
    }
    who.retain(|e| !e.is_empty());
    let (mut seen_commits, mut seen_landings) = (HashSet::new(), HashSet::new());
    let mut identities = HashSet::new();
    for repo in repos.values() {
        if !identities.insert(repo.identity.clone()) {
            continue; // another clone of a repo already read
        }
        if let Some(log) = git(
            &repo.dir,
            &[
                "log",
                "--branches",
                "--remotes",
                "--no-merges",
                "--format=%H%x1f%ae%x1f%aI",
            ],
        ) {
            tally_commits(&log, &who, &mut seen_commits, window, tz, &mut out.weekly);
        }
        for branch in integration_refs(&repo.dir) {
            if let Some(log) = git(
                &repo.dir,
                &[
                    "log",
                    "--first-parent",
                    "--format=%H%x1f%P%x1f%ae%x1f%cI%x1f%s",
                    &branch,
                ],
            ) {
                tally_landings(
                    &log,
                    &repo.identity,
                    &who,
                    &mut seen_landings,
                    window,
                    tz,
                    &mut out.weekly,
                );
            }
        }
    }
    out.summary = GitSummary {
        enabled: true,
        authors: who.into_iter().collect(),
        repos: identities.len(),
        projects_total: projects.len(),
        projects_tracked: out.tracked.len(),
    };
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc() -> FixedOffset {
        FixedOffset::east_opt(0).unwrap()
    }
    fn me() -> BTreeSet<String> {
        ["me@x.io".to_string()].into()
    }
    fn line(f: &[&str]) -> String {
        f.join("\u{1f}")
    }

    #[test]
    fn pr_numbers_come_from_squash_and_merge_subjects() {
        assert_eq!(pr_number("feat(report): add Runs view (#1)"), Some(1));
        assert_eq!(pr_number("fix: thing (#1234)  "), Some(1234));
        assert_eq!(pr_number("Merge pull request #57 from me/feat"), Some(57));
        assert_eq!(pr_number("fix: mention (#12) in the middle"), None);
        assert_eq!(pr_number("fix: not a number (#abc)"), None);
        assert_eq!(pr_number("Merge branch 'feat'"), None);
        assert_eq!(pr_number("(#)"), None);
    }

    #[test]
    fn syncing_the_remote_is_not_a_landing() {
        assert!(is_sync_merge("Merge branch 'main' of github.com:me/repo"));
        assert!(is_sync_merge("Merge remote-tracking branch 'origin/main'"));
        assert!(!is_sync_merge("Merge branch 'feat/x'"));
    }

    #[test]
    fn moving_history_between_integration_branches_is_not_new_work() {
        assert!(
            is_sync_merge("Merge branch 'main' into develop"),
            "back-merge"
        );
        assert!(
            is_sync_merge("Merge branch 'develop'"),
            "release merge into main"
        );
        assert!(
            !is_sync_merge("Merge branch 'feat/main-page'"),
            "only whole branch names count"
        );
        assert!(!is_sync_merge("Merge branch 'developer-docs'"));
        assert_eq!(landing("Merge branch 'main' into develop", 2), None);
        assert_eq!(
            landing("chore: bump submodules", 2),
            Some(Landing::Merge),
            "a custom merge subject is still a merge"
        );
    }

    #[test]
    fn a_landing_is_a_pr_or_a_real_merge() {
        assert_eq!(
            landing("feat: x (#9)", 1),
            Some(Landing::Pr(9)),
            "a squash merge has one parent"
        );
        assert_eq!(landing("Merge branch 'feat/x'", 2), Some(Landing::Merge));
        assert_eq!(landing("Merge branch 'main' of github.com:me/r", 2), None);
        assert_eq!(landing("fix: plain commit on main", 1), None);
    }

    #[test]
    fn only_my_non_merge_commits_count_and_a_clone_does_not_double_them() {
        let log = [
            line(&["a1", "me@x.io", "2026-11-02T10:00:00+00:00"]),
            line(&["a2", "ME@X.IO", "2026-11-03T10:00:00+00:00"]),
            line(&["a3", "other@x.io", "2026-11-03T11:00:00+00:00"]),
            line(&["a4", "me@x.io", "2026-11-10T10:00:00+00:00"]),
        ]
        .join("\n");
        let (mut seen, mut w) = (HashSet::new(), BTreeMap::new());
        tally_commits(&log, &me(), &mut seen, (None, None), &utc(), &mut w);
        tally_commits(&log, &me(), &mut seen, (None, None), &utc(), &mut w); // the same history from a worktree
        assert_eq!(
            w["2026-11-02"].commits, 2,
            "email is case-insensitive, others' commits are not mine"
        );
        assert_eq!(w["2026-11-09"].commits, 1);
    }

    #[test]
    fn landings_split_into_prs_and_other_merges_and_a_pr_counts_once() {
        let log = [
            line(&[
                "m1",
                "p1",
                "me@x.io",
                "2026-11-03T10:00:00+00:00",
                "feat: a (#1)",
            ]),
            line(&[
                "m2",
                "p1 p2",
                "me@x.io",
                "2026-11-04T10:00:00+00:00",
                "Merge branch 'feat/b'",
            ]),
            line(&[
                "m3",
                "p1",
                "other@x.io",
                "2026-11-05T10:00:00+00:00",
                "feat: theirs (#2)",
            ]),
            line(&[
                "m4",
                "p1 p2",
                "me@x.io",
                "2026-11-05T11:00:00+00:00",
                "Merge branch 'main' of github.com:me/r",
            ]),
            line(&[
                "m5",
                "p1",
                "me@x.io",
                "2026-11-12T10:00:00+00:00",
                "fix: c (#3)",
            ]),
            line(&[
                "m6",
                "p1",
                "me@x.io",
                "2026-11-12T11:00:00+00:00",
                "chore: no landing",
            ]),
        ]
        .join("\n");
        let (mut seen, mut w) = (HashSet::new(), BTreeMap::new());
        tally_landings(
            &log,
            "github.com/me/r",
            &me(),
            &mut seen,
            (None, None),
            &utc(),
            &mut w,
        );
        tally_landings(
            &log,
            "github.com/me/r",
            &me(),
            &mut seen,
            (None, None),
            &utc(),
            &mut w,
        ); // a second clone
        assert_eq!(
            w["2026-11-02"],
            WeekOutcome {
                commits: 0,
                merged_prs: 1,
                other_merges: 1
            }
        );
        assert_eq!(
            w["2026-11-09"],
            WeekOutcome {
                commits: 0,
                merged_prs: 1,
                other_merges: 0
            }
        );
    }

    #[test]
    fn the_same_pr_number_in_two_different_repos_is_two_prs() {
        let l = |sha: &str| {
            line(&[
                sha,
                "p1",
                "me@x.io",
                "2026-11-03T10:00:00+00:00",
                "feat: a (#1)",
            ])
        };
        let (mut seen, mut w) = (HashSet::new(), BTreeMap::new());
        tally_landings(
            &l("m1"),
            "repo-a",
            &me(),
            &mut seen,
            (None, None),
            &utc(),
            &mut w,
        );
        tally_landings(
            &l("m2"),
            "repo-b",
            &me(),
            &mut seen,
            (None, None),
            &utc(),
            &mut w,
        );
        assert_eq!(w["2026-11-02"].merged_prs, 2);
    }

    #[test]
    fn one_landing_is_counted_once_however_often_it_is_seen() {
        let l = |sha: &str| {
            line(&[
                sha,
                "p1",
                "me@x.io",
                "2026-11-03T10:00:00+00:00",
                "feat: a (#1)",
            ])
        };
        let merge = line(&[
            "m9",
            "p1 p2",
            "me@x.io",
            "2026-11-03T11:00:00+00:00",
            "chore: bump",
        ]);
        let (mut seen, mut w) = (HashSet::new(), BTreeMap::new());
        // the same commit through two clones whose identities differ (one has no remote), then main and develop
        tally_landings(
            &l("m1"),
            "clone-a",
            &me(),
            &mut seen,
            (None, None),
            &utc(),
            &mut w,
        );
        tally_landings(
            &l("m1"),
            "clone-b",
            &me(),
            &mut seen,
            (None, None),
            &utc(),
            &mut w,
        );
        // a cherry-pick of PR 1 into a release branch: another commit, the same PR number
        tally_landings(
            &l("m7"),
            "clone-a",
            &me(),
            &mut seen,
            (None, None),
            &utc(),
            &mut w,
        );
        tally_landings(
            &merge,
            "clone-a",
            &me(),
            &mut seen,
            (None, None),
            &utc(),
            &mut w,
        );
        tally_landings(
            &merge,
            "clone-a",
            &me(),
            &mut seen,
            (None, None),
            &utc(),
            &mut w,
        );
        assert_eq!(
            (w["2026-11-02"].merged_prs, w["2026-11-02"].other_merges),
            (1, 1)
        );
    }

    #[test]
    fn the_date_window_and_the_time_zone_decide_the_week() {
        let l = line(&["a1", "me@x.io", "2026-11-15T23:30:00+00:00"]); // Sunday night in UTC
        let (mut seen, mut w) = (HashSet::new(), BTreeMap::new());
        tally_commits(&l, &me(), &mut seen, (None, None), &utc(), &mut w);
        assert_eq!(w["2026-11-09"].commits, 1);
        let (mut seen, mut w8) = (HashSet::new(), BTreeMap::new());
        tally_commits(
            &l,
            &me(),
            &mut seen,
            (None, None),
            &FixedOffset::east_opt(8 * 3600).unwrap(),
            &mut w8,
        );
        assert_eq!(w8["2026-11-16"].commits, 1, "already Monday in +08:00");
        let (mut seen, mut w2) = (HashSet::new(), BTreeMap::new());
        let from = NaiveDate::from_ymd_opt(2026, 11, 16);
        tally_commits(&l, &me(), &mut seen, (from, None), &utc(), &mut w2);
        assert!(w2.is_empty(), "outside the window");
    }
}
