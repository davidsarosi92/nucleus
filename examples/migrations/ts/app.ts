// The sample project's TypeScript program (PLAN.md §M14.16 M6): the
// functions nucleus codegen made of queries.sql (queries.ts), on a server
// whose schema migrations/ made.  Run by ../run.sh.
import assert from "node:assert/strict";
import { connect } from "../../../drivers/js/src/index.ts";
import { addPost, createUser, postCounts, postsOnPlan, upgrade, userByEmail } from "./queries.ts";

const db = await connect({ port: Number(process.env.NUCLEUS_PORT ?? 20035) });

// A transaction: the user and their posts, or none of them.
await db.transaction(async (tx) => {
  const ann = await createUser(tx, 100, "ann@ts.example", "Ann");
  assert.ok(ann !== null && ann.plan === "free" && ann.joined instanceof Date);
  await addPost(tx, 100, 100, "Hello", "The first post.");
  await addPost(tx, 101, 100, "Again", null);
});

const ann = await userByEmail(db, "ann@ts.example");
assert.ok(ann !== null && ann.display_name === "Ann");
assert.equal(await userByEmail(db, "nobody@ts.example"), null);
assert.equal(await upgrade(db, ann.id), 1);

const posts = await postsOnPlan(db, "pro", 10);
assert.deepEqual(
  posts.filter((p) => p.author === "ann@ts.example").map((p) => [p.title, p.body]),
  [["Again", null], ["Hello", "The first post."]],
);
const counts = await postCounts(db);
assert.equal(Number(counts.find((c) => c.email === "ann@ts.example")?.posts), 2);
await db.close();
console.log("migrations example (ts): ok");
