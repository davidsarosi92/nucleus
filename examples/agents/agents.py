"""What an AI agent keeps in nucleus (PLAN.md section M16.5 S7): the
patterns of examples/agents/schema.sql, run end to end with a stand-in
for the model and its tools, so it needs no network.

    python3 examples/agents/agents.py                      # the local server
    python3 examples/agents/agents.py file:agent.db        # in this process
    python3 examples/agents/agents.py nucleus://nucleus:nucleus@127.0.0.1:20035/nucleus

1. Conversation memory — every turn kept, the last ones read back as the
   model's context.
2. Semantic memory — what the agent learned about a user, recalled by
   meaning and words; erased with the user.
3. Run state — a run's state checkpointed with each step, every tool call
   logged, in the step's own transaction.
4. The agents' task queue — a planner hands steps to workers; each step is
   done exactly once with its effects, a flaky tool retried, a worker that
   dies mid-step replaced when its lease runs out.
5. An experiment on a branch — the agent tries a change on a copy of the
   database; the main one is untouched until someone decides.
"""

import os
import sys
import time

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "..", "drivers", "python"))
import nucleus  # noqa: E402

port = int(os.environ.get("NUCLEUS_PORT", nucleus.DEFAULT_PORT))
url = sys.argv[1] if len(sys.argv) > 1 else f"nucleus://nucleus:nucleus@127.0.0.1:{port}/nucleus"
schema = os.path.join(os.path.dirname(__file__), "schema.sql")


def setup(db):
    db.execute("DROP BRANCH IF EXISTS agent_sandbox")
    for drop in ("QUEUE agent_tasks", "STREAM agent_steps", "TABLE agent_runs",
                 "TABLE agent_messages", "TABLE agent_memory", "MODEL agent_words"):
        db.execute(f"DROP {drop.split()[0]} IF EXISTS {drop.split()[1]}")
    with open(schema) as f:
        db.execute(f.read())
    # Reads wait for vectors still being made (a demo writes, then reads).
    db.execute("SET embedding.pending = wait")


# ---------------------------------------------------------------- 1. turns

def say(db, conversation, role, content):
    """Appends a turn to a conversation (two writers of one conversation
    conflict on the turn's key; one of them is retried)."""
    db.execute(
        "INSERT INTO agent_messages (conversation, turn, role, content) "
        "SELECT $1, coalesce(max(turn), 0) + 1, $2, $3 FROM agent_messages WHERE conversation = $1",
        [conversation, role, content],
    )


def context(db, conversation, last=6):
    """The conversation's last turns, oldest first: the model's context."""
    return db.execute(
        "SELECT role, content FROM (SELECT turn, role, content FROM agent_messages "
        "WHERE conversation = $1 ORDER BY turn DESC LIMIT $2) t ORDER BY turn",
        [conversation, last],
    ).rows


# ----------------------------------------------------- 3, 4. runs and tasks

FORECAST = {"Szeged": 21, "Oslo": 4, "Pécs": 19}


def tool(name, args, attempt):
    """The tools, standing in for real ones: `search` times out on its
    first attempt (a flaky service)."""
    if name == "weather":
        return {"city": args["city"], "celsius": FORECAST[args["city"]]}
    if name == "search":
        if attempt == 1:
            raise TimeoutError("search timed out")
        return {"query": args["query"], "hits": 3}
    raise ValueError(f"no tool {name}")


def plan(db, agent, goal, steps):
    """A run and its steps, handed to the workers — in one transaction, so
    there is never a run without its tasks."""
    with db.transaction():
        run = db.execute("INSERT INTO agent_runs (agent, goal) VALUES ($1, $2) RETURNING id",
                         [agent, goal]).rows[0][0]
        tasks = db.queue_of("agent_tasks")
        for name, args in steps:
            tasks.add({"run": run, "tool": name, "input": args})
    return run


def step(db, task):
    """One step of a run: the tool called, its call logged, the run's state
    checkpointed — all in the task's transaction, with its FINISH."""
    output = tool(task["tool"], task["input"], task["attempt"])
    db.execute("INSERT INTO agent_steps (run, tool, input, output) VALUES ($1, $2, $3, $4)",
               [task["run"], task["tool"], task["input"], output])
    state = db.execute("SELECT state FROM agent_runs WHERE id = $1", [task["run"]]).rows[0][0]
    state[f"{task['tool']}:{task['id']}"] = output
    left = db.execute("SELECT count(*) FROM agent_tasks WHERE run = $1 AND id <> $2",
                      [task["run"], task["id"]]).rows[0][0]
    db.execute("UPDATE agent_runs SET state = $2, status = $3, updated = now() WHERE id = $1",
               [task["run"], state, "running" if left else "done"])


with nucleus.connect(url) as db:
    print("connected:", db.server_version)
    setup(db)

    # 1. Conversation memory, and 2. what the agent learns from it.
    say(db, "c1", "user", "Hi! I live in Szeged and I like short answers.")
    db.remember("agent_memory", "ann", "Ann lives in Szeged", kind="fact")
    db.remember("agent_memory", "ann", "Ann prefers short answers", kind="preference")
    say(db, "c1", "assistant", "Noted.")
    say(db, "c1", "user", "What will the weather be like at home?")
    recalled = db.recall("agent_memory", "ann", "where does the user live", k=1)
    print("context:", context(db, "c1", last=2))
    print("recalled:", [m["content"] for m in recalled])
    assert recalled[0]["content"] == "Ann lives in Szeged"
    assert [r[0] for r in context(db, "c1")] == ["user", "assistant", "user"]

    # 3, 4. A run planned into tasks; workers do each step exactly once.
    run = plan(db, "weather-bot", "weather at Ann's home, and news",
               [("weather", {"city": "Szeged"}), ("search", {"query": "Szeged news"}),
                ("weather", {"city": "Oslo"})])
    tasks = db.queue_of("agent_tasks")
    # A worker takes a step under a short lease and dies before finishing.
    [lost] = tasks.take(lease_ms=200)
    print("a worker died holding task", lost["id"])
    time.sleep(0.3)
    # The others go on: the lost step comes back, the flaky one is retried.
    with nucleus.connect(url) as worker:
        for _ in range(100):
            if not db.execute("SELECT count(*) FROM agent_tasks WHERE run = $1", [run]).rows[0][0]:
                break
            worker.queue_of("agent_tasks").process(lambda t: step(worker, t), limit=5)
            time.sleep(0.02)
    status, state = db.execute("SELECT status, state FROM agent_runs WHERE id = $1", [run]).rows[0]
    calls = db.execute("SELECT tool, count(*) FROM agent_steps WHERE run = $1 GROUP BY tool ORDER BY tool",
                       [run]).rows
    print("run:", status, "state:", sorted(state), "tool calls:", calls)
    assert status == "done" and len(state) == 3
    assert calls == [("search", 1), ("weather", 2)], calls   # each step once
    say(db, "c1", "assistant", f"{state[next(k for k in state if 'weather' in k)]['celsius']} °C in Szeged.")

    # 5. An experiment on a branch: the agent drops its memory there.
    if not url.startswith(":memory:"):
        db.execute("CREATE BRANCH agent_sandbox")
        with nucleus.connect(url, database="agent_sandbox") as sandbox:
            sandbox.execute("DELETE FROM agent_messages WHERE conversation = 'c1'")
            tried = sandbox.execute("SELECT count(*) FROM agent_messages").rows[0][0]
        kept = db.execute("SELECT count(*) FROM agent_messages").rows[0][0]
        print("on the branch:", tried, "turns; in the database:", kept)
        assert tried == 0 and kept == 4
        db.execute("DROP BRANCH agent_sandbox")

    # And a user erased on request: their memories go, the rest stays —
    # and no file holds them any more (found: the search after the scrub).
    erasure, rows, scrubbed, found = db.forget_subject("ann")
    assert rows == 2 and scrubbed and found in (0, None), (rows, scrubbed, found)
    assert db.recall("agent_memory", "ann", "Szeged") == []
    print("ok")
