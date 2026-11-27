# Dominion

<img src="assets/dominion.png" alt="Dominion logo" width="96" height="96">

Dominion is a system resource monitor that lets you be the dictator of your own computer’s resources. Chrome gets 1 GB of RAM because you said so. That part is the plan. Today Dominion monitors processes and shows where your disk space went.

- **Processes** shows live CPU, memory, and disk I/O, with a filter by name or PID.
- **Storage** lists mounted file systems and ranks folders and files by size. Completed scans are saved to SQLite and restored when you reopen Dominion. Start another scan when you need fresh results.

## Install on Linux

Releases support x86-64 Linux with glibc 2.36 or newer and an X11 or Wayland desktop. Sign in with a GitHub account that can access this repository.

```sh
gh auth login
gh release download --repo alexander-vox/Dominion --pattern install.sh
sh install.sh --desktop
```

The installer places `dominion` in `~/.local/bin`. Use `--desktop` to add the app-menu shortcut and icon. Run `dominion` to open the app.

### Update and edge builds

```sh
dominion --update           # Keep the installed channel
dominion --update --edge    # Switch to edge
dominion --update --stable  # Switch to stable
```

For a first-time edge install, use `sh install.sh --edge --desktop`. The script also accepts `--update` for an existing install in `~/.local/bin`. Edge is a rolling prerelease channel.

### Cargo Binstall

The tagged stable build is also available through Cargo Binstall. Use the GitHub repository as the manifest source.

```sh
gh auth setup-git
cargo binstall --git https://github.com/alexander-vox/Dominion.git dominion
dominion --desktop
```

Cargo Binstall uses your GitHub login for the private release asset. Repeat the Cargo Binstall command to update a Cargo-managed installation, so its package metadata stays in sync. Use the shell installer for the rolling edge channel.

## Build from source

```sh
cargo run --locked
```
