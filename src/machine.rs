//! 机器核心：状态 = 会话日志的折叠，决策 = 纯函数，UI/IO = 无状态执行器。
//!
//! 架构不变量（事件词汇表逐步落地；未落地项只记录在此，不留死代码）：
//!
//! - **机器只做决定，永不执行**：[`next_action`] 读历史输出 [`Action`]，
//!   解释器（`Agent::turn`）执行并把结果写回日志。机器永不 await IO。
//! - **回调只收通知，不回传数据**：`ui::Ui` 是机器的 Notice 通道；
//!   需要结果的操作（如未来的审批门）必须以事件形式回到决策函数。
//! - **状态 = 日志的折叠**：`Session` 是唯一持久状态，任何时刻可由
//!   `Session::load` 重建；内存里不允许存在第二个真相来源。
//!
//! | 词汇 | 状态 | 成员 |
//! |---|---|---|
//! | Input | 已落地（隐式） | UserLine（`turn` 入参）、Delta（SSE 流）、ToolFinished（execute 返回值） |
//! | Command | 未落地 | Cancel / New / Resume / Exit —— 带外，任何状态可达 |
//! | Notice | 已落地 | `ui::Ui` 的六个方法 |
//! | Effect | 未落地 | 目前由解释器直写；出现第二个事件源（审批/取消）时提为显式枚举 |

use std::collections::HashSet;

use crate::types::{Message, Role, ToolCall};

/// 崩溃自愈为未执行的工具调用合成的占位结果。必须是确定性常量：
/// 同一份日志每次 load 都要合成出逐字节相同的历史（前缀缓存依赖）。
pub const INTERRUPTED_RESULT: &str = "error: interrupted before execution; no result was recorded";

/// 下一拍该做什么。由日志折叠得出的机器决策出口。
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// 历史就绪（末尾是 user，或工具结果已齐全），需要发起新的模型子请求。
    CallModel,
    /// 末尾 assistant 声明的 tool_calls 尚有未执行的，执行它（按声明顺序）。
    ExecTool(ToolCall),
    /// 末尾 assistant 已给出最终回答，本回合结束。
    Done,
}

/// 决策函数：读历史，输出下一拍。无 IO、无时钟，表驱动可测。
///
/// 多工具调用的关键：assistant 一次声明 c1、c2 时，历史会以
/// `Assistant(calls) → Tool(c1) → Tool(c2)` 逐条增长。只看末尾消息会把
/// 「末尾是 Tool」误判成结果齐全，漏执行 c2 就发请求（API 400）。
/// 因此必须定位最后一条 assistant，比对它的全部声明与紧随的结果串。
pub fn next_action(messages: &[Message]) -> Option<Action> {
    let last = messages.last()?;
    match last.role {
        Role::User => Some(Action::CallModel),
        Role::Tool | Role::Assistant => {
            let ai = messages.iter().rposition(|m| m.role == Role::Assistant)?;
            let calls = messages[ai].tool_calls.as_deref().unwrap_or_default();
            if calls.is_empty() {
                // 末尾 assistant 无调用 → 回答完毕。（末尾 Tool 却找不到
                // 带调用的 assistant 属于非法历史：与"错误即文本"一致，
                // 停下而不是 panic，让用户看到并处置。）
                return Some(Action::Done);
            }
            let answered: HashSet<&str> = messages[ai + 1..]
                .iter()
                .filter(|m| m.role == Role::Tool)
                .filter_map(|m| m.tool_call_id.as_deref())
                .collect();
            match calls.iter().find(|c| !answered.contains(c.id.as_str())) {
                Some(call) => Some(Action::ExecTool(call.clone())),
                None => Some(Action::CallModel),
            }
        }
        Role::System => None,
    }
}

/// 崩溃自愈：为「声明了 tool_calls 但结果不全」的 assistant 补上占位结果，
/// 使历史重新满足 API 约束（每个 call 恰好一个紧随的 tool 结果）。
/// 只修改传入的内存视图，**不写文件**——日志 append-only 永不重写，
/// 下次 load 会确定性地重新合成同样的内容。
/// 返回补插的消息数。
pub fn heal(messages: &mut Vec<Message>) -> usize {
    let mut inserted = 0;
    let mut i = 0;
    while i < messages.len() {
        if messages[i].tool_calls.is_none() {
            i += 1;
            continue;
        }
        // 定位该 assistant 紧随的 tool 结果串的终点
        let mut j = i + 1;
        while j < messages.len() && messages[j].role == Role::Tool {
            j += 1;
        }
        let answered: HashSet<&str> = messages[i + 1..j]
            .iter()
            .filter_map(|m| m.tool_call_id.as_deref())
            .collect();
        let missing: Vec<String> = messages[i]
            .tool_calls
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter(|c| !answered.contains(c.id.as_str()))
            .map(|c| c.id.clone())
            .collect();
        for (n, id) in missing.iter().enumerate() {
            messages.insert(j + inserted + n, Message::tool(id, INTERRUPTED_RESULT));
        }
        inserted += missing.len();
        i = j + inserted;
    }
    inserted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ToolCall, ToolCallFunction};

    fn assistant(calls: Vec<ToolCall>) -> Message {
        Message {
            role: Role::Assistant,
            content: Some(String::new()),
            reasoning_content: None,
            tool_calls: (!calls.is_empty()).then_some(calls),
            tool_call_id: None,
        }
    }

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            r#type: "function".into(),
            function: ToolCallFunction {
                name: "Bash".into(),
                arguments: "{}".into(),
            },
        }
    }

    #[test]
    fn empty_history_has_no_action() {
        assert_eq!(next_action(&[]), None);
    }

    #[test]
    fn user_tail_requests_model() {
        let msgs = vec![Message::user("问")];
        assert_eq!(next_action(&msgs), Some(Action::CallModel));
    }

    #[test]
    fn plain_assistant_tail_ends_turn() {
        let mut m = assistant(vec![]);
        m.content = Some("答案".into());
        let msgs = vec![Message::user("问"), m];
        assert_eq!(next_action(&msgs), Some(Action::Done));
    }

    #[test]
    fn declared_calls_execute_in_declaration_order() {
        let msgs = vec![Message::user("问"), assistant(vec![call("a"), call("b")])];
        assert_eq!(
            next_action(&msgs),
            Some(Action::ExecTool(call("a"))),
            "先执行先声明的调用"
        );
    }

    #[test]
    fn partial_results_run_remaining_call_before_requesting() {
        // 关键回归：Assistant(a,b) → Tool(a) 之后必须继续执行 b，
        // 而不是拿着缺 b 结果的历史去发请求（API 400）。
        let msgs = vec![
            Message::user("问"),
            assistant(vec![call("a"), call("b")]),
            Message::tool("a", "ok"),
        ];
        assert_eq!(
            next_action(&msgs),
            Some(Action::ExecTool(call("b"))),
            "b 的结果未落盘，必须先执行"
        );
    }

    #[test]
    fn complete_results_request_model() {
        let msgs = vec![
            Message::user("问"),
            assistant(vec![call("a"), call("b")]),
            Message::tool("a", "ok"),
            Message::tool("b", "ok"),
        ];
        assert_eq!(next_action(&msgs), Some(Action::CallModel));
    }

    #[test]
    fn healthy_history_is_not_healed() {
        let mut msgs = vec![
            Message::user("问"),
            assistant(vec![call("a")]),
            Message::tool("a", "ok"),
            Message::user("再问"),
        ];
        assert_eq!(heal(&mut msgs), 0);
        assert_eq!(msgs.len(), 4);
    }

    #[test]
    fn orphan_calls_get_synthetic_results() {
        // 崩溃现场：两个调用都没执行就断了
        let mut msgs = vec![Message::user("问"), assistant(vec![call("a"), call("b")])];
        assert_eq!(heal(&mut msgs), 2);
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[2].tool_call_id.as_deref(), Some("a"));
        assert_eq!(msgs[3].tool_call_id.as_deref(), Some("b"));
        assert_eq!(
            msgs[2].content.as_deref(),
            Some(INTERRUPTED_RESULT),
            "合成文本必须逐字节确定（前缀缓存依赖）"
        );
    }

    #[test]
    fn partial_run_is_completed_in_place() {
        // 崩溃现场：c1 已落盘、c2 丢失 → c2 的占位结果插在结果串末尾
        let mut msgs = vec![
            Message::user("问"),
            assistant(vec![call("a"), call("b")]),
            Message::tool("a", "ok"),
            Message::user("下一问"),
        ];
        assert_eq!(heal(&mut msgs), 1);
        assert_eq!(msgs.len(), 5);
        assert_eq!(msgs[2].tool_call_id.as_deref(), Some("a"));
        assert_eq!(
            msgs[3].tool_call_id.as_deref(),
            Some("b"),
            "补在既有结果之后"
        );
        assert_eq!(msgs[4].role, Role::User, "原有消息不被挪动");
    }

    #[test]
    fn healed_history_yields_callmodel() {
        let mut msgs = vec![Message::user("问"), assistant(vec![call("a")])];
        heal(&mut msgs);
        assert_eq!(next_action(&msgs), Some(Action::CallModel));
    }
}
