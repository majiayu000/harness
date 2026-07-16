use super::*;

#[test]
fn declarative_submission_requires_a_non_empty_prompt() {
    for prompt in [None, Some("   ".to_string())] {
        let request = CreateTaskRequest {
            definition_id: Some("docs_review".to_string()),
            prompt,
            ..Default::default()
        };

        assert!(matches!(
            DefaultExecutionService::validate_request(&request),
            Err(EnqueueTaskError::BadRequest(message))
                if message == "declarative workflow submissions require a non-empty prompt"
        ));
    }
}
