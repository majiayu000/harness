use super::{internal, ApiError, EvalRunMarker, EvalRunReport, MAX_EVAL_RUNS};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

#[cfg(unix)]
pub(super) fn create_or_open_child(parent: &File, name: &str) -> Result<File, ApiError> {
    match rustix::fs::mkdirat(parent, name, rustix::fs::Mode::RWXU) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
        Err(error) => return Err(internal(error)),
    }
    open_child(parent, name)
}

#[cfg(unix)]
fn open_child(parent: &File, name: &str) -> Result<File, ApiError> {
    rustix::fs::openat(
        parent,
        name,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| ApiError::BadRequest(format!("unsafe eval report directory {name}: {error}")))
}

#[cfg(unix)]
pub(super) fn read_project_file(root: &Path, path: &Path) -> Result<String, ApiError> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| ApiError::BadRequest("eval manifest is outside project root".to_string()))?;
    let mut directory = File::from(
        rustix::fs::open(
            root,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )
        .map_err(internal)?,
    );
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        let name = component.as_os_str();
        if components.peek().is_some() {
            directory = File::from(
                rustix::fs::openat(
                    &directory,
                    name,
                    rustix::fs::OFlags::RDONLY
                        | rustix::fs::OFlags::DIRECTORY
                        | rustix::fs::OFlags::NOFOLLOW,
                    rustix::fs::Mode::empty(),
                )
                .map_err(internal)?,
            );
        } else {
            let mut file = File::from(
                rustix::fs::openat(
                    &directory,
                    name,
                    rustix::fs::OFlags::RDONLY
                        | rustix::fs::OFlags::NOFOLLOW
                        | rustix::fs::OFlags::NONBLOCK
                        | rustix::fs::OFlags::CLOEXEC,
                    rustix::fs::Mode::empty(),
                )
                .map_err(internal)?,
            );
            if !file.metadata().map_err(internal)?.file_type().is_file() {
                return Err(ApiError::BadRequest(
                    "eval manifest is not a regular file".to_string(),
                ));
            }
            let mut content = String::new();
            file.read_to_string(&mut content).map_err(internal)?;
            return Ok(content);
        }
    }
    Err(ApiError::BadRequest(
        "eval manifest path is empty".to_string(),
    ))
}

#[cfg(unix)]
pub(super) struct EvalCandidate {
    pub(super) modified: std::time::SystemTime,
    pub(super) report: Option<EvalRunReport>,
    pub(super) marker: Option<EvalRunMarker>,
    pub(super) label: String,
}

#[cfg(unix)]
fn open_optional_entry(
    parent: &File,
    name: &str,
    directory: bool,
) -> Result<Option<File>, ApiError> {
    let mut flags =
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC;
    if directory {
        flags |= rustix::fs::OFlags::DIRECTORY;
    } else {
        flags |= rustix::fs::OFlags::NONBLOCK;
    }
    match rustix::fs::openat(parent, name, flags, rustix::fs::Mode::empty()) {
        Ok(file) => {
            let file = File::from(file);
            if !directory && !file.metadata().map_err(internal)?.file_type().is_file() {
                return Err(ApiError::BadRequest(format!(
                    "eval entry {name} is not a regular file"
                )));
            }
            Ok(Some(file))
        }
        Err(rustix::io::Errno::NOENT) => Ok(None),
        Err(error) => Err(ApiError::BadRequest(format!(
            "unsafe eval entry {name}: {error}"
        ))),
    }
}

#[cfg(unix)]
pub(super) fn scan_eval_candidates(
    root: &Path,
) -> Result<(Vec<EvalCandidate>, Vec<String>, bool), ApiError> {
    let root_directory = File::from(
        rustix::fs::open(
            root,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )
        .map_err(internal)?,
    );
    let Some(artifacts) = open_optional_entry(&root_directory, "artifacts", true)? else {
        return Ok((Vec::new(), Vec::new(), false));
    };
    let Some(evals) = open_optional_entry(&artifacts, "eval", true)? else {
        return Ok((Vec::new(), Vec::new(), false));
    };
    let mut candidates = Vec::new();
    let mut errors = Vec::new();
    let mut unresolved = false;
    for item in rustix::fs::Dir::read_from(&evals).map_err(internal)? {
        let item = item.map_err(internal)?;
        let name = item.file_name();
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        let label = format!(
            "{}/artifacts/eval/{}",
            root.display(),
            name.to_string_lossy()
        );
        let run_directory = match rustix::fs::openat(
            &evals,
            name,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        ) {
            Ok(directory) => File::from(directory),
            Err(rustix::io::Errno::NOENT | rustix::io::Errno::NOTDIR) => continue,
            Err(error) => {
                errors.push(format!("{label}: unsafe run directory: {error}"));
                unresolved = true;
                continue;
            }
        };
        let report_file = match open_optional_entry(&run_directory, "report.json", false) {
            Ok(report) => report,
            Err(error) => {
                errors.push(format!("{label}: {error}"));
                unresolved = true;
                continue;
            }
        };
        let marker_file = match open_optional_entry(&run_directory, "run-state.json", false) {
            Ok(marker) => marker,
            Err(error) => {
                errors.push(format!("{label}: {error}"));
                unresolved = true;
                continue;
            }
        };
        let modified = report_file
            .as_ref()
            .or(marker_file.as_ref())
            .map(|file| file.metadata().and_then(|metadata| metadata.modified()))
            .transpose()
            .map_err(internal)?;
        let Some(modified) = modified else { continue };
        let marker: Option<EvalRunMarker> = match marker_file {
            Some(file) => match serde_json::from_reader(file) {
                Ok(marker) => Some(marker),
                Err(error) => {
                    errors.push(format!("{label}/run-state.json: {error}"));
                    unresolved = true;
                    None
                }
            },
            None => None,
        };
        let report: Option<EvalRunReport> = match report_file {
            Some(file) => match serde_json::from_reader(file) {
                Ok(report) => Some(report),
                Err(error) => {
                    errors.push(format!("{label}/report.json: {error}"));
                    None
                }
            },
            None => None,
        };
        if let Some(marker) = marker.as_ref() {
            match marker.status.as_str() {
                "running" => match report.as_ref() {
                    Some(saved) if saved.run_id == marker.run_id => {}
                    Some(_) => {
                        errors.push(format!("{label}/report.json: run ID differs from marker"));
                        unresolved = true;
                    }
                    None => unresolved = true,
                },
                "failed" => {}
                _ => {
                    errors.push(format!("{label}/run-state.json: invalid status"));
                    unresolved = true;
                }
            }
        }
        if report.is_none() && marker.is_none() {
            continue;
        }
        candidates.push(EvalCandidate {
            modified,
            report,
            marker,
            label,
        });
        candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.modified));
        candidates.truncate(MAX_EVAL_RUNS);
    }
    Ok((candidates, errors, unresolved))
}

#[cfg(unix)]
#[derive(Debug)]
pub(super) struct EvalReportOutput {
    directory_path: PathBuf,
    directory: File,
}

#[cfg(unix)]
impl EvalReportOutput {
    pub(super) fn create(root: &Path, run_id: &str) -> Result<Self, ApiError> {
        let root_directory = File::from(
            rustix::fs::open(
                root,
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::DIRECTORY
                    | rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            )
            .map_err(internal)?,
        );
        let artifacts = create_or_open_child(&root_directory, "artifacts")?;
        let evals = create_or_open_child(&artifacts, "eval")?;
        rustix::fs::mkdirat(&evals, run_id, rustix::fs::Mode::RWXU).map_err(internal)?;
        let directory = open_child(&evals, run_id)?;
        Ok(Self {
            directory_path: root.join("artifacts/eval").join(run_id),
            directory,
        })
    }

    pub(super) fn write(&self, report: &EvalRunReport) -> Result<(), ApiError> {
        let bytes = serde_json::to_vec_pretty(report).map_err(internal)?;
        self.publish("report.json", &bytes)
    }

    pub(super) fn write_marker(&self, marker: &EvalRunMarker) -> Result<(), ApiError> {
        let bytes = serde_json::to_vec(marker).map_err(internal)?;
        self.publish("run-state.json", &bytes)
    }

    fn publish(&self, name: &str, bytes: &[u8]) -> Result<(), ApiError> {
        let temporary = format!("{name}.{}.tmp", uuid::Uuid::new_v4());

        let result = (|| {
            let file = rustix::fs::openat(
                &self.directory,
                temporary.as_str(),
                rustix::fs::OFlags::WRONLY
                    | rustix::fs::OFlags::CREATE
                    | rustix::fs::OFlags::EXCL
                    | rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            )
            .map_err(internal)?;
            let mut file = File::from(file);
            file.write_all(bytes).map_err(internal)?;
            file.sync_all().map_err(internal)?;
            rustix::fs::renameat(&self.directory, temporary.as_str(), &self.directory, name)
                .map_err(internal)
        })();

        if result.is_err() {
            let cleanup = rustix::fs::unlinkat(
                &self.directory,
                temporary.as_str(),
                rustix::fs::AtFlags::empty(),
            )
            .map_err(std::io::Error::from);
            if let Err(error) = cleanup {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::error!(path = %self.directory_path.display(), %error, "failed to remove incomplete eval report");
                }
            }
        }
        result
    }
}
