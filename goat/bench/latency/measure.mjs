// Run only on the TCC-granted WS13 host, with Calculator approved in the manifest.
// Usage: node ws13-measure.mjs /path/to/helper-executable /absolute/manifest.yaml N direct|batch /absolute/result-prefix
import { spawn } from 'node:child_process';
import { createInterface } from 'node:readline';
import { createWriteStream, writeFileSync } from 'node:fs';
import { performance } from 'node:perf_hooks';

const [helper, manifest, nRaw, mode, prefix] = process.argv.slice(2);
const n = Number(nRaw);
if (!helper || !manifest?.startsWith('/') || !Number.isInteger(n) || n < 50 || !['direct', 'batch'].includes(mode) || !prefix?.startsWith('/')) {
  throw new Error('usage: node ws13-measure.mjs HELPER ABS_MANIFEST N>=50 direct|batch ABS_PREFIX');
}
for (const key of ['CUA_DRIVER_WINDOW_CHANGE_TIMEOUT_MS', 'CUA_DRIVER_WINDOW_CHANGE_POLL_MS']) {
  if (process.env[key] !== undefined) throw new Error(`Unset ${key}: measure defaults`);
}
const stderr = createWriteStream(`${prefix}.stderr`);
const child = spawn(helper, ['--mcp', '--capability-manifest', manifest], { stdio: ['pipe', 'pipe', 'pipe'], env: process.env });
child.stderr.pipe(stderr);
let next = 0;
const pending = new Map();
let closed = false;
child.on('error', error => { for (const p of pending.values()) p.reject(error); pending.clear(); });
child.on('exit', (code, signal) => {
  closed = true;
  for (const p of pending.values()) p.reject(new Error(`helper exited ${code}/${signal}`));
  pending.clear();
});
createInterface({ input: child.stdout }).on('line', line => {
  try {
    const message = JSON.parse(line);
    const p = pending.get(message.id);
    if (p) { pending.delete(message.id); message.error ? p.reject(new Error(JSON.stringify(message.error))) : p.resolve(message.result); }
    else if (message.method && message.id !== undefined) {
      child.stdin.write(`${JSON.stringify({jsonrpc:'2.0',id:message.id,error:{code:-32601,message:'Unsupported harness callback'}})}\n`);
    }
  } catch (error) { for (const p of pending.values()) p.reject(error); pending.clear(); }
});
function rpc(method, params) {
  if (closed) return Promise.reject(new Error('helper already exited'));
  const id = ++next;
  return new Promise((resolve, reject) => {
    const timeout = setTimeout(() => { pending.delete(id); reject(new Error(`MCP timeout: ${method}`)); }, 30000);
    pending.set(id, {resolve:value => {clearTimeout(timeout);resolve(value);}, reject:error => {clearTimeout(timeout);reject(error);}});
    child.stdin.write(`${JSON.stringify({jsonrpc:'2.0',id,method,params})}\n`);
  });
}
async function tool(name, args) {
  const result = await rpc('tools/call', {name, arguments:args});
  if (result.isError) throw new Error(`Tool refused: ${name}: ${JSON.stringify(result)}`);
  return result;
}
function text(result) { return (result.content ?? []).filter(c => c.type === 'text').map(c => c.text).join('\n'); }
const rows = [];
try {
  await rpc('initialize', {protocolVersion:'2025-06-18',capabilities:{},clientInfo:{name:'ws13-latency',version:'1'}});
  child.stdin.write(`${JSON.stringify({jsonrpc:'2.0',method:'notifications/initialized'})}\n`);
  const listing = await tool('list_windows', {app:'com.apple.calculator',on_screen_only:true});
  const windows = listing.structuredContent?.windows;
  if (!Array.isArray(windows) || windows.length !== 1) throw new Error('Keep exactly one visible Calculator window');
  const {pid, window_id} = windows[0];
  const state = await tool('get_window_state', {pid,window_id,include_screenshot:false,tree_format:'elements'});
  const elements = state.structuredContent?.elements;
  if (!Array.isArray(elements)) throw new Error('Missing structured AX elements');
  const clear = elements.filter(e => e.role === 'AXButton' && /^(All Clear|AC)$/i.test(e.label ?? ''));
  if (clear.length !== 1 || !clear[0].element_token) throw new Error('Set Calculator to English Basic mode, press Escape, and verify one All Clear AXButton');
  const args = {pid,window_id,element_token:clear[0].element_token,action:'press',delivery_mode:'background'};
  // run_steps reports only each step's first message line, not the raw route/effect, so prove the
  // AX route once with a direct click before timing batches, then check every batch strictly.
  const preflight = (await tool('click',args)).structuredContent;
  if (!(preflight?.path === 'ax' || preflight?.route === 'accessibility') || preflight?.effect !== 'unverifiable') {
    throw new Error('Preflight click did not take the AX route: inspect the response before measuring');
  }
  const checkBatch = (structured) => {
    const steps = structured?.steps;
    if (structured?.ok !== true || !Array.isArray(steps) || steps.length !== 3) throw new Error('Batch did not return three steps');
    for (const step of steps) {
      const raw = JSON.stringify(step).toLowerCase();
      if (step.ok !== true || step.recovered === true || 'recovery' in step || /no-?op|skipped|recover/.test(raw)) {
        throw new Error('Batch step failed, recovered or did nothing: ' + JSON.stringify(step));
      }
    }
  };
  const act = async () => {
    const started = performance.now();
    const result = mode === 'direct' ? await tool('click',args) : await tool('run_steps',{steps:Array.from({length:3},()=>({tool:'click',args}))});
    const ms = performance.now() - started;
    if (mode === 'direct') {
      const structured = result.structuredContent;
      if (!(structured?.path === 'ax' || structured?.route === 'accessibility') || structured?.effect !== 'unverifiable') {
        throw new Error('Unexpected click route/effect: inspect response before accepting measurements');
      }
    } else {
      checkBatch(result.structuredContent);
    }
    return ms;
  };
  const cold = process.env.WS13_COLD === '1';
  const warmup = cold ? 0 : 5;
  for (let i=0;i<warmup;i++) await act();
  for (let i=0;i<n;i++) {
    if (cold) await new Promise(resolve => setTimeout(resolve, 5000));
    rows.push({iteration:i+1,ms:await act()});
  }
  const sorted = rows.map(r=>r.ms).sort((a,b)=>a-b);
  const median = sorted.length % 2 ? sorted[(sorted.length-1)/2] : (sorted[sorted.length/2-1]+sorted[sorted.length/2])/2;
  const summary = {mode,n,median_ms:median,p95_ms:sorted[Math.ceil(.95*n)-1],trace:process.env.CUA_DRIVER_GOAT_LATENCY_TRACE==='1',warmup,cold_idle_ms:cold?5000:0};
  writeFileSync(`${prefix}.json`,JSON.stringify({summary,rows},null,2)+'\n');
  console.log(JSON.stringify(summary));
} finally {
  child.stdin.end();
  if (!closed) {
    const exited = new Promise(resolve=>child.once('exit',resolve));
    const deadline = setTimeout(()=>child.kill('SIGTERM'),15000);
    await exited; clearTimeout(deadline);
  }
}
