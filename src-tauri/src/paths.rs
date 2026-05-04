use std::fs;
use std::path::PathBuf;

pub fn data_dir() -> PathBuf {
    let mut p = dirs::home_dir().expect("no home directory");
    p.push(".margin");
    p
}

pub fn models_dir() -> PathBuf {
    data_dir().join("models")
}

pub fn meetings_dir() -> PathBuf {
    data_dir().join("meetings")
}

pub fn notes_dir() -> PathBuf {
    data_dir().join("notes")
}

pub fn logs_dir() -> PathBuf {
    data_dir().join("logs")
}

/// Idempotent — safe to call repeatedly. Creates ~/.margin and its subdirs.
pub fn init() -> std::io::Result<()> {
    for p in [
        data_dir(),
        models_dir(),
        meetings_dir(),
        notes_dir(),
        logs_dir(),
    ] {
        fs::create_dir_all(&p)?;
    }
    Ok(())
}
