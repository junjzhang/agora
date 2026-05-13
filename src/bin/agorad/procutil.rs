//! Process-tree utilities. Reads /proc/*/children and /proc/*/cmdline to
//! locate child processes and detect SSH descendants.

use std::fs;

/// Walk down from `pid` looking for a shell (zsh/bash/fish/sh). Used to
/// recover the working directory of a terminal: the terminal's own /proc
/// cwd is its launch dir, the useful cwd belongs to the shell descendant.
/// Returns the shell's cwd when found.
pub(crate) fn find_shell_cwd(start_pid: i32) -> Option<String> {
    let mut stack = vec![(start_pid, 0u32)];
    while let Some((pid, depth)) = stack.pop() {
        if depth > 8 {
            continue;
        }
        let comm = fs::read_to_string(format!("/proc/{pid}/comm"))
            .ok()
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        if matches!(comm.as_str(), "zsh" | "bash" | "fish" | "sh" | "dash") {
            if let Ok(p) = fs::read_link(format!("/proc/{pid}/cwd")) {
                let s = p.to_string_lossy().into_owned();
                if s.starts_with('/') && s != "/" {
                    return Some(s);
                }
            }
        }
        let Ok(children) = fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")) else {
            continue;
        };
        for c in children.split_whitespace() {
            if let Ok(cpid) = c.parse::<i32>() {
                stack.push((cpid, depth + 1));
            }
        }
    }
    None
}

/// Walk down the child chain from `pid`; stop at the first process with
/// !=1 children. Returns the final pid (which may be `pid` itself if it
/// has 0 or >1 children).
pub(crate) fn find_leaf_child(mut pid: i32) -> i32 {
    for _ in 0..20 {
        let Ok(children_str) = fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
        else {
            break;
        };
        let children: Vec<i32> = children_str
            .split_whitespace()
            .filter_map(|s| s.parse().ok())
            .collect();
        if children.len() != 1 {
            break;
        }
        pid = children[0];
    }
    pid
}

/// If any descendant of `pid` is `ssh` (or `kitten ssh`), return its host
/// argument. Used by `agora promote` to detect a terminal already SSH'd
/// somewhere and propose remote-root promotion.
pub(crate) fn find_ssh_host_in_children(pid: i32) -> Option<String> {
    find_ssh_host_recursive(pid, 0)
}

fn find_ssh_host_recursive(pid: i32, depth: u32) -> Option<String> {
    if depth > 10 {
        return None;
    }
    let children_str = fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")).ok()?;
    for child_str in children_str.split_whitespace() {
        let Some(child) = child_str.parse::<i32>().ok() else {
            continue;
        };
        let Ok(cmdline) = fs::read_to_string(format!("/proc/{child}/cmdline")) else {
            continue;
        };
        let args: Vec<&str> = cmdline.split('\0').filter(|s| !s.is_empty()).collect();
        let bin = args.first().copied().unwrap_or("");
        let is_ssh = bin.ends_with("ssh")
            || (bin.ends_with("kitten") && args.get(1).copied() == Some("ssh"));
        if is_ssh {
            return args
                .iter()
                .rev()
                .find(|a| !a.starts_with('-') && !a.contains(':'))
                .map(|s| s.to_string());
        }
        if let Some(host) = find_ssh_host_recursive(child, depth + 1) {
            return Some(host);
        }
    }
    None
}
