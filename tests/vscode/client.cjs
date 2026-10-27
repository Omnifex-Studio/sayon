// Runs the VS Code extension's client against `sayon lsp`, with a stand-in
// for the part of VS Code's API it uses, as VS Code would: a document is
// open, its diagnostics arrive, and the extension's providers answer.
//
//   node tests/vscode/client.cjs <the sayon program> <editors/vscode>
"use strict";

const Module = require("module");
const fs = require("fs");
const os = require("os");
const path = require("path");
const { pathToFileURL } = require("url");

const [sayon, extensionDir] = process.argv.slice(2);

class Position {
  constructor(line, character) {
    this.line = line;
    this.character = character;
  }
}
class Range {
  constructor(start, end) {
    this.start = start;
    this.end = end;
  }
}
class Location {
  constructor(uri, range) {
    this.uri = uri;
    this.range = range;
  }
}
class Uri {
  constructor(text) {
    this.text = text;
    this.scheme = text.slice(0, text.indexOf(":"));
  }
  toString() {
    return this.text;
  }
  static parse(text) {
    return new Uri(text);
  }
}
class Hover {
  constructor(contents, range) {
    this.contents = contents;
    this.range = range;
  }
}
class MarkdownString {
  constructor(value) {
    this.value = value;
  }
}
class CompletionItem {
  constructor(label, kind) {
    this.label = label;
    this.kind = kind;
  }
}
class Diagnostic {
  constructor(range, message, severity) {
    this.range = range;
    this.message = message;
    this.severity = severity;
  }
}
class DiagnosticRelatedInformation {
  constructor(location, message) {
    this.location = location;
    this.message = message;
  }
}
class WorkspaceEdit {
  constructor() {
    this.edits = [];
  }
  replace(uri, range, newText) {
    this.edits.push({ uri, range, newText });
  }
}

const disposable = { dispose() {} };
const providers = {};
const shown = new Map();
const waiters = [];
const listeners = { open: [], change: [], close: [] };
const event = (list) => (listener) => {
  list.push(listener);
  return disposable;
};
const register = (name) => (_selector, provider) => {
  providers[name] = provider;
  return disposable;
};
const documents = [];

const vscode = {
  Position,
  Range,
  Location,
  Uri,
  Hover,
  MarkdownString,
  CompletionItem,
  CompletionItemKind: { Text: 0 },
  Diagnostic,
  DiagnosticSeverity: { Error: 0, Warning: 1 },
  DiagnosticRelatedInformation,
  TextEdit: { replace: (range, newText) => ({ range, newText }) },
  WorkspaceEdit,
  languages: {
    createDiagnosticCollection: () => ({
      set(uri, list) {
        shown.set(uri.toString(), list);
        for (const waiter of waiters.splice(0)) waiter();
      },
      delete(uri) {
        shown.delete(uri.toString());
      },
      dispose() {},
    }),
    registerHoverProvider: register("hover"),
    registerDefinitionProvider: register("definition"),
    registerReferenceProvider: register("references"),
    registerRenameProvider: register("rename"),
    registerCompletionItemProvider: register("completion"),
    registerDocumentFormattingEditProvider: register("formatting"),
  },
  workspace: {
    textDocuments: documents,
    workspaceFolders: undefined,
    getConfiguration: () => ({ get: (key) => (key === "path" ? sayon : undefined) }),
    onDidOpenTextDocument: event(listeners.open),
    onDidChangeTextDocument: event(listeners.change),
    onDidCloseTextDocument: event(listeners.close),
  },
  window: {
    showErrorMessage: (message) => {
      console.error(message);
      process.exit(1);
    },
    createOutputChannel: () => ({ append() {}, appendLine() {}, dispose() {} }),
  },
  commands: { registerCommand: () => disposable },
};

const load = Module._load;
Module._load = function (request, parent, isMain) {
  return request === "vscode" ? vscode : load.call(this, request, parent, isMain);
};

function check(condition, message) {
  if (!condition) {
    console.error(`failed: ${message}`);
    process.exit(1);
  }
}

/// The position of the `nth` place `needle` is in `text`, `extra` on.
function find(text, needle, extra = 0) {
  const offset = text.indexOf(needle) + extra;
  const before = text.slice(0, offset);
  const line = before.split("\n").length - 1;
  return new Position(line, offset - (before.lastIndexOf("\n") + 1));
}

/// The diagnostics shown for a document, once they come.
function diagnostics(uri, wanted) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("no diagnostics came")), 60000);
    const look = () => {
      const list = shown.get(uri);
      if (list && wanted(list)) {
        clearTimeout(timer);
        resolve(list);
      } else {
        waiters.push(look);
      }
    };
    look();
  });
}

async function main() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "sayon-vscode-"));
  const file = path.join(dir, "main.sayon");
  let text = [
    'graphics = import("standard/graphics.sayon")',
    "",
    "function triple(value: integer) -> integer",
    "    return value * 3",
    "",
    "count = triple(14)",
    "print(count, unknown)",
    "canvas = graphics.canvas(8, 8)",
    "",
  ].join("\n");
  fs.writeFileSync(file, text);
  const uri = pathToFileURL(file).href;
  const document = {
    uri: Uri.parse(uri),
    languageId: "sayon",
    version: 1,
    getText: () => text,
  };
  documents.push(document);

  const extension = require(path.join(extensionDir, "extension.js"));
  extension.activate({ subscriptions: [] });

  const first = await diagnostics(uri, (list) => list.length > 0);
  check(first.length === 1 && first[0].code === "E_UNDEFINED_NAME", `diagnostics ${JSON.stringify(first)}`);
  check(first[0].range.start.line === 6, "the diagnostic's line");

  const hover = await providers.hover.provideHover(document, find(text, "triple(14)", 1));
  check(hover && hover.contents.value.includes("function triple(value: integer) -> integer"), "hover");

  const definition = await providers.definition.provideDefinition(document, find(text, "triple(14)", 1));
  check(definition && definition.range.start.line === 2, "definition");

  const references = await providers.references.provideReferences(document, find(text, "count", 0), {
    includeDeclaration: true,
  });
  check(references.length === 2, `references ${references.length}`);

  const prepared = await providers.rename.prepareRename(document, find(text, "count", 0));
  check(prepared.placeholder === "count", "prepareRename");
  const renamed = await providers.rename.provideRenameEdits(document, find(text, "count", 0), "total");
  check(renamed.edits.length === 2 && renamed.edits.every((e) => e.newText === "total"), "rename");
  let refused = false;
  await providers.rename.prepareRename(document, find(text, "canvas(8", 1)).catch(() => {
    refused = true;
  });
  check(refused, "renaming the standard library's names is refused");

  // Typing a dot after a canvas, which the server learns of as a change.
  text = `${text}canvas.\n`;
  document.version = 2;
  for (const listener of listeners.change) listener({ document });
  const members = await providers.completion.provideCompletionItems(document, find(text, "canvas.\n", 7));
  const labels = members.map((item) => item.label);
  check(labels.includes("fill_circle") && labels.includes("smooth"), `completion ${labels.join(", ")}`);

  text = "x   =  1\nprint( x )\n";
  document.version = 3;
  for (const listener of listeners.change) listener({ document });
  const edits = await providers.formatting.provideDocumentFormattingEdits(document);
  check(edits.length === 1 && edits[0].newText === "x = 1\nprint(x)\n", "formatting");

  await extension.deactivate();
  fs.rmSync(dir, { recursive: true, force: true });
  console.log("the extension's client answered as VS Code asks");
}

main().catch((problem) => {
  console.error(problem);
  process.exit(1);
});
