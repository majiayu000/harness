use super::{write_json_output, write_string_output, EvalPromotionSummaryArgs};
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

pub(super) fn run(args: EvalPromotionSummaryArgs) -> anyhow::Result<i32> {
    let summary = promotion_summary(args)?;
    Ok(summary.exit_code)
}

fn promotion_summary(args: EvalPromotionSummaryArgs) -> anyhow::Result<PromotionSummary> {
    let input = read_promotion_summary_input(&args.input)?;
    let summary = build_promotion_summary(input);
    let markdown = render_promotion_summary_markdown(&summary);
    write_json_output(&summary, args.output.as_deref())?;
    write_string_output(&markdown, args.markdown_output.as_deref())?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        print!("{markdown}");
    }
    Ok(summary)
}

fn read_promotion_summary_input(path: &Path) -> anyhow::Result<PromotionSummaryInput> {
    let content = fs::read_to_string(path).with_context(|| {
        format!(
            "failed to read promotion summary input at {}",
            path.display()
        )
    })?;
    serde_json::from_str(&content)
        .with_context(|| format!("invalid promotion summary input {}", path.display()))
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct PromotionSummaryInput {
    #[serde(default)]
    suite: Option<String>,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    baseline: Option<String>,
    #[serde(default)]
    candidate: Option<String>,
    decision: PromotionDecision,
    #[serde(default)]
    no_change: bool,
    #[serde(default)]
    changes: Vec<String>,
    #[serde(default)]
    regressions: Vec<String>,
    #[serde(default)]
    gaps: Vec<String>,
    #[serde(default)]
    rules: Vec<String>,
    #[serde(default)]
    engine_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct PromotionSummary {
    suite: Option<String>,
    subject: Option<String>,
    baseline: Option<String>,
    candidate: Option<String>,
    decision: PromotionDecision,
    exit_code: i32,
    no_change: bool,
    changes: Vec<String>,
    regressions: Vec<String>,
    gaps: Vec<String>,
    rules: Vec<String>,
    engine_error: Option<String>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum PromotionDecision {
    #[serde(rename = "PROMOTE", alias = "promote")]
    Promote,
    #[serde(rename = "REVIEW", alias = "review")]
    Review,
    #[serde(rename = "BLOCK", alias = "block")]
    Block,
    #[serde(rename = "NO_BASELINE", alias = "no_baseline", alias = "no-baseline")]
    NoBaseline,
    #[serde(
        rename = "ENGINE_ERROR",
        alias = "engine_error",
        alias = "engine-error"
    )]
    EngineError,
}

impl PromotionDecision {
    const fn exit_code(self) -> i32 {
        match self {
            Self::Promote => 0,
            Self::Review | Self::NoBaseline => 2,
            Self::Block => 3,
            Self::EngineError => 1,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Promote => "PROMOTE",
            Self::Review => "REVIEW",
            Self::Block => "BLOCK",
            Self::NoBaseline => "NO_BASELINE",
            Self::EngineError => "ENGINE_ERROR",
        }
    }
}

fn build_promotion_summary(input: PromotionSummaryInput) -> PromotionSummary {
    let mut gaps = input.gaps;
    let mut rules = input.rules;
    if input.decision == PromotionDecision::NoBaseline && gaps.is_empty() {
        gaps.push("baseline evidence is missing".to_string());
    }
    if input.decision == PromotionDecision::EngineError
        && input
            .engine_error
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .is_empty()
    {
        rules.push("engine error was reported without details".to_string());
    }
    PromotionSummary {
        suite: input.suite,
        subject: input.subject,
        baseline: input.baseline,
        candidate: input.candidate,
        decision: input.decision,
        exit_code: input.decision.exit_code(),
        no_change: input.no_change,
        changes: input.changes,
        regressions: input.regressions,
        gaps,
        rules,
        engine_error: input.engine_error,
    }
}

fn render_promotion_summary_markdown(summary: &PromotionSummary) -> String {
    let mut output = String::new();
    output.push_str("# Agent Stack Regression\n\n");
    output.push_str(&format!("decision: `{}`\n", summary.decision.label()));
    output.push_str(&format!("exit_code: `{}`\n", summary.exit_code));
    output.push_str(&format!("no_change: `{}`\n", summary.no_change));
    append_optional_line(&mut output, "suite", summary.suite.as_deref());
    append_optional_line(&mut output, "subject", summary.subject.as_deref());
    append_optional_line(&mut output, "baseline", summary.baseline.as_deref());
    append_optional_line(&mut output, "candidate", summary.candidate.as_deref());
    append_optional_line(&mut output, "engine_error", summary.engine_error.as_deref());
    append_markdown_list(&mut output, "Changes", &summary.changes);
    append_markdown_list(&mut output, "Regressions", &summary.regressions);
    append_markdown_list(&mut output, "Gaps", &summary.gaps);
    append_markdown_list(&mut output, "Rules", &summary.rules);
    output
}

fn append_optional_line(output: &mut String, label: &str, value: Option<&str>) {
    if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
        output.push_str(&format!("{label}: `{}`\n", value.trim()));
    }
}

fn append_markdown_list(output: &mut String, heading: &str, items: &[String]) {
    output.push_str(&format!("\n## {heading}\n"));
    if items.is_empty() {
        output.push_str("- none\n");
    } else {
        for item in items {
            output.push_str(&format!("- {}\n", item.trim()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::EvalPromotionSummaryArgs;
    use super::*;

    #[test]
    fn promotion_summary_maps_decisions_to_stable_exit_codes() {
        let decisions = [
            (PromotionDecision::Promote, 0),
            (PromotionDecision::Review, 2),
            (PromotionDecision::NoBaseline, 2),
            (PromotionDecision::Block, 3),
            (PromotionDecision::EngineError, 1),
        ];
        for (decision, expected_code) in decisions {
            let summary = build_promotion_summary(PromotionSummaryInput {
                suite: Some("agent-stack".to_string()),
                subject: Some("regression".to_string()),
                baseline: Some("baseline.json".to_string()),
                candidate: Some("candidate.json".to_string()),
                decision,
                no_change: decision == PromotionDecision::Promote,
                changes: Vec::new(),
                regressions: Vec::new(),
                gaps: Vec::new(),
                rules: Vec::new(),
                engine_error: (decision == PromotionDecision::EngineError)
                    .then(|| "diff engine failed".to_string()),
            });

            assert_eq!(summary.exit_code, expected_code, "{decision:?}");
        }
    }

    #[test]
    fn promotion_summary_keeps_no_change_separate_from_verdict() {
        let summary = build_promotion_summary(PromotionSummaryInput {
            suite: Some("agent-stack".to_string()),
            subject: None,
            baseline: None,
            candidate: None,
            decision: PromotionDecision::Review,
            no_change: true,
            changes: Vec::new(),
            regressions: vec!["policy check requires review".to_string()],
            gaps: Vec::new(),
            rules: vec!["review decisions fail the required check".to_string()],
            engine_error: None,
        });
        let rendered = render_promotion_summary_markdown(&summary);

        assert_eq!(summary.exit_code, 2);
        assert!(summary.no_change);
        assert!(rendered.contains("decision: `REVIEW`"));
        assert!(rendered.contains("no_change: `true`"));
        assert!(rendered.contains("policy check requires review"));
    }

    #[test]
    fn promotion_summary_writes_json_and_markdown_outputs() {
        let tempdir = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("tempdir should be creatable: {error}"));
        let input_path = tempdir.path().join("input.json");
        let json_path = tempdir.path().join("nested").join("summary.json");
        let markdown_path = tempdir.path().join("nested").join("summary.md");
        fs::write(
            &input_path,
            r#"{
              "suite": "agent-stack",
              "subject": "regression",
              "decision": "no_baseline",
              "no_change": true
            }"#,
        )
        .unwrap_or_else(|error| panic!("input should write: {error}"));

        let summary = promotion_summary(EvalPromotionSummaryArgs {
            input: input_path,
            json: true,
            output: Some(json_path.clone()),
            markdown_output: Some(markdown_path.clone()),
        })
        .unwrap_or_else(|error| panic!("promotion summary should render: {error}"));

        assert_eq!(summary.decision, PromotionDecision::NoBaseline);
        assert_eq!(summary.exit_code, 2);
        assert!(summary
            .gaps
            .contains(&"baseline evidence is missing".to_string()));
        let json = fs::read_to_string(json_path)
            .unwrap_or_else(|error| panic!("json should read: {error}"));
        assert!(json.contains("\"decision\": \"NO_BASELINE\""));
        let markdown = fs::read_to_string(markdown_path)
            .unwrap_or_else(|error| panic!("markdown should read: {error}"));
        assert!(markdown.contains("# Agent Stack Regression"));
        assert!(markdown.contains("decision: `NO_BASELINE`"));
    }
}
