"""What landed, end to end: real git repos + transcripts -> cc-stats -> data.json (run after cargo build).

The repo has a main and a develop branch (origin/HEAD absent, like a fresh local repo), merges into develop
the way a git-flow repo does, and a commit by someone else. Dates are fixed so the weeks are known."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import sys
import unittest

sys.dont_write_bytecode = True   # importing the sibling test module must not leave tests/__pycache__ behind
from runs_regression import BIN, assistant, prompt

ME, OTHER = 'me@example.com', 'other@example.com'


class Git:
    def __init__(self, repo, home):
        self.repo, self.home = Path(repo), Path(home)

    def run(self, *args, who=ME, when=None, check=True):
        env = {**os.environ, 'HOME': str(self.home), 'GIT_CONFIG_NOSYSTEM': '1', 'GIT_CONFIG_GLOBAL': os.devnull,
               'GIT_AUTHOR_NAME': who, 'GIT_AUTHOR_EMAIL': who, 'GIT_COMMITTER_NAME': who, 'GIT_COMMITTER_EMAIL': who}
        if when:
            env['GIT_AUTHOR_DATE'] = env['GIT_COMMITTER_DATE'] = when
        r = subprocess.run(['git', '-c', 'commit.gpgsign=false', '-C', str(self.repo), *args], env=env, capture_output=True, text=True)
        if check and r.returncode:
            raise AssertionError(f'git {args}: {r.stderr}')
        return r.stdout

    def commit(self, message, when, who=ME):
        slug = ''.join(c if c.isalnum() else '_' for c in message)
        (self.repo / f'{slug}.txt').write_text(f'{message}\n{when}\n')   # one file per commit: merges never conflict
        self.run('add', '-A')
        self.run('commit', '-q', '-m', message, who=who, when=when)

    def merge(self, branch, message, when):
        self.run('merge', '-q', '--no-ff', '-m', message, branch, when=when)


def build_history(repo, home):
    g = Git(repo, home)
    repo.mkdir(parents=True)
    g.run('init', '-q', '-b', 'main')
    g.run('config', 'user.email', ME)
    g.commit('init', '2026-11-02T09:00:00+00:00')
    g.run('branch', 'develop')
    g.run('checkout', '-q', '-b', 'feat/a', 'develop')
    g.commit('a1', '2026-11-03T09:00:00+00:00')
    g.commit('a2', '2026-11-03T09:30:00+00:00')
    g.run('checkout', '-q', 'develop')
    g.merge('feat/a', 'feat: add a (#7)', '2026-11-04T10:00:00+00:00')            # a PR landing
    g.run('checkout', '-q', '-b', 'feat/b', 'develop')
    g.commit('b1', '2026-11-04T10:30:00+00:00')
    g.run('checkout', '-q', 'develop')
    g.merge('feat/b', 'chore: bump submodules', '2026-11-04T11:00:00+00:00')      # a merge with a custom subject
    g.run('checkout', '-q', 'main')
    g.commit('hotfix', '2026-11-05T09:00:00+00:00')
    g.run('checkout', '-q', 'develop')
    g.merge('main', "Merge branch 'main' into develop", '2026-11-05T10:00:00+00:00')   # housekeeping, not a landing
    g.commit('feat: theirs (#9)', '2026-11-10T09:00:00+00:00', who=OTHER)             # someone else's PR
    g.commit('fix: z (#8)', '2026-11-10T10:00:00+00:00')                              # my squash-style PR landing
    return g


class Outcomes(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory(prefix='cc-stats-git-')
        cls.base = Path(cls.tmp.name)
        cls.home = cls.base / 'home'
        cls.home.mkdir()
        cls.repo = cls.base / 'repo-a'
        g = build_history(cls.repo, cls.home)
        remote = 'https://example.com/me/repo-a.git'      # both checkouts point at the same remote, as two clones do
        g.run('remote', 'add', 'origin', remote)
        cls.clone = cls.base / 'repo-a-clone'
        env = {**os.environ, 'HOME': str(cls.home), 'GIT_CONFIG_GLOBAL': os.devnull}
        subprocess.run(['git', 'clone', '-q', str(cls.repo), str(cls.clone)], check=True, capture_output=True, env=env)
        subprocess.run(['git', '-C', str(cls.clone), 'remote', 'set-url', 'origin', remote], check=True, capture_output=True, env=env)
        cls.gone = '/nonexistent/cc-stats-test/other'     # a project whose directory is not on this machine

        def session(cwd, day, hour):   # a 10 minute run: prompt, then the agent's reply ten minutes later
            return [prompt(f'2026-11-{day}T{hour}:00:00Z', f'{cwd}{day}{hour}', 'work', cwd),
                    assistant(f'2026-11-{day}T{hour}:10:00Z', [], cwd)]
        projects = cls.base / 'projects'
        files = {'proj-a/s.jsonl': session(str(cls.repo), '03', '12') + session(str(cls.repo), '10', '12'),
                 'proj-clone/s.jsonl': session(str(cls.clone), '03', '14'),
                 'proj-gone/s.jsonl': session(cls.gone, '03', '13')}
        for name, rows in files.items():
            path = projects / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(''.join(json.dumps(r) + '\n' for r in rows))
        cls.projects = projects

    @classmethod
    def tearDownClass(cls):
        cls.tmp.cleanup()

    def report(self, *extra):
        out = self.base / f'out{abs(hash(extra))}'
        env = {**os.environ, 'HOME': str(self.home), 'GIT_CONFIG_GLOBAL': os.devnull, 'GIT_CONFIG_NOSYSTEM': '1'}
        subprocess.run([str(BIN), '--dir', str(self.projects), '--out', str(out), '--all', '--tz', 'UTC', '--format', 'json', *extra],
                       capture_output=True, text=True, check=True, env=env)
        d = json.loads((out / 'data.json').read_text())
        return d, {w['week']: w for w in d['runs_weekly_total']}

    def test_merges_commits_and_coverage_are_counted_from_git(self):
        d, wk = self.report()
        g = d['runs_git']
        # the clone shares history with repo-a: one repo's worth of work, not two
        self.assertEqual((g['enabled'], g['authors'], g['projects_total'], g['projects_tracked']), (True, [ME], 3, 2))
        w1, w2 = wk['2026-11-02'], wk['2026-11-09']
        # week of 11-02: PR 7 and the "bump submodules" merge. The back-merge of main and the other author's PR are not mine.
        self.assertEqual((w1['merged_prs'], w1['other_merges'], w1['landed']), (1, 1, 2))
        self.assertEqual((w2['merged_prs'], w2['other_merges'], w2['landed']), (1, 0, 1))
        # my non-merge commits: init, a1, a2, b1, hotfix in the first week; "fix: z" in the second
        self.assertEqual((w1['commits'], w2['commits']), (5, 1))

    def test_a_clone_of_the_same_repo_does_not_double_the_counts(self):
        d, wk = self.report()
        self.assertEqual(d['runs_git']['repos'], 1)
        self.assertEqual(sum(w['landed'] for w in wk.values()), 3)
        self.assertEqual(sum(w['commits'] for w in wk.values()), 6)

    def test_per_hour_figures_divide_by_the_hours_of_projects_git_can_see(self):
        _, wk = self.report()
        w = wk['2026-11-02']
        # three 10 minute runs that week: repo-a, its clone (both tracked) and a project git cannot see
        self.assertAlmostEqual(w['hours_all'] * 3600, 1800)
        self.assertAlmostEqual(w['hours_tracked'] * 3600, 1200)
        self.assertAlmostEqual(w['landed_per_agent_hour'], 2 / (1200 / 3600))
        self.assertAlmostEqual(w['prs_per_agent_hour'], 1 / (1200 / 3600))
        self.assertAlmostEqual(w['commits_per_agent_hour'], 5 / (1200 / 3600))

    def test_no_git_reads_nothing(self):
        d, wk = self.report('--no-git')
        self.assertFalse(d['runs_git']['enabled'])
        for w in wk.values():
            self.assertEqual((w['landed'], w['commits'], w['hours_tracked'], w['landed_per_agent_hour']), (0, 0, 0, 0))

    def test_git_author_chooses_whose_work_counts(self):
        d, wk = self.report('--git-author', OTHER)
        self.assertEqual(d['runs_git']['authors'], [OTHER])
        # only the other author's commit and PR 9
        self.assertEqual((wk['2026-11-09']['commits'], wk['2026-11-09']['merged_prs']), (1, 1))
        self.assertEqual((wk['2026-11-02']['landed'], wk['2026-11-02']['commits']), (0, 0))

    def test_anonymize_does_not_publish_the_author_email(self):
        d, wk = self.report('--anonymize')
        self.assertEqual(d['runs_git']['authors'], [])
        self.assertEqual(wk['2026-11-02']['landed'], 2, 'the counts are still there')

    def test_the_date_window_limits_what_is_counted(self):
        _, wk = self.report('--from', '2026-11-09')
        self.assertNotIn('2026-11-02', {w for w, v in wk.items() if v['landed'] or v['commits']})
        self.assertEqual(wk['2026-11-09']['landed'], 1)


class RepoConfigCannotRunPrograms(unittest.TestCase):
    """A repository's own config must not make reading its history run a program. A signed commit plus
    `log.showSignature` in the repo config used to make `git log` execute `gpg.program`."""

    def test_git_log_does_not_run_the_configured_gpg_program(self):
        with tempfile.TemporaryDirectory(prefix='cc-stats-cfg-') as tmp:
            base = Path(tmp)
            home = base / 'home'
            home.mkdir()
            repo = base / 'repo'
            g = Git(repo, home)
            repo.mkdir()
            g.run('init', '-q', '-b', 'main')
            g.run('config', 'user.email', ME)
            g.commit('init', '2026-11-02T09:00:00+00:00')
            # a commit object with a (fake) gpgsig header: enough for git to try to verify it
            tree = g.run('rev-parse', 'HEAD^{tree}').strip()
            parent = g.run('rev-parse', 'HEAD').strip()
            raw = (f'tree {tree}\nparent {parent}\nauthor {ME} <{ME}> 1790000000 +0000\n'
                   f'committer {ME} <{ME}> 1790000000 +0000\n'
                   'gpgsig -----BEGIN PGP SIGNATURE-----\n \n fake\n -----END PGP SIGNATURE-----\n\nsigned work (#3)\n')
            signed = subprocess.run(['git', '-C', str(repo), 'hash-object', '-t', 'commit', '-w', '--stdin'],
                                    input=raw, capture_output=True, text=True, check=True).stdout.strip()
            g.run('update-ref', 'refs/heads/main', signed)
            marker = base / 'ran'
            gpg = base / 'evil-gpg'
            gpg.write_text(f'#!/bin/sh\ntouch {marker}\nexit 1\n')
            gpg.chmod(0o755)
            g.run('config', 'log.showSignature', 'true')
            g.run('config', 'gpg.program', str(gpg))

            projects = base / 'projects'
            rows = [prompt('2026-11-03T12:00:00Z', 'cfg', 'work', str(repo)), assistant('2026-11-03T12:10:00Z', [], str(repo))]
            (projects / 'proj-cfg').mkdir(parents=True)
            (projects / 'proj-cfg/s.jsonl').write_text(''.join(json.dumps(r) + '\n' for r in rows))
            env = {**os.environ, 'HOME': str(home), 'GIT_CONFIG_GLOBAL': os.devnull, 'GIT_CONFIG_NOSYSTEM': '1'}
            r = subprocess.run([str(BIN), '--dir', str(projects), '--out', str(base / 'out'), '--all', '--tz', 'UTC',
                                '--format', 'json'], capture_output=True, text=True, env=env)
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertFalse(marker.exists(), 'the repository config ran gpg.program')


if __name__ == '__main__':
    unittest.main()
