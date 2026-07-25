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
palette = ["ctrl-`", "ctrl-space"]
redraw = ["f5"]
next_screen = []
previous_screen = []
jump_modifiers = []

[bindings.shell]
detach = ["ctrl-d"]
next_screen = ["tab"]
previous_screen = ["backtab"]
help = ["?", "f1", "alt-h"]

[bindings.leader]
palette = ["s"]
next_screen = ["n", "tab", "right"]
previous_screen = ["p", "backtab", "left"]
scroll_up = ["k", "up"]
scroll_down = ["j", "down"]
close = ["x"]
detach = ["d"]
help = ["?", "h"]
jump_modifiers = ["none"]

[bindings.action]
next_screen = ["alt-right"]
previous_screen = ["alt-left"]
scroll_up = ["alt-up"]
scroll_down = ["alt-down"]
close = ["alt-x"]
detach = ["alt-d"]
help = ["alt-?"]
clear_query = ["ctrl-u"]
jump_modifiers = ["alt"]

[bindings.session]
release_driver = ["f2"]
take_driver = ["f3"]
clear = ["ctrl-l", "cmd-k"]
interrupt = ["ctrl-c"]
detach = ["ctrl-d"]
delete_to_start = ["ctrl-u", "cmd-backspace"]
word_left = ["alt-b", "alt-left"]
word_right = ["alt-f", "alt-right"]
line_start = ["ctrl-a", "cmd-left"]
line_end = ["ctrl-e", "cmd-right"]
delete_word = ["ctrl-w", "alt-backspace"]
complete = ["tab"]
scroll_up = ["pageup"]
scroll_down = ["pagedown"]
scroll_top = ["ctrl-home"]
scroll_bottom = ["ctrl-end"]

[bindings.dashboard]
up = ["k"]
down = ["j"]
view = ["v"]
take = ["t"]
search = ["/"]
new = ["n"]
rename = ["r"]
delete = ["x"]
stop = ["!"]
keybindings = ["b"]
close = ["q"]
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
    println!();
    println!("[bindings.shell]");
    print_binding_list("detach", &config.bindings.shell_detach);
    print_binding_list("next_screen", &config.bindings.shell_next_screen);
    print_binding_list("previous_screen", &config.bindings.shell_previous_screen);
    print_binding_list("help", &config.bindings.shell_help);
    println!();
    println!("[bindings.leader]");
    print_binding_list("palette", &config.bindings.leader_palette);
    print_binding_list("next_screen", &config.bindings.leader_next_screen);
    print_binding_list("previous_screen", &config.bindings.leader_previous_screen);
    print_binding_list("scroll_up", &config.bindings.leader_scroll_up);
    print_binding_list("scroll_down", &config.bindings.leader_scroll_down);
    print_binding_list("close", &config.bindings.leader_close);
    print_binding_list("detach", &config.bindings.leader_detach);
    print_binding_list("help", &config.bindings.leader_help);
    print_modifier_list("jump_modifiers", &config.bindings.leader_jump_modifiers);
    println!();
    println!("[bindings.action]");
    print_binding_list("next_screen", &config.bindings.action_next_screen);
    print_binding_list("previous_screen", &config.bindings.action_previous_screen);
    print_binding_list("scroll_up", &config.bindings.action_scroll_up);
    print_binding_list("scroll_down", &config.bindings.action_scroll_down);
    print_binding_list("close", &config.bindings.action_close);
    print_binding_list("detach", &config.bindings.action_detach);
    print_binding_list("help", &config.bindings.action_help);
    print_binding_list("clear_query", &config.bindings.action_clear_query);
    print_modifier_list("jump_modifiers", &config.bindings.action_jump_modifiers);
    println!();
    println!("[bindings.session]");
    print_binding_list("release_driver", &config.bindings.session_release_driver);
    print_binding_list("take_driver", &config.bindings.session_take_driver);
    print_binding_list("clear", &config.bindings.session_clear);
    print_binding_list("interrupt", &config.bindings.session_interrupt);
    print_binding_list("detach", &config.bindings.session_detach);
    print_binding_list("delete_to_start", &config.bindings.session_delete_to_start);
    print_binding_list("word_left", &config.bindings.session_word_left);
    print_binding_list("word_right", &config.bindings.session_word_right);
    print_binding_list("line_start", &config.bindings.session_line_start);
    print_binding_list("line_end", &config.bindings.session_line_end);
    print_binding_list("delete_word", &config.bindings.session_delete_word);
    print_binding_list("complete", &config.bindings.session_complete);
    print_binding_list("scroll_up", &config.bindings.session_scroll_up);
    print_binding_list("scroll_down", &config.bindings.session_scroll_down);
    print_binding_list("scroll_top", &config.bindings.session_scroll_top);
    print_binding_list("scroll_bottom", &config.bindings.session_scroll_bottom);
    println!();
    println!("[bindings.dashboard]");
    print_binding_list("up", &config.bindings.dashboard_up);
    print_binding_list("down", &config.bindings.dashboard_down);
    print_binding_list("view", &config.bindings.dashboard_view);
    print_binding_list("take", &config.bindings.dashboard_take);
    print_binding_list("search", &config.bindings.dashboard_search);
    print_binding_list("new", &config.bindings.dashboard_new);
    print_binding_list("rename", &config.bindings.dashboard_rename);
    print_binding_list("delete", &config.bindings.dashboard_delete);
    print_binding_list("stop", &config.bindings.dashboard_stop);
    print_binding_list("keybindings", &config.bindings.dashboard_keybindings);
    print_binding_list("close", &config.bindings.dashboard_close);
}

fn print_modifier_list(name: &str, modifiers: &[KeyModifiers]) {
    let values = modifiers
        .iter()
        .map(|modifiers| format!("\"{}\"", modifier_label(*modifiers)))
        .collect::<Vec<_>>()
        .join(", ");
    println!("{name} = [{values}]");
}
