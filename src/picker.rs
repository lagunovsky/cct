//! Multi-select TUI picker with checkboxes and an incremental filter.

use std::io::{stdout, Write};
use std::time::Duration;

use anyhow::{anyhow, Result};
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::style::Print;
use crossterm::terminal::{
    self, disable_raw_mode, enable_raw_mode, Clear, ClearType, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use crossterm::{queue, ExecutableCommand};

use crate::term::{trunc_to_width, BOLD, CYAN, DIM, GREEN, RESET};

#[derive(Clone, Debug)]
pub struct PickerItem {
    pub id: String,
    pub line1: String,
    pub line2: String,
    pub haystack: String,
}

pub fn run_multi_picker(items: &[PickerItem], title: &str) -> Result<Vec<String>> {
    if items.is_empty() {
        return Err(anyhow!("nothing to pick"));
    }
    if !crate::term::stdin_is_tty() || !crate::term::stdout_is_tty() {
        return fallback_text_picker(items, title);
    }
    enable_raw_mode()?;
    let mut out = stdout();
    out.execute(EnterAlternateScreen)?;
    out.execute(Hide)?;

    let result = run_loop(items, title, &mut out);

    out.execute(Show).ok();
    out.execute(LeaveAlternateScreen).ok();
    disable_raw_mode().ok();
    result
}

fn run_loop(items: &[PickerItem], title: &str, out: &mut std::io::Stdout) -> Result<Vec<String>> {
    let mut selected = vec![false; items.len()];
    let mut cursor = 0usize;
    let mut filter = String::new();
    let mut filter_mode = false;

    loop {
        let visible = collect_visible(items, &filter);
        cursor = cursor.min(visible.len().saturating_sub(1));

        let (cols, rows) = terminal::size().unwrap_or((80, 24));
        draw(
            out,
            items,
            &selected,
            &visible,
            cursor,
            &filter,
            filter_mode,
            title,
            cols,
            rows,
        )?;

        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if filter_mode {
                    match key.code {
                        KeyCode::Esc | KeyCode::Enter => filter_mode = false,
                        KeyCode::Backspace => {
                            filter.pop();
                            cursor = 0;
                        }
                        KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                            filter.push(ch);
                            cursor = 0;
                        }
                        KeyCode::Up | KeyCode::Down => {
                            filter_mode = false;
                            move_cursor(&mut cursor, &visible, key.code);
                        }
                        _ => {}
                    }
                    continue;
                }

                match key.code {
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Err(anyhow!("aborted"));
                    }
                    KeyCode::Char('q') | KeyCode::Esc => {
                        if !filter.is_empty() {
                            filter.clear();
                            cursor = 0;
                        } else {
                            return Err(anyhow!("aborted"));
                        }
                    }
                    KeyCode::Enter => {
                        let mut result: Vec<String> = selected
                            .iter()
                            .enumerate()
                            .filter(|(_, b)| **b)
                            .map(|(i, _)| items[i].id.clone())
                            .collect();
                        if result.is_empty() && !visible.is_empty() {
                            result = vec![items[visible[cursor]].id.clone()];
                        }
                        if result.is_empty() {
                            return Err(anyhow!("nothing selected"));
                        }
                        return Ok(result);
                    }
                    KeyCode::Char(' ') if cursor < visible.len() => {
                        let idx = visible[cursor];
                        selected[idx] = !selected[idx];
                    }
                    KeyCode::Char('a') => {
                        for &i in &visible {
                            selected[i] = true;
                        }
                    }
                    KeyCode::Char('A') => {
                        for &i in &visible {
                            selected[i] = false;
                        }
                    }
                    KeyCode::Char('/') => filter_mode = true,
                    KeyCode::Up | KeyCode::Char('k') => {
                        move_cursor(&mut cursor, &visible, KeyCode::Up)
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        move_cursor(&mut cursor, &visible, KeyCode::Down)
                    }
                    KeyCode::Home | KeyCode::Char('g') => cursor = 0,
                    KeyCode::End | KeyCode::Char('G') if !visible.is_empty() => {
                        cursor = visible.len() - 1;
                    }
                    _ => {}
                }
            }
        }
    }
}

fn collect_visible(items: &[PickerItem], filter: &str) -> Vec<usize> {
    if filter.is_empty() {
        return (0..items.len()).collect();
    }
    let needle = filter.to_lowercase();
    items
        .iter()
        .enumerate()
        .filter(|(_, it)| it.haystack.contains(&needle))
        .map(|(i, _)| i)
        .collect()
}

fn move_cursor(cursor: &mut usize, visible: &[usize], code: KeyCode) {
    match code {
        KeyCode::Up if *cursor > 0 => {
            *cursor -= 1;
        }
        KeyCode::Down if *cursor + 1 < visible.len() => {
            *cursor += 1;
        }
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn draw(
    out: &mut std::io::Stdout,
    items: &[PickerItem],
    selected: &[bool],
    visible: &[usize],
    cursor: usize,
    filter: &str,
    filter_mode: bool,
    title: &str,
    cols: u16,
    rows: u16,
) -> Result<()> {
    let cols = cols as usize;
    let rows = rows as usize;
    queue!(
        out,
        Clear(ClearType::All),
        MoveTo(0, 0),
        Print(format!("{BOLD}{}{RESET}\r\n", trunc_to_width(title, cols))),
    )?;
    let sel_count = selected.iter().filter(|b| **b).count();
    queue!(
        out,
        Print(format!(
            "{DIM}{}/{} visible · {} selected{RESET}\r\n\r\n",
            visible.len(),
            items.len(),
            sel_count
        ))
    )?;

    let viewport = rows.saturating_sub(7).max(4);
    let items_per_view = (viewport / 2).max(1);
    let mut offset = 0usize;
    if cursor >= items_per_view {
        offset = cursor.saturating_sub(items_per_view / 2);
    }
    if offset + items_per_view > visible.len() {
        offset = visible.len().saturating_sub(items_per_view);
    }
    let end = (offset + items_per_view).min(visible.len());

    for (j, &i) in visible.iter().enumerate().take(end).skip(offset) {
        let box_str = if selected[i] {
            format!("[{GREEN}{BOLD}x{RESET}]")
        } else {
            format!("{DIM}[ ]{RESET}")
        };
        let arrow = if j == cursor {
            format!("{CYAN}▶ {RESET}")
        } else {
            "  ".to_string()
        };
        let line1 = trunc_to_width(&items[i].line1, cols.saturating_sub(7));
        queue!(out, Print(format!("{arrow}{box_str} {line1}\r\n")))?;
        if !items[i].line2.is_empty() {
            let preview = trunc_to_width(&items[i].line2, cols.saturating_sub(9));
            queue!(out, Print(format!("       {DIM}{preview}{RESET}\r\n")))?;
        }
    }

    queue!(out, Print("\r\n"))?;
    if filter_mode {
        queue!(
            out,
            Print(format!("{BOLD}Filter:{RESET} {CYAN}{filter}_{RESET}\r\n")),
            Print(format!("{DIM}enter/esc: exit filter{RESET}\r\n"))
        )?;
    } else {
        if filter.is_empty() {
            queue!(out, Print("\r\n"))?;
        } else {
            queue!(out, Print(format!("{BOLD}Filter:{RESET} {filter}\r\n")))?;
        }
        queue!(
            out,
            Print(format!(
                "{DIM}↑↓ move · space toggle · / filter · a all · A none · enter confirm · q quit{RESET}\r\n"
            ))
        )?;
    }

    out.flush()?;
    Ok(())
}

fn fallback_text_picker(items: &[PickerItem], title: &str) -> Result<Vec<String>> {
    println!("{BOLD}{title}{RESET}\n");
    for (i, it) in items.iter().enumerate() {
        println!("  {CYAN}{:2}.{RESET} {}", i + 1, it.line1);
        if !it.line2.is_empty() {
            println!("       {DIM}{}{RESET}", it.line2);
        }
    }
    print!("\nSelect (e.g. 1,3,5 or 2-4 or all): ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let idx = parse_selection(&line, items.len())?;
    Ok(idx.into_iter().map(|n| items[n].id.clone()).collect())
}

pub fn parse_selection(s: &str, max: usize) -> Result<Vec<usize>> {
    let s = s.trim().to_lowercase();
    if s.is_empty() {
        return Err(anyhow!("nothing selected"));
    }
    if s == "all" || s == "*" {
        return Ok((0..max).collect());
    }
    let mut seen: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some(dash_idx) = part.find('-') {
            if dash_idx == 0 {
                return Err(anyhow!("bad range: {part}"));
            }
            let a: usize = part[..dash_idx]
                .trim()
                .parse()
                .map_err(|_| anyhow!("bad range: {part}"))?;
            let b: usize = part[dash_idx + 1..]
                .trim()
                .parse()
                .map_err(|_| anyhow!("bad range: {part}"))?;
            let (lo, hi) = if a > b { (b, a) } else { (a, b) };
            for i in lo..=hi {
                if i < 1 || i > max {
                    return Err(anyhow!("out of range: {i}"));
                }
                seen.insert(i - 1);
            }
            continue;
        }
        let n: usize = part.parse().map_err(|_| anyhow!("bad number: {part}"))?;
        if n < 1 || n > max {
            return Err(anyhow!("out of range: {n}"));
        }
        seen.insert(n - 1);
    }
    if seen.is_empty() {
        return Err(anyhow!("nothing selected"));
    }
    Ok(seen.into_iter().collect())
}

pub fn confirm(prompt: &str) -> bool {
    use std::io::{stdin, stdout, Write};
    print!("{prompt}");
    stdout().flush().ok();
    let mut line = String::new();
    if stdin().read_line(&mut line).is_err() {
        return false;
    }
    let trimmed = line.trim().to_lowercase();
    matches!(trimmed.as_str(), "y" | "yes")
}
