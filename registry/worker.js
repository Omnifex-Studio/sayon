// Sayon's package registry, as a Cloudflare Worker. It speaks the registry
// protocol (docs/registry.md) over HTTPS: package information and search,
// archives by name and version, and publication by account tokens. D1 (the
// binding `DB`, schema.sql) holds accounts, packages, and versions; KV (the
// binding `ARCHIVES`) holds each version's archive, which never changes once
// published. The secret `ADMIN_TOKEN` lets the operator make accounts.
//
// The same code runs in Node for tests and local development (local.mjs),
// with stand-ins for D1 and KV.

const PROTOCOL = 1;
const DOMAIN = new TextEncoder().encode("sayon-package-digest-1\0");
const LARGEST = 10 * 1024 * 1024;
const MOST_FILES = 10000;
const NAME = /^[a-z][a-z0-9_]{0,63}$/;
const VERSION = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$/;

/// A request the registry refuses, with its status and why.
class Refusal extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

export default {
  async fetch(request, env) {
    try {
      return await route(request, env);
    } catch (problem) {
      if (problem instanceof Refusal) return json({ error: problem.message }, problem.status);
      console.error(problem);
      return json({ error: "the registry failed to answer; try again later" }, 500);
    }
  },
};

function json(value, status = 200) {
  return new Response(JSON.stringify(value), {
    status,
    headers: { "content-type": "application/json; charset=utf-8" },
  });
}

async function route(request, env) {
  const url = new URL(request.url);
  const parts = url.pathname.split("/").filter(Boolean).map(decodeURIComponent);
  const method = request.method;
  if (parts.length === 0 && method === "GET") {
    return new Response(
      "Sayon's package registry, protocol 1.\n" +
        "Sayon finds packages here with `sayon search`, `sayon info`, and `sayon install`.\n" +
        "The protocol: https://github.com/Omnifex-Studio/sayon/blob/main/docs/registry.md\n",
      { headers: { "content-type": "text/plain; charset=utf-8" } },
    );
  }
  if (parts[0] !== "api" || parts[1] !== "v1") throw new Refusal(404, "there is nothing at this address");
  const [what, name, version, part] = parts.slice(2);
  if (what === undefined && method === "GET") return json({ protocol: PROTOCOL });
  if (what === "search" && method === "GET" && name === undefined) return search(url, env);
  if (what === "me" && method === "GET" && name === undefined) {
    return json({ name: await authenticate(request, env) });
  }
  if (what === "users" && method === "POST" && name === undefined) return createUser(request, env);
  if (what === "packages" && name !== undefined) {
    if (version === undefined && method === "GET") return information(name, env);
    if (version !== undefined && part === undefined && method === "PUT") {
      return publish(name, version, request, env);
    }
    if (part === "archive" && parts.length === 6 && method === "GET") return archive(name, version, env);
  }
  throw new Refusal(404, "the registry does not answer this request");
}

// ----- accounts ------------------------------------------------------------------

async function sha256(bytes) {
  return new Uint8Array(await crypto.subtle.digest("SHA-256", bytes));
}

const hex = (bytes) => [...bytes].map((b) => b.toString(16).padStart(2, "0")).join("");

function bearer(request) {
  const header = request.headers.get("authorization") ?? "";
  const match = /^Bearer\s+(\S+)$/i.exec(header);
  return match ? match[1] : null;
}

/// The account a request's token belongs to.
async function authenticate(request, env) {
  const token = bearer(request);
  if (!token) throw new Refusal(401, "this needs an account's token: run `sayon login` first");
  const hash = hex(await sha256(new TextEncoder().encode(token)));
  const user = await env.DB.prepare("SELECT name FROM users WHERE token_hash = ?1").bind(hash).first();
  if (!user) throw new Refusal(401, "the token is not one this registry knows: run `sayon login` again");
  return user.name;
}

/// Whether two strings are equal, taking as long whatever they hold.
function same(a, b) {
  const x = new TextEncoder().encode(a);
  const y = new TextEncoder().encode(b);
  let difference = x.length ^ y.length;
  for (let i = 0; i < Math.max(x.length, y.length); i++) difference |= (x[i] ?? 0) ^ (y[i] ?? 0);
  return difference === 0;
}

/// The operator makes an account, or gives one a new token: `{"name": ...,
/// "replace": true}`, with the registry's admin token.
async function createUser(request, env) {
  const token = bearer(request);
  if (!env.ADMIN_TOKEN || !token || !same(token, env.ADMIN_TOKEN)) {
    throw new Refusal(403, "only the registry's operator makes accounts");
  }
  const body = await request.json().catch(() => null);
  const name = body?.name;
  if (typeof name !== "string" || !NAME.test(name)) {
    throw new Refusal(400, "an account's name is lowercase letters, digits, and `_`, starting with a letter");
  }
  const bytes = crypto.getRandomValues(new Uint8Array(32));
  const issued = `sayon_${hex(bytes)}`;
  const hash = hex(await sha256(new TextEncoder().encode(issued)));
  const existing = await env.DB.prepare("SELECT name FROM users WHERE name = ?1").bind(name).first();
  if (existing && !body.replace) throw new Refusal(409, `the account \`${name}\` already exists`);
  if (existing) {
    await env.DB.prepare("UPDATE users SET token_hash = ?2 WHERE name = ?1").bind(name, hash).run();
  } else {
    await env.DB.prepare("INSERT INTO users (name, token_hash, created) VALUES (?1, ?2, ?3)")
      .bind(name, hash, new Date().toISOString())
      .run();
  }
  return json({ name, token: issued }, existing ? 200 : 201);
}

// ----- versions ---------------------------------------------------------------------

function parseVersion(text) {
  const match = VERSION.exec(text);
  if (!match) return null;
  return { numbers: [Number(match[1]), Number(match[2]), Number(match[3])], pre: match[4] ? match[4].slice(1) : "" };
}

/// Semantic versioning's order: a pre-release before its release.
function compareVersions(a, b) {
  const x = parseVersion(a);
  const y = parseVersion(b);
  for (let i = 0; i < 3; i++) if (x.numbers[i] !== y.numbers[i]) return x.numbers[i] - y.numbers[i];
  if (!x.pre || !y.pre) return (x.pre ? -1 : 0) - (y.pre ? -1 : 0);
  const p = x.pre.split(".");
  const q = y.pre.split(".");
  for (let i = 0; i < Math.max(p.length, q.length); i++) {
    if (p[i] === undefined) return -1;
    if (q[i] === undefined) return 1;
    const m = /^\d+$/.test(p[i]);
    const n = /^\d+$/.test(q[i]);
    if (m && n && Number(p[i]) !== Number(q[i])) return Number(p[i]) - Number(q[i]);
    if (m !== n) return m ? -1 : 1;
    if (p[i] !== q[i]) return p[i] < q[i] ? -1 : 1;
  }
  return 0;
}

// ----- archives ----------------------------------------------------------------------

/// The files of an archive, checked as Sayon checks them: each path once, in
/// order, none differing from another only in case, none hidden, and none
/// leaving the package.
function entries(bytes) {
  const damaged = () => new Refusal(400, "the archive is damaged or not a Sayon package");
  if (bytes.length < DOMAIN.length || !DOMAIN.every((b, i) => bytes[i] === b)) throw damaged();
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const decoder = new TextDecoder("utf-8", { fatal: true });
  const files = [];
  const folded = new Set();
  let at = DOMAIN.length;
  const length = () => {
    if (at + 8 > bytes.length) throw damaged();
    const value = view.getBigUint64(at, true);
    at += 8;
    if (value > BigInt(bytes.length)) throw damaged();
    return Number(value);
  };
  while (at < bytes.length) {
    if (files.length >= MOST_FILES) throw new Refusal(400, "the archive holds too many files");
    const pathLength = length();
    if (at + pathLength > bytes.length) throw damaged();
    let path;
    try {
      path = decoder.decode(bytes.subarray(at, at + pathLength));
    } catch {
      throw damaged();
    }
    at += pathLength;
    const contentLength = length();
    if (at + contentLength > bytes.length) throw damaged();
    const content = bytes.subarray(at, at + contentLength);
    at += contentLength;
    const parts = path.split("/");
    const top = parts[0];
    const valid =
      path.length > 0 &&
      !/[\\:\0]/.test(path) &&
      parts.every((p) => p.length > 0 && !p.startsWith(".")) &&
      !["build", "target", "sayon.lock"].includes(top);
    if (!valid) throw new Refusal(400, `the archive holds \`${path}\`, which a package cannot`);
    if (files.length > 0 && files[files.length - 1].path >= path) {
      throw new Refusal(400, `the archive holds \`${path}\` out of order or twice`);
    }
    if (folded.has(path.toLowerCase())) {
      throw new Refusal(400, `the archive holds \`${path}\` and another path that differs only in case`);
    }
    folded.add(path.toLowerCase());
    files.push({ path, content });
  }
  return files;
}

/// The parts of a manifest the registry reads: TOML's tables, strings,
/// arrays of strings, and tables of strings, as Sayon writes manifests.
function readManifest(text) {
  const tables = {};
  let table = tables;
  const value = (raw) => {
    raw = raw.trim();
    if (raw.startsWith('"')) return JSON.parse(raw.replace(/\\u([0-9a-fA-F]{4})/g, "\\u$1"));
    if (raw.startsWith("[")) {
      const inner = raw.slice(1, -1).trim();
      return inner ? splitTop(inner).map(value) : [];
    }
    if (raw.startsWith("{")) {
      const result = {};
      const inner = raw.slice(1, -1).trim();
      for (const pair of inner ? splitTop(inner) : []) {
        const [key, item] = splitKey(pair);
        result[key] = value(item);
      }
      return result;
    }
    if (raw === "true" || raw === "false") return raw === "true";
    if (/^-?\d+$/.test(raw)) return Number(raw);
    throw new Refusal(400, `sayon.toml has a value Sayon does not read: ${raw}`);
  };
  for (const original of text.split(/\r?\n/)) {
    const line = stripComment(original).trim();
    if (!line) continue;
    const header = /^\[([A-Za-z0-9_]+)\]$/.exec(line);
    if (header) {
      table = tables[header[1]] ??= {};
      continue;
    }
    const [key, raw] = splitKey(line);
    table[key] = value(raw);
  }
  return tables;
}

function stripComment(line) {
  let quoted = false;
  for (let i = 0; i < line.length; i++) {
    if (line[i] === "\\" && quoted) i++;
    else if (line[i] === '"') quoted = !quoted;
    else if (line[i] === "#" && !quoted) return line.slice(0, i);
  }
  return line;
}

function splitKey(text) {
  const index = text.indexOf("=");
  if (index < 0) throw new Refusal(400, `sayon.toml has a line Sayon does not read: ${text}`);
  return [text.slice(0, index).trim().replace(/^"|"$/g, ""), text.slice(index + 1)];
}

/// Splits at the commas outside strings and brackets.
function splitTop(text) {
  const parts = [];
  let depth = 0;
  let quoted = false;
  let start = 0;
  for (let i = 0; i < text.length; i++) {
    const c = text[i];
    if (c === "\\" && quoted) i++;
    else if (c === '"') quoted = !quoted;
    else if (!quoted && (c === "[" || c === "{")) depth++;
    else if (!quoted && (c === "]" || c === "}")) depth--;
    else if (!quoted && depth === 0 && c === ",") {
      parts.push(text.slice(start, i));
      start = i + 1;
    }
  }
  if (text.slice(start).trim()) parts.push(text.slice(start));
  return parts;
}

/// A manifest's dependencies on registry packages, as the index tells them.
function dependencies(manifest) {
  const found = [];
  for (const [alias, spec] of Object.entries(manifest.dependencies ?? {})) {
    const entry = typeof spec === "string" ? { version: spec } : spec;
    if (typeof entry.version !== "string") {
      throw new Refusal(
        400,
        `the dependency \`${alias}\` is not from a registry; a published package depends only on registry packages`,
      );
    }
    found.push({
      alias,
      package: entry.package ?? alias,
      requirement: entry.version,
      ...(entry.registry ? { registry: entry.registry } : {}),
    });
  }
  return found.sort((a, b) => (a.alias < b.alias ? -1 : 1));
}

// ----- the protocol's answers ----------------------------------------------------------

async function versionsOf(name, env) {
  const { results } = await env.DB.prepare(
    "SELECT version, digest, size, dependencies, published, publisher FROM versions WHERE name = ?1",
  )
    .bind(name)
    .all();
  return results
    .map((row) => ({
      version: row.version,
      digest: row.digest,
      size: row.size,
      published: row.published,
      publisher: row.publisher,
      dependencies: JSON.parse(row.dependencies),
    }))
    .sort((a, b) => compareVersions(a.version, b.version));
}

async function information(name, env) {
  if (!NAME.test(name)) throw new Refusal(400, `\`${name}\` is not a package name`);
  const found = await env.DB.prepare("SELECT name, owner, description, latest FROM packages WHERE name = ?1")
    .bind(name)
    .first();
  if (!found) throw new Refusal(404, `there is no package \`${name}\``);
  return json({ ...found, versions: await versionsOf(name, env) });
}

async function search(url, env) {
  const query = (url.searchParams.get("q") ?? "").trim().slice(0, 64);
  const limit = Math.min(Math.max(Number(url.searchParams.get("limit")) || 20, 1), 50);
  const pattern = `%${query.replace(/[\\%_]/g, (c) => `\\${c}`)}%`;
  const { results } = await env.DB.prepare(
    "SELECT name, description, latest FROM packages WHERE name LIKE ?1 ESCAPE '\\' OR description LIKE ?1 ESCAPE '\\' " +
      "ORDER BY name = ?2 DESC, name LIMIT ?3",
  )
    .bind(pattern, query, limit)
    .all();
  return json({ packages: results });
}

async function archive(name, version, env) {
  if (!NAME.test(name) || !VERSION.test(version)) throw new Refusal(400, "no such package version");
  const row = await env.DB.prepare("SELECT digest FROM versions WHERE name = ?1 AND version = ?2")
    .bind(name, version)
    .first();
  const bytes = row && (await env.ARCHIVES.get(`archive/${name}/${version}`, "arrayBuffer"));
  if (!bytes) throw new Refusal(404, `there is no version ${version} of \`${name}\``);
  return new Response(bytes, {
    headers: {
      "content-type": "application/octet-stream",
      "x-sayon-digest": row.digest,
      "cache-control": "public, max-age=31536000, immutable",
    },
  });
}

/// Publishes a version: the archive, checked, with its manifest naming the
/// package and the version, by the package's owner or, for a new package,
/// by whoever publishes it first. A version never changes.
async function publish(name, version, request, env) {
  const user = await authenticate(request, env);
  if (!NAME.test(name)) throw new Refusal(400, `\`${name}\` is not a package name`);
  if (!VERSION.test(version)) throw new Refusal(400, `\`${version}\` is not a version, such as 1.0.0`);
  if (Number(request.headers.get("content-length") ?? 0) > LARGEST) {
    throw new Refusal(413, "a package holds 10 MiB at most");
  }
  const bytes = new Uint8Array(await request.arrayBuffer());
  if (bytes.length > LARGEST) throw new Refusal(413, "a package holds 10 MiB at most");
  const files = entries(bytes);
  const manifestFile = files.find((f) => f.path === "sayon.toml");
  if (!manifestFile) throw new Refusal(400, "the archive has no sayon.toml");
  const manifest = readManifest(new TextDecoder().decode(manifestFile.content));
  const declared = manifest.package ?? {};
  if (declared.name !== name || declared.version !== version) {
    throw new Refusal(400, `the archive's sayon.toml is ${declared.name} ${declared.version}, not ${name} ${version}`);
  }
  const edges = dependencies(manifest);
  const digest = `sha256:${hex(await sha256(bytes))}`;
  const owned = await env.DB.prepare("SELECT owner FROM packages WHERE name = ?1").bind(name).first();
  if (owned && owned.owner !== user) {
    throw new Refusal(403, `\`${name}\` belongs to the account \`${owned.owner}\``);
  }
  const published = await env.DB.prepare("SELECT digest FROM versions WHERE name = ?1 AND version = ?2")
    .bind(name, version)
    .first();
  if (published) {
    throw new Refusal(409, `${name} ${version} is already published, and versions never change; publish a new version`);
  }
  // The latest is the greatest release, or, before the first release, the
  // greatest pre-release.
  const all = [...(await versionsOf(name, env)).map((v) => v.version), version].sort(compareVersions);
  const releases = all.filter((v) => !parseVersion(v).pre);
  const latest = (releases.length ? releases : all).at(-1);
  const description = typeof declared.description === "string" ? declared.description : "";
  const now = new Date().toISOString();
  // The archive goes first, so that no version is ever listed without one.
  // The rows go together, and only for the package's owner, so that two
  // publications at once cannot take a name or a version from each other.
  await env.ARCHIVES.put(`archive/${name}/${version}`, bytes);
  await env.DB.batch([
    env.DB.prepare(
      "INSERT INTO packages (name, owner, description, latest, created, updated) VALUES (?1, ?2, ?3, ?4, ?5, ?5) " +
        "ON CONFLICT (name) DO UPDATE SET description = excluded.description, latest = excluded.latest, " +
        "updated = excluded.updated WHERE packages.owner = excluded.owner",
    ).bind(name, user, description, latest, now),
    env.DB.prepare(
      "INSERT INTO versions (name, version, digest, size, dependencies, published, publisher) " +
        "SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7 WHERE EXISTS (SELECT 1 FROM packages WHERE name = ?1 AND owner = ?7) " +
        "ON CONFLICT DO NOTHING",
    ).bind(name, version, digest, bytes.length, JSON.stringify(edges), now, user),
  ]);
  const stored = await env.DB.prepare("SELECT digest, publisher FROM versions WHERE name = ?1 AND version = ?2")
    .bind(name, version)
    .first();
  if (!stored || stored.publisher !== user || stored.digest !== digest) {
    throw new Refusal(409, `${name} ${version} was published by another request at the same time`);
  }
  return json({ name, version, digest }, 201);
}
