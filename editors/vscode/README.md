# Sayon for Visual Studio Code

Editing support for the [Sayon](https://github.com/Omnifex-Studio/sayon) programming language:

- Syntax highlighting, including expressions inside string interpolation
- The Sayon icon for `.sayon` files
- Comment toggling (`#` for lines, `##` for blocks), bracket matching, and auto-closing pairs
- Indentation after block headers, with four spaces and LF line endings by default
- From Sayon's language server, `sayon lsp`: errors and warnings as you type, hover with signatures and doc comments, Go to Definition, Find All References, Rename Symbol, completion, and Format Document

The language server comes with Sayon: the extension runs the program the setting `sayon.path` names, or else `sayon` from the `PATH`, or else the one Sayon's setup installed, as when VS Code was already open while it ran. **Sayon: Restart the Language Server** starts it again, as after updating Sayon. The extension has no dependencies of its own.

## Installing from the repository

From the repository, package the extension and install it:

```sh
python scripts/vscode_package.py
code --install-extension target/editor/sayon-0.0.1.vsix
```

Or, from this directory, with the `vsce` tool:

```sh
npx @vscode/vsce package
code --install-extension sayon-0.0.1.vsix
```

Or copy this directory into your VS Code extensions folder (`~/.vscode/extensions/sayon`) and restart VS Code.
