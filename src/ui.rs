use console::{style, StyledObject};

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
