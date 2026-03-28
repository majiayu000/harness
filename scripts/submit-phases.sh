#!/bin/bash
# Submit VibeGuard absorption phases 1-8 to harness server sequentially.
# Each task creates a branch, implements the phase, and opens a PR.
# Waits for each task to complete before submitting the next.

set -euo pipefail

HARNESS_URL="http://localhost:9800"

submit_task() {
  local prompt="$1"
  local response
  response=$(curl -s -X POST "$HARNESS_URL/tasks" \
    -H "Content-Type: application/json" \
    -d "{\"prompt\": $(echo "$prompt" | jq -Rs .), \"wait_secs\": 120, \"max_rounds\": 5, \"turn_timeout_secs\": 900}")
  echo "$response" | jq -r '.task_id'
}

wait_for_task() {
  local task_id="$1"
  local phase="$2"
  echo "[$(date +%H:%M)] Phase $phase: task $task_id started, polling..."
  while true; do
    local state
    state=$(curl -s "$HARNESS_URL/tasks/$task_id")
    local status
    status=$(echo "$state" | jq -r '.status')
    local pr_url
    pr_url=$(echo "$state" | jq -r '.pr_url // empty')

    case "$status" in
      done)
        echo "[$(date +%H:%M)] Phase $phase: DONE${pr_url:+ — $pr_url}"
        return 0
        ;;
      failed)
        local error
        error=$(echo "$state" | jq -r '.error // "unknown"')
        echo "[$(date +%H:%M)] Phase $phase: FAILED — $error"
        return 1
        ;;
      *)
        sleep 30
        ;;
    esac
  done
}

run_phase() {
  local phase="$1"
  local prompt="$2"
  echo ""
  echo "=========================================="
  echo "  Phase $phase"
  echo "=========================================="
  local task_id
  task_id=$(submit_task "$prompt")
  if [ -z "$task_id" ] || [ "$task_id" = "null" ]; then
    echo "ERROR: failed to submit Phase $phase"
    return 1
  fi
  wait_for_task "$task_id" "$phase" || true
}

# ── Phase 1: Turn Interceptor Framework ──
run_phase 1 "Implement Phase 1: Turn Interceptor Framework for VibeGuard absorption.

BRANCH: feat/phase1-interceptors (create from current HEAD)

TASK:
1. Create crates/harness-core/src/interceptor.rs (~60 lines):
   - InterceptResult struct with fields: decision (Decision), reason (Option<String>), request (Option<AgentRequest>)
   - TurnInterceptor trait (async_trait, Send+Sync) with methods:
     - fn name(&self) -> &str
     - async fn pre_execute(&self, req: &AgentRequest) -> InterceptResult
     - async fn post_execute(&self, req: &AgentRequest, resp: &AgentResponse)
     - async fn on_error(&self, req: &AgentRequest, error: &str)
   - Default impl for post_execute and on_error (no-op)

2. Add 'pub mod interceptor;' to crates/harness-core/src/lib.rs

3. Add to crates/harness-server/src/http.rs AppState struct:
   pub interceptors: Vec<Arc<dyn harness_core::interceptor::TurnInterceptor>>
   Initialize as empty vec in build_app_state()
   Update all test helpers that create AppState to include interceptors: vec![]

4. Modify crates/harness-server/src/task_executor.rs:
   - Accept interceptors parameter in run_task
   - Before agent.execute(): iterate interceptors, call pre_execute. If any returns Block, abort with error.
   - If pre_execute returns Pass with Some(request), use modified request.
   - After agent.execute(): iterate interceptors, call post_execute.
   - On error: iterate interceptors, call on_error.

5. Update spawn_task in task_runner.rs to pass interceptors through.

6. Add test: mock interceptor that blocks → task fails with blocked message.

VERIFY: cargo check && cargo test (all existing + new tests pass)
THEN: Create branch, commit, push, open PR. Print PR_URL=<url> on last line."

# ── Phase 2: Complexity Routing + Contract Validator ──
run_phase 2 "Implement Phase 2: Complexity Routing and Contract Validation for VibeGuard absorption.

BRANCH: feat/phase2-complexity-routing (create from current HEAD)

TASK:
1. Create crates/harness-server/src/complexity_router.rs (~100 lines):
   - pub fn classify(prompt: &str, issue: Option<u64>, pr: Option<u64>) -> TaskClassification
   - Heuristic: count file path patterns in prompt (paths with extensions like .rs, .ts, .py)
     - 0-2 files → Simple
     - 3-5 files → Medium
     - 6+ files → Complex
   - If issue or pr present, at least Medium
   - TaskClassification is already defined in harness_core::agent

2. Create crates/harness-server/src/contract_validator.rs (~80 lines):
   - Implement TurnInterceptor trait (from Phase 1 / harness_core::interceptor)
   - pre_execute: validate prompt has >= 10 chars, contains action verb
   - Returns Warn (not Block) for missing acceptance criteria
   - Returns Block for empty/too-short prompts

3. Add TaskClassify method to crates/harness-protocol/src/methods.rs:
   TaskClassify { prompt: String, issue: Option<u64>, pr: Option<u64> }

4. Add handler in crates/harness-server/src/handlers/ for TaskClassify:
   Calls classify() and returns the TaskClassification as JSON

5. Wire TaskClassify into router.rs dispatch

6. Add complexity_router and contract_validator modules to lib.rs

7. Tests: simple prompt → Simple, multi-file prompt → Complex, empty prompt → Block

VERIFY: cargo check && cargo test
THEN: Create branch, commit, push, open PR. Print PR_URL=<url> on last line."

# ── Phase 3: Built-in System Skills ──
run_phase 3 "Implement Phase 3: Embed 10 Built-in Skills for VibeGuard absorption.

BRANCH: feat/phase3-builtin-skills (create from current HEAD)

TASK:
1. Create directory skills/ at project root with 10 markdown files:
   - interview.md: Deep requirements interview before large features. Input: task description. Output: structured SPEC with scope, constraints, risks, acceptance criteria.
   - exec-plan.md: Generate execution plan from SPEC. Track milestones, decisions, surprises across sessions.
   - preflight.md: Pre-implementation constraint generation. Scan applicable rules, identify affected files, estimate complexity, output constraints list.
   - check.md: Run all guard scripts, output project health report with pass/warn/block counts and recommendations.
   - build-fix.md: Read build errors, locate root cause, apply minimal fix, verify build passes.
   - review.md: Structured code review. Run guards for baseline, then review security→logic→quality→performance.
   - cross-review.md: Dual-model adversarial review. Primary reviews with P0-P3, challenger validates. Iterate up to 3 rounds.
   - learn.md: Extract reusable rules or skills from error patterns or successful fixes.
   - gc.md: Garbage collection. Archive old logs, clean worktrees, scan for code smells.
   - stats.md: Aggregate hook statistics. Show pass/warn/block rates, compliance trends, top violated rules.

   Each file format:
   # <Name>
   ## Trigger
   <when to use>
   ## Input
   <what the agent receives>
   ## Procedure
   <step by step instructions>
   ## Output Format
   <structured output template>
   ## Constraints
   <guardrails>

2. Add load_builtin() method to crates/harness-skills/src/store.rs:
   pub fn load_builtin(&mut self) {
       let builtins = [
           (\"interview\", include_str!(\"../../../skills/interview.md\")),
           // ... 9 more
       ];
       for (name, content) in builtins {
           // Only add if no existing skill with same name (dedup)
           if self.get_by_name(name).is_none() {
               self.skills.push(Skill {
                   id: SkillId::from_str(name),
                   name: name.to_string(),
                   description: content.lines().next().unwrap_or(\"\").to_string(),
                   content: content.to_string(),
                   trigger_patterns: vec![],
                   version: \"1.0\".to_string(),
                   author: \"system\".to_string(),
                   location: SkillLocation::System,
               });
           }
       }
   }
   Note: You may need to add a get_by_name() helper or use existing search.

3. Call skills.load_builtin() in crates/harness-server/src/http.rs build_app_state() after creating SkillStore.

4. Tests: load_builtin gives 10 skills, all location System. User skill with same name overrides.

VERIFY: cargo check && cargo test
THEN: Create branch, commit, push, open PR. Print PR_URL=<url> on last line."

# ── Phase 4: Preflight Constraint Generation ──
run_phase 4 "Implement Phase 4: Preflight Constraint Generation for VibeGuard absorption.

BRANCH: feat/phase4-preflight (create from current HEAD)

TASK:
1. Create crates/harness-server/src/handlers/preflight.rs (~120 lines):
   - PreflightResult struct: constraints (Vec<String>), affected_files (Vec<PathBuf>), baseline_violations (usize), recommended_complexity (TaskComplexity)
   - pub async fn run_preflight(agent, skills, rules, project_root, task_description) -> Result<PreflightResult>
   - Loads 'preflight' skill from SkillStore
   - Gathers applicable rules from RuleEngine
   - Builds prompt with task description + rules + project context
   - Executes agent with read-only context
   - Parses structured output for CONSTRAINTS, AFFECTED_FILES, RISK, COMPLEXITY sections

2. Add Preflight method to crates/harness-protocol/src/methods.rs:
   Preflight { project_root: PathBuf, task_description: String }

3. Add handler that calls run_preflight and returns PreflightResult as JSON

4. Wire Preflight into router.rs dispatch and handlers/mod.rs

5. Tests: preflight returns structured result with constraints list

VERIFY: cargo check && cargo test
THEN: Create branch, commit, push, open PR. Print PR_URL=<url> on last line."

# ── Phase 5: Learn Feedback Loop ──
run_phase 5 "Implement Phase 5: Learn Feedback Loop for VibeGuard absorption.

BRANCH: feat/phase5-learn-feedback (create from current HEAD)

TASK:
1. Create crates/harness-server/src/handlers/learn.rs (~130 lines):
   - pub async fn learn_rules(agent, gc_agent, rules, project_root) -> Result<Vec<Rule>>
     Reads adopted drafts from gc_agent.drafts(), builds prompt asking agent to extract reusable guard rules, parses output into Rule structs, returns them.
   - pub async fn learn_skills(agent, gc_agent, skills, project_root) -> Result<Vec<Skill>>
     Same but extracts reusable skills from adopted drafts.

2. Add add_rule(&mut self, rule: Rule) to crates/harness-rules/src/engine.rs:
   Dedup by rule_id — if exists, skip.

3. Add to crates/harness-protocol/src/methods.rs:
   LearnRules { project_root: PathBuf }
   LearnSkills { project_root: PathBuf }

4. Add handlers and wire into router.rs dispatch

5. Tests: mock adopted draft → learn extracts rule with correct ID/severity

VERIFY: cargo check && cargo test
THEN: Create branch, commit, push, open PR. Print PR_URL=<url> on last line."

# ── Phase 6: Health Report + Stats ──
run_phase 6 "Implement Phase 6: Health Report and Stats for VibeGuard absorption.

BRANCH: feat/phase6-health-stats (create from current HEAD)

TASK:
1. Create crates/harness-observe/src/health.rs (~120 lines):
   - ViolationSummary: rule_id, count, severity
   - SignalSummary: signal_type, count, last_detected
   - EventSummary: total, pass_count, warn_count, block_count, escalate_count
   - HealthReport: quality (QualityReport), violation_summary, signal_summary, event_summary, recommendations (Vec<String>)
   - pub fn generate_health_report(events, violations) -> HealthReport
     Groups violations by rule_id, counts event decisions, generates recommendations based on findings

2. Create crates/harness-observe/src/stats.rs (~100 lines):
   - HookStats: hook (String), total (usize), pass_rate (f64), warn_rate (f64), block_rate (f64)
   - ComplianceTrend: period (String), pass_rate (f64), violation_count (usize), grade (Grade)
   - pub fn aggregate_hook_stats(events) -> Vec<HookStats>
   - pub fn compute_trends(events, period_days) -> Vec<ComplianceTrend>

3. Add pub mod health; pub mod stats; to crates/harness-observe/src/lib.rs
   Reference these types through module paths: health::HealthReport, stats::HookStats, stats::ComplianceTrend.

4. Add to crates/harness-protocol/src/methods.rs:
   HealthCheck { project_root: PathBuf }
   StatsQuery { since: Option<chrono::DateTime<chrono::Utc>>, until: Option<chrono::DateTime<chrono::Utc>> }

5. Add handlers and wire into router.rs

6. Tests: zero events → Grade A. Events with violations → degraded. Stats groups by hook.

VERIFY: cargo check && cargo test
THEN: Create branch, commit, push, open PR. Print PR_URL=<url> on last line."

# ── Phase 7: Cross-Review Orchestration ──
run_phase 7 "Implement Phase 7: Cross-Review Orchestration for VibeGuard absorption.

BRANCH: feat/phase7-cross-review (create from current HEAD)

TASK:
1. Create crates/harness-server/src/handlers/cross_review.rs (~130 lines):
   - CrossReviewResult struct: primary_review (String), challenger_review (String), consensus_issues (Vec<String>), contested_issues (Vec<String>), rounds (u32), final_verdict (String: APPROVED or NOT_CONVERGED)
   - pub async fn run_cross_review(primary_agent, challenger_agent, project_root, target, max_rounds) -> Result<CrossReviewResult>
   - Flow: primary reviews → challenger challenges (CONFIRMED/FALSE-POSITIVE/MISSED) → iterate up to max_rounds
   - Graceful degradation: if challenger unavailable, single-model review

2. Add to crates/harness-protocol/src/methods.rs:
   CrossReview { project_root: PathBuf, target: String, max_rounds: Option<u32> }

3. Add handler, wire into router.rs dispatch

4. Tests: two mock agents → consensus extracted. Single agent → graceful degradation.

VERIFY: cargo check && cargo test
THEN: Create branch, commit, push, open PR. Print PR_URL=<url> on last line."

# ── Phase 8: Periodic Scheduling ──
run_phase 8 "Implement Phase 8: Periodic Scheduling for VibeGuard absorption.

BRANCH: feat/phase8-scheduler (create from current HEAD)

TASK:
1. Create crates/harness-server/src/scheduler.rs (~100 lines):
   - Scheduler struct with gc_interval (Duration) and health_interval (Duration)
   - pub fn from_grade(grade: Grade) -> Self
     A → 7d, B → 3d, C → 1d, D → 1h for gc_interval
     health_interval always 24h
   - pub fn start(self, state: Arc<AppState>)
     Spawns background tokio tasks:
     - GC task: loop { sleep(gc_interval); trigger GcRun }
     - Health task: loop { sleep(health_interval); log health report }

2. Add pub mod scheduler; to crates/harness-server/src/lib.rs

3. Optionally start scheduler in crates/harness-server/src/http.rs serve() after building AppState
   (compute initial grade from events, create Scheduler, start it)

4. Tests: from_grade(Grade::D) → 1h interval. from_grade(Grade::A) → 7d interval.

VERIFY: cargo check && cargo test
THEN: Create branch, commit, push, open PR. Print PR_URL=<url> on last line."

echo ""
echo "=========================================="
echo "  All phases submitted"
echo "=========================================="
