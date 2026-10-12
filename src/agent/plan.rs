//! Plan→Execute 分離 (#65 C1)。依頼を小さな手順に割り、**各手順を新しい短い文脈で**実行する。
//!
//! 小型モデルが L2 で落ちる形は「途中までは正しいが、終盤で前半の約束を忘れる」。原因の一つは
//! 1 つの文脈にツール出力が積み上がることなので、手順ごとに子エージェント (`Task`) を立てて
//! 文脈を捨てる。親に残るのは手順の見出しと 1 行の結果だけになる。
//!
//! ここは「計画の表現」だけを持つ。実行は [`crate::agent::Session`] 側 (`plan_and_execute`)。

/// 計画を作らせる長さの下限。これより短い依頼は分解しても手順が 1 つになる。
pub const MIN_PROMPT_CHARS: usize = 80;
/// 手順数の上限。超えたぶんは捨てる (子 1 つにつき LLM が何往復もするため、上限が無いと
/// 1 ターンの費用が読めない)。
pub const MAX_STEPS: usize = 6;
const MAX_TITLE_CHARS: usize = 120;
const MAX_DETAIL_CHARS: usize = 600;
/// 親に残す 1 手順ぶんの結果の長さ。これ以上は切る (C1 の狙いは親の文脈を短く保つこと)。
const MAX_OUTCOME_CHARS: usize = 400;

pub const PLAN_SYSTEM: &str = "You split a software task into steps that can each be done \
     independently, in order, by a separate worker who sees only your description of the step.\n\n\
     Reply with JSON only, in this shape:\n\
     {\"steps\": [{\"title\": \"short imperative title\", \"detail\": \"what to do, which files, \
     what the result must look like\"}]}\n\n\
     Rules:\n\
     - At most 6 steps. Fewer is better. One step if the task is small.\n\
     - Each step must be verifiable on its own and must leave the code in a working state.\n\
     - Order them so that a step only needs what the earlier steps produced.\n\
     - `detail` is written for the worker, not for the user: name the files, the commands and the \
     exact behaviour the step must end with. The worker does not see the conversation.\n\
     - Do not invent requirements the task does not state. Do not add steps for \
     documentation, tests or refactoring unless the task asks for them.\n\
     - No prose outside the JSON.";

/// 手順を実行する子 (`plan-step`) に渡すツール。**親に見えている範囲と同じ**に絞る
/// (`AskUserQuestion` だけ外す — 子は利用者と直接やり取りしない)。残るものが無ければ `None`
/// で、そのときは手順を実行できない (呼び出し側は分解しない)。
pub fn step_tools(cfg: &crate::config::Config) -> Option<crate::tools::registry::ToolRegistry> {
    let mut tools = crate::tools::registry::default_registry();
    tools.apply_profile(cfg.agent.tool_profile, &cfg.agent.tools);
    let keep: Vec<String> = tools
        .names()
        .into_iter()
        .filter(|n| *n != "AskUserQuestion")
        .map(str::to_string)
        .collect();
    if keep.is_empty() {
        return None;
    }
    // 空リストは「絞らない」の意味になるので、ここに来るのは keep が空でないときだけ。
    tools.apply_profile(crate::config::ToolProfile::Full, &keep);
    Some(tools)
}

/// 手順を実行できる構成か (`plan-step` が登録される条件と同じ)。
pub fn can_run_steps(cfg: &crate::config::Config) -> bool {
    step_tools(cfg).is_some()
}

/// 手順 1 つ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub title: String,
    pub detail: String,
}

/// 実行した手順とその結果 (親の履歴に残す 1 行ぶん)。
#[derive(Debug, Clone)]
pub struct Outcome {
    pub title: String,
    pub summary: String,
    pub ok: bool,
}

/// 計画の応答を読む。JSON が本命だが、小型モデルは番号付きリストで返すこともあるので
/// そちらも拾う。読めなければ空 (呼び出し側は分解せず普通に走る)。
pub fn parse_plan(text: &str) -> Vec<Step> {
    let body = strip_fences(text.trim());
    let steps = json_steps(body).unwrap_or_else(|| numbered_steps(body));
    steps
        .into_iter()
        .filter(|s| !s.title.is_empty())
        .map(|s| Step {
            title: clamp(&s.title, MAX_TITLE_CHARS),
            detail: clamp(&s.detail, MAX_DETAIL_CHARS),
        })
        .take(MAX_STEPS)
        .collect()
}

/// ```json``` などのフェンスを外し、前後の散文を落として JSON 本体だけを返す。
fn strip_fences(text: &str) -> &str {
    let mut body = text;
    if let Some(rest) = body.strip_prefix("```") {
        // ``` の直後の言語指定を飛ばす。
        let rest = rest.split_once('\n').map(|(_, r)| r).unwrap_or(rest);
        body = rest.rsplit_once("```").map(|(b, _)| b).unwrap_or(rest);
    }
    let body = body.trim();
    // 前後に説明文が付いていても、最初の `{` から最後の `}` までを試す。
    match (body.find(['{', '[']), body.rfind(['}', ']'])) {
        (Some(a), Some(b)) if a < b => &body[a..=b],
        _ => body,
    }
}

/// `{"steps": [...]}`、素の配列、文字列だけの配列を受ける。
fn json_steps(body: &str) -> Option<Vec<Step>> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let array = match &value {
        serde_json::Value::Array(a) => a.clone(),
        serde_json::Value::Object(o) => o
            .get("steps")
            .or_else(|| o.get("plan"))
            .and_then(|v| v.as_array())
            .cloned()?,
        _ => return None,
    };
    Some(
        array
            .iter()
            .map(|item| match item {
                serde_json::Value::String(s) => Step {
                    title: s.trim().to_string(),
                    detail: String::new(),
                },
                _ => {
                    let get = |k: &str| {
                        item.get(k)
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .trim()
                            .to_string()
                    };
                    let title = match get("title") {
                        t if !t.is_empty() => t,
                        _ => get("step"),
                    };
                    let detail = match get("detail") {
                        d if !d.is_empty() => d,
                        _ => get("description"),
                    };
                    Step { title, detail }
                }
            })
            .collect(),
    )
}

/// `1. 見出し` の並び。続く字下げ行は detail として足す。
fn numbered_steps(body: &str) -> Vec<Step> {
    let mut steps: Vec<Step> = Vec::new();
    for line in body.lines() {
        let trimmed = line.trim();
        let numbered = trimmed
            .split_once(['.', ')'])
            .filter(|(head, _)| !head.is_empty() && head.chars().all(|c| c.is_ascii_digit()))
            .map(|(_, rest)| rest.trim());
        match numbered {
            Some(title) if !title.is_empty() => steps.push(Step {
                title: title.to_string(),
                detail: String::new(),
            }),
            // 見出しの続き (字下げされた説明) は直前の手順の detail に足す。
            _ => {
                if let Some(last) = steps.last_mut()
                    && !trimmed.is_empty()
                    && line.starts_with(char::is_whitespace)
                {
                    if !last.detail.is_empty() {
                        last.detail.push(' ');
                    }
                    last.detail.push_str(trimmed);
                }
            }
        }
    }
    steps
}

fn clamp(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((cut, _)) => text[..cut].trim_end().to_string(),
        None => text.to_string(),
    }
}

/// 子に渡す依頼文。**会話は見せない**ので、元の依頼と手順と「前の手順が残したもの」を書く。
pub fn step_prompt(request: &str, steps: &[Step], index: usize, done: &[Outcome]) -> String {
    let step = &steps[index];
    let mut out = format!(
        "The user asked for this:\n\n{request}\n\n\
         A plan was made for it. You are doing **step {} of {}** and nothing else:\n\n\
         ## {}\n{}\n",
        index + 1,
        steps.len(),
        step.title,
        step.detail
    );
    if !done.is_empty() {
        out.push_str("\nAlready done by earlier steps (do not redo):\n");
        for (i, o) in done.iter().enumerate() {
            out.push_str(&format!("{}. {} — {}\n", i + 1, o.title, o.summary));
        }
    }
    if let Some(next) = steps.get(index + 1) {
        out.push_str(&format!(
            "\nA later step will do: {}. Leave that to it.\n",
            next.title
        ));
    }
    out.push_str(
        "\nWork in the current directory. Do the step, check that it works, then reply with one or \
         two sentences: which files you changed and what the next step needs to know. No tool call \
         in that last message.",
    );
    out
}

/// 手順の結果を親の履歴に残す注記。親はこの後、普通にターンを続けて利用者に答える。
pub fn outcome_note(outcomes: &[Outcome], total: usize) -> String {
    let mut out = format!(
        "plan→execute: {} of {} steps ran in their own context.\n",
        outcomes.len(),
        total
    );
    for (i, o) in outcomes.iter().enumerate() {
        out.push_str(&format!(
            "{}. [{}] {} — {}\n",
            i + 1,
            if o.ok { "done" } else { "failed" },
            o.title,
            o.summary
        ));
    }
    let stopped = outcomes.len() < total || outcomes.iter().any(|o| !o.ok);
    out.push_str(if stopped {
        "\nThe plan did not finish. Pick it up from the failed step yourself: read what is there \
         now, fix it, then answer the user."
    } else {
        "\nThis work is already done — do not redo it. Check the result against what the user \
         asked (read the files, run it), then answer the user."
    });
    out
}

/// 子の結果に付く resume 用の id の案内 (#77) **だけ**を抜く。後ろには hook の追記
/// (`[post-tool hook] …` や hook が寄せた文脈) が続くことがあるので、巻き込まない
/// (#154 のレビュー: 一緒に切っていて親に届かなくなっていた)。
fn without_resume_hint(output: &str) -> String {
    let Some(start) = output.rfind("\n\n[sub-agent id: ") else {
        return output.to_string();
    };
    let tail = &output[start..];
    match tail.find(']') {
        Some(end) => format!("{}{}", &output[..start], &tail[end + 1..]),
        // 閉じ括弧が無い (切れている) ときは、そこから後ろを捨てる。
        None => output[..start].to_string(),
    }
}

/// 子の最終応答を 1 行ぶんに詰める。
pub fn summarize(output: &str) -> String {
    // 親に積むのは要約だけでよく、400 字の予算を resume の案内に食わせない。
    let text = without_resume_hint(output);
    let one_line = text
        .trim()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    match one_line.is_empty() {
        true => "(no summary)".to_string(),
        false => clamp(&one_line, MAX_OUTCOME_CHARS),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_in_a_fence_with_prose_around_it_is_read() {
        let steps = parse_plan(
            "Sure, here is the plan:\n```json\n{\"steps\": [\
             {\"title\": \"Add the parser\", \"detail\": \"in src/parse.py\"},\
             {\"title\": \"Wire the CLI\", \"description\": \"scan subcommand\"}]}\n```\nHope that helps.",
        );
        assert_eq!(
            steps,
            vec![
                Step {
                    title: "Add the parser".into(),
                    detail: "in src/parse.py".into()
                },
                Step {
                    title: "Wire the CLI".into(),
                    detail: "scan subcommand".into()
                },
            ]
        );
    }

    #[test]
    fn a_numbered_list_is_read_when_there_is_no_json() {
        let steps = parse_plan(
            "1. Write the scanner\n   walk the directory, collect sizes\n2) Add the report command\n",
        );
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].title, "Write the scanner");
        assert_eq!(steps[0].detail, "walk the directory, collect sizes");
        assert_eq!(steps[1].title, "Add the report command");
    }

    #[test]
    fn unreadable_answers_give_no_plan_and_long_ones_are_capped() {
        assert!(parse_plan("I think we should start by looking around.").is_empty());
        assert!(parse_plan("").is_empty());
        let many = (1..=20)
            .map(|i| format!("{{\"title\": \"step {i}\"}}"))
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(parse_plan(&format!("[{many}]")).len(), MAX_STEPS);
        let long = "x".repeat(MAX_TITLE_CHARS + 50);
        let one = parse_plan(&format!("[{{\"title\": \"{long}\"}}]"));
        assert_eq!(one[0].title.chars().count(), MAX_TITLE_CHARS);
    }

    #[test]
    fn a_step_prompt_carries_the_request_the_step_and_what_came_before() {
        let steps = vec![
            Step {
                title: "First".into(),
                detail: "do A".into(),
            },
            Step {
                title: "Second".into(),
                detail: "do B".into(),
            },
        ];
        let done = [Outcome {
            title: "First".into(),
            summary: "wrote a.py".into(),
            ok: true,
        }];
        let p = step_prompt("build a scanner", &steps, 1, &done);
        assert!(p.contains("build a scanner"), "{p}");
        assert!(p.contains("step 2 of 2") && p.contains("do B"), "{p}");
        assert!(p.contains("First — wrote a.py"), "{p}");
        // 最後の手順なので「後の手順」の案内は出ない。
        assert!(!p.contains("A later step"), "{p}");
        // 1 つ目には出る。
        assert!(step_prompt("x", &steps, 0, &[]).contains("A later step will do: Second"));
    }

    #[test]
    fn the_note_tells_the_parent_to_finish_the_plan_only_when_it_broke() {
        let ok = [
            Outcome {
                title: "First".into(),
                summary: "wrote a.py".into(),
                ok: true,
            },
            Outcome {
                title: "Second".into(),
                summary: "wrote b.py".into(),
                ok: true,
            },
        ];
        let note = outcome_note(&ok, 2);
        assert!(note.contains("do not redo it"), "{note}");
        assert!(!note.contains("did not finish"), "{note}");
        // 途中で止まった計画は、親に引き継ぎを促す。
        let broke = [Outcome {
            title: "First".into(),
            summary: "python3: command not found".into(),
            ok: false,
        }];
        let note = outcome_note(&broke, 2);
        assert!(
            note.contains("did not finish") && note.contains("failed"),
            "{note}"
        );
    }

    #[test]
    fn a_childs_answer_becomes_one_clamped_line() {
        assert_eq!(
            summarize("  Added scanner.py.\n\nIt walks the tree.\n"),
            "Added scanner.py. It walks the tree."
        );
        assert_eq!(summarize("   "), "(no summary)");
        // resume 用の id の尾は落とす (親に積む 400 字をこれに食わせない)。
        assert_eq!(
            summarize(
                "Added scanner.py.\n\n[sub-agent id: agent_1; pass resume: \"agent_1\" to continue it with its context]"
            ),
            "Added scanner.py."
        );
        // 尾の**後ろ**に付く hook の追記は巻き込まない (#154 のレビュー)。
        assert_eq!(
            summarize(
                "Added scanner.py.\n\n[sub-agent id: agent_1; pass resume: \"agent_1\"]\n                 [post-tool hook] lint is dirty"
            ),
            "Added scanner.py. [post-tool hook] lint is dirty"
        );
        // 閉じ括弧が無い (切れている) ときは、そこから後ろを捨てる。
        assert_eq!(
            summarize("Added scanner.py.\n\n[sub-agent id: agent_1"),
            "Added scanner.py."
        );
        assert_eq!(
            summarize(&"y".repeat(MAX_OUTCOME_CHARS + 10))
                .chars()
                .count(),
            MAX_OUTCOME_CHARS
        );
    }
}
