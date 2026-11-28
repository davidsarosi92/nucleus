-- What an AI agent keeps in nucleus (PLAN.md §M16.5 S7; examples/agents):
-- five patterns, each a few tables, all in one database and one
-- transaction when the agent's work needs them together.

-- 1. Conversation memory: every turn of every conversation, in order.
CREATE TABLE agent_messages (
    conversation TEXT NOT NULL,
    turn INT NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('system', 'user', 'assistant', 'tool')),
    content TEXT NOT NULL,
    at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (conversation, turn)
);

-- 2. Semantic memory: what the agent learned about each user, recalled by
-- meaning and by words; whose a memory is is its subject (FORGET SUBJECT
-- erases it).  `words` is a model of lexical vectors that needs no
-- network; a real agent names a model of meaning.
CREATE MODEL agent_words USING hashing WITH (dimensions = 256);
CREATE MEMORY STORE agent_memory USING agent_words;

-- 3. Run state: an agent's run, its state checkpointed with each step,
-- and every tool call it made, appended in the step's transaction.
CREATE TABLE agent_runs (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    agent TEXT NOT NULL,
    goal TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'running' CHECK (status IN ('running', 'done', 'failed')),
    state JSON NOT NULL DEFAULT '{}',
    updated TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE STREAM agent_steps (run BIGINT NOT NULL, tool TEXT NOT NULL, input JSON, output JSON)
    WITH (retention = '30 days');

-- 4. The agents' task queue: work handed between agents, each task done
-- exactly once with its effects, a failing one retried, then a dead letter.
CREATE QUEUE agent_tasks (run BIGINT NOT NULL, tool TEXT NOT NULL, input JSON)
    WITH (lease = '1 minute', max_attempts = 3, backoff = '10 milliseconds');

-- 5. Experiments on a branch: CREATE BRANCH gives an agent a copy of the
-- database to try a change in (no rows copied); see agents.py / agents.ts.
