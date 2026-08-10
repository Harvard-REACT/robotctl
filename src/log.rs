pub fn info(msg: impl AsRef<str>) {
    eprintln!("[INFO] {}", msg.as_ref());
}

pub fn warn(msg: impl AsRef<str>) {
    eprintln!("[WARN] {}", msg.as_ref());
}

pub fn error(msg: impl AsRef<str>) {
    eprintln!("[ERROR] {}", msg.as_ref());
}
