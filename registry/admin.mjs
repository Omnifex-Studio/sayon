// Makes an account on a registry, or gives one a new token, and prints the
// account's token, which `sayon login` then takes. The registry's admin
// token comes from the environment's SAYON_REGISTRY_ADMIN_TOKEN.
//
//   node registry/admin.mjs <registry address> <account> [--replace]

const [registry, name, flag] = process.argv.slice(2);
const admin = process.env.SAYON_REGISTRY_ADMIN_TOKEN;
if (!registry || !name || (flag && flag !== "--replace")) {
  console.error("usage: node registry/admin.mjs <registry address> <account> [--replace]");
  process.exit(2);
}
if (!admin) {
  console.error("set SAYON_REGISTRY_ADMIN_TOKEN to the registry's admin token first");
  process.exit(2);
}
const response = await fetch(new URL("/api/v1/users", registry), {
  method: "POST",
  headers: { authorization: `Bearer ${admin}`, "content-type": "application/json" },
  body: JSON.stringify({ name, replace: flag === "--replace" }),
});
const answer = await response.json().catch(() => ({ error: `the registry answered ${response.status}` }));
if (!response.ok) {
  console.error(answer.error);
  process.exit(1);
}
console.log(answer.token);
