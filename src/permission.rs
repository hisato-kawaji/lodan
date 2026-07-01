use std::collections::HashSet;
use std::io::{self, BufRead, Write};
use std::sync::Mutex;

#[derive(Debug, Default)]
pub struct SessionPolicy {
    pub always_tools: HashSet<String>,
    pub always_commands: HashSet<String>,
}

pub struct PermissionGate {
    auto_approve: bool,
    policy: Mutex<SessionPolicy>,
}

impl PermissionGate {
    pub fn new(auto_approve: bool) -> Self {
        Self {
            auto_approve,
            policy: Mutex::new(SessionPolicy::default()),
        }
    }

    pub fn allow(&self, tool_name: &str, args: &serde_json::Value) -> bool {
        if self.auto_approve {
            return true;
        }
        if let Ok(p) = self.policy.lock() {
            if p.always_tools.contains(tool_name) {
                return true;
            }
            if tool_name == "Bash"
                && let Some(cmd) = args.get("command").and_then(|v| v.as_str())
                && p.always_commands.contains(cmd)
            {
                return true;
            }
        }
        self.prompt(tool_name, args)
    }

    fn prompt(&self, tool_name: &str, args: &serde_json::Value) -> bool {
        let summary = summarize(tool_name, args);
        let stdin = io::stdin();
        let mut stdout = io::stdout().lock();
        loop {
            let _ = writeln!(
                stdout,
                "{} Allow {}: {summary}",
                crate::term::yellow("[lodan]"),
                crate::term::bold(tool_name),
            );
            let _ = writeln!(
                stdout,
                "{}",
                crate::term::dim(&format!(
                    "  (y) yes once  (n) no  (a) always allow {tool_name}  (e) always allow this exact"
                ))
            );
            let _ = write!(stdout, "{} ", crate::term::yellow(">"));
            let _ = stdout.flush();

            let mut line = String::new();
            if stdin.lock().read_line(&mut line).is_err() {
                return false;
            }
            match line.trim() {
                "y" | "Y" | "" => return true,
                "n" | "N" => return false,
                "a" | "A" => {
                    if let Ok(mut p) = self.policy.lock() {
                        p.always_tools.insert(tool_name.to_string());
                    }
                    return true;
                }
                "e" | "E" => {
                    if tool_name == "Bash"
                        && let Some(cmd) = args.get("command").and_then(|v| v.as_str())
                    {
                        if let Ok(mut p) = self.policy.lock() {
                            p.always_commands.insert(cmd.to_string());
                        }
                        return true;
                    }
                    if let Ok(mut p) = self.policy.lock() {
                        p.always_tools.insert(tool_name.to_string());
                    }
                    return true;
                }
                _ => continue,
            }
        }
    }
}

fn summarize(tool: &str, args: &serde_json::Value) -> String {
    match tool {
        "Bash" => args
            .get("command")
            .and_then(|v| v.as_str())
            .map(|s| format!("`{s}`"))
            .unwrap_or_else(|| args.to_string()),
        "Write" | "Edit" => args
            .get("path")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| args.to_string()),
        _ => args.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_approve_short_circuits() {
        let g = PermissionGate::new(true);
        assert!(g.allow("Bash", &serde_json::json!({"command":"ls"})));
    }

    #[test]
    fn always_tool_membership() {
        let g = PermissionGate::new(false);
        g.policy
            .lock()
            .unwrap()
            .always_tools
            .insert("Write".to_string());
        assert!(g.allow("Write", &serde_json::json!({"path":"/x"})));
    }

    #[test]
    fn always_command_membership() {
        let g = PermissionGate::new(false);
        g.policy
            .lock()
            .unwrap()
            .always_commands
            .insert("ls -la".to_string());
        assert!(g.allow("Bash", &serde_json::json!({"command":"ls -la"})));
    }
}
