const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');
const html = fs.readFileSync(path.join(__dirname, '../templates/report.html'), 'utf8');
const script = html.match(/<script>\s*([\s\S]*?)<\/script>/)[1].replace(/\ninit\(\);\s*$/, '');
const tokens = input => ({input, output:0, cache_create_5m:0, cache_create_1h:0, cache_read:0, iterations:1});
const row = (date, cost) => ({date, project:'P', file:'p/session.jsonl', session_id:'session',
  start:date+'T09:00:00+08:00', end:date+'T10:00:00+08:00', active_sec:3600,
  total_sec:3600, messages:1, cost_usd:cost, tokens:tokens(cost*100), models:['claude-opus-5'],
  by_hour:Array.from({length:24}, (_,i)=>i===9?3600:0), by_weekday:[3600,0,0,0,0,0,0],
  by_model:{'claude-opus-5':{messages:1,tokens:tokens(cost*100),cost_usd:cost}},
  by_tool:{Read:1}, by_skill:{test:1}, by_branch:{main:{messages:1,tokens:tokens(cost*100),cost_usd:cost,active_sec:3600}}
});
const first=row('2026-10-01',1), second=row('2026-10-02',2);
const data={summary:{earliest:'2026-10-01',latest:'2026-10-02'},sessions:[{...first,end:second.end,cost_usd:3}],
  session_days:[first,second],projects:[],daily:[]};
const context=vm.createContext({console});
vm.runInContext(script.replace('/*__CC_STATS_DATA__*/null',JSON.stringify(data))+`
  globalThis.api={state, computeFiltered, aggregateProjects, mergedSessions};
`,context);
const {api}=context;
api.state.from='2026-10-02'; api.state.to='2026-10-02'; api.state.projects=new Set(['P']);
let f=api.computeFiltered();
assert.equal(f.sessions,1,'a session that began yesterday is included');
assert.equal(f.cost,2,'only usage on the selected date is charged');
assert.equal(f.tokens.input,200);
assert.equal(f.sessions_list[0].by_model['claude-opus-5'].cost_usd,2);
assert.equal(f.sessions_list[0].by_tool.Read,1);
assert.equal(api.aggregateProjects(api.mergedSessions().sessions,'2026-10-02','2026-10-02')[0].sessions,1);
api.state.from='2026-10-01';
f=api.computeFiltered();
assert.equal(f.cost,3);
assert.equal(f.sessions,1,'daily slices do not inflate session counts');
assert.equal(f.active,7200);
assert.equal(f.sessions_list[0].by_model['claude-opus-5'].cost_usd,3);
assert.equal(f.sessions_list[0].by_tool.Read,2);
assert.equal(f.sessions_list[0].by_hour[9],7200);
assert.equal(f.sessions_list[0].tokens.iterations,2);
// An idle-only date must not erase models collected on an earlier date.
vm.runInContext(`DATA.session_days.push({...DATA.session_days[1], date:'2026-10-03',
  start:'2026-10-03T00:00:00+08:00', end:'2026-10-03T01:00:00+08:00',
  models:[], cost_usd:0, tokens:{input:0}, messages:0});`,context);
api.state.to='2026-10-03';
assert.equal(api.computeFiltered().sessions_list[0].models[0],'claude-opus-5');
api.state.omitRules=['P'];
assert.equal(api.computeFiltered().cost,0,'omit rules still apply to slices');
console.log('Report date filters: cost, tokens, models, tools, activity, session counts and omit rules passed.');
