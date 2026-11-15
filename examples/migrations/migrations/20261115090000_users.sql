-- migrate:up
CREATE TYPE plan AS ENUM ('free', 'pro');
CREATE TABLE users (
  id BIGINT PRIMARY KEY,
  email TEXT NOT NULL UNIQUE,
  name TEXT,
  plan plan NOT NULL DEFAULT 'free',
  joined TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- migrate:down
DROP TABLE users;
DROP TYPE plan;
