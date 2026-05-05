pub fn heading(text: impl AsRef<str>) -> String {
    format!("\x1b[1;36m{}\x1b[0m", text.as_ref())
}

pub fn label(text: impl AsRef<str>) -> String {
    format!("\x1b[1;34m{}\x1b[0m", text.as_ref())
}

pub fn success(text: impl AsRef<str>) -> String {
    format!("\x1b[1;32m{}\x1b[0m", text.as_ref())
}

pub fn warning(text: impl AsRef<str>) -> String {
    format!("\x1b[1;33m{}\x1b[0m", text.as_ref())
}

pub fn error(text: impl AsRef<str>) -> String {
    format!("\x1b[1;31m{}\x1b[0m", text.as_ref())
}

pub fn accent(text: impl AsRef<str>) -> String {
    format!("\x1b[1;35m{}\x1b[0m", text.as_ref())
}

pub fn info(text: impl AsRef<str>) -> String {
    format!("\x1b[36m{}\x1b[0m", text.as_ref())
}

pub fn dim(text: impl AsRef<str>) -> String {
    format!("\x1b[2;37m{}\x1b[0m", text.as_ref())
}

pub fn tool(text: impl AsRef<str>) -> String {
    format!("\x1b[1;36m{}\x1b[0m", text.as_ref())
}

pub fn command(text: impl AsRef<str>) -> String {
    format!("\x1b[1;33m{}\x1b[0m", text.as_ref())
}

pub fn path(text: impl AsRef<str>) -> String {
    format!("\x1b[32m{}\x1b[0m", text.as_ref())
}
