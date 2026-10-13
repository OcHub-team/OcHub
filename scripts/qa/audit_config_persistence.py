#!/usr/bin/env python3
"""Exercise config failure paths using a built ochcli and disposable fixtures.

Run: python3 scripts/qa/audit_config_persistence.py --binary target/debug/ochcli
Fault injection and writes use temporary homes and synthetic credentials.
This is not an OS sandbox; the audit report records path-isolation limits.
Each result compares persisted state with an explicit safety invariant.
"""

import argparse
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import tempfile


class Fixture:
    def __init__(self, binary):
        self.binary = str(binary)
        self.temp = tempfile.TemporaryDirectory(prefix="ochub-config-audit-")
        self.home = Path(self.temp.name)
        self.env = os.environ.copy()
        for key in ("OCHUB_DATA_DIR", "OCHUB_SOCKET", "OCHUB_LANG"):
            self.env.pop(key, None)
        self.env["OCHUB_TEST_HOME"] = str(self.home)
        self.env["XDG_DATA_HOME"] = str(self.home / ".local/share")

    def close(self):
        for path in self.home.rglob("*"):
            if path.is_dir():
                path.chmod(0o700)
        self.temp.cleanup()

    def run(self, *args, ok=True):
        result = subprocess.run(
            [self.binary, "--offline", "--direct", "--json", *args],
            env=self.env,
            capture_output=True,
            text=True,
            timeout=30,
        )
        payload = json.loads(result.stdout) if result.stdout.strip() else {}
        if result.returncode and not payload.get("error"):
            try:
                payload["error"] = json.loads(result.stderr)["error"]
            except (json.JSONDecodeError, KeyError):
                payload["error"] = result.stderr.strip()
        if ok and result.returncode:
            raise RuntimeError(f"{args}: {result.returncode}: {payload}: {result.stderr}")
        return result.returncode, payload

    def write_json(self, name, value):
        path = self.home / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(value))
        return path

    def add(self, app, identifier, settings):
        path = self.write_json(identifier + ".json", {
            "id": identifier, "name": identifier, "settingsConfig": settings,
        })
        self.run("provider", "add", "--app", app, "--from", str(path))

    def current(self, app):
        with sqlite3.connect(self.home / ".ochub/ochub.db") as db:
            return [row[0] for row in db.execute(
                "SELECT id FROM providers WHERE app_type=? AND is_current=1", (app,),
            )]

    def provider(self, app, identifier):
        with sqlite3.connect(self.home / ".ochub/ochub.db") as db:
            return json.loads(db.execute(
                "SELECT settings_config FROM providers WHERE app_type=? AND id=?",
                (app, identifier),
            ).fetchone()[0])

    def install(self, identifier, file_specs, mode="switch"):
        manifest = (
            'manifest_version = 1\n[app]\n'
            f'id = "{identifier}"\nname = "Audit {identifier}"\n'
            'accent = "#123456"\nsort_order = 100\n'
            f'mode = "{mode}"\n[app.icon]\nglyph = "A"\n'
            f'[app.config_dir]\ndefault = "~/.{identifier}"\n'
        )
        for spec in file_specs:
            manifest += "\n[[files]]\n"
            manifest += "\n".join(f"{key} = {json.dumps(value)}" for key, value in spec.items())
            manifest += "\n"
        path = self.home / (identifier + ".toml")
        path.write_text(manifest)
        self.run("plugin", "install", str(path))
        return self.home / ("." + identifier)


def claude_settings(identifier):
    return {"env": {"ANTHROPIC_AUTH_TOKEN": "audit-token-" + identifier,
                    "ANTHROPIC_BASE_URL": "https://example.invalid/" + identifier}}


def builtin_switch_success(f):
    for identifier in ("alpha", "beta"):
        f.add("claude", identifier, claude_settings(identifier))
    f.run("provider", "switch", "--app", "claude", "beta", "--on-drift", "revert")
    path = f.home / ".claude/settings.json"
    settings = json.loads((f.home / ".ochub/settings.json").read_text())
    return {
        "invariant": "successful built-in switching keeps DB, local current, and live config consistent",
        "passed": f.current("claude") == ["beta"]
                  and settings.get("currentProviderClaude") == "beta"
                  and json.loads(path.read_text()) == claude_settings("beta"),
    }


def plugin_format_roundtrip(f):
    formats = {"json": "json", "toml": "toml", "yaml": "yaml", "env": "env"}
    root = f.install("audit-roundtrip", [
        {"id": fmt, "path": "config." + ext, "format": fmt, "store_key": fmt}
        for fmt, ext in formats.items()
    ])
    f.add("audit-roundtrip", "alpha", {fmt: {"TOKEN": "audit-only"} for fmt in formats})
    f.run("provider", "switch", "--app", "audit-roundtrip", "alpha", "--on-drift", "revert")
    _, preview = f.run("provider", "preview", "--app", "audit-roundtrip", "alpha")
    return {
        "invariant": "switch-mode plugins write and read back JSON/TOML/YAML/env without drift",
        "passed": not preview["data"]["wouldChange"]
                  and all((root / ("config." + ext)).is_file() for ext in formats.values()),
    }


def switch_write_failure(f):
    for identifier in ("alpha", "beta"):
        f.add("claude", identifier, claude_settings(identifier))
    f.run("provider", "switch", "--app", "claude", "alpha", "--on-drift", "revert")
    path = f.home / ".claude/settings.json"
    before = path.read_bytes()
    path.parent.chmod(0o500)
    try:
        code, payload = f.run("provider", "switch", "--app", "claude", "beta",
                              "--on-drift", "revert", ok=False)
    finally:
        path.parent.chmod(0o700)
    current = f.current("claude")
    local_current = json.loads((f.home / ".ochub/settings.json").read_text()).get("currentProviderClaude")
    return {
        "invariant": "failed switch keeps old current provider and live bytes",
        "passed": code != 0 and current == ["alpha"] and local_current == "alpha" and path.read_bytes() == before,
        "exitCode": code, "dbCurrent": current, "settingsCurrent": local_current,
        "liveUnchanged": path.read_bytes() == before, "error": payload.get("error"),
    }


def update_write_failure(f):
    f.add("claude", "alpha", claude_settings("alpha"))
    f.run("provider", "switch", "--app", "claude", "alpha", "--on-drift", "revert")
    path = f.home / ".claude/settings.json"
    before = path.read_bytes()
    old_provider = f.provider("claude", "alpha")
    patch = f.write_json("patch.json", {"settingsConfig": claude_settings("new")})
    path.parent.chmod(0o500)
    try:
        code, payload = f.run("provider", "edit", "--app", "claude", "alpha",
                              "--patch", str(patch), ok=False)
    finally:
        path.parent.chmod(0o700)
    changed = f.provider("claude", "alpha") != old_provider
    return {
        "invariant": "failed active-provider edit leaves DB and live state unchanged",
        "passed": code != 0 and not changed and path.read_bytes() == before,
        "exitCode": code, "dbChanged": changed,
        "liveUnchanged": path.read_bytes() == before, "error": payload.get("error"),
    }


def common_snippet_write_failure(f):
    f.add("claude", "alpha", claude_settings("alpha"))
    f.run("provider", "switch", "--app", "claude", "alpha", "--on-drift", "revert")
    _, old = f.run("config", "common", "get", "--app", "claude")
    path = f.home / ".claude/settings.json"
    before = path.read_bytes()
    source = f.write_json("snippet.json", {"alwaysThinkingEnabled": True})
    path.parent.chmod(0o500)
    try:
        code, payload = f.run("config", "common", "set", "--app", "claude",
                              "--from", str(source), ok=False)
    finally:
        path.parent.chmod(0o700)
    _, after = f.run("config", "common", "get", "--app", "claude")
    return {
        "invariant": "failed common-snippet save leaves the stored snippet and live file unchanged",
        "passed": code != 0 and old["data"] == after["data"] and path.read_bytes() == before,
        "exitCode": code, "dbSnippetChanged": old["data"] != after["data"],
        "liveUnchanged": path.read_bytes() == before, "error": payload.get("error"),
    }


def damaged_settings(f):
    original = '{"themeMode":"dark","language":"ja",\n'
    path = f.home / ".ochub/settings.json"
    path.parent.mkdir()
    path.write_text(original)
    code, payload = f.run("settings", "set", "showInTray", "false", ok=False)
    preserved = path.read_text() == original
    backups = [str(p.relative_to(f.home)) for p in f.home.rglob("*")
               if p.is_file() and p != path and p.read_bytes() == original.encode()]
    return {
        "invariant": "an unreadable settings document is retained or backed up before any default overwrite",
        "passed": preserved or bool(backups), "exitCode": code,
        "originalPreserved": preserved, "originalBackups": backups,
        "error": payload.get("error"),
    }


def plugin_multifile_failure(f):
    root = f.install("audit-two", [
        {"id": "first", "path": "first.json", "format": "json", "store_key": "first"},
        {"id": "second", "path": "locked/second.json", "format": "json", "store_key": "second"},
    ])
    for identifier in ("alpha", "beta"):
        f.add("audit-two", identifier, {"first": {"value": identifier}, "second": {"value": identifier}})
    f.run("provider", "switch", "--app", "audit-two", "alpha", "--on-drift", "revert")
    (root / "locked").chmod(0o500)
    try:
        code, payload = f.run("provider", "switch", "--app", "audit-two", "beta",
                              "--on-drift", "revert", ok=False)
    finally:
        (root / "locked").chmod(0o700)
    first = json.loads((root / "first.json").read_text())["value"]
    second = json.loads((root / "locked/second.json").read_text())["value"]
    return {
        "invariant": "a failed multi-file plugin switch restores every file",
        "passed": code != 0 and first == second == "alpha",
        "exitCode": code, "first": first, "second": second,
        "dbCurrent": f.current("audit-two"), "error": payload.get("error"),
    }


def plugin_replace_absent(f):
    root = f.install("audit-absent", [{
        "id": "config", "path": "config.json", "format": "json", "store_key": "config",
        "write": "replace", "absent_preserves": True,
    }])
    f.add("audit-absent", "alpha", {"config": {"keep": "value"}})
    f.add("audit-absent", "beta", {})
    f.run("provider", "switch", "--app", "audit-absent", "alpha", "--on-drift", "revert")
    before = (root / "config.json").read_bytes()
    f.run("provider", "switch", "--app", "audit-absent", "beta", "--on-drift", "revert")
    return {
        "invariant": "absent_preserves also preserves absent replace-mode stores",
        "passed": (root / "config.json").read_bytes() == before,
        "after": json.loads((root / "config.json").read_text()),
    }


def plugin_json_permissions(f):
    root = f.install("audit-perms", [{
        "id": "config", "path": "config.json", "format": "json", "store_key": "config",
        "write": "merge_shallow", "dir_mode": "0700", "file_mode": "0600",
    }])
    root.mkdir(parents=True, exist_ok=True)
    root.chmod(0o755)
    (root / "config.json").write_text('{}')
    (root / "config.json").chmod(0o644)
    f.add("audit-perms", "alpha", {"config": {"token": "audit-only"}})
    f.run("provider", "switch", "--app", "audit-perms", "alpha", "--on-drift", "revert")
    file_mode = (root / "config.json").stat().st_mode & 0o777
    dir_mode = root.stat().st_mode & 0o777
    return {
        "invariant": "JSON merge writes honor file_mode and dir_mode from the manifest",
        "passed": file_mode == 0o600 and dir_mode == 0o700,
        "fileMode": oct(file_mode), "dirMode": oct(dir_mode),
    }


def plugin_secret_roundtrip(f):
    f.install("audit-secrets", [{
        "id": "config", "path": "config.json", "format": "json", "store_key": "config",
    }])
    f.add("audit-secrets", "alpha", {"config": {"api_key": "audit-original-secret"}})
    _, shown = f.run("provider", "show", "--app", "audit-secrets", "alpha")
    provider = shown["data"]["provider"]
    assert provider["settingsConfig"]["config"]["api_key"] == "******"
    provider["name"] = "Renamed only"
    source = f.write_json("redacted.json", provider)
    f.run("provider", "edit", "--app", "audit-secrets", "alpha", "--from", str(source))
    stored = f.provider("audit-secrets", "alpha")["config"]["api_key"]
    return {
        "invariant": "editing a redacted plugin provider preserves its existing secret",
        "passed": stored == "audit-original-secret", "storedPlaceholder": stored == "******",
    }


def plugin_additive_semantics(f):
    root = f.install("audit-additive", [{
        "id": "config", "path": "config.json", "format": "json", "store_key": "config",
    }], mode="additive")
    for identifier in ("alpha", "beta"):
        f.add("audit-additive", identifier, {"config": {"providers": {identifier: {"model": "test"}}}})
        f.run("provider", "add-to-live", "--app", "audit-additive", identifier)
    providers = json.loads((root / "config.json").read_text())["providers"]
    code, payload = f.run("provider", "remove-from-live", "--app", "audit-additive", "beta", ok=False)
    return {
        "invariant": "an accepted additive plugin retains both providers and supports removal",
        "passed": set(providers) == {"alpha", "beta"} and code == 0,
        "providersAfterTwoAdds": list(providers), "removeExitCode": code,
        "error": payload.get("error"),
    }


def plugin_path_confinement(f):
    # The escaped file is still inside this fixture's disposable home.
    root = f.install("audit-path", [{
        "id": "config", "path": "../outside-plugin.json", "format": "json", "store_key": "config",
    }])
    f.add("audit-path", "alpha", {"config": {"value": "audit-only"}})
    code, payload = f.run("provider", "switch", "--app", "audit-path", "alpha",
                          "--on-drift", "revert", ok=False)
    escaped = (root.parent / "outside-plugin.json").exists()
    return {
        "invariant": "a declared relative file path cannot escape the plugin config directory",
        "passed": not escaped, "exitCode": code, "escapedFileCreated": escaped,
        "error": payload.get("error"),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/debug/ochcli"))
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if os.name != "posix" or os.geteuid() == 0:
        parser.error("failure injection requires a non-root Unix account")
    binary = args.binary.resolve(strict=True)
    results = {}
    for case in (builtin_switch_success, plugin_format_roundtrip,
                 switch_write_failure, update_write_failure, common_snippet_write_failure, damaged_settings,
                 plugin_multifile_failure, plugin_replace_absent, plugin_json_permissions,
                 plugin_secret_roundtrip, plugin_additive_semantics, plugin_path_confinement):
        fixture = Fixture(binary)
        try:
            results[case.__name__] = case(fixture)
        except Exception as error:
            results[case.__name__] = {"harnessError": str(error), "passed": False}
        finally:
            fixture.close()
    text = json.dumps(results, ensure_ascii=False, indent=2)
    print(text)
    if args.output:
        args.output.write_text(text + "\n")
    return 0 if all(result["passed"] for result in results.values()) else 1


if __name__ == "__main__":
    raise SystemExit(main())
