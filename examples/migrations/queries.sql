-- The application's queries: `nucleus codegen <language> --queries
-- queries.sql` makes a typed function of each (run.sh --generate).

-- name: create_user :one
-- A new user, on the free plan.
-- params: id, email, display_name?
INSERT INTO users (id, email, display_name) VALUES ($1, $2, $3)
RETURNING id, email, plan, joined;

-- name: user_by_email :one
SELECT id, email, display_name, plan FROM users WHERE email = $1;

-- name: upgrade :exec
UPDATE users SET plan = 'pro' WHERE id = $1;

-- name: add_post :exec
-- params: id, author, title, body?
INSERT INTO posts (id, author, title, body) VALUES ($1, $2, $3, $4);

-- name: posts_on_plan :many
-- The newest posts of the users on a plan, each with its author's email.
SELECT p.id, p.title, p.body, u.email AS author
FROM posts p JOIN users u ON u.id = p.author
WHERE u.plan = $1
ORDER BY p.id DESC
LIMIT $2;

-- name: post_counts :many
-- Every user and how many posts they wrote (none: 0).
SELECT u.email, count(p.id) AS posts
FROM users u LEFT JOIN posts p ON p.author = u.id
GROUP BY u.email
ORDER BY u.email;
