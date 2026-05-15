use anyhow::{bail, Result};
use console::{style, truncate_str, Key, StyledObject, Term};

pub(crate) fn title(text: &str) -> StyledObject<&str> {
    style(text).cyan().bold()
}

pub(crate) fn subtitle(text: &str) -> StyledObject<&str> {
    style(text).dim()
}

pub(crate) fn label(text: &str) -> StyledObject<String> {
    style(format!("{text}:")).blue().bold()
}

pub(crate) fn name(text: &str) -> StyledObject<&str> {
    style(text).green().bold()
}

pub(crate) fn path(text: &str) -> StyledObject<&str> {
    style(text).cyan()
}

pub(crate) fn value(text: &str) -> StyledObject<&str> {
    style(text).cyan()
}

pub(crate) fn success(text: &str) -> StyledObject<&str> {
    style(text).green().bold()
}

pub(crate) fn warning(text: &str) -> StyledObject<&str> {
    style(text).yellow().bold()
}

pub(crate) fn error(text: &str) -> StyledObject<&str> {
    style(text).red().bold()
}

pub(crate) fn muted(text: &str) -> StyledObject<&str> {
    style(text).dim()
}

pub(crate) fn enabled(value: bool) -> StyledObject<&'static str> {
    if value {
        success("enabled")
    } else {
        muted("disabled")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MenuAction {
    Selected(usize),
    Back,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Navigation {
    Done,
    Back,
}

pub(crate) fn select<T: AsRef<str>>(
    prompt: &str,
    items: &[T],
    default: usize,
) -> Result<MenuAction> {
    if items.is_empty() {
        bail!("menu must contain at least one item");
    }

    let term = Term::stderr();
    let mut selected = default.min(items.len().saturating_sub(1));
    let rendered_lines = items.len() + 1;
    let mut rendered = false;

    loop {
        if rendered {
            term.clear_last_lines(rendered_lines)?;
        }
        rendered = true;

        let terminal_width = term.size().1 as usize;
        let prompt_suffix = "← back  → select  ↑↓ move";
        let prompt_line = fit_line(&format!("{prompt}: {prompt_suffix}"), terminal_width);
        term.write_line(&prompt_line)?;
        for (index, item) in items.iter().enumerate() {
            let prefix = if index == selected {
                success("❯").to_string()
            } else {
                muted(" ").to_string()
            };
            let item = fit_line(item.as_ref(), terminal_width.saturating_sub(4));
            if index == selected {
                term.write_line(&format!("  {prefix} {}", value(&item)))?;
            } else {
                term.write_line(&format!("  {prefix} {item}"))?;
            }
        }
        term.flush()?;

        match term.read_key()? {
            Key::ArrowDown | Key::Tab | Key::Char('j') => {
                selected = (selected + 1) % items.len();
            }
            Key::ArrowUp | Key::BackTab | Key::Char('k') => {
                selected = (selected + items.len() - 1) % items.len();
            }
            Key::ArrowLeft | Key::Escape | Key::Char('h') | Key::Char('q') => {
                term.clear_last_lines(rendered_lines)?;
                term.write_line(&format!("{} {}", label(prompt), muted("back")))?;
                return Ok(MenuAction::Back);
            }
            Key::ArrowRight | Key::Enter | Key::Char('l') | Key::Char(' ') => {
                term.clear_last_lines(rendered_lines)?;
                term.write_line(&format!(
                    "{} {}",
                    label(prompt),
                    value(items[selected].as_ref())
                ))?;
                return Ok(MenuAction::Selected(selected));
            }
            _ => {}
        }
    }
}

fn fit_line(text: &str, width: usize) -> String {
    truncate_str(text, width.max(1), "…").into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use console::measure_text_width;

    #[test]
    fn test_fit_line_keeps_short_text() {
        assert_eq!(fit_line("short", 10), "short");
    }

    #[test]
    fn test_fit_line_truncates_to_display_width() {
        let line = fit_line("a very long workspace label", 10);
        assert!(measure_text_width(&line) <= 10);
        assert!(line.ends_with('…'));
    }
}
