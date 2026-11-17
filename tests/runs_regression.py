"""Runs view, end to end: hand-built transcripts -> cc-stats -> data.json (run after cargo build)."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
BIN = ROOT / os.environ.get('CC_STATS_BIN', 'target/debug/cc-stats')
_n = [0]


def _id():
    _n[0] += 1
    return f'id{_n[0]}'


def _base(kind, ts, cwd, **extra):
    return {'type': kind, 'uuid': extra.pop('uuid', None) or _id(), 'timestamp': ts, 'cwd': cwd, **extra}


def prompt(ts, uuid, text='do it', cwd='/fixture/a'):
    return _base('user', ts, cwd, uuid=uuid, origin={'kind': 'human'}, message={'role': 'user', 'content': text})


def command(ts, uuid, name, cwd='/fixture/a'):
    text = f'<command-message>{name}</command-message>\n<command-name>/{name}</command-name>'
    return prompt(ts, uuid, text, cwd)


def assistant(ts, skills=(), cwd='/fixture/a', sidechain=False):
    content = [{'type': 'tool_use', 'id': 't', 'name': 'Skill', 'input': {'skill': s}} for s in skills]
    return _base('assistant', ts, cwd, requestId=_id(), isSidechain=sidechain, message={
        'id': _id(), 'model': 'claude-opus-5', 'content': content or [{'type': 'text', 'text': 'ok'}],
        'usage': {'input_tokens': 1, 'output_tokens': 1}})


def tool_result(ts, cwd='/fixture/a'):
    return _base('user', ts, cwd, message={'role': 'user', 'content': [{'type': 'tool_result', 'tool_use_id': 't', 'content': 'ok'}]})


def interrupt(ts, cwd='/fixture/a'):
    return _base('user', ts, cwd, message={'role': 'user', 'content': [{'type': 'text', 'text': '[Request interrupted by user]'}]})


# p1: /spectra-apply, one interrupt mid-run, then a 3 h silence that must be capped at the 15 min gap
P1 = [command('2026-11-02T10:00:00Z', 'p1', 'spectra-apply'), assistant('2026-11-02T10:00:30Z'),
      tool_result('2026-11-02T10:01:00Z'), assistant('2026-11-02T10:10:00Z'), interrupt('2026-11-02T10:10:05Z'),
      assistant('2026-11-02T10:20:00Z'), assistant('2026-11-02T13:20:00Z')]
SESSION_A = P1 + [
    # p2: no command, first skill is the trigger; the run ends on an interrupt
    prompt('2026-11-03T09:00:00Z', 'p2'), assistant('2026-11-03T09:00:20Z', ['spectra-verify']), interrupt('2026-11-03T09:00:25Z'),
    # p3: plain prompt; the subagent line after it must not extend it
    prompt('2026-11-03T10:00:00Z', 'p3'), assistant('2026-11-03T10:01:00Z'), assistant('2026-11-03T10:30:00Z', sidechain=True),
    # p4: local command, p5: prompt nobody answered -> neither is a run
    command('2026-11-03T11:00:00Z', 'p4', 'compact'), assistant('2026-11-03T11:00:05Z'),
    prompt('2026-11-04T09:00:00Z', 'p5'), tool_result('2026-11-04T09:00:10Z'),
    # p7 overlaps the /goal run in project B
    prompt('2026-11-09T08:05:00Z', 'p7'), assistant('2026-11-09T08:15:00Z'),
]
# a resumed session copies the start of p1 into a new file and adds p6
SESSION_A_RESUMED = P1[:3] + [prompt('2026-11-05T09:00:00Z', 'p6'), assistant('2026-11-05T09:00:40Z')]
SUBAGENT = [prompt('2026-11-02T11:00:00Z', 'sub1'), assistant('2026-11-02T11:30:00Z')]
# session C fans out: the main agent works 08:00-08:10 while two subagents overlap each other (08:01-08:09, 08:02-08:08)
SESSION_C = [command('2026-11-16T08:00:00Z', 'c1', 'fan-out', '/fixture/c'), assistant('2026-11-16T08:10:00Z', cwd='/fixture/c')]


def sidechain(ts, kind, cwd='/fixture/c'):
    return {**(prompt(ts, _id(), 'sub task', cwd) if kind == 'user' else assistant(ts, cwd=cwd)), 'isSidechain': True}


SUB_C_A = [sidechain('2026-11-16T08:01:00Z', 'user'), sidechain('2026-11-16T08:09:00Z', 'assistant')]
SUB_C_B = [sidechain('2026-11-16T08:02:00Z', 'user'), sidechain('2026-11-16T08:08:00Z', 'assistant')]
SESSION_B = [command('2026-11-09T08:00:00Z', 'q1', 'goal', '/fixture/b'), assistant('2026-11-09T08:10:00Z', cwd='/fixture/b')]


class RunsRegression(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory(prefix='cc-stats-runs-')
        base = Path(cls.tmp.name)
        files = {'proj-a/s1.jsonl': SESSION_A, 'proj-a/s1-resumed.jsonl': SESSION_A_RESUMED,
                 'proj-a/s1/subagents/agent-1.jsonl': SUBAGENT, 'proj-b/s2.jsonl': SESSION_B,
                 'proj-c/s3.jsonl': SESSION_C, 'proj-c/s3/subagents/agent-a.jsonl': SUB_C_A,
                 'proj-c/s3/subagents/agent-b.jsonl': SUB_C_B}
        for name, rows in files.items():
            path = base / 'projects' / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(''.join(json.dumps(r) + '\n' for r in rows))
        subprocess.run([str(BIN), '--dir', str(base / 'projects'), '--out', str(base / 'out'),
                        '--all', '--tz', 'UTC', '--format', 'json'], capture_output=True, text=True, check=True)
        cls.data_path = base / 'out/data.json'
        cls.data = json.loads(cls.data_path.read_text())
        cls.by_trigger = {t['name']: t for t in cls.data['by_trigger']}

    @classmethod
    def tearDownClass(cls):
        cls.tmp.cleanup()

    def test_only_answered_main_session_prompts_become_runs_and_resume_copies_count_once(self):
        # p1 p2 p3 p6 p7 q1 c1; not p4 (local command), p5 (unanswered), the subagents' lines, nor the copy of p1
        self.assertEqual(len(self.data['runs']), 7)
        self.assertEqual(sorted(t['runs'] for t in self.data['by_trigger']), [1, 1, 1, 1, 3])

    def test_triggers_are_discovered_from_commands_then_skills_then_plain_prompts(self):
        self.assertEqual(set(self.by_trigger), {'spectra-apply', 'spectra-verify', 'goal', 'fan-out', '(plain prompt)'})

    def test_gaps_are_capped_so_a_three_hour_silence_counts_fifteen_minutes(self):
        # 30+30+540+5+595 s of activity, then the 3 h gap counts as 900 s
        self.assertEqual(self.by_trigger['spectra-apply']['max_sec'], 2100)

    def test_interrupts_are_counted_and_the_last_marks_how_a_run_ended(self):
        apply_ = next(r for r in self.data['runs'] if r['trigger'] == 'spectra-apply')
        verify = next(r for r in self.data['runs'] if r['trigger'] == 'spectra-verify')
        self.assertEqual((apply_['interrupts'], apply_['ended_by_interrupt']), (1, False))
        self.assertEqual((verify['interrupts'], verify['ended_by_interrupt'], verify['dur_sec']), (1, True, 25))

    def test_subagent_traffic_does_not_extend_the_parent_run(self):
        plain = sorted(r['dur_sec'] for r in self.data['runs'] if r['trigger'] == '(plain prompt)')
        self.assertEqual(plain, [40, 60, 600])

    def test_trigger_summary_uses_nearest_rank_percentiles(self):
        plain = self.by_trigger['(plain prompt)']
        self.assertEqual((plain['median_sec'], plain['p90_sec'], plain['max_sec']), (60, 600, 600))

    def test_weekly_rows_and_parallelism_come_from_run_intervals(self):
        weeks = {w['week']: w for w in self.data['runs_weekly_total']}
        self.assertEqual(set(weeks), {'2026-11-02', '2026-11-09', '2026-11-16'})
        w = weeks['2026-11-09']  # /goal 08:00-08:10 and p7 08:05-08:15 overlap for 5 minutes
        self.assertEqual(w['peak_parallel'], 2)
        self.assertAlmostEqual(w['hours'] * 3600, 1200)
        self.assertAlmostEqual(w['union_hours'] * 3600, 900)
        self.assertAlmostEqual(w['avg_parallel'], 1200 / 900)
        self.assertEqual((w['peak_parallel_all'], w['subagent_hours'], w['peak_subagents']), (2, 0, 0), 'no subagents that week: both views agree')
        self.assertAlmostEqual(w['avg_parallel_all'], w['avg_parallel'])

    def test_subagents_are_counted_as_working_threads_not_as_extra_waiting(self):
        w = {x['week']: x for x in self.data['runs_weekly_total']}['2026-11-16']
        # main 08:00-08:10 (600 s) with subagents 08:01-08:09 and 08:02-08:08 inside it
        self.assertEqual((w['peak_parallel'], w['peak_parallel_all'], w['peak_subagents']), (1, 2, 2))
        self.assertAlmostEqual(w['hours'] * 3600, 600)
        self.assertAlmostEqual(w['subagent_hours'] * 3600, 840)
        # main time while a subagent works is dropped: 120 s of main + 840 s of subagents
        self.assertAlmostEqual(w['hours_all'] * 3600, 960)
        self.assertAlmostEqual(w['union_hours_all'] * 3600, 600)
        self.assertAlmostEqual(w['avg_parallel_all'], 1.6)

    def test_a_lone_subagent_far_from_the_main_run_adds_hours_not_parallelism(self):
        w = {x['week']: x for x in self.data['runs_weekly_total']}['2026-11-02']
        # the s1 subagent ran 11:00 with a 30 min silence (capped to 15 min), after every main run of the week
        self.assertAlmostEqual(w['subagent_hours'] * 3600, 900)
        self.assertEqual(w['peak_parallel_all'], w['peak_parallel'])
        self.assertAlmostEqual(w['hours_all'] * 3600, w['hours'] * 3600 + 900)

    def test_run_exclude_drops_a_command_on_request(self):
        with tempfile.TemporaryDirectory(prefix='cc-stats-runs-ex-') as tmp:
            out = Path(tmp) / 'out'
            subprocess.run([str(BIN), '--dir', str(self.data_path.parents[1] / 'projects'), '--out', str(out), '--all',
                            '--tz', 'UTC', '--format', 'json', '--run-exclude', 'GOAL'], capture_output=True, text=True, check=True)
            names = {t['name'] for t in json.loads((out / 'data.json').read_text())['by_trigger']}
        self.assertNotIn('goal', names)

    def test_the_viewer_recomputes_the_same_numbers_as_data_json(self):
        r = subprocess.run(['node', str(ROOT / 'tests/runs_view.cjs'), str(self.data_path)], capture_output=True, text=True)
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)



EVIL = '</script><img src=x onerror=alert(1)>'


class ReportEmbedding(unittest.TestCase):
    """Names come from transcripts, so they must not be able to end the <script> element they are embedded in."""

    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory(prefix='cc-stats-embed-')
        base = Path(cls.tmp.name)
        rows = [command('2026-11-02T10:00:00Z', 'e1', EVIL, '/fixture/<x>'), assistant('2026-11-02T10:00:30Z', [EVIL + '2\u2028\u2029'], '/fixture/<x>')]
        path = base / 'projects/proj-x/s.jsonl'
        path.parent.mkdir(parents=True)
        path.write_text(''.join(json.dumps(r) + '\n' for r in rows))
        subprocess.run([str(BIN), '--dir', str(base / 'projects'), '--out', str(base / 'out'), '--all', '--tz', 'UTC'],
                       capture_output=True, text=True, check=True)
        cls.html = (base / 'out/report.html').read_text()
        cls.template = (ROOT / 'templates/report.html').read_text()

    @classmethod
    def tearDownClass(cls):
        cls.tmp.cleanup()

    def test_a_hostile_name_cannot_end_the_script_element(self):
        self.assertEqual(self.html.count('</script'), self.template.count('</script'))
        self.assertNotIn('<img src=x onerror', self.html)
        self.assertEqual(self.html.count('<!--'), self.template.count('<!--'))

    def test_the_embedded_data_still_round_trips_exactly(self):
        i = self.html.index('const DATA = ') + len('const DATA = ')
        data, _ = json.JSONDecoder().raw_decode(self.html[i:])
        self.assertEqual(data['runs'][0]['trigger'], EVIL)
        self.assertEqual(data['runs'][0]['skills'], [EVIL + '2\u2028\u2029'])

    def test_line_separators_are_escaped_too(self):
        # U+2028 / U+2029 were line terminators in JS string literals before ES2019
        self.assertNotIn('\u2028', self.html.split('const DATA = ', 1)[1].split('const STORAGE_KEY', 1)[0])


if __name__ == '__main__':
    unittest.main()
