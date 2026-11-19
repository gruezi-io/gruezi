//! Artifact movement at the workflow boundary: what a job uploads and what a
//! later job downloads, as real files in the scripted run.

use super::runner::Runner;
use anyhow::{Result, anyhow};
use std::{fs, path::PathBuf};

impl Runner<'_> {
    pub(super) fn upload(&self, name: &str, pattern: &str) -> Result<()> {
        let files = self.expand(pattern)?;
        if files.is_empty() {
            return Err(anyhow!("artifact {name} has no files"));
        }
        let dest = self.world.state.join("artifacts").join(name);
        fs::create_dir_all(&dest)?;
        for file in files {
            let file_name = file
                .file_name()
                .ok_or_else(|| anyhow!("{} has no file name", file.display()))?;
            fs::copy(&file, dest.join(file_name))?;
        }
        self.world.record(&format!("upload {name}"))
    }

    pub(super) fn download(&self, name: &str, path: &str) -> Result<()> {
        let dest = PathBuf::from(path);
        fs::create_dir_all(&dest)?;
        let source = self.world.state.join("artifacts").join(name);
        if !source.is_dir() {
            return Err(anyhow!("no artifact named {name}"));
        }
        copy_files(&source, &dest)?;
        self.world.record(&format!("download {name}"))
    }

    pub(super) fn download_pattern(&self, pattern: &str, path: &str, merge: bool) -> Result<()> {
        let dest = PathBuf::from(path);
        fs::create_dir_all(&dest)?;
        let prefix = pattern.trim_end_matches('*');
        let mut sources: Vec<PathBuf> = fs::read_dir(self.world.state.join("artifacts"))?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(prefix))
            })
            .collect();
        sources.sort();
        if sources.is_empty() {
            return Err(anyhow!("no artifact matches {pattern}"));
        }
        for source in sources {
            let target = match (merge, source.file_name()) {
                (true, _) => dest.clone(),
                (false, Some(name)) => dest.join(name),
                (false, None) => return Err(anyhow!("artifact has no name")),
            };
            if !merge {
                fs::create_dir_all(&target)?;
            }
            copy_files(&source, &target)?;
        }
        self.world.record(&format!("download {pattern}"))
    }

    /// The files of `pattern`, relative to the checkout unless absolute.
    pub(super) fn expand(&self, pattern: &str) -> Result<Vec<PathBuf>> {
        if let Some(prefix) = pattern.strip_suffix("/*") {
            let dir = self.world.fixture.work.join(prefix);
            let mut files: Vec<PathBuf> = fs::read_dir(&dir)?
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.is_file())
                .collect();
            files.sort();
            Ok(files)
        } else {
            let path = self.world.fixture.work.join(pattern);
            Ok(if path.is_file() {
                vec![path]
            } else {
                Vec::new()
            })
        }
    }
}

fn copy_files(source: &std::path::Path, dest: &std::path::Path) -> Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            fs::copy(entry.path(), dest.join(entry.file_name()))?;
        }
    }
    Ok(())
}
