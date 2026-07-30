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
ochcli remote probe
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

For an OcHub Desktop Remote Nodes connection:

1. Keep `ochcli` and `ochubd` from the same release in a stable path.
2. Make `ssh -o BatchMode=yes <alias> ochcli remote probe` succeed from the
   desktop computer.
3. Import or add the providers that should be switchable on this machine.
4. Add the SSH alias in **Remote nodes** in OcHub Desktop.

The desktop launches `ochcli remote serve --stdio` itself. Do not run it as a
TCP service and do not add a public listener. Inspect the device-local policy
with:

```sh
ochcli remote policy show
ochcli remote policy validate
```

Full WSL, SSH config, host-key verification, switching, and troubleshooting
instructions are available at
<https://docs.ochub.org/guides/remote-nodes>.

OcHub uses `~/.ochub` by default. Use `--data-dir PATH` or
`OCHUB_DATA_DIR=PATH` to operate on an isolated data directory.
