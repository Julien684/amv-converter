#!/usr/bin/env python3
"""
build_eopkg.py — Builds a .eopkg for amv-converter in the exact Solus
binary format.

The output is a ZIP containing:
  - metadata.xml
  - files.xml
  - install.tar.xz

No COMAR scripts (pure file package, no install/uninstall hooks needed).
No <RuntimeDependencies>: ffmpeg/ffprobe are checked at startup by the app
itself (GUI dialog). `eopkg install <url>` downloads a single file and
cannot resolve deps against the Solus repo database.
"""
import hashlib
import io
import os
import sys
import tarfile
import zipfile
from datetime import date
from pathlib import Path

# ──────────────────────────────────────────────────────────────────────────────
# Configuration
# ──────────────────────────────────────────────────────────────────────────────
SRC_DIR = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(".")
OUT_DIR = Path(sys.argv[2]) if len(sys.argv) > 2 else SRC_DIR

PACKAGE_NAME   = "amv-converter"
VERSION        = "0.2.0"
RELEASE        = 4
DIST_RELEASE   = 1
ARCH           = "x86_64"
DIST_NAME      = "Solus"
BUILD_HOST     = "solus-local"
HOME_PAGE      = "https://github.com/Julien684/amv-converter"
PACKAGER_NAME  = "Julien"
PACKAGER_EMAIL = "julien_solus@use.startmail.com"
PART_OF        = "audio.video"

# (dest_path, src_rel_path, mode)
FILE_LIST = [
    ("usr/bin/amv-converter",
     "target/release/amv-converter", 0o755),
    ("usr/share/applications/amv-converter.desktop",
     "amv-converter.desktop", 0o644),
    ("usr/share/icons/hicolor/128x128/apps/amv-converter.png",
     "assets/amv-converter-128.png", 0o644),
    ("usr/share/icons/hicolor/48x48/apps/amv-converter.png",
     "assets/amv-converter-48.png", 0o644),
    ("usr/share/icons/hicolor/24x24/apps/amv-converter.png",
     "assets/amv-converter-24.png", 0o644),
    ("usr/share/doc/amv-converter/README.md",
     "README.md", 0o644),
    ("usr/share/doc/amv-converter/assets/amv-converter-128.png",
     "assets/amv-converter-128.png", 0o644),
    ("usr/share/doc/amv-converter/assets/screenshot.png",
     "assets/screenshot.png", 0o644),
    ("usr/share/licenses/amv-converter/LICENSE",
     "LICENSE", 0o644),
    ("usr/share/metainfo/fr.julien68.AmvConverter.metainfo.xml", "fr.julien68.AmvConverter.metainfo.xml", 0o644),
]

RUNTIME_DEPS = []

OUT_FILENAME = f"{PACKAGE_NAME}-{VERSION}-{RELEASE}-{DIST_RELEASE}-{ARCH}.eopkg"

_FR_SUMMARY = ("Convertisseur graphique (Qt6) de vidéos au format AMV "
               "pour petits lecteurs MP3/vidéo")
_FR_DESCRIPTION = """amv-converter est un petit convertisseur graphique (Qt6) qui
transforme des vidéos au format AMV pour les petits lecteurs MP3/vidéo
bon marché (160x120, 128x96, 176x144, 160x128). L'application est
orientée par lots : on ajoute des fichiers ou un dossier entier, on
choisit la qualite et la resolution, puis tout est converti a la suite
avec une barre de progression et le temps restant. Le decodage peut etre
confie au GPU (VAAPI, NVDEC, QSV) avec repli automatique sur le CPU ;
l'encodeur AMV reste sur le CPU, faute d'encodeur materiel. L'audio
francais est selectionne automatiquement s'il est present dans la source.
Le format AMV est un hybride RIFF/AMV/AVI que ffmpeg encode nativement
(codecs amv + adpcm_ima_amv). Le letterbox automatique preserve le ratio
de l'image. Necessite ffmpeg et ffprobe dans le PATH."""


# ──────────────────────────────────────────────────────────────────────────────
# Helpers
# ──────────────────────────────────────────────────────────────────────────────
def sha1_hex(data: bytes) -> str:
    return hashlib.sha1(data).hexdigest()


def file_type_for(path: str) -> str:
    """Return the eopkg file type for a given path (no leading /)."""
    if path.startswith("usr/bin/") or path.startswith("bin/"):
        return "executable"
    if path.startswith("usr/libexec/") or path.startswith("usr/sbin/") or path.startswith("sbin/"):
        return "executable"
    if path.startswith("usr/include/"):
        return "header"
    if path.startswith("usr/lib/"):
        return "library"
    if path.startswith("usr/share/man/"):
        return "man"
    if path.startswith("usr/share/info/"):
        return "info"
    if path.startswith("usr/share/locale/"):
        return "localedata"
    if path.startswith("usr/share/doc/") or path.startswith("usr/share/gtk-doc/") or path.startswith("usr/share/help/"):
        return "doc"
    if path.startswith("usr/lib/pkgconfig/") or path.startswith("usr/lib32/pkgconfig/") or path.startswith("usr/lib64/pkgconfig/"):
        return "data"
    if path.startswith("etc/"):
        return "config"
    return "data"


# ──────────────────────────────────────────────────────────────────────────────
# Build
# ──────────────────────────────────────────────────────────────────────────────
def build():
    out_path = OUT_DIR / OUT_FILENAME
    print(f"Building: {out_path}")

    file_entries = []  # (dest_path, bytes, mode)
    for dest_path, src_rel, mode in FILE_LIST:
        src = SRC_DIR / src_rel
        if not src.exists():
            print(f"  WARNING: source file not found: {src}", file=sys.stderr)
            continue
        data = src.read_bytes()
        file_entries.append((dest_path, data, mode))
        print(f"  + {dest_path}  ({len(data)} bytes, mode {oct(mode)})")

    tar_buf = io.BytesIO()
    with tarfile.open(fileobj=tar_buf, mode="w:xz") as tar:
        for dest_path, data, mode in file_entries:
            info = tarfile.TarInfo(name=dest_path)
            info.size = len(data)
            info.mode = mode
            info.uid = 0
            info.gid = 0
            info.mtime = 0
            tar.addfile(info, io.BytesIO(data))
    install_tar_xz = tar_buf.getvalue()
    print(f"  install.tar.xz: {len(install_tar_xz)} bytes")

    files_parts = ['<Files>']
    for dest_path, data, mode in file_entries:
        ftype = file_type_for(dest_path)
        mode_str = f"{mode:04o}"
        h = sha1_hex(data)
        files_parts.append("  <File>")
        files_parts.append(f"    <Path>{dest_path}</Path>")
        files_parts.append(f"    <Type>{ftype}</Type>")
        files_parts.append(f"    <Size>{len(data)}</Size>")
        files_parts.append(f"    <Uid>0</Uid>")
        files_parts.append(f"    <Gid>0</Gid>")
        files_parts.append(f"    <Mode>{mode_str}</Mode>")
        files_parts.append(f"    <Hash>{h}</Hash>")
        files_parts.append("  </File>")
    files_parts.append("</Files>")
    files_xml = "\n".join(files_parts).encode("utf-8")
    print(f"  files.xml: {len(files_xml)} bytes")

    today = date.today().isoformat()
    installed_size = sum(len(data) for _, data, _ in file_entries)

    deps_xml = ""
    if RUNTIME_DEPS:
        deps_xml = "    <RuntimeDependencies>\n"
        for dep in RUNTIME_DEPS:
            deps_xml += f"        <Dependency>{dep}</Dependency>\n"
        deps_xml += "    </RuntimeDependencies>\n"

    def a(s: str) -> str:
        return s.encode("ascii", "replace").decode("ascii")

    metadata_xml = f"""<PISI>
    <Source>
        <Name>{PACKAGE_NAME}</Name>
        <Homepage>{HOME_PAGE}</Homepage>
        <Packager>
            <Name>{PACKAGER_NAME}</Name>
            <Email>{PACKAGER_EMAIL}</Email>
        </Packager>
    </Source>
    <Package>
        <Name>{PACKAGE_NAME}</Name>
        <Summary xml:lang="en">{a("Graphical Qt6 converter for videos to the AMV format, for small MP3/video players")}</Summary>
        <Description xml:lang="en">{a("""amv-converter is a small graphical Qt6 tool that batch-converts videos
to the AMV format for cheap MP3/video players (160x120, 128x96,
176x144, 160x128). Add files or a whole folder, pick the quality and
the resolution, then convert everything in sequence. The French audio
track is selected automatically when present in the source. The AMV
format is a RIFF/AMV/AVI hybrid that ffmpeg encodes natively (amv +
adpcm_ima_amv codecs). Automatic letterboxing preserves the image
aspect ratio. Requires ffmpeg and ffprobe in the PATH.""")}</Description>
        <Summary xml:lang="fr">{_FR_SUMMARY}</Summary>
        <Description xml:lang="fr">{_FR_DESCRIPTION}</Description>
        <PartOf>{PART_OF}</PartOf>
        <License>GPL-3.0-or-later</License>
{deps_xml}        <History>
            <Update release="3">
                <Date>{today}</Date>
                <Version>{VERSION}</Version>
                <Comment>{PACKAGE_NAME} {VERSION} : capture d'ecran ajoutee au README (embarquee dans la documentation du paquet)</Comment>
                <Name>{PACKAGER_NAME}</Name>
                <Email>{PACKAGER_EMAIL}</Email>
            </Update>
            <Update release="2">
                <Date>{today}</Date>
                <Version>{VERSION}</Version>
                <Comment>{PACKAGE_NAME} {VERSION} : barre de progression et temps restant repares, option de decodage GPU (VAAPI/NVDEC/QSV) avec repli CPU, icone d'application</Comment>
                <Name>{PACKAGER_NAME}</Name>
                <Email>{PACKAGER_EMAIL}</Email>
            </Update>
            <Update release="1">
                <Date>{today}</Date>
                <Version>0.1.0</Version>
                <Comment>{PACKAGE_NAME} 0.1.0 : premier paquet (GUI Qt6, conversion par lots vers AMV)</Comment>
                <Name>{PACKAGER_NAME}</Name>
                <Email>{PACKAGER_EMAIL}</Email>
            </Update>
        </History>
        <BuildHost>{BUILD_HOST}</BuildHost>
        <Distribution>{DIST_NAME}</Distribution>
        <DistributionRelease>{DIST_RELEASE}</DistributionRelease>
        <Architecture>{ARCH}</Architecture>
        <InstalledSize>{installed_size}</InstalledSize>
        <PackageFormat>1.2</PackageFormat>
        <Source>
            <Name>{PACKAGE_NAME}</Name>
            <Homepage>{HOME_PAGE}</Homepage>
            <Packager>
                <Name>{PACKAGER_NAME}</Name>
                <Email>{PACKAGER_EMAIL}</Email>
            </Packager>
        </Source>
    </Package>
</PISI>""".encode("utf-8")
    print(f"  metadata.xml: {len(metadata_xml)} bytes")

    OUT_DIR.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(out_path, "w", compression=zipfile.ZIP_DEFLATED) as zf:
        zf.writestr("metadata.xml", metadata_xml)
        zf.writestr("files.xml", files_xml)
        zf.writestr("install.tar.xz", install_tar_xz)

    size = out_path.stat().st_size
    print(f"\n  ✓ {out_path}  ({size} bytes)")
    print(f"  SHA1: {sha1_hex(out_path.read_bytes())}")
    return str(out_path)


if __name__ == "__main__":
    build()
