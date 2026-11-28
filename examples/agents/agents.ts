// What an AI agent keeps in nucleus (PLAN.md section M16.5 S7): the
// patterns of examples/agents/schema.sql, run end to end with a stand-in
// for the model and its tools, so it needs no network.  agents.py is the
// same program in Python.
//
//   node examples/agents/agents.ts                    # the local server
//   node examples/agents/agents.ts file:agent.db      # in this process
//   node examples/agents/agents.ts nucleus://nucleus:nucleus@127.0.0.1:20035/nucleus
//
// 1. Conversation memory — every turn kept, the last ones read back as the
//    model's context.
// 2. Semantic memory — what the agent learned about a user, recalled by
//    meaning and words; erased with the user.
// 3. Run state — a run's state checkpointed with each step, every tool call
//    logged, in the step's own transaction.
// 4. The agents' task queue — a planner hands steps to workers; each step
//    is done exactly once with its effects, a flaky tool retried, a worker
//    that dies mid-step replaced when its lease runs out.
// 5. An experiment on a branch — the agent tries a change on a copy of the
//    database; the main one is untouched until someone decides.

import { readFileSync } from "node:fs";
import { connect, type Client, type Task, type Value } from "../../drivers/js/src/index.ts";

const port = process.env.NUCLEUS_PORT ?? "20035";
const url = process.argv[2] ?? `nucleus://nucleus:nucleus@127.0.0.1:${port}/nucleus`;
const schema = readFileSync(new URL("schema.sql", import.meta.url), "utf8");
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
type Args = Record<string, unknown>;

function check(ok: boolean, what: unknown): void {
  if (!ok) throw new Error(`unexpected: ${JSON.stringify(what)}`);
}

async function setup(db: Client): Promise<void> {
  await db.query("DROP BRANCH IF EXISTS agent_sandbox");
  for (const [kind, name] of [
    ["QUEUE", "agent_tasks"],
    ["STREAM", "agent_steps"],
    ["TABLE", "agent_runs"],
    ["TABLE", "agent_messages"],
    ["TABLE", "agent_memory"],
    ["MODEL", "agent_words"],
  ]) {
    await db.query(`DROP ${kind} IF EXISTS ${name}`);
  }
  await db.query(schema);
  // Reads wait for vectors still being made (a demo writes, then reads).
  await db.query("SET embedding.pending = wait");
}

// ------------------------------------------------------------------ 1. turns

/** Appends a turn to a conversation (two writers of one conversation
 * conflict on the turn's key; one of them is retried). */
async function say(db: Client, conversation: string, role: string, content: string): Promise<void> {
  await db.query(
    "INSERT INTO agent_messages (conversation, turn, role, content) " +
      "SELECT $1, coalesce(max(turn), 0) + 1, $2, $3 FROM agent_messages WHERE conversation = $1",
    [conversation, role, content],
  );
}

/** The conversation's last turns, oldest first: the model's context. */
async function context(db: Client, conversation: string, last = 6): Promise<Value[][]> {
  const r = await db.query(
    "SELECT role, content FROM (SELECT turn, role, content FROM agent_messages " +
      "WHERE conversation = $1 ORDER BY turn DESC LIMIT $2) t ORDER BY turn",
    [conversation, last],
  );
  return r.rows;
}

// ------------------------------------------------------- 3, 4. runs and tasks

const forecast: Record<string, number> = { Szeged: 21, Oslo: 4, Pécs: 19 };

/** The tools, standing in for real ones: `search` times out on its first
 * attempt (a flaky service). */
function tool(name: string, args: Args, attempt: number): Args {
  if (name === "weather") return { city: args.city, celsius: forecast[String(args.city)] };
  if (name === "search") {
    if (attempt === 1) throw new Error("search timed out");
    return { query: args.query, hits: 3 };
  }
  throw new Error(`no tool ${name}`);
}

/** A run and its steps, handed to the workers — in one transaction, so
 * there is never a run without its tasks. */
async function plan(db: Client, agent: string, goal: string, steps: [string, Args][]): Promise<number> {
  return db.transaction(async (c) => {
    const r = await c.query("INSERT INTO agent_runs (agent, goal) VALUES ($1, $2) RETURNING id", [agent, goal]);
    const run = Number(r.rows[0][0]);
    const tasks = c.queueOf("agent_tasks");
    for (const [name, args] of steps) await tasks.add({ run, tool: name, input: args as Value });
    return run;
  });
}

/** One step of a run: the tool called, its call logged, the run's state
 * checkpointed — all in the task's transaction, with its FINISH. */
async function step(db: Client, task: Task): Promise<void> {
  const output = tool(String(task.tool), task.input as Args, Number(task.attempt));
  await db.query("INSERT INTO agent_steps (run, tool, input, output) VALUES ($1, $2, $3, $4)", [
    task.run,
    task.tool,
    task.input,
    output as Value,
  ]);
  const state = (await db.query("SELECT state FROM agent_runs WHERE id = $1", [task.run])).rows[0][0] as Args;
  state[`${task.tool}:${task.id}`] = output;
  const left = (await db.query("SELECT count(*) FROM agent_tasks WHERE run = $1 AND id <> $2", [task.run, task.id]))
    .rows[0][0];
  await db.query("UPDATE agent_runs SET state = $2, status = $3, updated = now() WHERE id = $1", [
    task.run,
    state as Value,
    Number(left) ? "running" : "done",
  ]);
}

const db = await connect(url);
console.log("connected:", db.serverVersion);
await setup(db);

// 1. Conversation memory, and 2. what the agent learns from it.
await say(db, "c1", "user", "Hi! I live in Szeged and I like short answers.");
await db.remember("agent_memory", "ann", "Ann lives in Szeged", { kind: "fact" });
await db.remember("agent_memory", "ann", "Ann prefers short answers", { kind: "preference" });
await say(db, "c1", "assistant", "Noted.");
await say(db, "c1", "user", "What will the weather be like at home?");
const recalled = await db.recall("agent_memory", "ann", "where does the user live", 1);
console.log("context:", await context(db, "c1", 2));
console.log("recalled:", recalled.map((m) => m.content));
check(recalled[0].content === "Ann lives in Szeged", recalled);
check((await context(db, "c1")).map((r) => r[0]).join() === "user,assistant,user", "roles");

// 3, 4. A run planned into tasks; workers do each step exactly once.
const run = await plan(db, "weather-bot", "weather at Ann's home, and news", [
  ["weather", { city: "Szeged" }],
  ["search", { query: "Szeged news" }],
  ["weather", { city: "Oslo" }],
]);
// A worker takes a step under a short lease and dies before finishing.
const [lost] = await db.queueOf("agent_tasks").take(1, 200);
console.log("a worker died holding task", Number(lost.id));
await sleep(300);
// The others go on: the lost step comes back, the flaky one is retried.
const worker = await connect(url);
for (let i = 0; i < 100; i++) {
  const left = (await db.query("SELECT count(*) FROM agent_tasks WHERE run = $1", [run])).rows[0][0];
  if (Number(left) === 0) break;
  await worker.queueOf("agent_tasks").process((t) => step(worker, t), { limit: 5 });
  await sleep(20);
}
await worker.close();
const [status, state] = (await db.query("SELECT status, state FROM agent_runs WHERE id = $1", [run])).rows[0] as [
  string,
  Args,
];
const calls = (
  await db.query("SELECT tool, count(*) FROM agent_steps WHERE run = $1 GROUP BY tool ORDER BY tool", [run])
).rows.map((r) => [r[0], Number(r[1])]);
console.log("run:", status, "state:", Object.keys(state).sort(), "tool calls:", calls);
check(status === "done" && Object.keys(state).length === 3, state);
check(JSON.stringify(calls) === '[["search",1],["weather",2]]', calls); // each step once
const szeged = Object.entries(state).find(([k]) => k.startsWith("weather"))?.[1] as Args;
await say(db, "c1", "assistant", `${szeged.celsius} °C in Szeged.`);

// 5. An experiment on a branch: the agent drops its memory there.
if (!url.startsWith(":memory:")) {
  await db.query("CREATE BRANCH agent_sandbox");
  const sandbox = await connect(url, { database: "agent_sandbox" });
  await sandbox.query("DELETE FROM agent_messages WHERE conversation = 'c1'");
  const tried = Number((await sandbox.query("SELECT count(*) FROM agent_messages")).rows[0][0]);
  await sandbox.close();
  const kept = Number((await db.query("SELECT count(*) FROM agent_messages")).rows[0][0]);
  console.log("on the branch:", tried, "turns; in the database:", kept);
  check(tried === 0 && kept === 4, { tried, kept });
  await db.query("DROP BRANCH agent_sandbox");
}

// And a user erased on request: their memories go, the rest stays.
await db.forgetSubject("ann");
check((await db.recall("agent_memory", "ann", "Szeged")).length === 0, "erased");
await db.close();
console.log("ok");
