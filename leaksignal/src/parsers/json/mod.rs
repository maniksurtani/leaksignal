use anyhow::Result;
use std::sync::Arc;

use indexmap::IndexMap;
use leakpolicy::{MatchContext, PathConfiguration};

mod parse;

use crate::{
    evaluator::{self, MatcherMetadata, MatcherState},
    pipe::PipeReader,
    policy::{ContentType, Policy, PolicyAction},
    proto::Match,
};

use super::ParseResponse;

/// returns (key, value) matcher states
fn prepare_match_state<'a>(
    policy: &'a Policy,
    configuration: &'a IndexMap<Arc<String>, PathConfiguration>,
) -> (MatcherState<'a>, MatcherState<'a>) {
    let mut key_match_state = MatcherState::default();
    let mut value_match_state = MatcherState::default();

    for (category_name, action) in configuration {
        if !action.category_config.content_types.is_empty() {
            if !action
                .category_config
                .content_types
                .contains(&ContentType::Json)
            {
                continue;
            }
        }

        let mut do_key = true;
        let mut do_value = true;
        if !action.category_config.contexts.is_empty() {
            if !action
                .category_config
                .contexts
                .contains(&MatchContext::Keys)
            {
                do_key = false;
            }
            if !action
                .category_config
                .contexts
                .contains(&MatchContext::Values)
            {
                do_value = false;
            }
        }
        if !do_key && !do_value {
            continue;
        }

        if matches!(
            action.category_config.action.unwrap_or_default(),
            PolicyAction::Ignore
        ) {
            continue;
        }

        let metadata = MatcherMetadata {
            policy_path: action.matcher_path.clone(),
            category_name: category_name.to_string(),
            action: action.category_config.action.unwrap_or_default(),
            local_report_style: action.report_style,
            correlation: None,
        };

        if do_key {
            evaluator::prepare_matches(
                &policy,
                &**category_name,
                &mut key_match_state,
                &metadata,
                &action.category_config.ignore,
            );
        }
        if do_value {
            evaluator::prepare_matches(
                &policy,
                &**category_name,
                &mut value_match_state,
                &metadata,
                &action.category_config.ignore,
            );
        }
    }

    (key_match_state, value_match_state)
}

pub async fn parse_json(
    policy: &Policy,
    body: &mut PipeReader,
    configuration: &IndexMap<Arc<String>, PathConfiguration>,
    matches: &mut Vec<Match>,
) -> Result<ParseResponse> {
    let (key_matcher, value_matcher) = prepare_match_state(policy, configuration);

    let mut key_matches = vec![];

    parse::parse_json(
        body,
        |key, start, _end| match key_matcher.do_matching(start, 0, &*key, &mut key_matches) {
            ParseResponse::Continue => None,
            ParseResponse::Block => Some(ParseResponse::Block),
        },
        |value, start, _end| match value_matcher.do_matching(start, 0, &*value, matches) {
            ParseResponse::Continue => None,
            ParseResponse::Block => Some(ParseResponse::Block),
        },
    )
    .await?;

    matches.extend(key_matches);

    Ok(ParseResponse::Continue)
}
