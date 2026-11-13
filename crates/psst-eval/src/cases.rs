use std::{
    collections::BTreeSet,
    fmt, fs, io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, bail, ensure};
use psst_config::{Profile, ProfilesFile, ServerFiles};
use psst_dict::Dictionary;
use psst_proto::EngineTrace;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Synthetic,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Synthetic => "synthetic",
        })
    }
}

/// Tuning happens only on `tune`; acceptance is measured on `holdout`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Split {
    Tune,
    Holdout,
}

impl fmt::Display for Split {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Tune => "tune",
            Self::Holdout => "holdout",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    /// Also the clip's name: `clips/<id>.flac`.
    pub id: String,
    pub lang: String,
    pub source: Source,
    pub split: Split,
    pub reference: String,
    #[serde(default)]
    pub terms: Vec<String>,
    /// Any change of its output fails the gate, even an improvement.
    #[serde(default)]
    pub must_pass: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_output: Option<EngineTrace>,
    /// Recorded with `eval.bias = false`: no prompt at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_output_raw: Option<EngineTrace>,
}

impl Case {
    pub fn traces(&self) -> anyhow::Result<Option<(&EngineTrace, &EngineTrace)>> {
        match (&self.engine_output, &self.engine_output_raw) {
            (Some(output), Some(raw)) => Ok(Some((output, raw))),
            (None, None) => Ok(None),
            _ => bail!("case {} has only one of engine_output and engine_output_raw; run `psst-eval audio`", self.id),
        }
    }
}

pub fn read_cases(path: &Path) -> anyhow::Result<Vec<Case>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("cannot read {}", path.display())),
    };
    let mut cases: Vec<Case> = Vec::new();
    let mut ids = BTreeSet::new();
    for (number, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let case: Case =
            serde_json::from_str(line).with_context(|| format!("{}:{}: invalid case", path.display(), number + 1))?;
        ensure!(!case.id.is_empty(), "{}:{}: empty id", path.display(), number + 1);
        ensure!(ids.insert(case.id.clone()), "{}:{}: duplicate id {}", path.display(), number + 1, case.id);
        cases.push(case);
    }
    Ok(cases)
}

pub fn write_cases(path: &Path, cases: &[Case]) -> anyhow::Result<()> {
    let mut text = String::new();
    for case in cases {
        text.push_str(&serde_json::to_string(case).expect("cases serialise"));
        text.push('\n');
    }
    write_atomic(path, text.as_bytes())
}

pub(crate) fn write_atomic(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    let dir = path.parent().filter(|dir| !dir.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir).with_context(|| format!("cannot write in {}", dir.display()))?;
    io::Write::write_all(&mut tmp, contents).with_context(|| format!("cannot write {}", path.display()))?;
    // Keep the file's mode; a new file gets the usual 0644 rather than the temp file's 0600.
    let permissions = fs::metadata(path).map(|m| m.permissions()).unwrap_or_else(|_| fs::Permissions::from_mode(0o644));
    fs::set_permissions(tmp.path(), permissions).with_context(|| format!("cannot write {}", path.display()))?;
    tmp.persist(path).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct EvalSet {
    pub dir: PathBuf,
    pub files: ServerFiles,
    pub dictionary: Dictionary,
    pub cases: Vec<Case>,
}

impl EvalSet {
    pub const CASES: &'static str = "cases.jsonl";
    pub const BASELINE: &'static str = "baseline.json";

    pub fn load(dir: &Path) -> anyhow::Result<Self> {
        ensure!(dir.is_dir(), "{} is not a directory", dir.display());
        let files = ServerFiles::load(dir)?;
        let dictionary = Dictionary::from_file(&files.dictionary)
            .with_context(|| format!("{}: invalid dictionary", dir.join("dictionary.toml").display()))?;
        let cases = read_cases(&dir.join(Self::CASES))?;
        Ok(Self { dir: dir.to_owned(), files, dictionary, cases })
    }

    pub fn cases_path(&self) -> PathBuf {
        self.dir.join(Self::CASES)
    }

    pub fn baseline_path(&self) -> PathBuf {
        self.dir.join(Self::BASELINE)
    }

    pub fn clip_path(&self, id: &str) -> PathBuf {
        self.dir.join("clips").join(format!("{id}.flac"))
    }

    pub fn profile_for(&self, lang: &str) -> anyhow::Result<Profile> {
        profile_for(&self.files.profiles, lang)
    }
}

pub fn profile_for(profiles: &ProfilesFile, lang: &str) -> anyhow::Result<Profile> {
    if let Some(default) = profiles.resolve(psst_config::DEFAULT_PROFILE).filter(|p| p.language == lang) {
        return Ok(default);
    }
    let mut matching = profiles.resolve_all().into_iter().filter(|p| p.language == lang);
    match (matching.next(), matching.next()) {
        (Some(profile), None) => Ok(profile),
        (None, _) => bail!("profiles.toml has no profile for language `{lang}`"),
        (Some(a), Some(b)) => {
            bail!("profiles.toml has several profiles for language `{lang}` ({}, {}…)", a.name, b.name)
        }
    }
}
