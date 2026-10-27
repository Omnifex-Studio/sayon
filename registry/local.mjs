// Runs the registry (worker.js) on this computer, for tests and local work:
// D1 becomes a SQLite file and KV a folder, both under `--data`.
//
//   node registry/local.mjs [--port 0] [--data <folder>] [--account <name>]...
//
// It listens on 127.0.0.1 and prints one line of JSON once it does: the
// registry's address, and a token for each `--account` it made. The admin
// token is the environment's SAYON_REGISTRY_ADMIN_TOKEN, when it has one.

import { createServer } from "node:http";
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { DatabaseSync } from "node:sqlite";
import worker from "./worker.js";

const here = dirname(fileURLToPath(import.meta.url));

function options(args) {
  const found = { port: 0, data: null, accounts: [] };
  for (let i = 0; i < args.length; i++) {
    const value = args[i + 1];
    if (args[i] === "--port") found.port = Number(value);
    else if (args[i] === "--data") found.data = value;
    else if (args[i] === "--account") found.accounts.push(value);
    else {
      console.error(`unknown option ${args[i]}`);
      process.exit(2);
    }
    i++;
  }
  return found;
}

/// D1's interface over SQLite: statements with `bind`, `first`, `all`, and
/// `run`, and `batch`, which runs statements in one transaction.
class Database {
  constructor(path) {
    this.db = new DatabaseSync(path);
    this.db.exec("PRAGMA foreign_keys = ON");
    this.db.exec(readFileSync(join(here, "schema.sql"), "utf8"));
  }

  prepare(sql) {
    const statement = this.db.prepare(sql);
    const bound = (values) => ({
      bind: (...more) => bound(more),
      first: async () => statement.get(...values) ?? null,
      all: async () => ({ results: statement.all(...values) }),
      run: async () => {
        statement.run(...values);
        return { success: true };
      },
      runNow: () => statement.run(...values),
    });
    return bound([]);
  }

  async batch(statements) {
    this.db.exec("BEGIN");
    try {
      for (const statement of statements) statement.runNow();
      this.db.exec("COMMIT");
    } catch (problem) {
      this.db.exec("ROLLBACK");
      throw problem;
    }
    return statements.map(() => ({ success: true }));
  }
}

/// KV's interface over a folder: a file for each key.
class Store {
  constructor(folder) {
    this.folder = folder;
  }

  path(key) {
    return join(this.folder, ...key.split("/").map(encodeURIComponent));
  }

  async get(key, type) {
    const path = this.path(key);
    if (!existsSync(path)) return null;
    const bytes = readFileSync(path);
    if (type === "arrayBuffer") return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.length);
    return bytes.toString("utf8");
  }

  async put(key, value) {
    const path = this.path(key);
    mkdirSync(dirname(path), { recursive: true });
    writeFileSync(path, value);
  }
}

const settings = options(process.argv.slice(2));
const data = settings.data ?? mkdtempSync(join(tmpdir(), "sayon-registry-"));
mkdirSync(data, { recursive: true });
const env = {
  DB: new Database(join(data, "registry.sqlite")),
  ARCHIVES: new Store(join(data, "archives")),
  ADMIN_TOKEN: process.env.SAYON_REGISTRY_ADMIN_TOKEN || crypto.randomUUID(),
};

const server = createServer(async (incoming, outgoing) => {
  const chunks = [];
  for await (const chunk of incoming) chunks.push(chunk);
  const body = Buffer.concat(chunks);
  const headers = new Headers();
  for (const [name, value] of Object.entries(incoming.headers)) {
    headers.set(name, Array.isArray(value) ? value.join(", ") : value);
  }
  const request = new Request(`http://${incoming.headers.host ?? "127.0.0.1"}${incoming.url}`, {
    method: incoming.method,
    headers,
    body: ["GET", "HEAD"].includes(incoming.method) ? undefined : body,
  });
  const response = await worker.fetch(request, env);
  const bytes = Buffer.from(await response.arrayBuffer());
  outgoing.writeHead(response.status, {
    ...Object.fromEntries(response.headers),
    "content-length": bytes.length,
  });
  outgoing.end(bytes);
});

server.listen(settings.port, "127.0.0.1", async () => {
  const url = `http://127.0.0.1:${server.address().port}`;
  const accounts = {};
  for (const name of settings.accounts) {
    const response = await worker.fetch(
      new Request(`${url}/api/v1/users`, {
        method: "POST",
        headers: { authorization: `Bearer ${env.ADMIN_TOKEN}`, "content-type": "application/json" },
        body: JSON.stringify({ name, replace: true }),
      }),
      env,
    );
    const made = await response.json();
    if (!made.token) {
      console.error(`cannot make the account ${name}: ${made.error}`);
      process.exit(1);
    }
    accounts[name] = made.token;
  }
  console.log(JSON.stringify({ url, data, accounts }));
});
