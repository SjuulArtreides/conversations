/* global process, Buffer, setTimeout, clearTimeout, fetch, WebSocket, console */
// Opt-in shared-SPA journey. Uses only Node builtins and an isolated Chromium profile.
import { spawn } from 'node:child_process';
import { readFile, writeFile, mkdir } from 'node:fs/promises';
import { join } from 'node:path';
const input = [];
for await (const chunk of process.stdin) input.push(chunk);
const config = JSON.parse(Buffer.concat(input).toString());
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const report = { assertions: [], screenshots: [] };
const check = (condition, name) => { if (!condition) throw new Error(name); report.assertions.push(name); };
await mkdir(config.output, { recursive: true });
const child = spawn(process.env.PRIVATE_TRANSPORT_CHROME, ['--headless=new', '--remote-debugging-port=0', '--remote-debugging-address=127.0.0.1', '--no-first-run', '--no-default-browser-check', '--disable-background-networking', '--disable-sync', '--disable-extensions', '--password-store=basic', '--use-mock-keychain', `--user-data-dir=${config.profile}`, 'about:blank'], { stdio: 'ignore' });
let ws;
let failureScreenshot;
try {
  let port;
  for (let attempt = 0; attempt < 200; attempt++) {
    try { port = (await readFile(join(config.profile, 'DevToolsActivePort'), 'utf8')).split('\n')[0]; break; } catch { await sleep(50); }
  }
  if (!port) throw new Error('Isolated Chromium did not start');
  const target = await (await fetch(`http://127.0.0.1:${port}/json/new?about:blank`, { method: 'PUT' })).json();
  ws = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((resolve, reject) => { ws.onopen = resolve; ws.onerror = reject; });
  let seq = 0;
  const pending = new Map();
  ws.onmessage = (event) => { const message = JSON.parse(event.data); if (message.id) { const entry = pending.get(message.id); if (entry) { clearTimeout(entry.timer); pending.delete(message.id); message.error ? entry.reject(new Error(`${entry.method}: ${message.error.message}`)) : entry.resolve(message.result); } } };
  const cdp = (method, params = {}) => new Promise((resolve, reject) => { const id = ++seq; const timer = setTimeout(() => reject(new Error(`CDP timeout: ${method}`)), 15000); pending.set(id, { resolve, reject, timer, method }); ws.send(JSON.stringify({ id, method, params })); });
  const evaluate = async (expression) => { const result = await cdp('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true }); if (result.exceptionDetails) throw new Error('Page evaluation failed'); return result.result.value; };
  const wait = async (expression, label) => { for (let attempt = 0; attempt < 150; attempt++) { if (await evaluate(`Boolean(${expression})`)) return; await sleep(100); } throw new Error(`Missing UI: ${label}`); };
  const click = async (expression) => { await evaluate(`(${expression})?.scrollIntoView({block:'center'})`); await sleep(150); const point = await evaluate(`(()=>{const el=${expression}; if(!el)return null; const r=el.getBoundingClientRect();return {x:r.x+r.width/2,y:r.y+r.height/2};})()`); if (!point) throw new Error('Missing click target'); await cdp('Input.dispatchMouseEvent', { type: 'mousePressed', button: 'left', clickCount: 1, ...point }); await cdp('Input.dispatchMouseEvent', { type: 'mouseReleased', button: 'left', clickCount: 1, ...point }); };
  // Fixed test expressions; labels and page data never become JavaScript source.
  const buttons = {
    actions: "[...document.querySelectorAll('button')].find(e=>e.getAttribute('aria-label')==='Actions' || e.textContent.trim()==='Actions')",
    display: "[...document.querySelectorAll('button')].find(e=>e.getAttribute('aria-label')==='Display' || e.textContent.trim()==='Display')",
    newSession: "[...document.querySelectorAll('button')].find(e=>e.getAttribute('aria-label')==='New session' || e.textContent.trim()==='New session')",
  };
  const screenshot = async (name) => { const result = await cdp('Page.captureScreenshot', { format: 'png', captureBeyondViewport: false }); await writeFile(join(config.output, `${name}.png`), Buffer.from(result.data, 'base64')); report.screenshots.push(`${name}.png`); };
  failureScreenshot = () => screenshot('failure');
  await cdp('Emulation.setDeviceMetricsOverride', { width: 1440, height: 1000, deviceScaleFactor: 1, mobile: false });
  await cdp('Page.navigate', { url: `${config.base}/#token=${encodeURIComponent(config.token)}` });
  await wait(`document.querySelectorAll('h3').length>=2 && document.querySelector('input[type="search"]')`, 'shared session cards');
  check(await evaluate('location.hash === ""'), 'bootstrap fragment cleared');
  check(await evaluate('document.querySelectorAll(\'input[type="search"]\').length===1'), 'one shared collection toolbar');
  await wait(`document.querySelector('h3')?.textContent==='Atlas follow-up'`, 'newer partial initially sorts first');
  check(true, 'baseline activity sorting puts newer partial first');
  await evaluate(`(()=>{const input=document.querySelector('input[type="search"]');Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,'value').set.call(input,'Atlas');input.dispatchEvent(new Event('input',{bubbles:true}));})()`);
  await wait(`document.querySelector('h3')?.textContent==='Atlas'`, 'exact title first');
  check(await evaluate(`[...document.querySelectorAll('h3')].map(e=>e.textContent).join('|')==='Atlas|Atlas follow-up'`), 'exact title precedes newer partial match');
  check(await evaluate(`!document.body.innerText.includes('11111111-1111-4111-8111-111111111111') && !document.body.innerText.includes('22222222-2222-4222-8222-222222222222')`), 'cards show titles without duplicate UUID text');
  await screenshot('01-shared-cards');
  await click(`document.querySelector('h3').closest('a')`);
  await wait(`document.querySelector('.xterm-screen') && ${buttons.actions}`, 'live session terminal');
  check(await evaluate(`!!(${buttons.display}) && !!(${buttons.newSession})`), 'shared Actions Display and New controls');
  await click(buttons.actions);
  await wait(`document.querySelector('[role="menu"]')`, 'Actions menu');
  check(await evaluate(`document.querySelector('[role="menu"]').textContent.includes('Rename')`), 'shared actions menu opens');
  await cdp('Input.dispatchKeyEvent', { type: 'keyDown', key: 'Escape', code: 'Escape', windowsVirtualKeyCode: 27 });
  await click(buttons.display);
  await wait(`document.querySelector('[role="menu"]')`, 'Display menu');
  check(await evaluate(`document.querySelector('[role="menu"]').textContent.includes('Fullscreen')`), 'shared display menu opens');
  await cdp('Input.dispatchKeyEvent', { type: 'keyDown', key: 'Escape', code: 'Escape', windowsVirtualKeyCode: 27 });
  await click(`[...document.querySelectorAll('button')].find(e=>e.getAttribute('aria-label')?.startsWith('Start with'))`);
  await wait(`document.querySelector('[role="menu"]')`, 'New session target menu');
  check(await evaluate(`document.querySelector('[role="menu"]').textContent.includes('Codex')`), 'shared New target menu opens without starting an agent');
  await cdp('Input.dispatchKeyEvent', { type: 'keyDown', key: 'Escape', code: 'Escape', windowsVirtualKeyCode: 27 });
  await click(`document.querySelector('.xterm-screen')`);
  await cdp('Input.insertText', { text: 'UI_PROOF_INPUT' });
  await screenshot('02-shared-live-terminal');
  // Return through actual shared navigation and open the other live card.
  await click(`[...document.querySelectorAll('a')].find(e=>e.textContent.trim()==='Sessions')`);
  await wait(`document.querySelector('h3')`, 'session overview');
  await click(`[...document.querySelectorAll('h3')].find(e=>e.textContent==='Atlas follow-up').closest('a')`);
  await wait(`document.querySelectorAll('[role="tab"]').length>=2`, 'two live session tabs');
  check(true, 'shared live tabs retain both sessions');
  await screenshot('03-shared-live-tabs');
  await click(`[...document.querySelectorAll('a')].find(e=>e.textContent.trim()==='Settings')`);
  await wait(`document.querySelector('#local-trust-heading')`, 'shared Settings trust panel');
  await wait(`document.body.textContent.includes('QA browser key')`, 'enrolled fixture key');
  check(await evaluate(`document.body.textContent.includes('QA browser key') && document.body.textContent.includes('Approved keys')`), 'shared Settings shows enrolled fixture key');
  await screenshot('04-shared-settings');
  report.passed = true;
} catch (error) {
  try { await failureScreenshot?.(); } catch { /* Preserve the original assertion failure if screenshot capture also fails. */ }
  report.passed = false;
  report.error = error.message;
  process.exitCode = 1;
} finally {
  if (ws) ws.close();
  child.kill('SIGTERM');
  await writeFile(join(config.output, 'assertions.json'), JSON.stringify(report, null, 2));
  console.log(JSON.stringify({ passed: report.passed, assertions: report.assertions.length, screenshots: report.screenshots.length, error: report.error }));
}
