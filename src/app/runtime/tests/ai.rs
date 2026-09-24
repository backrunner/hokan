use super::*;

#[test]
fn ai_request_owns_candidates_until_the_next_query() {
    let directory = tempfile::tempdir().expect("directory");
    let mut state = runtime_state(directory.path());
    let (output, join) = test_output();

    state.buffer.set_exact("git ".into(), 4).expect("buffer");
    refresh_context(&mut state, QueryId::new(1));
    state.candidates = vec![history_candidate(QueryId::new(1), "git status")];

    // The user activates RequestAi: the wait screen takes over the overlay.
    let config = Arc::new(Config::default());
    let (ai_sender, _ai_receiver) = crossbeam_channel::unbounded();
    let context = Arc::clone(state.context.as_ref().expect("context"));
    start_ai_request(&mut state, &context, &config, &ai_sender, &output).expect("ai request");
    assert!(state.ai_query.is_some());
    assert!(state.ai_owns_candidates);
    assert!(state.candidates.is_empty());

    // A provider batch for the same query id still passes the staleness
    // check (the buffer never moved) but must not wipe the AI wait screen.
    let result = provider_result(&state, vec![history_candidate(QueryId::new(1), "git push")]);
    handle_provider_result(result, &mut state, &output).expect("provider result");
    assert!(
        state.candidates.is_empty(),
        "provider batch must not replace the AI wait screen"
    );
    assert_eq!(
        state.status.as_deref(),
        Some("HK-AI-WAIT requesting commands; Esc cancels")
    );

    // The AI result lands and owns the candidate list…
    let generation = state.ai_query.as_ref().expect("active request").generation;
    handle_ai_result(
        AiResult {
            query_id: QueryId::new(1),
            generation,
            result: Ok(vec![ai_command("git commit -m wip")]),
        },
        &mut state,
        &output,
    )
    .expect("ai result");
    assert!(state.ai_query.is_none());
    assert!(
        state.ai_owns_candidates,
        "AI results keep owning the overlay until the next query"
    );
    assert_eq!(
        state
            .candidates
            .iter()
            .map(|candidate| candidate.display.primary.as_str())
            .collect::<Vec<_>>(),
        vec!["git commit -m wip"]
    );

    // …and a provider batch arriving after the AI result still must not
    // replace it.
    let result = provider_result(&state, vec![history_candidate(QueryId::new(1), "git push")]);
    handle_provider_result(result, &mut state, &output).expect("provider result");
    assert_eq!(
        state
            .candidates
            .iter()
            .map(|candidate| candidate.display.primary.as_str())
            .collect::<Vec<_>>(),
        vec!["git commit -m wip"],
        "late provider batch must not overwrite AI results"
    );

    // The next scheduled query hands the overlay back to providers.
    let worker = ProviderWorker::start(
        Arc::new(crate::completion::CompletionEngine::new(8, 12)),
        None,
    )
    .expect("provider worker");
    state.schedule_query(&worker).expect("schedule query");
    assert!(!state.ai_owns_candidates);
    assert_eq!(state.query_id, QueryId::new(1), "bumped from ZERO");
    let fresh_query = state.context.as_ref().expect("context").query_id;
    let result = provider_result(&state, vec![history_candidate(fresh_query, "git push")]);
    handle_provider_result(result, &mut state, &output).expect("provider result");
    assert_eq!(
        state
            .candidates
            .iter()
            .map(|candidate| candidate.display.primary.as_str())
            .collect::<Vec<_>>(),
        vec!["git push"],
        "provider batches are accepted again once the AI window closes"
    );
    output.restore_and_exit().expect("shutdown");
    join.join().expect("actor joins").expect("actor exits");
}

#[test]
fn stale_ai_result_never_takes_the_active_request() {
    let directory = tempfile::tempdir().expect("directory");
    let mut state = runtime_state(directory.path());
    let (output, join) = test_output();

    // Two consecutive AI requests share the query id because the buffer
    // never moved between them; request B (generation 2) is active while a
    // late result from the cancelled request A (generation 1) arrives.
    state.buffer.set_exact("git ".into(), 4).expect("buffer");
    refresh_context(&mut state, QueryId::new(1));
    state.ai_generation = 2;
    state.ai_owns_candidates = true;
    state.ai_query = Some(ActiveAiRequest {
        query_id: QueryId::new(1),
        generation: 2,
        cancel: tokio_util::sync::CancellationToken::new(),
    });
    let active_cancel = state
        .ai_query
        .as_ref()
        .expect("active request")
        .cancel
        .clone();

    handle_ai_result(
        AiResult {
            query_id: QueryId::new(1),
            generation: 1,
            result: Ok(vec![ai_command("git push")]),
        },
        &mut state,
        &output,
    )
    .expect("stale ai result");
    assert!(
        state.ai_query.is_some(),
        "the stale result must not take the active request's slot"
    );
    assert!(
        state.candidates.is_empty(),
        "the stale result must not paint its candidates"
    );

    // The active request is still cancellable…
    state.cancel_ai();
    assert!(active_cancel.is_cancelled());

    // …and its own result is the one that gets accepted.
    state.ai_query = Some(ActiveAiRequest {
        query_id: QueryId::new(1),
        generation: 2,
        cancel: tokio_util::sync::CancellationToken::new(),
    });
    handle_ai_result(
        AiResult {
            query_id: QueryId::new(1),
            generation: 2,
            result: Ok(vec![ai_command("git commit -m wip")]),
        },
        &mut state,
        &output,
    )
    .expect("active ai result");
    assert!(state.ai_query.is_none());
    assert_eq!(
        state
            .candidates
            .iter()
            .map(|candidate| candidate.display.primary.as_str())
            .collect::<Vec<_>>(),
        vec!["git commit -m wip"]
    );
    output.restore_and_exit().expect("shutdown");
    join.join().expect("actor joins").expect("actor exits");
}
