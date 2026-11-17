import { test } from 'node:test'
import assert from 'node:assert/strict'
import { EventEmitter } from 'node:events'
import { readFile } from 'node:fs/promises'

const source = await readFile(new URL('../crates/cli/assets/opencode-delivery/tui.js', import.meta.url), 'utf8')
const { startDelivery } = await import(`data:text/javascript;base64,${Buffer.from(source).toString('base64')}`)
const flush = () => new Promise(resolve => setImmediate(resolve))
function fixture(prompt = async input => ({ id: input.id, sessionID: input.sessionID, type: 'user' }), initialRoute = 'native-a') {
  let route = initialRoute
  const children = [], writes = [], calls = [], timers = [], reports = []
  const ctx = { ui: { router: { current: () => route ? { type: 'session', sessionID: route } : { type: 'home' } } }, client: { session: { prompt: async input => { calls.push(input); return prompt(input) } } } }
  const spawn = (_bin, args) => {
    const child = new EventEmitter()
    child.stdout = new EventEmitter(); child.stderr = new EventEmitter()
    child.stdin = { writable: true, write: text => writes.push(JSON.parse(text)) }
    child.kill = () => { child.killed = true }
    child.args = args; children.push(child); return child
  }
  const cleanup = startDelivery(ctx, { env: { CONVERSATIONS_WRAPPED: 'source', CONVERSATIONS_CLI: '/cli' }, spawn, setInterval: fn => { timers.push(fn); return fn }, clearInterval() {}, report(error) { reports.push(error.message) } })
  const emit = (id = 'one', child = children.at(-1)) => child.stdout.emit('data', Buffer.from(JSON.stringify({ type: 'message', message: { id, recipient_session_id: 'source' }, content: 'untrusted peer' }) + '\n'))
  return { calls, children, writes, reports, emit, cleanup, tick: () => timers[0](), route: value => { route = value } }
}
test('durable admission queues busy work and acknowledges matching record', async () => {
  const f = fixture(); f.emit(); await flush()
  assert.deepEqual(f.calls[0], { sessionID: 'native-a', id: 'msg_conversations_one', text: 'untrusted peer', delivery: 'queue', resume: true, metadata: { conversations_message_id: 'one' } })
  assert.deepEqual(f.writes, [{ type: 'accepted', message_id: 'one' }]); f.cleanup()
})
test('wrong native response never acknowledges', async () => {
  const f = fixture(async input => ({ id: input.id, sessionID: 'other', type: 'user' })); f.emit(); await flush(); assert.equal(f.writes.length, 0); f.tick(); assert.equal(f.children.length,1); assert.equal(f.calls.length,1); f.cleanup()
})
test('lost response retries the same durable native ID after sidecar restart', async () => {
  let attempt = 0
  const f = fixture(async input => { if (!attempt++) throw Error('lost response'); return { id: input.id, sessionID: input.sessionID, type: 'user' } })
  f.emit(); await flush(); assert.equal(f.children[0].killed,true); f.tick(); assert.equal(f.children.length,2); f.emit(); await flush()
  assert.equal(f.calls.length, 2); assert.equal(f.calls[0].id, f.calls[1].id); assert.equal(f.writes.length, 1); f.cleanup()
})
test('route switch stops old bridge and cannot misdeliver buffered messages', async () => {
  const f = fixture(); const old = f.children[0]; f.route('native-b'); f.tick(); f.emit('old', old); await flush()
  assert.equal(old.killed, true); assert.equal(f.calls.length, 0); assert.equal(f.children.length, 1); f.route('native-a'); f.tick(); assert.equal(f.children.length, 2); f.cleanup()
})
test('cleanup prevents acknowledgement of in-flight acceptance', async () => {
  let resolve
  const f = fixture(input => new Promise(done => { resolve = () => done({ id: input.id, sessionID: input.sessionID, type: 'user' }) }))
  f.emit(); await flush(); f.cleanup(); resolve(); await flush(); assert.equal(f.writes.length, 0)
})
test('two TUIs keep independent native identities', async () => {
  const a = fixture(), b = fixture(undefined, 'native-b'); a.emit('a'); b.emit('b'); await flush()
  assert.equal(a.calls[0].sessionID, 'native-a'); assert.equal(b.calls[0].sessionID, 'native-b'); a.cleanup(); b.cleanup()
})

test('UTF-8 split records retain peer text', async () => {
  const f = fixture()
  const record = Buffer.from(JSON.stringify({type:'message',message:{id:'utf',recipient_session_id:'source'},content:'hello 🌵'})+'\n')
  const split = record.indexOf(Buffer.from('🌵')) + 2
  f.children[0].stdout.emit('data',record.subarray(0,split))
  f.children[0].stdout.emit('data',record.subarray(split))
  await flush(); assert.equal(f.calls[0].text,'hello 🌵'); f.cleanup()
})
test('recipient mismatch never reaches native queue', async () => {
  const f = fixture()
  f.children[0].stdout.emit('data',Buffer.from(JSON.stringify({type:'message',message:{id:'wrong',recipient_session_id:'other'},content:'text'})+'\n'))
  await flush(); assert.equal(f.calls.length,0); assert.equal(f.writes.length,0); f.cleanup()
})
test('unwrapped TUI creates no sidecar', () => {
  let spawned=false
  const cleanup=startDelivery({}, {env:{},spawn(){spawned=true}})
  cleanup(); assert.equal(spawned,false)
})

test('late admission after picker switch cannot retarget the message into another native thread', async () => {
  let finish
  const f = fixture(input => new Promise(resolve => { finish = () => resolve({id:input.id,sessionID:input.sessionID,type:'user'}) }))
  f.emit('inflight'); await flush(); f.route('native-b'); f.tick(); finish(); await flush()
  assert.equal(f.writes.length,0); assert.equal(f.children.length,1)
  f.tick(); assert.equal(f.children.length,1); assert.equal(f.calls.length,1)
  f.route('native-a'); f.tick(); f.emit('inflight'); await flush()
  assert.equal(f.calls[1].sessionID,'native-a'); assert.equal(f.calls[1].id,f.calls[0].id)
  finish(); await flush(); assert.equal(f.writes.length,1); f.cleanup()
})

test('bridge failures are visible once per bounded reason while denied inbox access is quiet', () => {
  const f=fixture()
  for(let i=0;i<2;i++) { const child=f.children.at(-1); child.stderr.emit('data',Buffer.from('native session identity differs')); child.emit('close',1); f.tick() }
  assert.equal(f.reports.length,1); assert.match(f.reports[0],/identity/)
  const child=f.children.at(-1); child.stderr.emit('data',Buffer.from('project messaging is disabled for this session')); child.emit('close',1); f.tick()
  assert.equal(f.reports.length,1); f.cleanup()
})

test('bridge diagnostics arriving after exit are drained before close reporting', () => {
  const f=fixture(), child=f.children[0]
  child.emit('exit',1); child.stderr.emit('data',Buffer.from('final actionable diagnostic')); child.emit('close',1)
  assert.deepEqual(f.reports,['final actionable diagnostic']); f.cleanup()
})

test('late transport rejection from an old bridge cannot stop its replacement', async () => {
  let rejectFirst, attempt=0
  const f=fixture(input => {
    if(!attempt++) return new Promise((_resolve,reject) => {rejectFirst=reject})
    return {id:input.id,sessionID:input.sessionID,type:'user'}
  })
  f.emit('retry'); await flush(); f.route('native-b'); f.tick(); f.route('native-a'); f.tick()
  const replacement=f.children[1]
  rejectFirst(new Error('old transport disconnected')); await flush()
  assert.equal(replacement.killed,undefined)
  f.emit('retry',replacement); await flush()
  assert.deepEqual(f.writes,[{type:'accepted',message_id:'retry'}]); f.cleanup()
})
