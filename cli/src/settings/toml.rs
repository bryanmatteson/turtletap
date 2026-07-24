//! TOML configuration template and rendering.

use turtletap::ShellConfig;

use super::*;

pub(super) const TOML_TEMPLATE: &str = r#"# TurtleTap settings

[shell]
mouse_capture = false
direct_detach = true
tick_rate_ms = 100

[theme]
chrome = "white"
muted = "dark-gray"
accent = "cyan"
working = "blue"
attention = "yellow"
failed = "red"
complete = "green"

[theme.selected]
foreground = "black"
background = "cyan"

[bindings]
# The first entry is shown in the footer and help. Additional entries are fallbacks.
leaders = ["ctrl-g"]
palette = ["ctrl-`", "ctrl-space", "ctrl-p"]
redraw = ["ctrl-/", "ctrl-_"]
next_screen = []
previous_screen = []
jump_modifiers = []

# Shortcuts active on shell-managed surfaces.
shell_detach = ["ctrl-d"]
shell_next_screen = ["tab"]
shell_previous_screen = ["backtab"]
shell_help = ["?"]

# Keys pressed after the leader.
leader_palette = ["s"]
leader_next_screen = ["n", "tab", "right"]
leader_previous_screen = ["p", "backtab", "left"]
leader_scroll_up = ["k", "up"]
leader_scroll_down = ["j", "down"]
leader_close = ["x"]
leader_detach = ["d"]
leader_help = ["?", "h"]
leader_jump_modifiers = ["none"]

# Accelerators active only while the action bar is open.
action_next_screen = ["alt-right"]
action_previous_screen = ["alt-left"]
action_scroll_up = ["alt-up"]
action_scroll_down = ["alt-down"]
action_close = ["alt-x"]
action_detach = ["alt-d"]
action_help = ["alt-?"]
action_clear_query = ["ctrl-u"]
action_jump_modifiers = ["alt"]

# Resident command-session shortcuts.
session_release_driver = ["f2"]
session_take_driver = ["f3"]
session_clear = ["cmd-k", "ctrl-l"]
session_interrupt = ["ctrl-c"]
session_detach = ["ctrl-d"]
session_delete_to_start = ["ctrl-u", "cmd-backspace"]
session_word_left = ["alt-left", "alt-b"]
session_word_right = ["alt-right", "alt-f"]
session_line_start = ["cmd-left"]
session_line_end = ["cmd-right"]
session_delete_word = ["alt-backspace"]
session_complete = ["tab"]
session_scroll_up = ["pageup"]
session_scroll_down = ["pagedown"]
session_scroll_top = ["ctrl-home"]
session_scroll_bottom = ["ctrl-end"]

# Overview/dashboard action shortcuts.
dashboard_up = ["k"]
dashboard_down = ["j"]
dashboard_view = ["v"]
dashboard_take = ["t"]
dashboard_search = ["/"]
dashboard_new = ["n"]
dashboard_rename = ["r"]
dashboard_delete = ["x"]
dashboard_stop = ["!"]
dashboard_keybindings = ["b"]
dashboard_close = ["q"]
"#;

pub(super) fn print_resolved_toml(config: &ShellConfig) {
    println!("[shell]");
    println!("mouse_capture = {}", config.mouse_capture);
    println!("direct_detach = {}", config.direct_detach);
    println!("tick_rate_ms = {}", config.tick_rate.as_millis());
    let (chrome, width, narrow, min_content) = chrome_fields(config.chrome);
    println!("chrome = \"{chrome}\"");
    if chrome == "rail" {
        println!("rail_width = {width}");
        println!("rail_narrow = {narrow}");
        println!("rail_min_content = {min_content}");
    }
    println!();
    println!("[theme]");
    println!("chrome = \"{}\"", style_foreground(config.theme.chrome));
    println!("muted = \"{}\"", style_foreground(config.theme.muted));
    println!("accent = \"{}\"", style_foreground(config.theme.accent));
    println!("working = \"{}\"", style_foreground(config.theme.working));
    println!(
        "attention = \"{}\"",
        style_foreground(config.theme.attention)
    );
    println!("failed = \"{}\"", style_foreground(config.theme.failed));
    println!("complete = \"{}\"", style_foreground(config.theme.complete));
    println!();
    println!("[theme.selected]");
    println!(
        "foreground = \"{}\"",
        style_foreground(config.theme.selected)
    );
    println!("background = \"{}\"", color_name(config.theme.selected.bg));
    println!();
    println!("[bindings]");
    print_binding_list("leaders", &config.bindings.leaders);
    print_binding_list("palette", &config.bindings.palette);
    print_binding_list("redraw", &config.bindings.redraw);
    print_binding_list("next_screen", &config.bindings.next_screen);
    print_binding_list("previous_screen", &config.bindings.previous_screen);
    print_modifier_list("jump_modifiers", &config.bindings.jump_modifiers);
    print_binding_list("shell_detach", &config.bindings.shell_detach);
    print_binding_list("shell_next_screen", &config.bindings.shell_next_screen);
    print_binding_list(
        "shell_previous_screen",
        &config.bindings.shell_previous_screen,
    );
    print_binding_list("shell_help", &config.bindings.shell_help);
    print_binding_list("leader_palette", &config.bindings.leader_palette);
    print_binding_list("leader_next_screen", &config.bindings.leader_next_screen);
    print_binding_list(
        "leader_previous_screen",
        &config.bindings.leader_previous_screen,
    );
    print_binding_list("leader_scroll_up", &config.bindings.leader_scroll_up);
    print_binding_list("leader_scroll_down", &config.bindings.leader_scroll_down);
    print_binding_list("leader_close", &config.bindings.leader_close);
    print_binding_list("leader_detach", &config.bindings.leader_detach);
    print_binding_list("leader_help", &config.bindings.leader_help);
    print_modifier_list(
        "leader_jump_modifiers",
        &config.bindings.leader_jump_modifiers,
    );
    print_binding_list("action_next_screen", &config.bindings.action_next_screen);
    print_binding_list(
        "action_previous_screen",
        &config.bindings.action_previous_screen,
    );
    print_binding_list("action_scroll_up", &config.bindings.action_scroll_up);
    print_binding_list("action_scroll_down", &config.bindings.action_scroll_down);
    print_binding_list("action_close", &config.bindings.action_close);
    print_binding_list("action_detach", &config.bindings.action_detach);
    print_binding_list("action_help", &config.bindings.action_help);
    print_binding_list("action_clear_query", &config.bindings.action_clear_query);
    print_modifier_list(
        "action_jump_modifiers",
        &config.bindings.action_jump_modifiers,
    );
    print_binding_list(
        "session_release_driver",
        &config.bindings.session_release_driver,
    );
    print_binding_list("session_take_driver", &config.bindings.session_take_driver);
    print_binding_list("session_clear", &config.bindings.session_clear);
    print_binding_list("session_interrupt", &config.bindings.session_interrupt);
    print_binding_list("session_detach", &config.bindings.session_detach);
    print_binding_list(
        "session_delete_to_start",
        &config.bindings.session_delete_to_start,
    );
    print_binding_list("session_word_left", &config.bindings.session_word_left);
    print_binding_list("session_word_right", &config.bindings.session_word_right);
    print_binding_list("session_line_start", &config.bindings.session_line_start);
    print_binding_list("session_line_end", &config.bindings.session_line_end);
    print_binding_list("session_delete_word", &config.bindings.session_delete_word);
    print_binding_list("session_complete", &config.bindings.session_complete);
    print_binding_list("session_scroll_up", &config.bindings.session_scroll_up);
    print_binding_list("session_scroll_down", &config.bindings.session_scroll_down);
    print_binding_list("session_scroll_top", &config.bindings.session_scroll_top);
    print_binding_list(
        "session_scroll_bottom",
        &config.bindings.session_scroll_bottom,
    );
    print_binding_list("dashboard_up", &config.bindings.dashboard_up);
    print_binding_list("dashboard_down", &config.bindings.dashboard_down);
    print_binding_list("dashboard_view", &config.bindings.dashboard_view);
    print_binding_list("dashboard_take", &config.bindings.dashboard_take);
    print_binding_list("dashboard_search", &config.bindings.dashboard_search);
    print_binding_list("dashboard_new", &config.bindings.dashboard_new);
    print_binding_list("dashboard_rename", &config.bindings.dashboard_rename);
    print_binding_list("dashboard_delete", &config.bindings.dashboard_delete);
    print_binding_list("dashboard_stop", &config.bindings.dashboard_stop);
    print_binding_list(
        "dashboard_keybindings",
        &config.bindings.dashboard_keybindings,
    );
    print_binding_list("dashboard_close", &config.bindings.dashboard_close);
}

fn print_modifier_list(name: &str, modifiers: &[KeyModifiers]) {
    let values = modifiers
        .iter()
        .map(|modifiers| format!("\"{}\"", modifier_label(*modifiers)))
        .collect::<Vec<_>>()
        .join(", ");
    println!("{name} = [{values}]");
}
