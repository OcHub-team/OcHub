# OcHub CLI

This archive provides the headless OcHub applications:

- `ochcli` configures and operates OcHub without the desktop GUI.
- `ochubd` is the optional local owner daemon used by `ochcli`.

Keep both binaries in the same directory and add that directory to `PATH`.
On macOS and Linux, make them executable if your archive tool did not preserve
permissions:

```sh
chmod +x ochcli ochubd
```

Confirm the installation:

```sh
ochcli version
ochcli doctor
```

To run the daemon only for the current user:

```sh
ochcli daemon install
ochcli daemon start
ochcli daemon status
```

Use `ochcli daemon uninstall --yes` to remove the user service. Run
`ochcli completion --help` to generate shell completions and `ochcli --help`
for the complete command tree.

OcHub uses `~/.ochub` by default. Use `--data-dir PATH` or
`OCHUB_DATA_DIR=PATH` to operate on an isolated data directory.
