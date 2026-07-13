use application::IvyApplication;
use libadwaita::{gio, glib};
use libadwaita::prelude::*;

mod application;
mod config;
mod helpers;
mod keyboard;
mod modals;
mod normal_widgets;
mod settings_window;
mod tmux_api;
mod tmux_widgets;

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
    });

    application.connect_command_line(move |app, cmdline| {
        let args: Vec<String> = cmdline
            .arguments()
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();

        match args.get(1).map(|arg| arg.as_str()) {
            Some("attach") => {
                let attach_argv = &args[2..];
                if attach_argv.is_empty() {
                    eprintln!("Error: attach requires a command, e.g.");
                    eprintln!("  ivyterm attach tmux -2 -C new-session -A -s main");
                    return 1;
                }
                app.new_tmux_window(attach_argv);
            }
            Some(arg) => {
                eprintln!("Error: unknown argument '{}'", arg);
                return 1;
            }
            None => {
                app.new_normal_window();
            }
        }

        0
    });
    application.run()
}
