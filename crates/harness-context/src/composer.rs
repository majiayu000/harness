use crate::{
    BudgetManifest, BytesDivFourEstimator, ComposeConfig, ComposeError, ComposeManifest,
    ComposeMode, ComposeRequest, Composition, ContextItem, ContextProvider, ContextQuotas,
    Degraded, Estimator, ItemClass, ManifestDecision, ManifestItem, Priority, ProviderError,
    ProviderId, Tokens,
};
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::sync::{mpsc, Arc};
use std::time::Duration;

#[derive(Clone)]
struct Proposal {
    provider_id: ProviderId,
    item: ContextItem,
    original_index: usize,
}

#[derive(Clone)]
struct SelectedItem {
    proposal: Proposal,
    content: String,
    tokens: Tokens,
    level: Option<String>,
    reason: Option<String>,
    nap: Option<harness_core::compress::NapStatus>,
    score: f32,
}

struct ManifestBuild {
    total: Tokens,
    effective: Tokens,
    used: Tokens,
    manifest_items: Vec<Option<ManifestItem>>,
    provider_errors: Vec<ProviderError>,
    warnings: Vec<String>,
}

pub struct ContextComposer {
    config: ComposeConfig,
    estimator: Box<dyn Estimator>,
    providers: Vec<Arc<dyn ContextProvider>>,
}

impl ContextComposer {
    pub fn new(config: ComposeConfig) -> Self {
        Self {
            config,
            estimator: Box::new(BytesDivFourEstimator),
            providers: Vec::new(),
        }
    }

    pub fn with_estimator(mut self, estimator: Box<dyn Estimator>) -> Self {
        self.estimator = estimator;
        self
    }

    pub fn with_provider(mut self, provider: Box<dyn ContextProvider>) -> Self {
        self.providers.push(Arc::from(provider));
        self
    }

    pub fn compose(&self, req: &ComposeRequest) -> Result<Composition, ComposeError> {
        self.compose_supplied(req, Vec::new())
    }

    pub fn compose_supplied(
        &self,
        req: &ComposeRequest,
        supplied_items: Vec<ContextItem>,
    ) -> Result<Composition, ComposeError> {
        let mut proposals = Vec::new();
        let mut provider_errors = Vec::new();
        let mut original_index = 0usize;

        for provider in &self.providers {
            let provider_id = provider.id();
            match self.propose_with_timeout(provider.clone(), provider_id.clone(), req) {
                Ok(items) => {
                    for item in items {
                        proposals.push(self.normalize_proposal(
                            provider_id.clone(),
                            item,
                            original_index,
                        ));
                        original_index += 1;
                    }
                }
                Err(mut error) => {
                    if error.provider_id.as_str().is_empty() {
                        error.provider_id = provider_id;
                    }
                    tracing::error!(
                        provider_id = %error.provider_id,
                        error = %error.message,
                        "context provider failed"
                    );
                    provider_errors.push(error);
                }
            }
        }

        if !supplied_items.is_empty() {
            let provider_id = ProviderId::new("supplied");
            for item in supplied_items {
                proposals.push(self.normalize_proposal(provider_id.clone(), item, original_index));
                original_index += 1;
            }
        }

        self.compose_proposals(req, proposals, provider_errors)
    }

    fn propose_with_timeout(
        &self,
        provider: Arc<dyn ContextProvider>,
        provider_id: ProviderId,
        req: &ComposeRequest,
    ) -> Result<Vec<ContextItem>, ProviderError> {
        let timeout = Duration::from_millis(self.config.provider_timeout_ms);
        if timeout.is_zero() {
            return provider.propose(req);
        }

        let req = req.clone();
        let (tx, rx) = mpsc::channel();
        let send_provider_id = provider_id.clone();
        std::thread::spawn(move || {
            let result = provider.propose(&req);
            if tx.send(result).is_err() {
                tracing::debug!(
                    provider_id = %send_provider_id,
                    "context provider result arrived after timeout"
                );
            }
        });

        match rx.recv_timeout(timeout) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => Err(ProviderError::new(
                provider_id.as_str(),
                format!(
                    "provider timed out after {} ms",
                    self.config.provider_timeout_ms
                ),
            )),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(ProviderError::new(
                provider_id.as_str(),
                "provider worker exited before returning context proposals",
            )),
        }
    }

    fn normalize_proposal(
        &self,
        provider_id: ProviderId,
        mut item: ContextItem,
        original_index: usize,
    ) -> Proposal {
        item.est_tokens = self.estimator.estimate(&item.content);
        Proposal {
            provider_id,
            item,
            original_index,
        }
    }

    fn compose_proposals(
        &self,
        req: &ComposeRequest,
        proposals: Vec<Proposal>,
        provider_errors: Vec<ProviderError>,
    ) -> Result<Composition, ComposeError> {
        let total_budget = if req.budget_hint == 0 {
            self.config.budget_tokens
        } else {
            req.budget_hint
        };
        let reserved_headroom = self.config.reserved_headroom.clamp(0.0, 0.95);
        let effective_budget =
            ((total_budget as f32) * (1.0 - reserved_headroom)).floor() as Tokens;
        let mut manifest_items: Vec<Option<ManifestItem>> = vec![None; proposals.len()];

        let mut warnings = Vec::new();
        let proposed_instruction_count = proposals
            .iter()
            .filter(|proposal| proposal.item.instruction_bearing)
            .count();
        if proposed_instruction_count > 15 {
            warnings.push(format!("constraint_overload:{proposed_instruction_count}"));
        }

        let survivor_indexes = dedupe(&proposals, &mut manifest_items, self.estimator.as_ref());
        let mut survivors = survivor_indexes
            .into_iter()
            .map(|idx| proposals[idx].clone())
            .collect::<Vec<_>>();
        survivors.sort_by(compare_selection_order);

        let mut selected = Vec::new();
        let mut used: Tokens = 0;
        let mut p0_total: Tokens = 0;
        for proposal in survivors
            .iter()
            .filter(|proposal| proposal.item.priority == Priority::P0)
        {
            p0_total = p0_total.saturating_add(proposal.item.est_tokens);
        }

        if p0_total > effective_budget {
            for proposal in survivors {
                if proposal.item.priority == Priority::P0 {
                    manifest_items[proposal.original_index] = Some(excluded_manifest_item(
                        &proposal,
                        proposal.item.est_tokens,
                        "mandatory_overflow",
                    ));
                } else {
                    manifest_items[proposal.original_index] = Some(excluded_manifest_item(
                        &proposal,
                        proposal.item.est_tokens,
                        "blocked_by_mandatory_overflow",
                    ));
                }
            }
            let manifest = build_manifest(
                req,
                self.config.mode,
                ManifestBuild {
                    total: total_budget,
                    effective: effective_budget,
                    used: 0,
                    manifest_items,
                    provider_errors,
                    warnings,
                },
            );
            return Err(ComposeError::MandatoryOverflow {
                mandatory: p0_total,
                effective: effective_budget,
                manifest: Box::new(manifest),
            });
        }

        let mut remaining = Vec::new();
        for proposal in survivors {
            if proposal.item.priority == Priority::P0 {
                used = used.saturating_add(proposal.item.est_tokens);
                selected.push(SelectedItem {
                    score: score(&proposal, &self.config.quotas),
                    tokens: proposal.item.est_tokens,
                    content: proposal.item.content.clone(),
                    level: None,
                    reason: None,
                    nap: None,
                    proposal,
                });
            } else {
                remaining.push(proposal);
            }
        }

        let quota_budget = effective_budget.saturating_sub(used);
        let mut class_remaining = BTreeMap::new();
        for class in [
            ItemClass::Rule,
            ItemClass::Skill,
            ItemClass::Contract,
            ItemClass::Brief,
            ItemClass::Draft,
        ] {
            let class_budget =
                ((quota_budget as f32) * self.config.quotas.for_class(class)).floor() as Tokens;
            class_remaining.insert(class, class_budget);
        }

        let mut deferred = Vec::new();
        for class in [
            ItemClass::Rule,
            ItemClass::Skill,
            ItemClass::Contract,
            ItemClass::Brief,
            ItemClass::Draft,
        ] {
            let mut class_items = remaining
                .iter()
                .filter(|proposal| proposal.item.class == class)
                .cloned()
                .collect::<Vec<_>>();
            class_items.sort_by(|left, right| compare_scored(left, right, &self.config.quotas));
            for proposal in class_items {
                let class_budget = class_remaining.entry(class).or_insert(0);
                if let Some(choice) = choose_fit(
                    &proposal,
                    *class_budget,
                    self.estimator.as_ref(),
                    &self.config.quotas,
                    "quota",
                ) {
                    *class_budget = class_budget.saturating_sub(choice.tokens);
                    used = used.saturating_add(choice.tokens);
                    selected.push(choice);
                } else {
                    deferred.push(proposal);
                }
            }
        }

        let mut global_remaining = effective_budget.saturating_sub(used);
        for unused in class_remaining.values() {
            global_remaining = global_remaining.saturating_add(*unused);
        }
        global_remaining = global_remaining.min(effective_budget.saturating_sub(used));
        deferred.sort_by(|left, right| compare_scored(left, right, &self.config.quotas));
        let mut excluded = Vec::new();
        for proposal in deferred {
            if let Some(choice) = choose_fit(
                &proposal,
                global_remaining,
                self.estimator.as_ref(),
                &self.config.quotas,
                "redistribute",
            ) {
                global_remaining = global_remaining.saturating_sub(choice.tokens);
                used = used.saturating_add(choice.tokens);
                selected.push(choice);
            } else {
                excluded.push(proposal);
            }
        }

        apply_constraint_guard(
            &mut selected,
            &mut used,
            self.estimator.as_ref(),
            &mut warnings,
        );
        selected.sort_by(compare_selected_render_order);

        for item in &selected {
            manifest_items[item.proposal.original_index] = Some(ManifestItem {
                id: item.proposal.item.id.clone(),
                provider_id: item.proposal.provider_id.clone(),
                class: item.proposal.item.class,
                decision: if item.level.is_some() {
                    ManifestDecision::Degraded
                } else {
                    ManifestDecision::Included
                },
                tokens: item.tokens,
                level: item.level.clone(),
                reason: item.reason.clone(),
                original_tokens: item.nap.is_some().then_some(item.proposal.item.est_tokens),
                compressed_tokens: item.nap.is_some().then_some(item.tokens),
                nap: item.nap,
            });
        }
        for proposal in excluded {
            manifest_items[proposal.original_index] = Some(excluded_manifest_item(
                &proposal,
                proposal.item.est_tokens,
                "budget",
            ));
        }

        let rendered = render_context(&selected);
        let manifest = build_manifest(
            req,
            self.config.mode,
            ManifestBuild {
                total: total_budget,
                effective: effective_budget,
                used,
                manifest_items,
                provider_errors,
                warnings,
            },
        );
        Ok(Composition { rendered, manifest })
    }
}

fn build_manifest(
    req: &ComposeRequest,
    mode: ComposeMode,
    input: ManifestBuild,
) -> ComposeManifest {
    let mut items = input
        .manifest_items
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    items.sort_by(|left, right| {
        left.id
            .cmp(&right.id)
            .then_with(|| left.provider_id.cmp(&right.provider_id))
    });
    ComposeManifest {
        v: 1,
        thread_id: req.thread_id.clone(),
        run_id: req.run_id.clone(),
        mode,
        budget: BudgetManifest {
            total: input.total,
            effective: input.effective,
            used: input.used,
        },
        items,
        provider_errors: input.provider_errors,
        warnings: input.warnings,
    }
}

fn dedupe(
    proposals: &[Proposal],
    manifest_items: &mut [Option<ManifestItem>],
    estimator: &dyn Estimator,
) -> Vec<usize> {
    let mut survivors_by_key: HashMap<String, usize> = HashMap::new();
    let mut survivor_indexes = Vec::new();

    for (idx, proposal) in proposals.iter().enumerate() {
        let Some(key) = proposal.item.dedupe_key.clone() else {
            survivor_indexes.push(idx);
            continue;
        };
        match survivors_by_key.get(&key).copied() {
            Some(existing_idx) => {
                let existing = &proposals[existing_idx];
                let winner = if compare_dedupe_survivor(proposal, existing) == Ordering::Less {
                    idx
                } else {
                    existing_idx
                };
                let loser = if winner == idx { existing_idx } else { idx };
                manifest_items[proposals[loser].original_index] = Some(excluded_manifest_item(
                    &proposals[loser],
                    estimator.estimate(&proposals[loser].item.content),
                    "dedupe",
                ));
                if winner == idx {
                    survivors_by_key.insert(key, idx);
                    survivor_indexes.retain(|existing| *existing != existing_idx);
                    survivor_indexes.push(idx);
                }
            }
            None => {
                survivors_by_key.insert(key, idx);
                survivor_indexes.push(idx);
            }
        }
    }

    survivor_indexes
}

fn compare_dedupe_survivor(left: &Proposal, right: &Proposal) -> Ordering {
    left.item
        .priority
        .rank()
        .cmp(&right.item.priority.rank())
        .then_with(|| {
            provider_precedence(&left.provider_id).cmp(&provider_precedence(&right.provider_id))
        })
        .then_with(|| left.item.id.cmp(&right.item.id))
}

fn compare_selection_order(left: &Proposal, right: &Proposal) -> Ordering {
    left.item
        .priority
        .rank()
        .cmp(&right.item.priority.rank())
        .then_with(|| compare_scored(left, right, &ContextQuotas::default()))
}

fn compare_scored(left: &Proposal, right: &Proposal, quotas: &ContextQuotas) -> Ordering {
    let left_score = score(left, quotas);
    let right_score = score(right, quotas);
    right_score
        .partial_cmp(&left_score)
        .unwrap_or(Ordering::Equal)
        .then_with(|| left.item.id.cmp(&right.item.id))
}

fn score(proposal: &Proposal, quotas: &ContextQuotas) -> f32 {
    proposal.item.relevance.clamp(0.0, 1.0) * quotas.for_class(proposal.item.class)
}

fn choose_fit(
    proposal: &Proposal,
    available: Tokens,
    estimator: &dyn Estimator,
    quotas: &ContextQuotas,
    reason: &str,
) -> Option<SelectedItem> {
    if proposal.item.est_tokens <= available {
        return Some(SelectedItem {
            proposal: proposal.clone(),
            content: proposal.item.content.clone(),
            tokens: proposal.item.est_tokens,
            level: None,
            reason: None,
            nap: None,
            score: score(proposal, quotas),
        });
    }
    for degraded in &proposal.item.degrade {
        let tokens = estimator.estimate(degraded.content());
        if tokens <= available {
            return Some(SelectedItem {
                proposal: proposal.clone(),
                content: degraded.content().to_string(),
                tokens,
                level: Some(degraded.level_name().to_string()),
                reason: Some(reason.to_string()),
                nap: degraded.nap(),
                score: score(proposal, quotas),
            });
        }
    }
    None
}

fn apply_constraint_guard(
    selected: &mut [SelectedItem],
    used: &mut Tokens,
    estimator: &dyn Estimator,
    warnings: &mut Vec<String>,
) {
    let mut instruction_count = selected
        .iter()
        .filter(|item| item.proposal.item.instruction_bearing)
        .count();
    if instruction_count <= 15 {
        return;
    }
    if !warnings
        .iter()
        .any(|warning| warning.starts_with("constraint_overload:"))
    {
        warnings.push(format!("constraint_overload:{instruction_count}"));
    }

    let mut indexes = selected
        .iter()
        .enumerate()
        .filter(|(_, item)| {
            item.proposal.item.instruction_bearing && item.proposal.item.priority == Priority::P2
        })
        .map(|(idx, item)| (idx, item.score, item.proposal.item.id.clone()))
        .collect::<Vec<_>>();
    indexes.sort_by(|left, right| {
        left.1
            .partial_cmp(&right.1)
            .unwrap_or(Ordering::Equal)
            .then_with(|| right.2.cmp(&left.2))
    });

    for (idx, _, _) in indexes {
        if instruction_count <= 15 {
            break;
        }
        let Some(pointer) = selected[idx]
            .proposal
            .item
            .degrade
            .iter()
            .find(|degraded| matches!(degraded, Degraded::Pointer(_)))
        else {
            continue;
        };
        let pointer_tokens = estimator.estimate(pointer.content());
        if selected[idx].level.as_deref() == Some("pointer") {
            continue;
        }
        if pointer_tokens > selected[idx].tokens {
            continue;
        }
        *used = used
            .saturating_sub(selected[idx].tokens)
            .saturating_add(pointer_tokens);
        selected[idx].content = pointer.content().to_string();
        selected[idx].tokens = pointer_tokens;
        selected[idx].level = Some("pointer".to_string());
        selected[idx].reason = Some("constraint_overload".to_string());
        selected[idx].nap = None;
        instruction_count -= 1;
    }
}

fn compare_selected_render_order(left: &SelectedItem, right: &SelectedItem) -> Ordering {
    left.proposal
        .item
        .class
        .render_order()
        .cmp(&right.proposal.item.class.render_order())
        .then_with(|| left.proposal.item.id.cmp(&right.proposal.item.id))
}

fn render_context(selected: &[SelectedItem]) -> String {
    let mut rendered = String::from("# Harness Context\n");
    let mut by_class: BTreeMap<u8, (ItemClass, Vec<&SelectedItem>)> = BTreeMap::new();
    for item in selected {
        by_class
            .entry(item.proposal.item.class.render_order())
            .or_insert_with(|| (item.proposal.item.class, Vec::new()))
            .1
            .push(item);
    }
    for (_, (class, items)) in by_class {
        rendered.push('\n');
        rendered.push_str("## ");
        rendered.push_str(class.section_title());
        rendered.push('\n');
        for item in items {
            rendered.push_str("\n<!-- ");
            rendered.push_str(item.proposal.item.id.as_str());
            rendered.push_str(" -->\n");
            rendered.push_str(&item.content);
            rendered.push('\n');
        }
    }
    rendered
}

fn excluded_manifest_item(proposal: &Proposal, tokens: Tokens, reason: &str) -> ManifestItem {
    ManifestItem {
        id: proposal.item.id.clone(),
        provider_id: proposal.provider_id.clone(),
        class: proposal.item.class,
        decision: ManifestDecision::Excluded,
        tokens,
        level: None,
        reason: Some(reason.to_string()),
        original_tokens: None,
        compressed_tokens: None,
        nap: None,
    }
}

fn provider_precedence(provider_id: &ProviderId) -> u8 {
    match provider_id.as_str() {
        "rules" => 0,
        "contract" => 1,
        "skills" => 2,
        "brief" => 3,
        "gc-drafts" => 4,
        "supplied" => 5,
        _ => 6,
    }
}
