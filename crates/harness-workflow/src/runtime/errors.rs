#[derive(Debug, thiserror::Error)]
#[error("runtime job not found: {runtime_job_id}")]
pub struct RuntimeJobNotFoundError {
    runtime_job_id: String,
}

impl RuntimeJobNotFoundError {
    pub fn new(runtime_job_id: impl Into<String>) -> Self {
        Self {
            runtime_job_id: runtime_job_id.into(),
        }
    }

    pub fn runtime_job_id(&self) -> &str {
        &self.runtime_job_id
    }
}
