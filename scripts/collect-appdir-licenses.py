#!/usr/bin/env python3
"""Record native libraries bundled in an AppDir and copy available license texts."""

import json
from pathlib import Path
import shutil
import subprocess
import sys


def command(*args):
    return subprocess.run(args, check=True, capture_output=True, text=True).stdout


def source_libraries(input_dir):
    sources = {}
    for binary in ("onedrive-daemon", "onedrive-desktop"):
        output = command("ldd", str(input_dir / "usr/bin" / binary))
        for line in output.splitlines():
            fields = line.strip().split()
            if len(fields) >= 3 and fields[1] == "=>":
                if fields[2] == "not":
                    raise RuntimeError(f"Unresolved dependency of {binary}: {line.strip()}")
                if fields[2].startswith("/"):
                    sources.setdefault(fields[0], Path(fields[2]))
    return sources


def package_for(path):
    candidates = (path, path.resolve())
    if shutil.which("rpm"):
        for candidate in candidates:
            result = subprocess.run(
                ("rpm", "-qf", "--qf", "%{NAME}", str(candidate)),
                capture_output=True, text=True,
            )
            if result.returncode == 0:
                return "rpm", result.stdout.strip()
    if shutil.which("dpkg-query"):
        for candidate in candidates:
            result = subprocess.run(
                ("dpkg-query", "-S", str(candidate)),
                capture_output=True, text=True,
            )
            if result.returncode == 0:
                # dpkg can return multiarch names such as libfoo:amd64.
                return "dpkg", result.stdout.splitlines()[0].rsplit(": ", 1)[0].split(", ")[0]
    raise RuntimeError(f"No installed package owns {path}")


def license_details(manager, package):
    if manager == "rpm":
        declared = command("rpm", "-q", "--qf", "%{LICENSE}", package).strip()
        files = [Path(name) for name in command("rpm", "-ql", package).splitlines()
                 if name.startswith("/usr/share/licenses/") and Path(name).is_file()]
    else:
        declared = None
        files = [Path(name) for name in command("dpkg-query", "-L", package).splitlines()
                 if name.startswith("/usr/share/doc/") and name.endswith("/copyright")
                 and Path(name).is_file()]
    return declared, files


def collect(input_dir, appdir):
    sources = source_libraries(input_dir)
    library_dir = appdir / "usr/lib"
    libraries = sorted(path for path in library_dir.iterdir()
                       if path.is_file() and ".so" in path.name)
    if not libraries:
        raise RuntimeError("No native libraries were bundled")
    target = appdir / "usr/share/licenses/onedrive-linux"
    records = []
    packages = {}

    def record_package(source):
        manager, package = package_for(source)
        key = (manager, package)
        if key not in packages:
            declared, files = license_details(manager, package)
            copied = []
            for license_file in files:
                relative = license_file.relative_to("/usr/share")
                destination = target / "native" / manager / package / relative
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(license_file, destination)
                copied.append(str(destination.relative_to(appdir)))
            packages[key] = {"manager": manager, "package": package,
                             "declared_license": declared, "license_files": copied}
        return manager, package

    for library in libraries:
        source = sources.get(library.name)
        if source is None:
            raise RuntimeError(f"Cannot identify source of bundled library {library.name}")
        manager, package = record_package(source)
        records.append({"library": library.name, "source": str(source),
                        "package": package, "manager": manager})
    icon_theme = appdir / "usr/share/icons/Adwaita/index.theme"
    resources = []
    if icon_theme.is_file():
        manager, package = record_package(Path("/usr/share/icons/Adwaita/index.theme"))
        resources.append({"resource": "usr/share/icons/Adwaita", "manager": manager,
                          "package": package})
    manifest = {"libraries": records, "resources": resources,
                "packages": sorted(packages.values(),
                key=lambda item: (item["manager"], item["package"]))}
    (target / "native-libraries.json").write_text(
        json.dumps(manifest, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    missing = [item["package"] for item in manifest["packages"] if not item["license_files"]]
    print(f"Recorded {len(records)} libraries from {len(packages)} packages; "
          f"{len(missing)} packages have no local license text")


if __name__ == "__main__":
    if len(sys.argv) != 3:
        sys.exit("Usage: collect-appdir-licenses.py INPUT_APPDIR BUNDLED_APPDIR")
    try:
        collect(Path(sys.argv[1]), Path(sys.argv[2]))
    except (OSError, subprocess.CalledProcessError, RuntimeError) as error:
        sys.exit(f"License collection failed: {error}")
