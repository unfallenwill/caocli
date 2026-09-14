//! A visual review dump: a single test that lays out a session the way a
//! reader would see it and prints what the screen actually drew -- one
//! line of styles and one of text per row. Useful when the shape of the
//! screen is what a change needs to be checked against, not the specific
//! assertion a regular test would make.

use std::time::Duration;

use ratatui::style::{Color, Modifier};

use crate::types::Usage;
use crate::ui::cell::Cell;

use super::super::notice::MachineNotice;
use super::row;
use super::screen_for_test;

#[test]
fn zz_visual_review_dump() {
    let mut screen = screen_for_test(96, 30);
    screen.state.model = Some("deepseek/deepseek-flash".to_owned());
    screen.state.show(Cell::Notice(
        "caocli \u{b7} session 20260910-224129 (12 messages) \u{b7} deepseek/deepseek-flash".into(),
    ));
    screen
        .state
        .view
        .transcript
        .push(Cell::user("why is the build slow?"));
    screen.state.view.transcript.push(Cell::Reasoning(
        "The user asks about build time. I should look at the Cargo profile and maybe check if there are heavy dependencies. Let me start by reading Cargo.toml and then check the target directory size.".into(),
    ));
    screen.state.view.transcript.push(Cell::Content(
        "Two things usually dominate: an unoptimized dev profile and relinking every dependency on each edit. Let me look.".into(),
    ));
    screen.state.view.transcript.push(Cell::from_tool_call(
        "Bash",
        r#"{"command":"ls -la target/debug | head -20"}"#,
    ));
    // Settle the open Bash step in place; its children (the `ls` output)
    // and its verdict (the parsed exit code) are part of the same cell.
    if let Some(Cell::Step(s)) = screen.state.view.transcript.last_mut() {
        s.push_output("total 4823136\ndrwxr-xr-x 12 user user 4096 ...\n");
        s.settle("exit_code: 0");
    }
    screen.state.view.transcript.push(Cell::from_tool_call(
        "Edit",
        r#"{"file_path":"Cargo.toml","old_string":"[profile.dev]\ndebug = 2","new_string":"[profile.dev]\ndebug = 0"}"#,
    ));
    if let Some(Cell::Step(s)) = screen.state.view.transcript.last_mut() {
        s.settle("ok: replaced 1 occurrence; /tmp/Cargo.toml is now 412 bytes");
    }
    screen.state.view.transcript.push(Cell::Content(
        "Setting `debug = 0` alone is usually worth a third of the link time. The other half is the linker: with `lld` the final link stops being the long pole.".into(),
    ));
    screen.state.apply(MachineNotice::Usage(
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
    screen.state.view.transcript.push(Cell::Failure(
        "no API key for DeepSeek: run /login deepseek".into(),
    ));
    screen.state.view.transcript.push(Cell::Interrupted);
    screen
        .state
        .view
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
