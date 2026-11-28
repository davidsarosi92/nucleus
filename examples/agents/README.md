# What an AI agent keeps in nucleus

[`schema.sql`](schema.sql) is the schema of five patterns an agent needs;
[`agents.py`](agents.py) and [`agents.ts`](agents.ts) run them end to end
— the same program in Python and TypeScript, with a stand-in for the
model and its tools, so they need no network:

```sh
python3 examples/agents/agents.py                 # the local server
node examples/agents/agents.ts file:agent.db      # a database in this process
```

| Pattern | In the schema | What it gives |
|---------|---------------|---------------|
| Conversation memory | `agent_messages` (conversation, turn) | every turn kept in order; the last ones read back as the model's context |
| Semantic memory | `CREATE MEMORY STORE agent_memory` | what the agent learned about a user, recalled by meaning and words (`remember`, `recall`), erased with the user (`FORGET SUBJECT`) |
| Run state | `agent_runs`, the stream `agent_steps` | a run's state checkpointed with each step; every tool call logged, append-only, in the step's transaction |
| The agents' task queue | `CREATE QUEUE agent_tasks` | a planner's steps handed to workers (`queue_of(...).process(work)`): each done exactly once with its effects; a flaky tool retried after a backoff, then a dead letter; a worker that dies mid-step replaced when its lease runs out |
| An experiment on a branch | `CREATE BRANCH agent_sandbox` | a copy of the database (no rows copied) to try a change in; the main one untouched |

The point is the transaction: a step's tool call, its log entry, the
run's new state and the task's `FINISH` commit together or not at all,
so a crash or a lost lease never leaves a step half done or done twice —
what a separate queue, cache and database cannot promise between them.
nucleus PLAN.md §M16.5 S7; DOCS.md §8.43 (streams), §8.45 (queues).

CI runs both programs against a server and in the process
(`scripts/driver-tests.sh`).
