"""Packages the VS Code extension as a .vsix, as `vsce package` would,
without it: the extension's files under `extension/`, Sayon's license
among them, with the package's manifest and content types beside them.

    python scripts/vscode_package.py [output .vsix]

Writes target/editor/sayon-<version>.vsix by default, for
`code --install-extension`.
"""

import json
import sys
import zipfile
from pathlib import Path
from xml.sax.saxutils import escape

root = Path(__file__).resolve().parent.parent
source = root / "editors" / "vscode"
package = json.loads((source / "package.json").read_text(encoding="utf-8"))
output = Path(sys.argv[1]) if len(sys.argv) > 1 else root / "target" / "editor" / f"sayon-{package['version']}.vsix"

files = [p for p in sorted(source.rglob("*")) if p.is_file() and not p.name.endswith(".vsix")]

manifest = f"""<?xml version="1.0" encoding="utf-8"?>
<PackageManifest Version="2.0.0" xmlns="http://schemas.microsoft.com/developer/vsx-schema/2011" xmlns:d="http://schemas.microsoft.com/developer/vsx-schema-design/2011">
  <Metadata>
    <Identity Language="en-US" Id="{escape(package['name'])}" Version="{escape(package['version'])}" Publisher="{escape(package['publisher'])}" />
    <DisplayName>{escape(package['displayName'])}</DisplayName>
    <Description xml:space="preserve">{escape(package['description'])}</Description>
    <Categories>{escape(','.join(package.get('categories', [])))}</Categories>
    <GalleryFlags>Public</GalleryFlags>
    <Properties>
      <Property Id="Microsoft.VisualStudio.Code.Engine" Value="{escape(package['engines']['vscode'])}" />
      <Property Id="Microsoft.VisualStudio.Code.ExtensionKind" Value="workspace" />
    </Properties>
    <License>extension/LICENSE.txt</License>
    <Icon>extension/{escape(package['icon'])}</Icon>
  </Metadata>
  <Installation>
    <InstallationTarget Id="Microsoft.VisualStudio.Code" />
  </Installation>
  <Dependencies />
  <Assets>
    <Asset Type="Microsoft.VisualStudio.Code.Manifest" Path="extension/package.json" Addressable="true" />
    <Asset Type="Microsoft.VisualStudio.Services.Content.Details" Path="extension/README.md" Addressable="true" />
    <Asset Type="Microsoft.VisualStudio.Services.Content.License" Path="extension/LICENSE.txt" Addressable="true" />
    <Asset Type="Microsoft.VisualStudio.Services.Icons.Default" Path="extension/{escape(package['icon'])}" Addressable="true" />
  </Assets>
</PackageManifest>
"""

types = {".json": "application/json", ".js": "application/javascript", ".png": "image/png",
         ".md": "text/markdown", ".txt": "text/plain", ".vsixmanifest": "text/xml"}
content_types = '<?xml version="1.0" encoding="utf-8"?>\n<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">' + "".join(
    f'<Default Extension="{ext}" ContentType="{kind}" />' for ext, kind in types.items()) + "</Types>\n"

output.parent.mkdir(parents=True, exist_ok=True)
with zipfile.ZipFile(output, "w", zipfile.ZIP_DEFLATED) as archive:
    archive.writestr("[Content_Types].xml", content_types)
    archive.writestr("extension.vsixmanifest", manifest)
    for path in files:
        archive.write(path, "extension/" + path.relative_to(source).as_posix())
    archive.write(root / "LICENSE", "extension/LICENSE.txt")
print(output, len(files) + 3, "entries")
