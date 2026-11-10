// Smoke test: app-server + mock Responses server -> agentMessage deltas stream over WebSocket.
// Usage: node scripts/smoke-appserver-stream.mjs <path-to-inocode.exe> <INOCODE_HOME dir (must exist)>
import { spawn } from "node:child_process";

const [exe, home] = process.argv.slice(2);
const MOCK = 18800, APP = 18801;
const procs = [];
const run = (cmd, args, env = {}) => {
  const p = spawn(cmd, args, { env: { ...process.env, ...env }, stdio: "ignore" });
  procs.push(p);
  return p;
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const done = (code) => { procs.forEach((p) => p.kill()); process.exit(code); };
setTimeout(() => { console.log("TIMEOUT"); done(2); }, 60000);

run("node", [new URL("./mock-responses-server.mjs", import.meta.url).pathname.replace(/^\/(\w:)/, "$1"), String(MOCK)]);
run(exe, ["app-server", "--listen", "ws://127.0.0.1:" + APP,
  "-c", "model_provider=mock", "-c", "model_providers.mock.name=mock",
  "-c", "model_providers.mock.base_url=http://127.0.0.1:" + MOCK + "/v1",
  "-c", "model_providers.mock.wire_api=responses"], { INOCODE_HOME: home });
await sleep(4000);

const ws = new WebSocket("ws://127.0.0.1:" + APP);
let id = 0; const pending = new Map(); let deltas = 0, first = "", completed = false;
const send = (method, params) => new Promise((res) => { const i = ++id; pending.set(i, res); ws.send(JSON.stringify({ id: i, method, params })); });
ws.onmessage = (e) => {
  const m = JSON.parse(e.data);
  if (m.id !== undefined && pending.has(m.id)) { pending.get(m.id)(m); pending.delete(m.id); return; }
  if (m.method === "item/agentMessage/delta") { deltas++; if (!first) first = m.params.delta; }
  if (m.method === "turn/completed") completed = true;
  if (m.method === "error") console.log("ERROR", JSON.stringify(m.params));
};
await new Promise((r) => (ws.onopen = r));
await send("initialize", { clientInfo: { name: "inocode_smoke", title: "smoke", version: "0" } });
ws.send(JSON.stringify({ method: "initialized" }));
const t = await send("thread/start", {});
const threadId = t.result.thread.id;
await send("turn/start", { threadId, input: [{ type: "text", text: "hello" }] });
for (let i = 0; i < 100 && !completed; i++) await sleep(200);
console.log(JSON.stringify({ deltas, first, completed }));
done(deltas > 1 && completed ? 0 : 1);
