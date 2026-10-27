// Sayon's extension for VS Code. Highlighting comes from the grammar; the
// rest comes from Sayon's language server, `sayon lsp`, which this starts
// and talks to in the language server protocol itself, with no library
// between them: diagnostics, hover, definitions, references, renaming,
// completion, and formatting.
"use strict";

const vscode = require("vscode");
const { spawn } = require("child_process");
const fs = require("fs");
const os = require("os");
const path = require("path");

const SELECTOR = { language: "sayon" };

/// The kinds of completion items: the protocol counts from 1, VS Code from 0.
const completionKind = (kind) => (kind ? kind - 1 : vscode.CompletionItemKind.Text);

const position = (p) => new vscode.Position(p.line, p.character);
const range = (r) => new vscode.Range(position(r.start), position(r.end));
const location = (l) => new vscode.Location(vscode.Uri.parse(l.uri), range(l.range));
const at = (document, p) => ({
  textDocument: { uri: document.uri.toString() },
  position: { line: p.line, character: p.character },
});

/// The sayon program to run: the one the setting `sayon.path` names, or else
/// the one on the PATH, or else the one Sayon's setup installs, which a
/// VS Code started before the setup cannot find on its PATH.
function sayonProgram() {
  const setting = vscode.workspace.getConfiguration("sayon").get("path");
  if (setting && setting !== "sayon") return setting;
  const names = process.platform === "win32" ? ["sayon.exe", "sayon.cmd"] : ["sayon"];
  const onPath = (process.env.PATH ?? "")
    .split(path.delimiter)
    .filter(Boolean)
    .some((dir) => names.some((name) => fs.existsSync(path.join(dir, name))));
  if (onPath) return "sayon";
  const installed =
    process.platform === "win32"
      ? [path.join(process.env.LOCALAPPDATA ?? "", "Programs", "Sayon", "bin", "sayon.exe")]
      : [path.join(os.homedir(), ".local", "share", "sayon", "bin", "sayon"), path.join(os.homedir(), ".local", "bin", "sayon")];
  return installed.find((candidate) => fs.existsSync(candidate)) ?? "sayon";
}

class Client {
  constructor(output) {
    this.output = output;
    this.pending = new Map();
    this.nextId = 0;
    this.buffer = Buffer.alloc(0);
    this.diagnostics = vscode.languages.createDiagnosticCollection("sayon");
    this.subscriptions = [this.diagnostics];
    this.opened = new Set();
    this.stopped = false;
  }

  /// Starts `sayon lsp`.
  start() {
    const program = sayonProgram();
    this.output.appendLine(`Starting ${program} lsp`);
    this.process = spawn(program, ["lsp"], { stdio: ["pipe", "pipe", "pipe"] });
    this.process.on("error", (problem) => {
      const message =
        `Sayon's language server could not start (${problem.message}). ` +
        "Install Sayon, or set `sayon.path` to the sayon program.";
      this.output.appendLine(message);
      vscode.window.showErrorMessage(message);
      for (const { reject } of this.pending.values()) reject(problem);
      this.pending.clear();
    });
    // Writing to a server that never started, or stopped, fails quietly.
    this.process.stdin.on("error", () => {});
    this.process.on("exit", (code) => {
      for (const { reject } of this.pending.values()) reject(new Error("the server stopped"));
      this.pending.clear();
      if (!this.stopped && code !== 0) {
        this.output.appendLine(`sayon lsp stopped with status ${code}`);
      }
    });
    this.process.stdout.on("data", (bytes) => this.receive(bytes));
    this.process.stderr.on("data", (bytes) => this.output.append(bytes.toString()));
    const root = vscode.workspace.workspaceFolders?.[0]?.uri.toString() ?? null;
    this.ready = this.request("initialize", {
      processId: process.pid,
      rootUri: root,
      capabilities: {
        general: { positionEncodings: ["utf-16"] },
        textDocument: { publishDiagnostics: { relatedInformation: true } },
      },
    }).then(() => {
      this.notify("initialized", {});
      for (const document of vscode.workspace.textDocuments) this.open(document);
    });
    this.watch();
    this.provide();
  }

  stop() {
    this.stopped = true;
    for (const subscription of this.subscriptions) subscription.dispose();
    if (!this.process || this.process.exitCode !== null) return undefined;
    return this.request("shutdown", null)
      .catch(() => {})
      .then(() => {
        this.notify("exit", null);
        this.process.stdin.end();
      });
  }

  send(message) {
    const body = Buffer.from(JSON.stringify({ jsonrpc: "2.0", ...message }), "utf8");
    this.process.stdin.write(`Content-Length: ${body.length}\r\n\r\n`);
    this.process.stdin.write(body);
  }

  notify(method, params) {
    if (this.process?.exitCode === null) this.send({ method, params });
  }

  request(method, params) {
    const id = ++this.nextId;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.send({ id, method, params });
    });
  }

  /// Takes the server's messages out of what it wrote: headers, a blank
  /// line, and as many bytes of JSON as the length says.
  receive(bytes) {
    this.buffer = Buffer.concat([this.buffer, bytes]);
    for (;;) {
      const end = this.buffer.indexOf("\r\n\r\n");
      if (end < 0) return;
      const headers = this.buffer.subarray(0, end).toString("ascii");
      const length = Number(/content-length:\s*(\d+)/i.exec(headers)?.[1]);
      const start = end + 4;
      if (this.buffer.length < start + length) return;
      const body = this.buffer.subarray(start, start + length).toString("utf8");
      this.buffer = this.buffer.subarray(start + length);
      this.dispatch(JSON.parse(body));
    }
  }

  dispatch(message) {
    if (message.id !== undefined && !message.method) {
      const waiting = this.pending.get(message.id);
      this.pending.delete(message.id);
      if (!waiting) return;
      if (message.error) waiting.reject(new Error(message.error.message));
      else waiting.resolve(message.result);
      return;
    }
    if (message.method === "textDocument/publishDiagnostics") {
      const { uri, diagnostics } = message.params;
      this.diagnostics.set(vscode.Uri.parse(uri), diagnostics.map((d) => this.diagnostic(d)));
    } else if (message.method === "window/logMessage") {
      this.output.appendLine(message.params.message);
    }
  }

  diagnostic(d) {
    const severity = d.severity === 1 ? vscode.DiagnosticSeverity.Error : vscode.DiagnosticSeverity.Warning;
    const shown = new vscode.Diagnostic(range(d.range), d.message, severity);
    shown.code = d.code;
    shown.source = d.source;
    shown.relatedInformation = (d.relatedInformation ?? []).map(
      (r) => new vscode.DiagnosticRelatedInformation(location(r.location), r.message),
    );
    return shown;
  }

  open(document) {
    if (document.languageId !== "sayon" || document.uri.scheme !== "file") return;
    const uri = document.uri.toString();
    if (this.opened.has(uri)) return;
    this.opened.add(uri);
    this.notify("textDocument/didOpen", {
      textDocument: { uri, languageId: "sayon", version: document.version, text: document.getText() },
    });
  }

  /// Keeps the server's copies of the open documents the editor's.
  watch() {
    this.subscriptions.push(
      vscode.workspace.onDidOpenTextDocument((document) => this.ready.then(() => this.open(document))),
      vscode.workspace.onDidChangeTextDocument(({ document }) => {
        const uri = document.uri.toString();
        if (!this.opened.has(uri)) return;
        this.ready.then(() =>
          this.notify("textDocument/didChange", {
            textDocument: { uri, version: document.version },
            contentChanges: [{ text: document.getText() }],
          }),
        );
      }),
      vscode.workspace.onDidCloseTextDocument((document) => {
        const uri = document.uri.toString();
        if (!this.opened.delete(uri)) return;
        this.ready.then(() => this.notify("textDocument/didClose", { textDocument: { uri } }));
        this.diagnostics.delete(document.uri);
      }),
    );
  }

  /// A request once the server is ready; nothing when it fails.
  ask(method, params) {
    return this.ready.then(() => this.request(method, params)).catch(() => null);
  }

  provide() {
    const languages = vscode.languages;
    this.subscriptions.push(
      languages.registerHoverProvider(SELECTOR, {
        provideHover: async (document, p) => {
          const hover = await this.ask("textDocument/hover", at(document, p));
          if (!hover) return null;
          return new vscode.Hover(new vscode.MarkdownString(hover.contents.value), hover.range && range(hover.range));
        },
      }),
      languages.registerDefinitionProvider(SELECTOR, {
        provideDefinition: async (document, p) => {
          const found = await this.ask("textDocument/definition", at(document, p));
          return found ? location(found) : null;
        },
      }),
      languages.registerReferenceProvider(SELECTOR, {
        provideReferences: async (document, p, context) => {
          const found = await this.ask("textDocument/references", {
            ...at(document, p),
            context: { includeDeclaration: context.includeDeclaration },
          });
          return (found ?? []).map(location);
        },
      }),
      languages.registerRenameProvider(SELECTOR, {
        // A name that cannot be renamed is refused with the server's reason.
        prepareRename: async (document, p) => {
          await this.ready;
          const prepared = await this.request("textDocument/prepareRename", at(document, p));
          return { range: range(prepared.range), placeholder: prepared.placeholder };
        },
        provideRenameEdits: async (document, p, newName) => {
          await this.ready;
          const renamed = await this.request("textDocument/rename", { ...at(document, p), newName });
          const edit = new vscode.WorkspaceEdit();
          for (const [uri, edits] of Object.entries(renamed.changes ?? {})) {
            for (const e of edits) edit.replace(vscode.Uri.parse(uri), range(e.range), e.newText);
          }
          return edit;
        },
      }),
      languages.registerCompletionItemProvider(
        SELECTOR,
        {
          provideCompletionItems: async (document, p) => {
            const list = await this.ask("textDocument/completion", at(document, p));
            return (list?.items ?? []).map((i) => {
              const item = new vscode.CompletionItem(i.label, completionKind(i.kind));
              item.detail = i.detail;
              return item;
            });
          },
        },
        ".",
      ),
      languages.registerDocumentFormattingEditProvider(SELECTOR, {
        provideDocumentFormattingEdits: async (document) => {
          const edits = await this.ask("textDocument/formatting", {
            textDocument: { uri: document.uri.toString() },
            options: { tabSize: 4, insertSpaces: true },
          });
          return (edits ?? []).map((e) => vscode.TextEdit.replace(range(e.range), e.newText));
        },
      }),
    );
  }
}

let client;

function activate(context) {
  const output = vscode.window.createOutputChannel("Sayon");
  context.subscriptions.push(output);
  client = new Client(output);
  client.start();
  const restart = async () => {
    await client.stop();
    client = new Client(output);
    client.start();
  };
  context.subscriptions.push(
    vscode.commands.registerCommand("sayon.restartServer", restart),
    // Another sayon program takes over at once.
    vscode.workspace.onDidChangeConfiguration((change) => {
      if (change.affectsConfiguration("sayon.path")) restart();
    }),
  );
}

function deactivate() {
  return client?.stop();
}

module.exports = { activate, deactivate };
