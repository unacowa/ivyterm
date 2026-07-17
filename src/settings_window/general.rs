use std::{cell::RefCell, rc::Rc};

use gtk4::{CheckButton, DropDown, Entry, FontButton, Label, PropertyExpression, StringObject};
use libadwaita::{glib, prelude::*, PreferencesGroup, PreferencesPage};

use crate::application::IvyApplication;
use crate::config::{theme_names, GlobalConfig, PredictiveEchoMode};

use super::{create_color_button, create_setting_row};

pub fn create_general_page(
    app: &IvyApplication,
    config: &Rc<RefCell<GlobalConfig>>,
) -> PreferencesPage {
    // Page 1: Color and Font dialogs
    let page = PreferencesPage::builder().title("General").build();

    let terminal_prefs = create_terminal_prefs(app, config);
    page.add(&terminal_prefs);

    // Color scheme
    let standard_colors = create_standard_colors(config);
    page.add(&standard_colors);
    let bright_colors = create_bright_colors(config);
    page.add(&bright_colors);

    // Build info (revision + timestamp) to identify the running binary
    let build_info = create_build_info();
    page.add(&build_info);

    page
}

fn create_build_info() -> PreferencesGroup {
    let build_info = PreferencesGroup::builder().title("Build").build();

    let version = Label::builder()
        .selectable(true)
        .label(env!("CARGO_PKG_VERSION"))
        .build();
    create_setting_row(&build_info, "Version", version);

    let revision = Label::builder()
        .selectable(true)
        .label(env!("IVYTERM_GIT_REVISION"))
        .build();
    create_setting_row(&build_info, "Git revision", revision);

    let build_time = Label::builder()
        .selectable(true)
        .label(env!("IVYTERM_BUILD_TIME"))
        .build();
    create_setting_row(&build_info, "Built at", build_time);

    build_info
}

fn create_terminal_prefs(
    app: &IvyApplication,
    config: &Rc<RefCell<GlobalConfig>>,
) -> PreferencesGroup {
    let borrowed = config.borrow();

    // Color theme: "Custom" (index 0) uses the individual colors below; any
    // other entry is a built-in scheme that overrides them.
    let names = theme_names();
    let mut theme_labels: Vec<&str> = vec!["Custom"];
    theme_labels.extend(names.iter().map(String::as_str));
    let theme = DropDown::from_strings(&theme_labels);
    // With ~170 themes, make the dropdown type-to-search over the entry text.
    theme.set_enable_search(true);
    theme.set_expression(Some(PropertyExpression::new(
        StringObject::static_type(),
        None::<gtk4::Expression>,
        "string",
    )));
    let selected_theme = match &borrowed.terminal.theme {
        Some(name) => names
            .iter()
            .position(|n| n == name)
            .map(|i| (i + 1) as u32)
            .unwrap_or(0),
        None => 0,
    };
    theme.set_selected(selected_theme);
    theme.connect_selected_notify(glib::clone!(
        #[weak]
        config,
        #[weak]
        app,
        move |dropdown| {
            let idx = dropdown.selected();
            // Index 0 is "Custom"; the rest map to theme_names() in order.
            let terminal = {
                let mut borrowed = config.borrow_mut();
                borrowed.terminal.theme = if idx == 0 {
                    None
                } else {
                    theme_names().get((idx - 1) as usize).cloned()
                };
                borrowed.terminal.clone()
            };
            // Apply immediately to all open terminals (live preview).
            app.apply_terminal_config(&terminal);
        }
    ));

    // Font Dialog
    let main_font = FontButton::builder()
        .font_desc(borrowed.terminal.font.as_ref())
        .build();
    main_font.connect_font_desc_notify(glib::clone!(
        #[weak]
        config,
        move |button| {
            let mut borrowed = config.borrow_mut();
            borrowed.terminal.font = button.font_desc().unwrap().into();
        }
    ));

    // Foreground color
    let foreground_color = create_color_button(&borrowed.terminal.foreground);
    foreground_color.connect_rgba_notify(glib::clone!(
        #[weak]
        config,
        move |button| {
            let mut borrowed = config.borrow_mut();
            borrowed.terminal.foreground = button.rgba().into();
        }
    ));

    // Background
    let background_color = create_color_button(&borrowed.terminal.background);
    background_color.connect_rgba_notify(glib::clone!(
        #[weak]
        config,
        move |button| {
            let mut borrowed = config.borrow_mut();
            borrowed.terminal.background = button.rgba().into();
        }
    ));

    // Scroll lines
    let scrollback = format!("{}", borrowed.terminal.scrollback_lines);
    let scrollback = Entry::builder().placeholder_text(&scrollback).build();
    scrollback.connect_has_focus_notify(glib::clone!(
        #[weak]
        config,
        move |scroll_lines| {
            let text = scroll_lines.text();
            if text.is_empty() {
                return;
            }

            if let Ok(new_scrollback) = text.parse::<u32>() {
                let mut borrowed = config.borrow_mut();
                borrowed.terminal.scrollback_lines = new_scrollback;
            }
        }
    ));
    // content.append(&scroll_lines);

    let terminal_bell = CheckButton::builder()
        .active(borrowed.terminal.terminal_bell)
        .build();
    terminal_bell.connect_toggled(glib::clone!(
        #[weak]
        config,
        move |terminal_bell| {
            let mut borrowed = config.borrow_mut();
            borrowed.terminal.terminal_bell = terminal_bell.is_active();
        }
    ));

    // Foreground color
    let split_color = create_color_button(&borrowed.terminal.split_handle_color);
    split_color.connect_rgba_notify(glib::clone!(
        #[weak]
        config,
        move |button| {
            let mut borrowed = config.borrow_mut();
            borrowed.terminal.split_handle_color = button.rgba().into();
        }
    ));

    // Predictive echo (mosh-style local echo in Tmux terminals)
    let predictive_echo = DropDown::from_strings(&["Off", "Auto", "Always"]);
    predictive_echo.set_selected(match borrowed.terminal.predictive_echo {
        PredictiveEchoMode::Off => 0,
        PredictiveEchoMode::Auto => 1,
        PredictiveEchoMode::Always => 2,
    });
    predictive_echo.connect_selected_notify(glib::clone!(
        #[weak]
        config,
        move |dropdown| {
            let mut borrowed = config.borrow_mut();
            borrowed.terminal.predictive_echo = match dropdown.selected() {
                0 => PredictiveEchoMode::Off,
                2 => PredictiveEchoMode::Always,
                _ => PredictiveEchoMode::Auto,
            };
        }
    ));

    // Build the page itself
    let terminal_font_color = PreferencesGroup::builder()
        .title("Terminal font and colors")
        .build();

    create_setting_row(&terminal_font_color, "Color theme", theme);
    create_setting_row(&terminal_font_color, "Terminal font", main_font);
    create_setting_row(&terminal_font_color, "Foreground color", foreground_color);
    create_setting_row(&terminal_font_color, "Background color", background_color);
    create_setting_row(&terminal_font_color, "Scrollback lines", scrollback);
    create_setting_row(&terminal_font_color, "Terminal bell", terminal_bell);
    create_setting_row(&terminal_font_color, "Split handle color", split_color);
    create_setting_row(
        &terminal_font_color,
        "Predictive echo (Tmux)",
        predictive_echo,
    );

    terminal_font_color
}

fn create_standard_colors(config: &Rc<RefCell<GlobalConfig>>) -> PreferencesGroup {
    // Build the page itself
    let standard_colors = PreferencesGroup::builder().title("Standard colors").build();

    let borrowed = config.borrow();

    for (idx, _) in borrowed.terminal.standard_colors.iter().enumerate() {
        let button = create_color_button(&borrowed.terminal.standard_colors[idx]);
        button.connect_rgba_notify(glib::clone!(
            #[weak]
            config,
            move |button| {
                let mut borrowed = config.borrow_mut();
                borrowed.terminal.standard_colors[idx] = button.rgba().into();
            }
        ));

        let name = format!("Standard color {}", idx);
        create_setting_row(&standard_colors, &name, button);
    }

    standard_colors
}

fn create_bright_colors(config: &Rc<RefCell<GlobalConfig>>) -> PreferencesGroup {
    // Build the page itself
    let bright_colors = PreferencesGroup::builder().title("Bright colors").build();

    let borrowed = config.borrow();

    for (idx, _) in borrowed.terminal.bright_colors.iter().enumerate() {
        let button = create_color_button(&borrowed.terminal.bright_colors[idx]);
        button.connect_rgba_notify(glib::clone!(
            #[weak]
            config,
            move |button| {
                let mut borrowed = config.borrow_mut();
                borrowed.terminal.bright_colors[idx] = button.rgba().into();
            }
        ));

        let name = format!("Bright color {}", idx);
        create_setting_row(&bright_colors, &name, button);
    }

    bright_colors
}
