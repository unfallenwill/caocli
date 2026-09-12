//! A visual review dump: a single test that lays out a session the way a
//! reader would see it and prints what the screen actually drew -- one
//! line of styles and one of text per row. Useful when the shape of the
//! screen is what a change needs to be checked against, not the specific
//! assertion a regular test would make.

use std::time::Duration;

use ratatui::style::{Color, Modifier};

use crate::types::Usage;
use crate::ui::cell::Cell;

use super::super::notice::Notice;
use super::row;
use super::screen_for_test;

#[test]
fn zz_visual_review_dump() {
    let mut screen = screen_for_test(96, 30);
    screen.state.status.set_model("deepseek/deepseek-flash");
    screen.state.show(Cell::Notice(
        "caocli \u{b7} session 20260910-224129 (12 messages) \u{b7} deepseek/deepseek-flash".into(),
    ));
    screen
        .state
        .transcript
        .push(Cell::user("why is the build slow?"));
    screen.state.transcript.push(Cell::Reasoning(
        "The user asks about build time. I should look at the Cargo profile and maybe check if there are heavy dependencies. Let me start by reading Cargo.toml and then check the target directory size.".into(),
    ));
    screen.state.transcript.push(Cell::Content(
        "Two things usually dominate: an unoptimized dev profile and relinking every dependency on each edit. Let me look.".into(),
    ));
    screen.state.transcript.push(Cell::tool_call(
        "Bash",
        r#"{"command":"ls -la target/debug | head -20"}"#,
    ));
    screen.state.transcript.push(Cell::ToolResult(
        "total 4823136\ndrwxr-xr-x 12 user user 4096 ...\n".into(),
    ));
    screen.state.transcript.push(Cell::tool_call(
        "Edit",
        r#"{"file_path":"Cargo.toml","old_string":"[profile.dev]\ndebug = 2","new_string":"[profile.dev]\ndebug = 0"}"#,
    ));
    screen
        .state
        .transcript
        .push(Cell::ToolResult("edited Cargo.toml\n".into()));
    screen.state.transcript.push(Cell::Content(
        "Setting `debug = 0` alone is usually worth a third of the link time. The other half is the linker: with `lld` the final link stops being the long pole.".into(),
    ));
    screen.state.apply(Notice::Usage(
        Usage {
            prompt_tokens: 12480,
            total_tokens: 12980,
            completion_tokens: 500,
            prompt_cache_hit_tokens: 12000,
            prompt_cache_miss_tokens: 480,
            prompt_tokens_details: None,
        },
        Duration::from_secs(9),
    ));
    screen.state.transcript.push(Cell::Failure(
        "no API key for DeepSeek: run /login deepseek".into(),
    ));
    screen.state.transcript.push(Cell::Interrupted);
    screen
        .state
        .transcript
        .push(Cell::Notice("/help for commands".into()));
    screen.draw().unwrap();
    let buf = screen.terminal.backend().buffer();
    for y in 0..buf.area.height {
        let mut line = String::new();
        for x in 0..buf.area.width {
            let c = &buf[(x, y)];
            let m = c.style().add_modifier;
            let tag = if m.contains(Modifier::DIM) {
                "d"
            } else if c.style().fg == Some(Color::Yellow) {
                "Y"
            } else if c.style().fg == Some(Color::Green) {
                "G"
            } else if c.style().fg == Some(Color::Red) {
                "R"
            } else {
                " "
            };
            line.push_str(tag);
        }
        println!("STYLE {y:02} {line}");
        println!("TEXT  {y:02} |{}|", row(&screen, y));
    }
}
