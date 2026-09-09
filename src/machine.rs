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
//! | Command | 部分落地 | Cancel 已落地（回合中 Ctrl-C；带外，在解释器层处理，不进 `next_action`）；New / Resume / Exit 未落地 |
//! | Notice | 已落地 | `ui::Ui` 的七个方法 |
//! | Effect | 未落地 | 目前由解释器直写；出现条件化效果组合（如审批门）时提为显式枚举 |

use std::collections::HashSet;

use crate::types::{Message, Role, ToolCall};

// ============================================================================
// 历史合法性规范（可执行形式）。
// "发给后端的请求历史必须满足什么"从此以这里为权威定义：
//   is_request_valid = windows_complete ∧ no_stray_tools
// 不满足的形状会被 DeepSeek/GLM 直接 400。`heal` 与请求构造都以它为
// 目标不变量；下方测试用有界全形状族穷举钉住「崩溃点自愈必合法」。
// ============================================================================

/// 每条带 tool_calls 的 assistant：其后的 tool 结果窗口与声明一一对应
/// （数量相等、id 集合相同）。
pub fn windows_complete(messages: &[Message]) -> bool {
    for (i, m) in messages.iter().enumerate() {
        let Some(calls) = &m.tool_calls else { continue };
        let mut j = i + 1;
        while j < messages.len() && messages[j].role == Role::Tool {
            j += 1;
        }
        let window = &messages[i + 1..j];
        if window.len() != calls.len() {
            return false;
        }
        let declared: HashSet<&str> = calls.iter().map(|c| c.id.as_str()).collect();
        let answered: HashSet<&str> = window
            .iter()
            .filter_map(|t| t.tool_call_id.as_deref())
            .collect();
        if declared != answered {
            return false;
        }
    }
    true
}

/// 所有 tool 结果都落在某个带（非空）调用的 assistant 的结果窗口内，无野结果。
pub fn no_stray_tools(messages: &[Message]) -> bool {
    let mut under_calls = false;
    for m in messages {
        match m.role {
            Role::Tool => {
                if !under_calls {
                    return false;
                }
            }
            _ => {
                under_calls = m.tool_calls.as_deref().is_some_and(|c| !c.is_empty());
            }
        }
    }
    true
}

/// 请求历史合法性。请求构造处有 debug tripwire 强制；heal 的目标不变量。
pub fn is_request_valid(messages: &[Message]) -> bool {
    windows_complete(messages) && no_stray_tools(messages)
}

/// 崩溃自愈为未执行的工具调用合成的占位结果。必须是确定性常量：
/// 同一份日志每次 load 都要合成出逐字节相同的历史（前缀缓存依赖）。
pub const INTERRUPTED_RESULT: &str = "error: interrupted before execution; no result was recorded";

/// 用户取消（Ctrl-C）时为未完成调用落盘的占位结果。确定性常量，理由同上。
/// 与 INTERRUPTED_RESULT 区分：前者是崩溃后 load 时合成（只进内存视图），
/// 后者是取消时真实落盘（进程还活着，必须写进文件）。
pub const CANCELLED_RESULT: &str = "error: cancelled by user before a result was recorded";

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

/// 已声明但结果窗口内未应答的调用 id（按声明顺序）。
/// 取消收尾的依据：这些调用需要补 CANCELLED_RESULT 才能闭合窗口，
/// 否则下一回合会复活僵尸调用（或构造请求时 400）。
pub fn open_call_ids(messages: &[Message]) -> Vec<String> {
    let mut open = Vec::new();
    let mut i = 0;
    while i < messages.len() {
        let Some(calls) = &messages[i].tool_calls else {
            i += 1;
            continue;
        };
        let mut j = i + 1;
        while j < messages.len() && messages[j].role == Role::Tool {
            j += 1;
        }
        let answered: HashSet<&str> = messages[i + 1..j]
            .iter()
            .filter_map(|m| m.tool_call_id.as_deref())
            .collect();
        open.extend(
            calls
                .iter()
                .filter(|c| !answered.contains(c.id.as_str()))
                .map(|c| c.id.clone()),
        );
        i = j;
    }
    open
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
            // j 已是当前向量的坐标：同一窗口内逐个追加只需 + n。
            // 不得混入跨窗口的累计插入数——那会让第二个窗口越界。
            messages.insert(j + n, Message::tool(id, INTERRUPTED_RESULT));
        }
        inserted += missing.len();
        // 跳过整个窗口（含新插入的占位结果）
        i = j + missing.len();
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

    /// 转移函数的穷举论证：assistant 声明 n 个调用后，紧随结果串是 2^n 个
    /// 子集之一。对 n ≤ 3 全枚举，检查每种情况的决策恰好是"按声明顺序的
    /// 第一个未执行调用"，全部执行完才转 CallModel。
    /// 这是对可达抽象状态的完备性检查，不是抽样覆盖。
    #[test]
    fn exhaustive_answer_subsets_yield_first_unanswered_call() {
        for n in 1..=3usize {
            let ids: Vec<String> = (0..n).map(|i| format!("c{i}")).collect();
            let calls: Vec<ToolCall> = ids.iter().map(|id| call(id)).collect();
            for mask in 0..(1u32 << n) {
                let mut msgs = vec![Message::user("q"), assistant(calls.clone())];
                for (i, id) in ids.iter().enumerate() {
                    if mask & (1 << i) != 0 {
                        msgs.push(Message::tool(id, "ok"));
                    }
                }
                let expected = match (0..n).find(|&i| mask & (1 << i) == 0) {
                    Some(i) => Action::ExecTool(call(&ids[i])),
                    None => Action::CallModel,
                };
                assert_eq!(
                    next_action(&msgs),
                    Some(expected),
                    "n={n}, 已落盘结果掩码={mask:#b}"
                );
            }
        }
    }

    /// heal 幂等：自愈过的历史再次自愈必须零插入、零改动。
    /// 这是"同一份日志每次 load 得到同一视图"的必要条件（前缀缓存依赖）。
    #[test]
    fn heal_is_idempotent() {
        let mut msgs = vec![
            Message::user("问"),
            assistant(vec![call("a"), call("b")]),
            Message::tool("a", "ok"),
            Message::user("下一问"),
        ];
        assert_eq!(heal(&mut msgs), 1);
        let once = msgs.clone();
        assert_eq!(heal(&mut msgs), 0, "第二次自愈不得再插入");
        assert_eq!(msgs, once, "第二次自愈不得改动任何消息");
    }

    /// 可执行规范自身的定向用例：接受健康形状，拒绝已知会被 400 的形状。
    #[test]
    fn spec_predicate_directed_cases() {
        let ok = |msgs: &[Message]| assert!(is_request_valid(msgs), "应为合法: {msgs:?}");
        let bad = |msgs: &[Message]| assert!(!is_request_valid(msgs), "应非法: {msgs:?}");

        ok(&[]);
        ok(&[Message::user("q"), assistant(vec![]), Message::user("再问")]);
        ok(&[
            Message::user("q"),
            assistant(vec![call("a"), call("b")]),
            Message::tool("a", "1"),
            Message::tool("b", "2"),
        ]);
        ok(&[
            assistant(vec![call("a")]),
            Message::tool("a", "1"),
            Message::user("下一轮"),
            assistant(vec![call("b")]),
            Message::tool("b", "2"),
        ]);
        // 孤儿声明（崩溃形态）：结果缺失
        bad(&[Message::user("q"), assistant(vec![call("a")])]);
        // 野结果：前面没有带调用的 assistant
        bad(&[Message::user("q"), Message::tool("a", "1")]);
        bad(&[assistant(vec![]), Message::tool("a", "1")]);
        // 重复结果
        bad(&[
            assistant(vec![call("a")]),
            Message::tool("a", "1"),
            Message::tool("a", "2"),
        ]);
        // id 对不上
        bad(&[assistant(vec![call("a")]), Message::tool("b", "1")]);
        // 窗口已被非 tool 消息关闭，结果迟到
        bad(&[
            assistant(vec![call("a")]),
            Message::user("x"),
            Message::tool("a", "1"),
        ]);
    }

    /// 有界穷举族：长度 ≤ 5、字母表 7 种消息（User；Assistant ∅/{a}/{b}/{a,b}；
    /// Tool(a)/Tool(b)）的全部 7^0+…+7^5 = 19608 个序列。
    fn bounded_family() -> Vec<Vec<Message>> {
        fn kind(d: u32) -> Message {
            match d {
                0 => Message::user("q"),
                1 => assistant(vec![]),
                2 => assistant(vec![call("a")]),
                3 => assistant(vec![call("b")]),
                4 => assistant(vec![call("a"), call("b")]),
                5 => Message::tool("a", "ok"),
                _ => Message::tool("b", "ok"),
            }
        }
        let mut out = Vec::new();
        for len in 0..=5u32 {
            for code in 0..7u32.pow(len) {
                let mut seq = Vec::with_capacity(len as usize);
                let mut c = code;
                for _ in 0..len {
                    seq.push(kind(c % 7));
                    c /= 7;
                }
                out.push(seq);
            }
        }
        out
    }

    /// 定理 A（有界穷举 · 全族 19608 形状）：任意历史经 heal 后，每个声明
    /// 的调用都在其结果窗口内得到应答（declared ⊆ answered）。
    /// 注意 heal 只补缺、不去重：完整合法性属于定理 B 的崩溃可达族——
    /// 这个定理边界本身就是规范的一部分。
    #[test]
    fn theorem_a_heal_answers_every_declared_call_exhaustive() {
        for mut seq in bounded_family() {
            heal(&mut seq);
            for (i, m) in seq.iter().enumerate() {
                let Some(calls) = &m.tool_calls else { continue };
                let mut j = i + 1;
                while j < seq.len() && seq[j].role == Role::Tool {
                    j += 1;
                }
                let answered: HashSet<&str> = seq[i + 1..j]
                    .iter()
                    .filter_map(|t| t.tool_call_id.as_deref())
                    .collect();
                for c in calls {
                    assert!(
                        answered.contains(c.id.as_str()),
                        "heal 后仍有未应答调用 {c:?}，形状: {seq:?}"
                    );
                }
            }
        }
    }

    /// 定理 B（有界穷举 · 崩溃可达族）：合法历史的每个前缀 = 进程可停在
    /// 的每个崩溃点。全部前缀经 heal 后必须满足可执行规范。
    /// 这把「六个崩溃点各有恢复路径」的人工枚举升级为机械检查。
    #[test]
    fn theorem_b_every_crash_prefix_of_valid_history_recovers() {
        let mut checked = 0usize;
        for seq in bounded_family() {
            if !is_request_valid(&seq) {
                continue;
            }
            for cut in 0..=seq.len() {
                let mut prefix = seq[..cut].to_vec();
                heal(&mut prefix);
                assert!(
                    is_request_valid(&prefix),
                    "截断点 {cut}/{} 自愈后仍非法: {prefix:?}（原序列 {seq:?}）",
                    seq.len()
                );
                checked += 1;
            }
        }
        assert!(checked > 1000, "穷举族意外缩水：仅 {checked} 个前缀");
    }

    /// 回归：多个窗口同时缺结果时，插入坐标必须逐窗口局部。
    /// 此形状曾使 heal 越界 panic（insertion index out of bounds）——
    /// 由定理 A 的穷举率先暴露。
    #[test]
    fn heal_two_deficient_windows_inserts_locally() {
        let mut msgs = vec![
            assistant(vec![call("a")]),
            assistant(vec![call("b")]),
            Message::tool("b", "ok"),
        ];
        assert_eq!(heal(&mut msgs), 1);
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[1].tool_call_id.as_deref(), Some("a"));
        assert_eq!(msgs[3].tool_call_id.as_deref(), Some("b"));
        assert!(is_request_valid(&msgs));
    }

    #[test]
    fn open_call_ids_reports_unanswered_declarations() {
        assert!(open_call_ids(&[Message::user("q")]).is_empty());
        let two = vec![Message::user("q"), assistant(vec![call("a"), call("b")])];
        assert_eq!(open_call_ids(&two), vec!["a", "b"]);
        let partial = vec![
            Message::user("q"),
            assistant(vec![call("a"), call("b")]),
            Message::tool("a", "ok"),
        ];
        assert_eq!(open_call_ids(&partial), vec!["b"]);
        let mut closed = partial;
        closed.push(Message::tool("b", "ok"));
        assert!(open_call_ids(&closed).is_empty());
    }

    #[test]
    fn healed_history_yields_callmodel() {
        let mut msgs = vec![Message::user("问"), assistant(vec![call("a")])];
        heal(&mut msgs);
        assert_eq!(next_action(&msgs), Some(Action::CallModel));
    }
}
