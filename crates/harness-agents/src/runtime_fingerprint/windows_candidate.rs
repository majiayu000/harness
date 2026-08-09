//! Lazily materialized Windows resolution candidates.

use harness_core::stack::Sha256Digest;
use std::sync::{Arc, OnceLock};

#[derive(Debug, Clone)]
enum CandidatePath {
    Exact(Vec<u16>),
    Search {
        base: Arc<[u16]>,
        relative: Vec<u16>,
        command: Arc<[u16]>,
    },
}

#[derive(Debug, Clone)]
pub struct WindowsResolvedCandidate {
    reference: CandidatePath,
    materialized_path: OnceLock<Vec<u16>>,
    candidate_digest: OnceLock<Sha256Digest>,
}

impl WindowsResolvedCandidate {
    pub(super) fn exact(path: Vec<u16>) -> Self {
        Self {
            reference: CandidatePath::Exact(path),
            materialized_path: OnceLock::new(),
            candidate_digest: OnceLock::new(),
        }
    }

    pub(super) fn search(base: Arc<[u16]>, relative: Vec<u16>, command: Arc<[u16]>) -> Self {
        Self {
            reference: CandidatePath::Search {
                base,
                relative,
                command,
            },
            materialized_path: OnceLock::new(),
            candidate_digest: OnceLock::new(),
        }
    }

    pub fn path(&self) -> &[u16] {
        match &self.reference {
            CandidatePath::Exact(path) => path,
            CandidatePath::Search {
                base,
                relative,
                command,
            } => self
                .materialized_path
                .get_or_init(|| materialize_search_path(base, relative, command)),
        }
    }

    pub fn candidate_digest(&self) -> &Sha256Digest {
        self.candidate_digest.get_or_init(|| {
            super::windows_resolution::digest_units(
                super::windows_resolution::CANDIDATE_DOMAIN,
                self.path(),
            )
        })
    }

    #[cfg(test)]
    pub(super) fn is_materialized(&self) -> bool {
        self.materialized_path.get().is_some() || self.candidate_digest.get().is_some()
    }
}

fn materialize_search_path(base: &[u16], relative: &[u16], command: &[u16]) -> Vec<u16> {
    let mut path = Vec::with_capacity(base.len() + relative.len() + command.len() + 2);
    path.extend_from_slice(base);
    if !relative.is_empty() {
        append_joined(&mut path, relative);
    }
    append_joined(&mut path, command);
    path
}

fn append_joined(path: &mut Vec<u16>, value: &[u16]) {
    if !path.last().is_some_and(|unit| is_separator(*unit)) {
        path.push(u16::from(b'\\'));
    }
    path.extend_from_slice(value);
}

fn is_separator(unit: u16) -> bool {
    unit == u16::from(b'\\') || unit == u16::from(b'/')
}
