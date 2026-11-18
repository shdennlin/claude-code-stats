// Runs view: the viewer's own aggregation and rendering, run against a fake DOM and a fake Chart.
// `node tests/runs_view.cjs [data.json]` -- with a data.json from cc-stats it also cross-checks every
// trigger against the numbers cc-stats computed itself.
const assert = require('node:assert/strict');
// arrays made inside the vm context have another prototype, so compare them by value
const same = (a, b, msg) => assert.equal(JSON.stringify(a), JSON.stringify(b), msg);
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');

class El {
  constructor(tag) { this.tag = tag; this.children = []; this.attrs = {}; this.handlers = {}; this.dataset = {};
    this.style = {}; this.hidden = false; this.value = ''; this.checked = false; this.className = ''; this._text = ''; }
  get classList() { const s = this; return {toggle(c, on) { s.className = (s.className.split(' ').filter(x => x && x !== c).concat(on ? [c] : [])).join(' '); }}; }
  get firstChild() { return this.children[0] || null; }
  get textContent() { return this._text + this.children.map(c => c.textContent).join(''); }
  set textContent(v) { this._text = String(v); this.children = []; }
  setAttribute(k, v) { this.attrs[k] = v; if (k === 'id') { this.id = v; registry.set(v, this); } }
  appendChild(c) { this.children.push(c); return c; }
  append(...cs) { cs.forEach(c => this.appendChild(c)); }
  removeChild(c) { this.children = this.children.filter(x => x !== c); }
  addEventListener(ev, fn) { (this.handlers[ev] = this.handlers[ev] || []).push(fn); }
  click() { (this.handlers.click || []).forEach(f => f({target: this})); }
  all(tag, out = []) { for (const c of this.children) { if (c.tag === tag) out.push(c); c.all(tag, out); } return out; }
}
const registry = new Map();
const tabs = [new El('button'), new El('button')];
tabs[0].dataset.view = 'overview'; tabs[1].dataset.view = 'runs';
const tbody = new El('tbody');
const document = {
  getElementById(id) { if (!registry.has(id)) { const e = new El('div'); e.id = id; registry.set(id, e); } return registry.get(id); },
  createElement: tag => new El(tag), createElementNS: (_, tag) => new El(tag),
  createTextNode: t => { const e = new El('#text'); e._text = String(t); return e; },
  querySelector: sel => (sel === '#t-runs tbody' ? tbody : new El('div')),
  querySelectorAll: sel => (sel === '#view-tabs button' ? tabs : []),
  addEventListener() {},
};
const created = [];
class Chart { constructor(canvas, cfg) { this.id = canvas.id; created.push({id: canvas.id, cfg, chart: this}); } destroy() { this.destroyed = true; } }

const html = fs.readFileSync(path.join(__dirname, '../templates/report.html'), 'utf8');
const script = html.match(/<script>\s*([\s\S]*?)<\/script>/)[1].replace(/\ninit\(\);\s*$/, '');

// ---- fixture: runs spread over three weeks, two projects ----
const day = n => new Date(Date.UTC(2026, 9, 26 + n)).toISOString().slice(0, 10);   // 2026-10-26 is a Monday
const mk = (n, trigger, dur, o = {}) => ({date: day(n), start: Date.UTC(2026, 9, 26 + n, 9) / 1000, dur_sec: dur, trigger,
  interrupts: 0, ended_by_interrupt: false, project: 'proj-a', ...o});
const runs = [
  ...[0, 1, 2, 7, 8, 14].map((n, i) => mk(n, 'spectra-apply', [600, 1200, 1800, 900, 2400, 3000][i], {interrupts: i % 2, ended_by_interrupt: i % 3 === 0, skills: ['spectra-verify']})),
  ...[0, 1, 2, 3, 4, 7].map(n => mk(n, '(plain prompt)', 30 + n * 10)),
  mk(15, 'brand-new', 700), mk(16, 'brand-new', 800), mk(16, 'brand-new', 900), mk(17, 'brand-new', 950), mk(18, 'brand-new', 700),
  mk(1, 'other-project-skill', 5000, {project: 'proj-b'}),
];
// the tab bar must be reachable without scrolling past the filters: right after the header, before <main>
assert.ok(html.indexOf('id="view-tabs"') > html.indexOf('</header>') && html.indexOf('id="view-tabs"') < html.indexOf('<main>'), 'tab bar sits between the header and the page body');
assert.match(html, /\.tabs\{[^}]*position:sticky/, 'and stays visible while scrolling');
const external = process.argv[2] ? JSON.parse(fs.readFileSync(process.argv[2], 'utf8')) : null;
const data = external || {summary: {earliest: day(0), latest: day(18), tz_offset: '+00:00'}, runs, runs_weekly_total: [
  {week: day(0), avg_parallel: 1.2, peak_parallel: 3}, {week: day(7), avg_parallel: 1.5, peak_parallel: 2}]};

const context = vm.createContext({console, document, Chart, localStorage: {getItem: () => null, setItem() {}}});
vm.runInContext(script.replace('/*__CC_STATS_DATA__*/null', JSON.stringify(data)) + `
  globalThis.api = {state, DATA, runsFiltered, runGroups, summarizeRuns, runWeekly, runPct, weekStart, weeksBetween, renderRuns, renderRunsHeat, initRuns, setView, RUN_COLS};
`, context);
const {api} = context;
assert.equal(api.state.runsOutcome, 'landed', 'merges, not commits, are the default outcome');
const open = (from, to) => { api.state.from = from; api.state.to = to;
  api.state.projects = new Set(api.runsFiltered.call(null) && [...new Set(api.DATA.runs.map(r => r.project))]); };
const reset = () => { created.length = 0; registry.clear(); tbody.children = []; };

// ---- cross-check against cc-stats' own numbers (only with a real data.json) ----
if (external) {
  open(external.summary.earliest, external.summary.latest);
  const groups = api.runGroups(api.runsFiltered(), 'trigger');
  api.renderRuns();                                   // also warms the first-seen cache the summaries use
  for (const t of external.by_trigger) {
    const s = api.summarizeRuns(t.name, groups.get(t.name));
    assert.equal(s.runs, t.runs, t.name + ' runs');
    assert.equal(s.median, t.median_sec, t.name + ' median');
    assert.equal(s.p90, t.p90_sec, t.name + ' p90');
    assert.equal(s.p99, t.p99_sec, t.name + ' p99');
    assert.equal(s.max, t.max_sec, t.name + ' max');
    assert.equal(s.interrupts, t.interrupts, t.name + ' interrupts');
    assert.ok(Math.abs(s.hours - t.hours) < 1e-9, t.name + ' hours');
    assert.equal(s.isNew, t.is_new, t.name + ' new flag');
    assert.equal(s.runner, t.runner, t.name + ' runner flag');
  }
  const wk = api.weekStart;
  for (const w of external.runs_weekly) {
    const rs = groups.get(w.trigger).filter(r => wk(r.date) === w.week);
    const got = api.runWeekly(rs, [w.week], 1)[0];
    assert.equal(got.n, w.runs, w.trigger + ' ' + w.week + ' n');
    assert.equal(got.median, w.median_sec, w.trigger + ' ' + w.week + ' median');
    assert.equal(got.p90, w.p90_sec, w.trigger + ' ' + w.week + ' p90');
  }
  console.log('Runs view: viewer numbers match cc-stats for', external.by_trigger.length, 'triggers and', external.runs_weekly.length, 'weekly rows.');
  process.exit(0);
}

// ---- pure aggregation ----
assert.equal(api.runPct([], .5), 0);
const ten = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
same([.5, .9, .99].map(p => api.runPct(ten, p)), [6, 10, 10], 'nearest-rank percentiles, same rule as cc-stats');
assert.equal(api.runPct([1, 2, 3, 4, 5, 6, 7], .5), 4, 'index is floor(n * p): 3.5 -> 3, not rounded up');
assert.equal(api.weekStart('2026-11-09'), '2026-11-09');
assert.equal(api.weekStart('2026-11-15'), '2026-11-09');
assert.equal(api.weekStart('2026-11-16'), '2026-11-16');
same(api.weeksBetween('2026-11-11', '2026-11-24'), ['2026-11-09', '2026-11-16', '2026-11-23']);

open(day(0), day(18));
api.state.omitRules = []; api.state.mergeRules = [];
assert.equal(api.runsFiltered().length, runs.length, 'whole window keeps everything');
open(day(7), day(18));
assert.ok(api.runsFiltered().every(r => r.date >= day(7)), 'date window');
open(day(0), day(18));
api.state.projects = new Set(['proj-a']);
assert.ok(!api.runsFiltered().some(r => r.project === 'proj-b'), 'project selection');
api.state.projects = new Set(['proj-a', 'proj-b']); api.state.omitRules = ['proj-b'];
assert.ok(!api.runsFiltered().some(r => r.project === 'proj-b'), 'omit rule');
api.state.omitRules = []; api.state.mergeRules = [{pattern: 'proj-b', label: 'proj-a'}];
assert.equal(api.runsFiltered().length, runs.length, 'merge rule folds proj-b into the selected proj-a');
api.state.mergeRules = [];
api.state.projects = new Set(['proj-a', 'proj-b']);

const byTrig = api.runGroups(api.runsFiltered(), 'trigger'), bySkill = api.runGroups(api.runsFiltered(), 'skill');
assert.equal(byTrig.get('spectra-apply').length, 6);
assert.ok(bySkill.get('spectra-verify') && bySkill.get('spectra-verify').length === 6, 'a skill used inside runs gets its own group');
assert.ok(!bySkill.has('(plain prompt)'), 'plain prompts are not a skill');
const apply = api.summarizeRuns('spectra-apply', byTrig.get('spectra-apply'));
same([apply.runs, apply.median, apply.p90, apply.max], [6, 1800, 3000, 3000]);
assert.ok(Math.abs(apply.hours - (600 + 1200 + 1800 + 900 + 2400 + 3000) / 3600) < 1e-9);
assert.equal(apply.runner, true, '>= 5 runs and >= 30% over 10 minutes');
assert.equal(api.summarizeRuns('brand-new', byTrig.get('brand-new')).isNew, true, 'first seen in the last 14 days');
assert.equal(apply.isNew, false, 'older than 14 days');
const weekly = api.runWeekly(byTrig.get('spectra-apply'), api.weeksBetween(day(0), day(18)), 3);
same(weekly.map(w => [w.n, w.ok]), [[3, true], [2, false], [1, false]], 'weeks with too few runs are not trusted');

// ---- rendering ----
reset();
api.state.view = 'runs'; api.state.runsTop = 4; api.state.runsMin = 5; api.state.runsPlain = false; api.state.runsGroup = 'trigger';
api.state.runsSort = {key: 'hours', dir: -1}; api.state.runsPinned = null;
open(day(0), day(18));
api.renderRuns();
const trend = created.filter(c => c.id.startsWith('ch-runs-trend-'));
same(trend.map(c => c.id), ['ch-runs-trend-0', 'ch-runs-trend-1'], 'min runs 5 leaves spectra-apply and brand-new; plain prompts are left out');
const t0 = trend[0].cfg;
assert.equal(t0.type, 'scatter');
assert.equal(t0.options.scales.y.type, 'logarithmic', 'durations span orders of magnitude');
assert.equal(t0.data.datasets[0].data.length + t0.data.datasets[1].data.length, 6, 'one dot per run');
assert.equal(t0.data.datasets[1].data.length, 2, 'hollow dots = runs you interrupted');
assert.ok(t0.data.datasets[2].data.length === 1 && t0.data.datasets[3].data.length === 1, 'weekly lines only where a week has >= 3 runs');
assert.ok(created.some(c => c.id === 'ch-runs-hours' && c.cfg.options.scales.y.stacked), 'stacked hours chart');
const par = created.find(c => c.id === 'ch-runs-par').cfg;
assert.equal(par.data.datasets[1].data[0], 3, 'parallelism peak comes from data.json');
const rowsOf = () => tbody.children.map(tr => tr.children[0].children[0].textContent);   // the name, without badges
const badgesOf = name => tbody.children.find(tr => tr.children[0].children[0].textContent === name).children[0].children.slice(1).map(b => b.textContent);
same(rowsOf(), ['spectra-apply', 'brand-new', '(plain prompt)'], 'table: rows with >= 5 runs, most agent hours first');
same(badgesOf('spectra-apply'), ['runner']);
same(badgesOf('brand-new'), ['runner', 'new']);   // 5 runs, all over 10 min, first seen within 14 days
same(badgesOf('(plain prompt)'), []);

// pinning, plain prompts, sorting, grouping
reset(); api.state.runsPinned = 'brand-new'; api.renderRuns();
assert.equal(created.find(c => c.id === 'ch-runs-trend-0').cfg.data.datasets[0].data.length + created.find(c => c.id === 'ch-runs-trend-0').cfg.data.datasets[1].data.length, 5, 'pinned trigger is charted first');
reset(); api.state.runsPinned = null; api.state.runsPlain = true; api.state.runsMin = 6; api.renderRuns();
assert.ok(created.filter(c => c.id.startsWith('ch-runs-trend-')).length === 2, 'plain prompts are charted on request (and min runs 6 leaves plain + spectra-apply)');
reset(); api.state.runsMin = 1; api.renderRuns();
const head = registry.get('t-runs-head').children.find(th => th.dataset.sort === 'runs');
head.click();
const counts = tbody.children.map(tr => Number(tr.children[1].textContent));
same(counts, [...counts].sort((a, b) => b - a), 'clicking a header sorts by it, descending first');
assert.ok(counts.length === 4 && counts[0] > counts[3], 'all four triggers listed at min runs 1');
registry.get('t-runs-head').children.find(th => th.dataset.sort === 'runs').click();
const asc = tbody.children.map(tr => Number(tr.children[1].textContent));
same(asc, [...asc].sort((a, b) => a - b), 'a second click reverses it');
reset(); api.state.runsGroup = 'skill'; api.renderRuns();
assert.ok(rowsOf().includes('spectra-verify'), 'skill grouping lists skills used inside runs');
assert.ok(tbody.children[0].children[9].children[0].tag === 'svg', 'a sparkline per row');
reset(); open('2030-01-01', '2030-01-07'); api.renderRuns();
assert.match(registry.get('runs-trend').textContent, /No trigger/, 'an empty window explains itself instead of throwing');

// ---- parallelism: subagents counted or not ----
const parOf = () => created.filter(c => c.id === 'ch-runs-par').pop().cfg;
const withSubs = [
  {week: day(0), avg_parallel: 1.2, peak_parallel: 3, avg_parallel_all: 2.5, peak_parallel_all: 7, subagent_hours: 4, peak_subagents: 5},
  {week: day(7), avg_parallel: 1.5, peak_parallel: 2, avg_parallel_all: 1.5, peak_parallel_all: 2, subagent_hours: 0, peak_subagents: 0}];
const legacy = api.DATA.runs_weekly_total;
api.DATA.runs_weekly_total = withSubs;
open(day(0), day(18)); api.state.runsMin = 5; api.state.runsPlain = false; api.state.runsGroup = 'trigger';
reset(); api.state.runsSubagents = true; api.renderRuns();
let par2 = parOf();
same(par2.data.datasets.map(d => d.label), ['average, subagents counted', 'peak at once, subagents counted', 'for comparison: average, main sessions only']);
same([par2.data.datasets[0].data[0], par2.data.datasets[1].data[0], par2.data.datasets[2].data[0]], [2.5, 7, 1.2], 'subagents counted, with the main-only average alongside');
assert.match(registry.get('runs-par-note').textContent, /not counted twice/);
reset(); api.state.runsSubagents = false; api.renderRuns();
par2 = parOf();
same(par2.data.datasets.map(d => d.data[0]), [1.2, 3], 'toggle off: main sessions only, no comparison line');
assert.match(registry.get('runs-par-note').textContent, /main sessions only/);
// data.json from before subagents were counted: fall back to main sessions, and the toggle is disabled
api.DATA.runs_weekly_total = legacy; api.state.runsSubagents = true;
reset(); api.renderRuns();
same(parOf().data.datasets.map(d => d.data[0]), [1.2, 3], 'old data.json: no *_all fields, so main sessions');
registry.clear(); api.initRuns();
assert.equal(registry.get('runs-subagents').disabled, true, 'toggle is disabled when the data has no subagent parallelism');
api.DATA.runs_weekly_total = withSubs; registry.clear(); api.initRuns();
assert.equal(registry.get('runs-subagents').disabled, false);

// ---- what landed ----
const base = {hours_all: 1, avg_parallel: 1, peak_parallel: 1};
const landedWeeks = [
  {...base, week: day(0),  hours_tracked: 40,  merged_prs: 3, other_merges: 5, landed: 8,  commits: 20},
  {...base, week: day(7),  hours_tracked: 5,   merged_prs: 1, other_merges: 1, landed: 2,  commits: 5},   // under 10 tracked hours: no rate
  {...base, week: day(14), hours_tracked: 50,  merged_prs: 6, other_merges: 4, landed: 10, commits: 30},
];
api.DATA.runs_weekly_total = landedWeeks;
api.DATA.runs_git = {enabled: true, authors: ['me@x.io'], repos: 3, projects_total: 10, projects_tracked: 4};
open(day(0), day(18)); api.state.runsMin = 5; api.state.runsGroup = 'trigger'; api.state.runsPinned = null;
const out = id => created.filter(c => c.id === id).pop().cfg;
const near = (a, b, msg) => assert.ok(Math.abs(a - b) < 1e-9, msg + ': ' + a + ' vs ' + b);
const cardVals = () => registry.get('runs-outcome-cards').children.map(c => c.children[1].textContent);

reset(); api.state.runsOutcome = 'landed'; api.renderRuns();
assert.equal(registry.get('runs-outcomes').hidden, false);
const cnt = out('ch-runs-out-count');
same(cnt.data.datasets.map(d => d.label), ['merged PRs', 'other merges'], 'merges are the default, split into PRs and other merges');
same(cnt.data.datasets.map(d => d.data), [[3, 1, 6], [5, 1, 4]]);
assert.equal(cnt.options.scales.y.stacked, true);
const rate = out('ch-runs-out-rate');
near(rate.data.datasets[0].data[0], 8 / 40, 'week 1: 8 merges / 40 hours');
assert.equal(rate.data.datasets[0].data[1], null, 'a week with under 10 tracked hours has no rate');
near(rate.data.datasets[0].data[2], 10 / 50, 'week 3: 10 / 50');
near(rate.data.datasets[1].data[1], 10 / 45, 'trailing rate pools the hours, not the weekly rates');
near(rate.data.datasets[1].data[2], 20 / 95);
same(cardVals().slice(0, 3), ['20', '55', '95.0'], 'merges landed, commits, tracked hours');
assert.equal(cardVals()[3], '0.21', 'merges per tracked agent hour over the window (20 / 95)');
same([cardVals()[4]], ['4 / 10'], 'coverage: projects with a repo here');
assert.match(registry.get('runs-outcome-note').textContent, /me@x\.io/, 'says whose work is counted');

reset(); api.state.runsOutcome = 'prs'; api.renderRuns();
same(out('ch-runs-out-count').data.datasets.map(d => d.data), [[3, 1, 6]], 'PRs only');
near(out('ch-runs-out-rate').data.datasets[0].data[2], 6 / 50);
reset(); api.state.runsOutcome = 'commits'; api.renderRuns();
same(out('ch-runs-out-count').data.datasets.map(d => d.data), [[20, 5, 30]], 'commits are available but not the default');
assert.equal(cardVals()[3], '0.58', 'commits per tracked agent hour (55 / 95)');
api.state.runsOutcome = 'landed';

// a window that starts later only counts its own weeks, and its trailing rate never reaches before the window
open(day(14), day(18)); reset(); api.renderRuns();
same(out('ch-runs-out-count').data.datasets.map(d => d.data), [[6], [4]]);
near(out('ch-runs-out-rate').data.datasets[1].data[0], 10 / 50, 'trailing rate uses only the weeks on screen');
open(day(0), day(18));

// --no-git, and a data.json from before git was read
api.DATA.runs_git = {enabled: false, authors: [], repos: 0, projects_total: 0, projects_tracked: 0};
reset(); api.renderRuns();
assert.equal(registry.get('runs-outcome-body').hidden, true);
assert.match(registry.get('runs-outcome-note').textContent, /--no-git/);
delete api.DATA.runs_git;
reset(); api.renderRuns();
assert.equal(registry.get('runs-outcomes').hidden, true, 'older data.json: no section at all');
api.DATA.runs_weekly_total = withSubs;

// ---- names come from transcripts: they are shown as text, never parsed as markup ----
const EVIL = '</script><img src=x onerror=alert(1)>';
api.DATA.runs_weekly_total = withSubs;
api.DATA.runs.push({...mk(3, EVIL, 400), skills: [EVIL + '2']});
open(day(0), day(18)); api.state.runsMin = 1; api.state.runsGroup = 'trigger'; api.state.runsPinned = EVIL;
reset(); api.renderRuns();
assert.ok(rowsOf().includes(EVIL), 'table shows the name verbatim as text');
assert.ok(registry.get('runs-trend').textContent.includes(EVIL), 'chart title shows it verbatim as text');
assert.ok(!html.slice(html.indexOf('// ---------- Runs ----------'), html.indexOf('function exportFiltered')).match(/innerHTML|insertAdjacentHTML|document\.write/), 'the Runs code never builds markup from data');
api.state.runsGroup = 'skill'; reset(); api.renderRuns();
assert.ok(rowsOf().includes(EVIL + '2'), 'skill names too');

// ---- when agents work together (runs_heat) ----
const heat = [];
for (let wd = 0; wd < 7; wd++) for (let h = 0; h < 24; h++) heat.push({weekday: wd, hour: h, hours: 0, avg_parallel: 0, peak_parallel: 0});
const heatAt = (wd, h) => heat[wd * 24 + h];
Object.assign(heatAt(0, 10), {hours: 10, avg_parallel: 2, peak_parallel: 3});   // the busiest cell
Object.assign(heatAt(0, 11), {hours: 1, avg_parallel: 5, peak_parallel: 5});    // under 2 h: blank, whatever its average
Object.assign(heatAt(0, 12), {hours: 5, avg_parallel: 1, peak_parallel: 1});    // half the top average
api.DATA.runs_heat = heat;
reset(); api.renderRuns();
const heatBox = registry.get('runs-heat');
assert.equal(heatBox.children.length, 25 + 7 * 25, 'a header row, then seven weekday rows of a label and 24 cells');
const heatCell = (wd, h) => heatBox.children[25 + wd * 25 + 1 + h];
assert.equal(heatCell(0, 10).style.background, 'rgba(124,158,255,1.00)', 'the top cell is full strength');
assert.equal(heatCell(0, 10).textContent, '2.0', 'the average is printed in the cell, not only in a tooltip');
assert.equal(heatCell(0, 12).textContent, '1.0', 'every shown cell prints its average');
assert.equal(heatCell(0, 11).textContent, '', 'a blank cell prints nothing');
assert.equal(heatCell(0, 11).style.background, 'transparent', 'a cell with under 2 hours is blank');
assert.equal(heatCell(0, 12).style.background, 'rgba(124,158,255,0.54)', 'half the top average is about half the strength');
assert.match(heatCell(0, 10).attrs.title, /Mon 10:00 · avg 2\.00 agents · peak 3 · 10\.0 h/, 'the tooltip gives the numbers');
assert.match(registry.get('runs-heat-note').textContent, /--tz/);
delete api.DATA.runs_heat;
reset(); api.renderRuns();
assert.equal(registry.get('runs-heat').children.length, 0, 'an older data.json shows no heatmap');
assert.match(registry.get('runs-heat-note').textContent, /older cc-stats/);

console.log('Runs view: aggregation, filters, charts and table behave.');
