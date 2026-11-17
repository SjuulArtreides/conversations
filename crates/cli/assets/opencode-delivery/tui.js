import { spawn as spawnProcess } from "node:child_process"
import { StringDecoder } from "node:string_decoder"

// This runs in the terminal client, not the shared server: only this client owns
// CONVERSATIONS_WRAPPED and the selected native session identity.
export function startDelivery(ctx, dependencies = {}) {
  const env = dependencies.env ?? process.env
  if (!env.CONVERSATIONS_WRAPPED) return () => {}
  if (typeof ctx.client?.session?.prompt !== "function" || typeof ctx.ui?.router?.current !== "function") {
    throw new Error("Conversations delivery requires OpenCode 2.0.27 or newer. Upgrade OpenCode and reopen this terminal.")
  }
  const spawn = dependencies.spawn ?? spawnProcess
  const every = dependencies.setInterval ?? setInterval
  const cancel = dependencies.clearInterval ?? clearInterval
  const surfaced = new Set()
  const report = error => {
    const message = String(error?.message ?? error).slice(0, 1000)
    if (/project messaging is disabled/i.test(message) || surfaced.has(message)) return
    surfaced.add(message)
    if (dependencies.report) dependencies.report(new Error(message))
    else if (typeof ctx.ui.toast?.show === "function") ctx.ui.toast.show({ title: "Conversations delivery", message, variant: "warning" })
    else console.error("Conversations delivery:", message)
  }
  let stopped = false
  let active
  let pinnedNative
  let warnedRoute
  function close() {
    if (!active) return
    const previous = active
    active = undefined
    previous.child.kill()
  }
  function current() {
    const route = ctx.ui.router.current()
    return route?.type === "session" && typeof route.sessionID === "string" ? route.sessionID : undefined
  }
  function sync() {
    if (stopped) return
    const native = current()
    if (active?.native === native) return
    close()
    if (!native) return
    // A Conversations terminal must not move an unconfirmed delivery to another
    // native thread. Switching the picker suspends delivery until its original
    // session returns; a new wrapped terminal establishes a new binding.
    pinnedNative ??= native
    if (native !== pinnedNative) {
      if (warnedRoute !== native) {
        warnedRoute = native
        const error = new Error("Delivery paused after switching OpenCode sessions. Return to the original session or reopen a wrapped terminal to receive messages in the new session.")
        if (typeof ctx.ui.toast?.show === "function") ctx.ui.toast.show({ title: "Conversations delivery paused", message: error.message, variant: "info" })
        else report(error)
      }
      return
    }
    warnedRoute = undefined
    let child
    try {
      child = spawn(env.CONVERSATIONS_CLI || "conversations", ["mcp", "bridge", "--native-session", native], {
        env, stdio: ["pipe", "pipe", "pipe"],
      })
    } catch (error) { report(error); return }
    const binding = { native, child, buffer: "", decoder: new StringDecoder("utf8"), diagnostic: "", chain: Promise.resolve() }
    active = binding
    const valid = () => !stopped && active === binding && current() === native
    child.on("error", error => { if (active === binding) { active = undefined; report(error) } })
    child.on("close", code => {
      if (active !== binding) return
      active = undefined
      if (binding.diagnostic.trim()) report(new Error(binding.diagnostic.trim()))
      else if (code) report(new Error(`Inbox bridge exited (${code}); delivery will retry. Check the Conversations CLI installation and session messaging permissions.`))
    })
    child.stdin.on?.("error", error => { if (valid()) report(error) })
    // The bridge never emits credentials. Drain diagnostic output so a disconnected
    // sidecar cannot block because its stderr pipe filled.
    child.stderr.on("data", chunk => { binding.diagnostic = (binding.diagnostic + chunk.toString("utf8")).slice(-1000) })
    child.stdout.on("data", chunk => {
      if (!valid()) return
      binding.buffer += binding.decoder.write(chunk)
      if (binding.buffer.length > 1024 * 1024) { close(); report(new Error("Inbox bridge record exceeded its size limit")); return }
      let newline
      while ((newline = binding.buffer.indexOf("\n")) >= 0) {
        const line = binding.buffer.slice(0, newline)
        binding.buffer = binding.buffer.slice(newline + 1)
        binding.chain = binding.chain.then(async () => {
          if (!valid()) return
          const envelope = JSON.parse(line)
          if (envelope.type !== "message" || typeof envelope.message?.id !== "string" || typeof envelope.content !== "string") throw new Error("Invalid inbox bridge record")
          if (envelope.message.recipient_session_id !== env.CONVERSATIONS_WRAPPED) throw new Error("Inbox bridge recipient mismatch")
          const id = `msg_conversations_${envelope.message.id}`
          let accepted
          try {
            accepted = await ctx.client.session.prompt({
              sessionID: native, id, text: envelope.content, delivery: "queue", resume: true,
              metadata: { conversations_message_id: envelope.message.id },
            })
          } catch (error) {
            // A disconnected request may already have committed. Reopen the bridge
            // so it re-emits this ID; OpenCode reconciles the same durable ID.
            if (active === binding) close()
            report(error)
            return
          }
          if (accepted?.id !== id || accepted.sessionID !== native || accepted.type !== "user") throw new Error("OpenCode did not confirm the expected durable inbox record")
          if (valid() && child.stdin.writable) child.stdin.write(JSON.stringify({ type: "accepted", message_id: envelope.message.id }) + "\n")
        }).catch(report)
      }
    })
  }
  sync()
  const timer = every(sync, 1000)
  return () => { stopped = true; cancel(timer); close() }
}

export default { id: "arrakis-conversations", setup: ctx => startDelivery(ctx) }
