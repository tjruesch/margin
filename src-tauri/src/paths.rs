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

pub fn index_db_path() -> PathBuf {
    data_dir().join("index.db")
}

pub fn logs_dir() -> PathBuf {
    data_dir().join("logs")
}

pub fn team_dir() -> PathBuf {
    data_dir().join("team")
}

/// Idempotent — safe to call repeatedly. Creates ~/.margin and its subdirs.
pub fn init() -> std::io::Result<()> {
    for p in [
        data_dir(),
        models_dir(),
        meetings_dir(),
        notes_dir(),
        logs_dir(),
        team_dir(),
    ] {
        fs::create_dir_all(&p)?;
    }
    Ok(())
}
