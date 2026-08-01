# OcHub CLI

This archive contains one executable: `ochcli`.

The same executable provides the command-line interface, the SSH Remote Nodes
bridge, and the persistent background owner. You do not need to download or
keep a separate `ochubd` binary.

## Install a managed node

When the machine is already reachable through an SSH alias, OcHub Desktop can
also perform this first installation. Add the node, choose **Install** when the
connection row reports that `ochcli` is missing, and confirm the detected
platform. The desktop downloads and verifies the signed raw executable,
uploads it over the host-key-pinned SSH connection, runs the same managed
installer below without `sudo`, then verifies the resulting remote protocol
and records its stable absolute path.

The manual equivalent is:

On macOS or Linux/WSL, make the downloaded file executable and run the managed
installer:

```sh
chmod +x ochcli
./ochcli node install
```

`node install` copies this release into a user-owned version directory, creates
`~/.local/bin/ochcli`, and installs a launchd or systemd user service when one
is available. On WSL without systemd, OcHub starts the owner in the background;
the next SSH Remote Nodes session starts it again after a WSL restart.

Make sure `~/.local/bin` is in the non-interactive SSH `PATH`, or configure its
absolute path in OcHub Desktop. Confirm the installation:

```sh
ochcli version
ochcli node status
ochcli doctor
ochcli remote probe
```

The lower-level `ochcli daemon ...` commands remain available for custom
process managers. Normal Remote Nodes installations should use `ochcli node
install` so the command and its background owner always switch versions
together.

## Connect from OcHub Desktop

1. Make `ssh -o BatchMode=yes <alias> ochcli remote probe` succeed from the
   desktop computer.
2. Import or add the providers that should be switchable on this machine.
3. Open **Remote nodes** in OcHub Desktop and add the SSH alias.

The desktop launches `ochcli remote serve --stdio` itself. Do not expose a TCP
management listener. Inspect the device-local policy with:

```sh
ochcli remote policy show
ochcli remote policy validate
```

## One-click updates

Initial installation or recovery of a legacy CLI uses the connection-row
**Install** / **Upgrade** action and does not depend on an existing remote
policy—the SSH login itself is the authorization boundary. Once a current node
is running, normal executable updates use the typed remote update capability
described below.

Subsequent remote update installation is intentionally disabled by the default
policy. To allow one-click updates, create or edit `~/.ochub/remote.toml` on
the node:

```toml
schemaVersion = 1
enabled = true
allowWrite = true
allowGatewayLifecycle = true
allowDaemonLifecycle = true
allowSecretsWrite = false
allowBackupRestore = false
allowUpdateInstall = true
```

Reconnect the node after changing the policy. OcHub Desktop first reads its
version and platform, then loads the release manifest; every installable
payload is independently protected by SHA-256 and the compiled release key.

- If the node can reach the exact release asset, it downloads and verifies the
  update directly.
- If it cannot, the desktop downloads and verifies the correct node binary and
  streams it over the existing SSH trust path. The node verifies it again
  before installation.

Activation uses a stable `current` link. OcHub restarts the background owner,
checks the reported version, and rolls back automatically if the health check
fails. A retained version can also be restored manually:

```sh
ochcli --yes node rollback
```

If an SSH connection still names the original bootstrap path, its remote
bridge automatically hands off to the managed `current` executable. Updating
the managed node therefore cannot leave SSH control on an older protocol.

Managed self-update currently supports macOS and Linux, including WSL. Full
WSL, SSH config, host-key verification, provider switching, update, and
troubleshooting instructions are available at
<https://docs.ochub.org/guides/remote-nodes>.

OcHub uses `~/.ochub` by default. Use `--data-dir PATH` or
`OCHUB_DATA_DIR=PATH` to operate on an isolated data directory.
