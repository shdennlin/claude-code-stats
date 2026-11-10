"""End-to-end regressions: run after cargo build (no third-party dependencies)."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
BIN = ROOT / os.environ.get('CC_STATS_BIN', 'target/debug/cc-stats')


def assistant(mid, ts, output=10, request='req', **usage):
    return {'type': 'assistant', 'uuid': f'{mid}-{ts}', 'timestamp': ts,
            'requestId': request, 'cwd': '/fixture', 'message': {
                'id': mid, 'model': 'claude-opus-5', 'content': [],
                'usage': {'input_tokens': 100, 'output_tokens': output,
                          'cache_read_input_tokens': 1000, **usage}}}


def with_advisor(row, model='claude-fable-5-2', input_tokens=200_000, output=9_000):
    """Mark `row` as a turn whose usage.iterations holds one host call plus one advisor call.

    Claude Code leaves the advisor call out of the top-level usage, so the top level must keep
    describing only the host call.
    """
    u = row['message']['usage']
    u['iterations'] = [
        {'type': 'message', 'input_tokens': u['input_tokens'], 'output_tokens': u['output_tokens'],
         'cache_read_input_tokens': u['cache_read_input_tokens']},
        {'type': 'advisor_message', 'model': model, 'input_tokens': input_tokens,
         'output_tokens': output, 'cache_read_input_tokens': 0, 'cache_creation_input_tokens': 0},
    ]
    return row


class UsageRegression(unittest.TestCase):
    def run_report(self, files, *args):
        with tempfile.TemporaryDirectory(prefix='cc-stats-regression-') as tmp:
            base = Path(tmp)
            for name, records in files.items():
                path = base / 'projects' / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(''.join(json.dumps(r) + '\n' for r in records))
            subprocess.run([str(BIN), '--dir', str(base / 'projects'), '--out',
                            str(base / 'out'), '--all', '--tz', '+08:00', *args],
                           capture_output=True, text=True, check=True)
            return json.loads((base / 'out/data.json').read_text())

    def test_stream_snapshots_merge_maximum_usage_without_losing_events(self):
        rows = [assistant('stream', '2026-10-02T01:00:00Z', 2),
                assistant('stream', '2026-10-02T01:00:01Z', 30),
                assistant('stream', '2026-10-02T01:00:02Z', 3)]
        r = self.run_report({'p/a.jsonl': rows})
        self.assertEqual(r['summary']['tokens']['input'], 100)
        self.assertEqual(r['summary']['tokens']['output'], 30)
        self.assertEqual(r['summary']['tokens']['cache_read'], 1000)
        self.assertEqual(r['summary']['messages'], 3)
        self.assertAlmostEqual(r['summary']['cost_usd'], .00175)

    def test_forked_transcript_is_not_billed_again(self):
        rows = [assistant('copy', '2026-10-02T01:00:00Z')]
        r = self.run_report({'p/a.jsonl': rows, 'p/b.jsonl': rows})
        self.assertEqual(r['summary']['tokens']['input'], 100)
        self.assertEqual(r['summary']['tokens']['output'], 10)
        self.assertAlmostEqual(sum(s['cost_usd'] for s in r['sessions']), .00125)

    def test_cross_file_snapshots_merge_each_category_not_just_last_row(self):
        early = assistant('snapshot', '2026-10-02T01:00:00Z', 2)
        late = assistant('snapshot', '2026-10-02T01:00:01Z', 30)
        late['message']['usage']['input_tokens'] = 1
        r = self.run_report({'p/a.jsonl': [early], 'p/b.jsonl': [late]})
        self.assertEqual(r['summary']['tokens']['input'], 100)
        self.assertEqual(r['summary']['tokens']['output'], 30)
        self.assertAlmostEqual(r['summary']['cost_usd'], .00175)

    def test_anonymized_daily_slices_keep_same_session_title(self):
        rows = [{'type': 'custom-title', 'customTitle': 'private title'},
                assistant('before', '2026-10-01T01:00:00Z'),
                assistant('after', '2026-10-02T01:00:00Z')]
        r = self.run_report({'p/a.jsonl': rows}, '--anonymize')
        for day in r['session_days']:
            self.assertEqual(day['file'], '')
            self.assertEqual(day['title'], r['sessions'][0]['title'])

    def test_equal_output_snapshot_can_complete_billing_modifiers(self):
        early = assistant('metadata', '2026-10-02T01:00:00Z')
        late = assistant('metadata', '2026-10-02T01:00:01Z',
                         speed='fast', inference_geo='us')
        r = self.run_report({'p/a.jsonl': [early, late]})
        self.assertAlmostEqual(r['summary']['cost_usd'], .00275)

    def test_more_complete_tokens_do_not_erase_known_billing_modifiers(self):
        early = assistant('metadata', '2026-10-02T01:00:00Z',
                          speed='fast', inference_geo='us')
        late = assistant('metadata', '2026-10-02T01:00:01Z', 40)
        r = self.run_report({'p/a.jsonl': [early, late]})
        self.assertAlmostEqual(r['summary']['cost_usd'], .0044)

    def test_request_id_distinguishes_independent_calls(self):
        rows = [assistant('same', '2026-10-02T01:00:00Z', request='r1'),
                assistant('same', '2026-10-02T01:00:01Z', request='r2')]
        r = self.run_report({'p/a.jsonl': rows})
        self.assertEqual(r['summary']['tokens']['input'], 200)

    def test_missing_request_id_still_deduplicates_by_message(self):
        rows = [assistant('same', '2026-10-02T01:00:00Z'),
                assistant('same', '2026-10-02T01:00:01Z')]
        for row in rows:
            row.pop('requestId')
        r = self.run_report({'p/a.jsonl': rows})
        self.assertEqual(r['summary']['tokens']['input'], 100)

    def test_cli_date_filter_includes_later_messages_of_older_session(self):
        rows = [assistant('before', '2026-10-01T01:00:00Z'),
                assistant('inside', '2026-10-02T01:00:00Z', 20),
                assistant('after', '2026-10-03T01:00:00Z', 30)]
        r = self.run_report({'p/a.jsonl': rows}, '--from', '2026-10-02', '--to', '2026-10-02')
        self.assertEqual(r['summary']['sessions'], 1)
        self.assertEqual(r['summary']['tokens']['output'], 20)
        self.assertEqual([d['date'] for d in r['daily']], ['2026-10-02'])
        self.assertAlmostEqual(r['summary']['cost_usd'], .0015)

    def test_cache_total_with_partial_ttl_split_keeps_all_writes(self):
        row = assistant('ttl', '2026-10-02T01:00:00Z',
                        cache_creation_input_tokens=100,
                        cache_creation={'ephemeral_1h_input_tokens': 60})
        r = self.run_report({'p/a.jsonl': [row]})
        self.assertEqual(r['summary']['tokens']['cache_create_5m'], 40)
        self.assertEqual(r['summary']['tokens']['cache_create_1h'], 60)

    def test_fractional_midnight_interval_does_not_lose_active_seconds(self):
        rows = [assistant('before', '2026-10-01T15:59:59.500Z'),
                assistant('after', '2026-10-01T16:00:00.500Z')]
        r = self.run_report({'p/a.jsonl': rows})
        self.assertEqual(r['summary']['active_sec'], 1)
        self.assertEqual(r['summary']['active_sec_union'], 1)
        self.assertEqual(sum(d['active_sec'] for d in r['session_days']), 1)

    def test_later_ttl_split_does_not_add_a_second_cache_write(self):
        early = assistant('ttl', '2026-10-02T01:00:00Z', cache_creation_input_tokens=100)
        late = assistant('ttl', '2026-10-02T01:00:01Z', cache_creation_input_tokens=100,
                         cache_creation={'ephemeral_1h_input_tokens': 60})
        r = self.run_report({'p/a.jsonl': [early, late]})
        self.assertEqual(r['summary']['tokens']['cache_create_5m'], 40)
        self.assertEqual(r['summary']['tokens']['cache_create_1h'], 60)

    def test_midnight_cli_filter_preserves_only_inside_active_interval(self):
        rows = [assistant('before', '2026-10-01T15:59:00Z'),
                assistant('after', '2026-10-01T16:01:00Z')]
        r = self.run_report({'p/a.jsonl': rows}, '--from', '2026-10-02', '--to', '2026-10-02')
        self.assertEqual(r['summary']['active_sec'], 60)
        self.assertEqual(r['summary']['tokens']['input'], 100)
        self.assertEqual(r['session_days'][0]['date'], '2026-10-02')

    def test_per_date_slices_reconcile_with_session_and_daily_costs(self):
        rows = [assistant('before', '2026-10-01T15:59:00Z'),
                assistant('after', '2026-10-01T16:01:00Z', 20)]
        r = self.run_report({'p/a.jsonl': rows})
        self.assertEqual(r['summary']['sessions'], 1)
        self.assertEqual(len(r['session_days']), 2)
        self.assertEqual(sum(s['active_sec'] for s in r['session_days']), 120)
        for day in r['daily']:
            slices = [s for s in r['session_days'] if s['date'] == day['date']]
            self.assertAlmostEqual(sum(s['cost_usd'] for s in slices), day['cost_usd'])
            self.assertEqual(sum(s['active_sec'] for s in slices), 60)

    def test_advisor_iteration_is_billed_to_its_own_model(self):
        # host: opus-5, 100 in / 10 out / 1000 cache read; advisor: fable-5-2, 200k in / 9k out.
        row = with_advisor(assistant('adv', '2026-10-02T01:00:00Z'))
        r = self.run_report({'p/a.jsonl': [row]})
        host = 100 * 5 + 10 * 25 + 1000 * .5          # opus-5 rates, per million
        advisor = 200_000 * 8 + 9_000 * 40            # fable-5-2 rates, per million
        by_model = {m['name']: m for m in r['by_model']}
        self.assertAlmostEqual(by_model['claude-opus-5']['cost_usd'], host / 1e6)
        self.assertAlmostEqual(by_model['claude-fable-5-2']['cost_usd'], advisor / 1e6)
        self.assertEqual(by_model['claude-fable-5-2']['tokens']['input'], 200_000)
        self.assertEqual(by_model['claude-fable-5-2']['tokens']['output'], 9_000)
        self.assertEqual(by_model['claude-fable-5-2']['messages'], 0)   # an iteration is not a message
        self.assertEqual(by_model['claude-opus-5']['messages'], 1)
        self.assertAlmostEqual(r['summary']['cost_usd'], (host + advisor) / 1e6)
        self.assertEqual(r['summary']['tokens']['input'], 100 + 200_000)
        self.assertEqual(r['summary']['messages'], 1)

    def test_advisor_iteration_is_counted_once_across_duplicate_records(self):
        # Claude Code writes one record per content block, all carrying the same usage; forks copy them too.
        rows = [with_advisor(assistant('adv', f'2026-10-02T01:00:0{i}Z')) for i in range(3)]
        r = self.run_report({'p/a.jsonl': rows, 'p/b.jsonl': rows})
        by_model = {m['name']: m for m in r['by_model']}
        self.assertEqual(by_model['claude-fable-5-2']['tokens']['input'], 200_000)
        self.assertAlmostEqual(by_model['claude-fable-5-2']['cost_usd'], (200_000 * 8 + 9_000 * 40) / 1e6)
        self.assertEqual(r['summary']['messages'], 6)   # raw event count is unchanged

    def test_advisor_cost_reconciles_across_session_day_and_hour_views(self):
        row = with_advisor(assistant('adv', '2026-10-02T01:00:00Z'))
        r = self.run_report({'p/a.jsonl': [row]})
        total = r['summary']['cost_usd']
        self.assertAlmostEqual(sum(s['cost_usd'] for s in r['sessions']), total)
        self.assertAlmostEqual(sum(s['cost_usd'] for s in r['session_days']), total)
        self.assertAlmostEqual(sum(d['cost_usd'] for d in r['daily']), total)
        self.assertAlmostEqual(sum(h['cost_usd'] for h in r['by_hour']), total)
        days_fable = sum(s['by_model'].get('claude-fable-5-2', {}).get('cost_usd', 0) for s in r['session_days'])
        self.assertAlmostEqual(days_fable, (200_000 * 8 + 9_000 * 40) / 1e6)

    def test_turn_without_advisor_is_unchanged_by_iterations_breakdown(self):
        row = assistant('plain', '2026-10-02T01:00:00Z')
        u = row['message']['usage']
        u['iterations'] = [{'type': 'message', 'input_tokens': 100, 'output_tokens': 10,
                            'cache_read_input_tokens': 1000}]
        r = self.run_report({'p/a.jsonl': [row]})
        self.assertAlmostEqual(r['summary']['cost_usd'], (100 * 5 + 10 * 25 + 1000 * .5) / 1e6)
        self.assertEqual([m['name'] for m in r['by_model']], ['claude-opus-5'])


if __name__ == '__main__':
    unittest.main()
