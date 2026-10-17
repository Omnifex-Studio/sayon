# Installing Sayon

Each release has an installer for each device it supports, named after the operating system and processor it is for:

| Device | Download | State |
|---|---|---|
| Windows, x64 processors (most PCs) | `sayon-<version>-windows-x64-setup.exe` | available |
| Windows, ARM64 processors | `sayon-<version>-windows-arm64-setup.exe` | not yet; arrives with the ARM64 validation in milestone M5 |
| Linux | `sayon-<version>-linux-x64-setup` | not yet; arrives with Linux validation |
| macOS | `sayon-<version>-macos-arm64-setup` | not yet; planned for milestone M7 |

To find your processor on Windows, open Settings > System > About and look at "System type". Downloads are published on the [releases page](https://github.com/Omnifex-Studio/sayon/releases), together with a `SHA256SUMS.txt` file for checking them: `Get-FileHash <file>` in PowerShell prints the checksum to compare. No release has been published yet; see [Building the downloads](#building-the-downloads) to make the installer yourself.

## Windows

Run the installer and answer its question. It installs Sayon for your user account, so it needs no administrator rights, and it:

- puts Sayon in `%LOCALAPPDATA%\Programs\Sayon`;
- adds that folder's `bin` to your `PATH`, so `sayon` works in terminals opened afterwards;
- adds Sayon to Settings > Apps, where it can be uninstalled;
- shows the Sayon icon on `.sayon` files, unless another program already opens them.

At the end it runs `sayon doctor`, which shows whether native builds are ready. Running and testing programs works straight away; `sayon build` and `sayon run --native` also need clang from [LLVM](https://releases.llvm.org).

Running a newer installer replaces the installed version. The new files are unpacked next to the old ones first, so an interrupted installation leaves the previous one working.

### Options

```text
sayon-setup [options]              Install Sayon for the current user
sayon-setup uninstall [options]    Remove Sayon

--prefix <folder>    Install into (or remove from) this folder instead
-y, --yes            Do not ask for confirmation
--no-path            Do not add Sayon to PATH
```

`--prefix` accepts only an empty folder or one that already holds Sayon, so it can never overwrite other files. For an unattended installation, pass `--yes`.

### Uninstalling

Remove Sayon from Settings > Apps, or run `uninstall.exe` in the installation folder. Uninstalling removes the installation folder and undoes each change listed above. Your own programs and projects are not touched.

### Without the installer

The `sayon-<version>-windows-x64.zip` archive holds the same files. Unpack it anywhere, add its `bin` folder to your `PATH`, and run `sayon doctor`.

## Building the downloads

`scripts/package.ps1` builds a release of the compiler and runtime, packs it into the archive, and builds the installer with that archive inside it, for the device it runs on:

```text
powershell -File scripts/package.ps1
```

The results are in `dist/`: the installer, the archive, and `SHA256SUMS.txt`. A plain `cargo build` builds an installer with no files inside, which says so when run.
