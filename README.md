<h1 align="center">ivyTerm - Linux terminal with Tmux control mode integration</h1>

<p align="center">
  <img alt="Alacritty - A fast, cross-platform, OpenGL terminal emulator"
       src="data/ivyterm_screenshot.png">
</p>

## About
ivyTerm is a terminal emulator written in `gtk4-rs` with Tmux control mode integration. Is is created in the spirit of Terminator terminal, but it also lets you use local and remote Tmux sessions directly from the terminal, over any transport that can carry a byte stream: SSH, [Eternal Terminal](https://eternalterminal.dev/), container runtimes (e.g. distrobox), or combinations thereof. Instead of having to configure Tmux on each remote host, ivyTerm will use Tmux's control mode to forward all keyboard input and keybindings to the remote Tmux session. In theory, you should notice no real difference between a local terminal session and a remote terminal session running through Tmux.

This project is a hobby of mine and is not intended to have many different features (like WezTerm, Kitty, etc). Instead the idea is to have simple and robust Tmux integration.

## Usage
Running `ivyterm` without arguments opens a window with a normal (local) terminal.

### Attaching to Tmux
```
ivyterm attach <command...>
```
Runs `<command...>` and speaks Tmux control mode to it. The command is executed as-is, without any parsing or wrapping, and is responsible for starting Tmux in control mode itself (`-2 -C new-session ...`). This keeps ivyTerm agnostic of the transport — anything that ends up running Tmux works:

```sh
# Local session
ivyterm attach tmux -2 -C new-session -A -s main

# Remote session over SSH
ivyterm attach ssh host tmux -2 -C new-session -A -s main

# Remote session over Eternal Terminal (note -CC, see below)
ivyterm attach et host -c 'tmux -2 -CC new-session -A -s main'

# Remote session inside a distrobox container
ivyterm attach ssh host distrobox enter arch -- tmux -2 -C new-session -A -s main
```

How the subprocess is run:
* The command runs on a local pty in raw mode rather than on pipes. Some transports (Eternal Terminal) require a real terminal and silently discard piped stdin.
* `TERM` is set to `xterm-256color` for the spawned command. Transports with a remote pty forward it to the target, which may not have the terminfo entry of the terminal ivyTerm was started from.
* The control mode parser tolerates `\r\n` line endings and discards everything the transport prints before the first `%begin`. Transports that run the command through a remote login shell (which prints a prompt and echoes the command first) therefore work fine.

### Eternal Terminal
`et` survives IP roaming and reconnects, but it has no equivalent of SSH's exec channel: `et -c <command>` types the command into a remote login shell running on a pty. Two things follow:
* Use `-CC` instead of `-C`, so Tmux disables terminal echo on that remote pty. Otherwise every control mode command ivyTerm sends would be echoed back into the output stream.
* The shell prompt and command echo preceding Tmux startup are discarded by the parser (see above); this is expected noise, not an error.

### ivysel session picker
`scripts/ivysel` is a small fzf-based picker: it opens a scratch terminal window (alacritty, kitty, foot, konsole or xterm — whichever is found first) listing the Tmux sessions on the target. Selecting an entry attaches to it in a new ivyTerm window; typing a new name creates that session. Requires `fzf`.

```sh
ivysel                                    # local sessions
ivysel -s host                            # sessions on host, attach over ssh
ivysel -e host                            # sessions on host, attach over et
                                          # (listing still happens over ssh)
ivysel -c "distrobox enter arch -- tmux"  # custom tmux command, combinable with -s/-e
```

## Planned features
* Some missing QoL features
* Windows support

## Installation
Dependencies (GTK 4, libadwaita and VTE)
```
# Ubuntu/Debian
sudo apt install libgtk-4-dev build-essential
sudo apt install libvte-2.91-gtk4-dev
sudo apt install libadwaita-1-dev
# Fedora
sudo yum install gtk4-devel
sudo yum install libadwaita-devel
sudo yum install vte291-gtk4-devel
```
Build ivyTerm
```
# Build
cargo build --release
# Run
cargo run --release
```
