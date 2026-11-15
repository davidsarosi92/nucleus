-- migrate:up
CREATE TABLE posts (
  id BIGINT PRIMARY KEY,
  author BIGINT NOT NULL REFERENCES users (id),
  title TEXT NOT NULL,
  body TEXT
);
CREATE INDEX posts_author ON posts (author);
ALTER TABLE users RENAME COLUMN name TO display_name;

-- migrate:down
ALTER TABLE users RENAME COLUMN display_name TO name;
DROP TABLE posts;
