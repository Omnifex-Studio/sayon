// Runs a Sayon program built for web pages in Node, through its generated
// glue: node node.mjs program.js
//
// What the program writes goes to this process's output and errors as it
// comes, and its exit status becomes this process's. SAYON_DEBUG_ALLOC in
// the environment is passed on as the host option of that name.

import { pathToFileURL } from "node:url";

const [glue] = process.argv.slice(2);
const { run } = await import(pathToFileURL(glue).href);
const streams = [null, process.stdout, process.stderr];
const status = await run({
  write: (stream, text) => streams[stream].write(text),
  options: { SAYON_DEBUG_ALLOC: process.env.SAYON_DEBUG_ALLOC },
});
process.exitCode = status;
