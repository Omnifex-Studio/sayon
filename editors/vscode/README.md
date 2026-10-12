# Sayon for Visual Studio Code

Editing support for the [Sayon](https://github.com/Omnifex-Studio/sayon) programming language:

- Syntax highlighting, including expressions inside string interpolation
- The Sayon icon for `.sayon` files
- Comment toggling (`#` for lines, `##` for blocks), bracket matching, and auto-closing pairs
- Indentation after block headers, with four spaces and LF line endings by default

## Installing from the repository

From this directory, package and install the extension with the `vsce` tool:

```sh
npx @vscode/vsce package
code --install-extension sayon-0.0.1.vsix
```

Or copy this directory into your VS Code extensions folder (`~/.vscode/extensions/sayon`) and restart VS Code.
