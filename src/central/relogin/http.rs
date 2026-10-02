use super::*;
pub(in crate::central) async fn status(
    State(broker): State<Broker>,
    headers: HeaderMap,
    body: Result<Json<Request>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, HttpError> {
    let _import = broker.imports.lock().await;
    let device = broker.authorize(&headers)?;
    let Json(request) =
        body.map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_request"))?;
    broker.owner(&device, &request.alias).await?;
    let state = broker
        .state
        .join("accounts")
        .join(account_key(&device.user, request.alias.trim()));
    let record = if request.id.is_empty() {
        current(&state).and_then(|r| r.context("no login operation"))
    } else {
        load(&state, &request.id)
    }
    .map_err(|_| broker.error(StatusCode::NOT_FOUND, "relogin_not_found"))?;
    if record.user != device.user || !record.alias.eq_ignore_ascii_case(request.alias.trim()) {
        return Err(broker.error(StatusCode::NOT_FOUND, "relogin_not_found"));
    }
    if record.device != device.id {
        return Err(broker.error(StatusCode::CONFLICT, "login_belongs_to_another_device"));
    }
    broker.authorize(&headers)?;
    Ok(response(&record))
}
pub(in crate::central) async fn cancel(
    State(broker): State<Broker>,
    headers: HeaderMap,
    body: Result<Json<Request>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, HttpError> {
    let _import = broker.imports.lock().await;
    let device = broker.authorize(&headers)?;
    let Json(request) =
        body.map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_request"))?;
    broker.owner(&device, &request.alias).await?;
    let state = broker
        .state
        .join("accounts")
        .join(account_key(&device.user, request.alias.trim()));
    let record = load(&state, &request.id)
        .map_err(|_| broker.error(StatusCode::NOT_FOUND, "relogin_not_found"))?;
    if record.user != device.user || !record.alias.eq_ignore_ascii_case(request.alias.trim()) {
        return Err(broker.error(StatusCode::NOT_FOUND, "relogin_not_found"));
    }
    if record.device != device.id {
        return Err(broker.error(StatusCode::CONFLICT, "login_belongs_to_another_device"));
    }
    if let Some(flag) = broker
        .relogins
        .lock()
        .expect("re-login lock")
        .get(&job_key(&state, &record.id))
    {
        flag.store(true, Ordering::Release);
    }
    // This is an acknowledgment, not proof of process exit. Poll for the terminal result.
    Ok(response(&record))
}
pub(in crate::central) async fn start(
    State(broker): State<Broker>,
    headers: HeaderMap,
    body: Result<Json<Request>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, HttpError> {
    let Json(request) =
        body.map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_request"))?;
    let permit = broker
        .work
        .clone()
        .try_acquire_owned()
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "server_busy"))?;
    let worker = broker.clone();
    let worker_headers = headers.clone();
    let response = tokio::spawn(async move {
        let _permit = permit;
        start_owned(worker, worker_headers, request).await
    })
    .await
    .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "relogin_failed"))??;
    broker.authorize(&headers)?;
    Ok(response)
}
async fn start_owned(
    broker: Broker,
    headers: HeaderMap,
    request: Request,
) -> Result<Response, HttpError> {
    validate_id(&request.id)
        .map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_request"))?;
    let _import = broker.imports.lock().await;
    let device = broker.authorize(&headers)?;
    let alias = store::validate_alias(&request.alias)
        .map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_alias"))?;
    let owner = broker.owner(&device, alias).await?;
    if broker.read_only
        || broker.stopping.load(Ordering::Acquire)
        || broker.ownership_unresolved.load(Ordering::Acquire)
    {
        return Err(broker.error(StatusCode::CONFLICT, "relogin_unavailable"));
    }
    let state = owner.lock().await.state.clone();
    let original_auth = owner.lock().await.vault.auth.clone();
    check_claim(
        state
            .parent()
            .ok_or_else(|| broker.error(StatusCode::SERVICE_UNAVAILABLE, "recovery_failed"))?,
        &broker.key,
        &state,
        &original_auth,
    )
    .map_err(|_| broker.error(StatusCode::CONFLICT, "account_already_owned"))?;
    if let Some(record) = current(&state)
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "recovery_failed"))?
        && record.device != device.id
        && !matches!(
            record.phase,
            Phase::Completed | Phase::Failed | Phase::Canceled
        )
    {
        return Err(broker.error(StatusCode::CONFLICT, "login_belongs_to_another_device"));
    }
    if let Some(record) = current(&state)
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "recovery_failed"))?
        && matches!(
            record.phase,
            Phase::Committing | Phase::Promoted | Phase::Retiring
        )
        && !broker
            .relogins
            .lock()
            .expect("re-login lock")
            .contains_key(&job_key(&state, &record.id))
    {
        let mut original = owner.lock().await;
        if let Some(rpc) = original.rpc.as_mut() {
            rpc.settle_and_stop()
                .await
                .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable"))?;
        } else {
            previous_owner_exited(&original.home)
                .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable"))?;
        }
        original.rpc = None;
        let repair = recover(&state, &broker.key)
            .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "recovery_failed"))?;
        if repair.blocked {
            return Err(broker.error(StatusCode::SERVICE_UNAVAILABLE, "relogin_commit_pending"));
        }
        original.vault = vault::load(&state, &broker.key)
            .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?;
        original.refresh_enabled = true;
        original.available = true;
        if verify_replacement(&mut original, &broker.binary)
            .await
            .is_err()
        {
            original.available = false;
            finish_rejection(&mut original)
                .await
                .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "recovery_failed"))?;
            return Ok(response(
                &current(&state)
                    .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "recovery_failed"))?
                    .ok_or_else(|| {
                        broker.error(StatusCode::SERVICE_UNAVAILABLE, "recovery_failed")
                    })?,
            ));
        }
        return Ok(response(
            &current(&state)
                .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?
                .ok_or_else(|| broker.error(StatusCode::SERVICE_UNAVAILABLE, "recovery_failed"))?,
        ));
    }
    if directory(&state, &request.id)
        .map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_request"))?
        .join("record.json")
        .try_exists()
        .unwrap_or(true)
    {
        let record = load(&state, &request.id)
            .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "recovery_failed"))?;
        if record.device != device.id {
            return Err(broker.error(StatusCode::CONFLICT, "login_belongs_to_another_device"));
        }
        return Ok(response(&record));
    }
    if let Some(record) = current(&state)
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "recovery_failed"))?
    {
        if broker
            .relogins
            .lock()
            .expect("re-login lock")
            .contains_key(&job_key(&state, &record.id))
        {
            return Ok(response(&record));
        }
        stopped(&state, &record)
            .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable"))?;
    }
    let permit = broker
        .work
        .clone()
        .try_acquire_owned()
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "server_busy"))?;
    let mut original = owner.lock().await;
    let record = Record {
        sequence: current(&state)
            .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "recovery_failed"))?
            .map_or(Ok(1), |r| {
                r.sequence
                    .checked_add(1)
                    .ok_or_else(|| broker.error(StatusCode::SERVICE_UNAVAILABLE, "recovery_failed"))
            })?,
        id: request.id,
        user: device.user,
        device: device.id,
        broker: process::Process::capture(std::process::id())
            .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable"))?,
        child: Child::NotStarted,
        candidate: None,
        alias: original.vault.alias.clone(),
        original_revision: vault::digest(
            &serde_json::to_vec(&original.vault.auth)
                .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?,
        ),
        candidate_revision: None,
        phase: Phase::Starting,
        code: None,
        error: None,
        retired: false,
    };
    publish(&state, &record)
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?;
    original.available = false;
    drop(original);
    let flag = Arc::new(AtomicBool::new(false));
    broker
        .relogins
        .lock()
        .expect("re-login lock")
        .insert(job_key(&state, &record.id), flag.clone());
    let worker = broker.clone();
    let initial_id = record.id.clone();
    let initial_state = state.clone();
    let worker_headers = headers.clone();
    let (ready, initial) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let _permit = permit;
        let mut record = record;
        let result = run(&worker, &worker_headers, &owner, &mut record, &flag, ready).await;
        if let Err(error) = result {
            let _import = worker.imports.lock().await;
            let _ = error; // Never log native output or credential data.
            owner.lock().await.available = false;
            match load(&state, &record.id) {
                Ok(durable) => {
                    record = durable;
                    if !matches!(
                        record.phase,
                        Phase::Committing | Phase::Promoted | Phase::Retiring
                    ) {
                        record.phase = Phase::Failed;
                    }
                    record.code = None;
                    record.error.get_or_insert_with(|| "relogin_failed".into());
                    if save(&state, &record).is_err() {
                        worker.ownership_unresolved.store(true, Ordering::Release);
                    }
                }
                Err(_) => {
                    if stopped(&state, &record).is_err() {
                        worker.ownership_unresolved.store(true, Ordering::Release);
                    }
                }
            }
            worker.record_failure("relogin_failed", "relogin", StatusCode::SERVICE_UNAVAILABLE);
        }
        worker
            .relogins
            .lock()
            .expect("re-login lock")
            .remove(&job_key(&state, &record.id));
    });
    drop(_import);
    let record = match initial.await {
        Ok(record) => record,
        Err(_) => load(&initial_state, &initial_id)
            .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?,
    };
    broker.authorize(&headers)?;
    Ok(response(&record))
}
