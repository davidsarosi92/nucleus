// The tutorial's steps from its SQL file (PLAN.md §M14.17 E5): a step
// begins with a line `-- ## Title`; the comment lines after it are its
// text (an empty `--` between paragraphs); the rest, to the next step, is
// its SQL.  Lines before the first step are the file's own notes.
export function parseTutorial(text) {
  const steps = [];
  let step = null;
  let inText = false;
  for (const line of text.split(/\r?\n/)) {
    const title = /^-- ## (.+)$/.exec(line);
    if (title) {
      step = { title: title[1].trim(), text: [], sql: [] };
      steps.push(step);
      inText = true;
      continue;
    }
    if (!step) continue;
    if (inText && /^--( |$)/.test(line)) {
      step.text.push(line.replace(/^-- ?/, ""));
      continue;
    }
    inText = false;
    step.sql.push(line);
  }
  return steps.map((s) => ({
    title: s.title,
    // Paragraphs: lines joined, an empty line between them.
    text: s.text.join("\n").trim().split(/\n\s*\n/).map((p) => p.replace(/\s*\n\s*/g, " ")).join("\n\n"),
    sql: s.sql.join("\n").trim() + "\n",
  }));
}
