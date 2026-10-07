#!/usr/bin/env python3
"""Collect dependency notices from the locked arm64 graph and installed Rust.

This maintenance tool does not choose or change Bree's own license. It uses no
network. objc2 declarations below were checked at each crate's recorded source
revision because those published crate archives omit their workspace license.
"""

import argparse
from html.parser import HTMLParser
import json
from pathlib import Path
import re
import subprocess
import sys


OBJC2_REVISIONS = {
    ("block2", "0.6.2"): "b4167b582b2f75f9a1be75495c41b765344fd03c",
    ("dispatch2", "0.3.1"): "8852b424193ca41602281b3d7540d7c8ed51e49a",
    ("objc2", "0.6.5"): "d7d2fa23ceaa5e6096c923b081040e5d81b3b9df",
    ("objc2-app-kit", "0.3.2"): "7b1abfd750a2cacaea71d6a56ecfb83cb7de560b",
    ("objc2-core-foundation", "0.3.2"): "7b1abfd750a2cacaea71d6a56ecfb83cb7de560b",
    ("objc2-encode", "4.1.0"): "8d214f5477365ffcbcbb7de058c86ed9a518efb7",
    ("objc2-foundation", "0.3.2"): "7b1abfd750a2cacaea71d6a56ecfb83cb7de560b",
}

# Verbatim upstream LICENSE.md, identical at the recorded revisions above.
# https://github.com/madsmtm/objc2/blob/<recorded revision>/LICENSE.md
OBJC2_DECLARATION = """# License

The licensing of these crates is a bit complicated:
- The crates `objc2`, `block2`, `objc2-foundation` and `objc2-encode` are
  [currently][#23] licensed under [the MIT license][MIT].
- All other crates are trio-licensed under the [Zlib], [Apache-2.0] or [MIT]
  license, at your option.

Furthermore, the crates are (usually automatically) derived from Apple SDKs,
and that may have implications for licensing, see below for details.

[#23]: https://github.com/madsmtm/objc2/issues/23
[MIT]: https://opensource.org/license/MIT
[Zlib]: https://zlib.net/zlib_license.html
[Apache-2.0]: https://www.apache.org/licenses/LICENSE-2.0


## Apple SDKs

These crates are derived from Apple SDKs shipped with Xcode. You can obtain a
copy of the Xcode license at:

https://www.apple.com/legal/sla/docs/xcode.pdf

Or by typing `xcodebuild -license` in your terminal.

From reading the license, it is unclear whether distributing derived works
such as these crates are allowed?

But in any case, to practically use these crates, you will have to link, and
that only works when you have the correct Xcode SDK available to provide the
required `.tbd` files, which is why we choose to still use the normal SPDX
identifiers in the crates (Xcode is required to use the crates, and when using
Xcode you have already agreed to the Xcode license).
"""


class PlainText(HTMLParser):
    """Preserve notice text and link destinations from Rust's supplied HTML."""

    blocks = {"p", "div", "pre", "li", "h1", "h2", "h3", "details", "summary"}

    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.parts = []
        self.links = []

    def handle_starttag(self, tag, attrs):
        if tag in self.blocks or tag == "br":
            self.parts.append("\n")
        if tag == "a":
            href = dict(attrs).get("href", "")
            self.links.append(href if not href.startswith("#") else "")

    def handle_endtag(self, tag):
        if tag == "a" and self.links:
            href = self.links.pop()
            if href:
                self.parts.append(f" ({href})")
        if tag in self.blocks:
            self.parts.append("\n")

    def handle_data(self, data):
        self.parts.append(data)

    def text(self):
        text = "\n".join(line.rstrip() for line in "".join(self.parts).splitlines())
        return re.sub(r"\n[ \t]*\n(?:[ \t]*\n)+", "\n\n", text).strip() + "\n"


def read(path):
    return path.read_text(encoding="utf-8").rstrip() + "\n"


def license_files(root, declared_file):
    found = []
    if declared_file:
        declared = Path(declared_file)
        if not declared.is_absolute():
            declared = root / declared
        if declared.is_file():
            found.append(declared)
    for path in root.rglob("*"):
        if not path.is_file() or path.suffix.lower() in {".rs", ".c", ".h"}:
            continue
        if path.name.upper().startswith(("LICENSE", "COPYRIGHT", "NOTICE", "COPYING", "UNLICENSE")):
            found.append(path)
    return sorted(set(found), key=lambda path: path.relative_to(root).as_posix())


def render(metadata, sysroot, rust_version):
    nodes = {node["id"] for node in metadata["resolve"]["nodes"]}
    packages = sorted(
        (package for package in metadata["packages"] if package["source"] is not None and package["id"] in nodes),
        key=lambda package: (package["name"], package["version"]),
    )
    rust_docs = sysroot / "share/doc/rust"
    mit = read(rust_docs / "licenses/MIT.txt")
    # Upstream objc2's current license declaration links MIT without an explicit
    # copyright line. Preserve that declaration and separately list the reported
    # authors rather than inventing a year or assigning copyright to an author.
    mit_terms = mit[mit.index("Permission is hereby granted"):]
    chunks = [
        "Bree — Third-party dependency notices\n\n"
        "These notices describe third-party components, not Bree's own license.\n"
        "Target: aarch64-apple-darwin\n"
        f"Resolved dependency packages: {len(packages)}\n"
        "The Cargo target-resolution graph is covered conservatively, including\n"
        "build tooling and transitive entries. This is not a claim that every\n"
        "listed package is linked into the distributed executable.\n"
        "Registry license and notice texts are preserved below. SPDX expressions\n"
        "are those reported by each package.\n\n"
    ]
    for package in packages:
        name, version = package["name"], package["version"]
        license_expression = package.get("license")
        if not license_expression and not package.get("license_file"):
            raise ValueError(f"Missing license declaration: {name} {version}")
        root = Path(package["manifest_path"]).parent
        chunks.append("=" * 78 + f"\n{name} {version}\n")
        chunks.append(f"SPDX: {license_expression or 'see supplied license file'}\n")
        if package.get("repository"):
            chunks.append(f"Upstream: {package['repository']}\n")
        if package.get("authors"):
            chunks.append("Authors reported by Cargo: " + "; ".join(package["authors"]) + "\n")
        files = license_files(root, package.get("license_file"))
        if files:
            for path in files:
                chunks.append(f"\n--- {path.relative_to(root).as_posix()} ---\n\n{read(path)}")
        else:
            revision = OBJC2_REVISIONS.get((name, version))
            if not revision:
                raise ValueError(f"Missing license text: {name} {version}")
            vcs = json.loads(read(root / ".cargo_vcs_info.json"))
            if vcs["git"]["sha1"] != revision:
                raise ValueError(f"Unreviewed upstream revision: {name} {version}")
            chunks.append(f"Source revision: {revision}\n")
            chunks.append(f"License source: https://github.com/madsmtm/objc2/blob/{revision}/LICENSE.md\n")
            chunks.append("Upstream workspace authors: Mads Marquart <mads@marquart.dk>\n")
            chunks.append("The published crate omits the workspace LICENSE.md; its\n"
                          "fixed-revision declaration is reproduced here. MIT is selected\n"
                          "where the declaration offers a license choice.\n")
            chunks.append(f"\n--- Upstream LICENSE.md ---\n\n{OBJC2_DECLARATION}")
            chunks.append(f"\n--- MIT license terms ---\n\n{mit_terms}")
        chunks.append("\n")

    library_notices = rust_docs / "COPYRIGHT-library.html"
    if not library_notices.is_file():
        raise ValueError("Rust toolchain does not supply COPYRIGHT-library.html")
    raw_library_notices = read(library_notices)
    parser = PlainText()
    parser.feed(raw_library_notices)
    chunks.append("=" * 78 + "\nRust Standard Library\n")
    chunks.append(f"Toolchain: {rust_version}\n")
    chunks.append("Upstream: https://github.com/rust-lang/rust\n")
    chunks.append("Source: Rust toolchain share/doc/rust/COPYRIGHT-library.html\n")
    chunks.append("The supplied standard-library notice includes cross-platform\n"
                  "components; not all entries are used by this macOS executable.\n\n")
    chunks.append(parser.text())
    # Include the standard licenses referenced by the in-tree library entries.
    # Dependency-specific texts are already embedded in COPYRIGHT-library.html.
    standard_licenses = ("MIT", "Apache-2.0", "LLVM-exception", "Unicode-3.0", "BSD-2-Clause")
    for name in standard_licenses:
        chunks.append(f"\n--- Rust standard license: {name} ---\n\n")
        chunks.append(read(rust_docs / "licenses" / f"{name}.txt"))
    result = "".join(chunks)
    if "/Users/" in result or re.search(r"(?:^|\s)/home/", result):
        raise ValueError("Local filesystem path found in generated notices")
    return result


def main():
    project = Path(__file__).resolve().parent.parent
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--metadata", type=Path, help="Saved cargo metadata JSON; otherwise resolve locked graph offline")
    parser.add_argument("--output", type=Path, default=project / "THIRD-PARTY-NOTICES.txt")
    parser.add_argument("--check", action="store_true", help="Compare generated notices without writing")
    args = parser.parse_args()
    if args.metadata:
        metadata = json.loads(read(args.metadata))
    else:
        metadata = json.loads(subprocess.check_output(
            ["cargo", "metadata", "--format-version", "1", "--locked", "--offline", "--filter-platform", "aarch64-apple-darwin"],
            cwd=project, text=True,
        ))
    sysroot = Path(subprocess.check_output(["rustc", "--print", "sysroot"], cwd=project, text=True).strip())
    rust_version = subprocess.check_output(["rustc", "--version"], cwd=project, text=True).strip()
    contents = render(metadata, sysroot, rust_version)
    if args.check:
        if not args.output.is_file() or read(args.output) != contents:
            raise ValueError("Third-party notices differ; regenerate for the locked toolchain")
        print("Third-party notices match the locked graph and toolchain.")
    else:
        args.output.write_text(contents, encoding="utf-8")
        print(f"Generated third-party notices: {len(contents.encode('utf-8'))} bytes")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, subprocess.CalledProcessError) as error:
        print(f"Cannot generate third-party notices: {error}", file=sys.stderr)
        sys.exit(1)
