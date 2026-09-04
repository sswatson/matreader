//! matreader: lazily explore MATLAB `.mat` files.

pub mod app;
pub mod format;
pub mod mat5;
pub mod mat73;
pub mod model;
pub mod ui;

use anyhow::{Context, Result};
use std::path::Path;

/// Open a `.mat` file with the appropriate backend.
pub fn open(path: &Path) -> Result<Box<dyn model::MatFile>> {
    let mut head = [0u8; 128];
    {
        use std::io::Read;
        let mut f =
            std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let n = f.read(&mut head)?;
        if n < 128 {
            anyhow::bail!("{} is too short to be a MAT-file", path.display());
        }
    }
    if mat5::is_v73(&head) {
        Ok(Box::new(mat73::Mat73File::open(path)?))
    } else {
        Ok(Box::new(mat5::Mat5File::open(path)?))
    }
}
