use application::IvyApplication;
use icon::render_badge_texture;
use libadwaita::{gio, glib};
use libadwaita::prelude::*;

mod application;
mod config;
mod helpers;
mod icon;
mod keyboard;
mod modals;
mod normal_widgets;
mod settings_window;
mod tmux_api;
mod tmux_widgets;

/// Leading `--badge-*` options split from the rest of the arguments
struct ParsedArgs {
    badge_color: Option<String>,
    badge_text: Option<String>,
    /// Everything after the leading options (e.g. `attach <cmd...>`),
    /// excluding the program name
    rest: Vec<String>,
}

/// Splits the leading `--badge-color`/`--badge-text` options (which must
/// precede any `attach` subcommand) from the rest. `args` includes the
/// program name at index 0.
fn parse_args(args: &[String]) -> ParsedArgs {
    let mut badge_color = None;
    let mut badge_text = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--badge-color" if i + 1 < args.len() => {
                badge_color = Some(args[i + 1].clone());
                i += 2;
            }
            "--badge-text" if i + 1 < args.len() => {
                badge_text = Some(args[i + 1].clone());
                i += 2;
            }
            _ => break,
        }
    }

    ParsedArgs {
        badge_color,
        badge_text,
        rest: args[i..].to_vec(),
    }
}

fn main() -> glib::ExitCode {
    // Handle --help before GTK initialization. Only the first argument is
    // considered, as "-h" may be part of the attach command
    let args: Vec<String> = std::env::args().collect();
    if let Some(first) = args.get(1) {
        if first == "--help" || first == "-h" {
            println!("Usage:");
            println!("  ivyterm                       Open a window with a normal terminal");
            println!("  ivyterm attach <command...>   Run <command...> and attach to it as a Tmux");
            println!("                                control mode client. The command must start");
            println!("                                Tmux in control mode itself");
            println!("  ivyterm -h, --help            Show this help message");
            println!();
            println!("Examples:");
            println!("  ivyterm attach tmux -2 -C new-session -A -s main");
            println!("  ivyterm attach ssh host tmux -2 -C new-session -A -s main");
            println!("  ivyterm attach et host -c 'tmux -2 -CC new-session -A -s main'");
            println!();
            println!("With a transport that runs the command through a remote shell/pty");
            println!("(e.g. et), use -CC so Tmux turns off terminal echo");
            println!();
            println!("Window icon (identifies the window at a glance):");
            println!("  --badge-color <COLOR>    Icon background color (e.g. '#c33', 'teal')");
            println!("  --badge-text <TEXT>      Up to 3 characters overlaid on the icon");
            std::process::exit(0);
        }
    }

    env_logger::init();

    let application = IvyApplication::new();
    application.set_flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE);

    // Initialize IvyApplication
    application.connect_startup(|app| {
        app.init_css_provider();
        app.init_keybindings();
        app.init_icon();
    });

    application.connect_command_line(move |app, cmdline| {
        let args: Vec<String> = cmdline
            .arguments()
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();

        // Compose the badge per invocation, not once at startup: with a
        // single application instance, a later `ivyterm --badge-* ...` is
        // forwarded here, and its window must get its own icon
        let parsed = parse_args(&args);
        let icon = render_badge_texture(parsed.badge_color.as_deref(), parsed.badge_text.as_deref());

        let rest = parsed.rest;
        match rest.first().map(|arg| arg.as_str()) {
            Some("attach") => {
                let attach_argv = &rest[1..];
                if attach_argv.is_empty() {
                    eprintln!("Error: attach requires a command, e.g.");
                    eprintln!("  ivyterm attach tmux -2 -C new-session -A -s main");
                    return 1;
                }
                app.new_tmux_window(attach_argv, icon.as_ref());
            }
            Some(arg) => {
                eprintln!("Error: unknown argument '{}'", arg);
                return 1;
            }
            None => {
                app.new_normal_window(icon.as_ref());
            }
        }

        0
    });
    application.run()
}
