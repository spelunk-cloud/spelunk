use super::color::cprintln;
use indicatif::{ProgressBar, ProgressStyle};
use std::io::IsTerminal as _;

pub(crate) fn is_tty() -> bool {
    std::io::stderr().is_terminal()
}

pub(crate) fn spinner(message: impl Into<std::borrow::Cow<'static, str>>) -> ProgressBar {
    if is_tty() && !crate::utils::is_agent_mode() {
        let sp = ProgressBar::new_spinner();
        sp.set_message(message);
        sp.enable_steady_tick(std::time::Duration::from_millis(80));
        sp
    } else {
        ProgressBar::hidden()
    }
}

pub(crate) fn progress_style(prefix: &str) -> ProgressStyle {
    ProgressStyle::with_template(&format!(
        "{{spinner:.cyan}} {prefix} [{{bar:38.cyan/blue}}] {{pos}}/{{len}} {{wide_msg}}"
    ))
    .unwrap()
    .progress_chars("=>-")
}

pub(crate) fn short_path(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

pub(crate) fn print_results_text(results: &[crate::search::SearchResult]) {
    let knn_count = results.iter().filter(|r| !r.from_graph).count();
    let has_graph = results.iter().any(|r| r.from_graph);
    let mut printed_graph_header = false;
    let mut display_idx = 0usize;

    for r in results {
        if r.from_graph && !printed_graph_header {
            cprintln!(
                "\x1b[2m── Graph neighbours ─────────────────────────────────────────\x1b[0m"
            );
            println!();
            display_idx = 0;
            printed_graph_header = true;
            let _ = (knn_count, has_graph); // suppress unused warnings
        }

        display_idx += 1;
        let name = r.name.as_deref().unwrap_or("<anonymous>");
        let suffix = if r.from_graph {
            "\x1b[2m [via graph]\x1b[0m".to_string()
        } else {
            format!("  dist: {:.4}", r.distance)
        };

        // Prefix with project name for results from linked projects.
        let project_prefix = if let Some(ref pname) = r.project_name {
            format!("\x1b[36m[{pname}]\x1b[0m ")
        } else {
            String::new()
        };

        cprintln!(
            "{:2}. {}\x1b[1m{}\x1b[0m  \x1b[2m{}:{}-{}\x1b[0m  \x1b[33m[{}: {}]\x1b[0m{}",
            display_idx,
            project_prefix,
            r.file_path,
            r.language,
            r.start_line,
            r.end_line,
            r.node_type,
            name,
            suffix,
        );

        if let Some(ref summary) = r.summary
            && !summary.is_empty()
        {
            println!("  Summary: {summary}");
        }

        if !r.governing_specs.is_empty() {
            cprintln!("    \x1b[2mSpec: {}\x1b[0m", r.governing_specs.join(", "));
        }

        let lines: Vec<&str> = r.content.lines().collect();
        let preview_lines = lines.len().min(6);
        for line in &lines[..preview_lines] {
            println!("    {line}");
        }
        if lines.len() > preview_lines {
            cprintln!(
                "    \x1b[2m… ({} more lines)\x1b[0m",
                lines.len() - preview_lines
            );
        }
        println!();
    }
}

pub(crate) fn print_chunks_text(chunks: &[crate::search::SearchResult]) {
    for (i, c) in chunks.iter().enumerate() {
        let name = c.name.as_deref().unwrap_or("<anonymous>");
        cprintln!(
            "{:2}. \x1b[2m{}:{}-{}\x1b[0m  \x1b[33m[{}: {}]\x1b[0m",
            i + 1,
            c.language,
            c.start_line,
            c.end_line,
            c.node_type,
            name,
        );
        let lines: Vec<&str> = c.content.lines().collect();
        let preview = lines.len().min(6);
        for line in &lines[..preview] {
            println!("    {line}");
        }
        if lines.len() > preview {
            cprintln!("    \x1b[2m… ({} more lines)\x1b[0m", lines.len() - preview);
        }
        println!();
    }
}
