use super::{
    load_openai_compatible_config, LiveSummaryEvent, LiveSummaryEventKind,
    LiveSummaryFrontendError, LiveSummaryProviderResponse, LiveSummaryPublicSnapshot,
    LiveSummaryRuntimeState, LiveSummaryScope, RecordingLiveSummarySnapshot,
    RecordingSummaryBindingTicket, RecordingSummaryIngress, StartLiveSummarySession,
    LIVE_SUMMARY_STATE_EVENT,
};
use crate::audio::transcription::event::TranscriptEvent;
use crate::database::repositories::live_summary::{
    LiveSummaryPreferencesView, LiveSummaryRepository, LiveSummaryStoreError,
};
use crate::database::repositories::recording_session_binding::RecordingSessionBindingRepository;
use crate::state::AppState;
use std::collections::HashSet;
use tauri::{AppHandle, Emitter, Manager, Runtime, State};

fn event_emit_error() -> LiveSummaryFrontendError {
    LiveSummaryFrontendError {
        code: "live_summary_event_emit_failed".to_string(),
        message: "实时总结状态已更新，但界面事件发送失败，请刷新后重试。".to_string(),
        retryable: true,
    }
}

fn emit_snapshot<R: Runtime>(
    app: &AppHandle<R>,
    kind: LiveSummaryEventKind,
    snapshot: LiveSummaryPublicSnapshot,
) -> Result<LiveSummaryPublicSnapshot, LiveSummaryFrontendError> {
    app.emit(
        LIVE_SUMMARY_STATE_EVENT,
        LiveSummaryEvent::new(kind, snapshot.clone()),
    )
    .map_err(|_| event_emit_error())?;
    Ok(snapshot)
}

#[tauri::command]
pub async fn api_live_summary_start_session<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, AppState>,
    runtime: State<'_, LiveSummaryRuntimeState>,
    meeting_id: String,
    template_id: Option<String>,
) -> Result<LiveSummaryPublicSnapshot, LiveSummaryFrontendError> {
    let preferences = LiveSummaryRepository::load_runtime_preferences(state.db_manager.pool())
        .await
        .map_err(|_| preferences_storage_error())?;
    let template_id = template_id.unwrap_or(preferences.template_id);
    let configuration = load_openai_compatible_config(
        state.db_manager.pool(),
        &template_id,
        preferences
            .custom_prompt
            .as_ref()
            .map(|value| value.expose_secret()),
    )
    .await;
    runtime.1.configure(configuration);
    ensure_provider_dispatcher(&app, &runtime).await;
    let snapshot = runtime
        .0
        .start_session(
            state.db_manager.pool(),
            StartLiveSummarySession {
                meeting_id,
                template_id: Some(template_id),
            },
        )
        .await
        .map_err(|error| error.frontend_error())?;
    emit_snapshot(&app, LiveSummaryEventKind::SessionStarted, snapshot)
}

/// Arm the Rust-only recording summary bridge. No renderer-provided session
/// ID or transcript body is accepted; queued trusted sink events are replayed
/// after the private provider configuration is loaded.
#[tauri::command]
pub async fn api_live_summary_prepare_recording<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, AppState>,
    runtime: State<'_, LiveSummaryRuntimeState>,
) -> Result<Vec<RecordingLiveSummarySnapshot>, LiveSummaryFrontendError> {
    configure_recording_runtime(&app, state.inner(), runtime.inner()).await?;
    Ok(runtime.3.list_snapshots().await)
}

/// Read-only recovery surface for the floating assistant. It returns every
/// active or pending opaque scope and never substitutes a fake meeting ID.
#[tauri::command]
pub async fn api_live_summary_list_recording_scopes<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, AppState>,
    runtime: State<'_, LiveSummaryRuntimeState>,
) -> Result<Vec<RecordingLiveSummarySnapshot>, LiveSummaryFrontendError> {
    configure_recording_runtime(&app, state.inner(), runtime.inner()).await?;
    Ok(runtime.3.list_snapshots().await)
}

/// Issue the existing one-time handle for the exact trusted recording folder.
/// The folder chooses a Rust-created scope; it cannot inject transcript text,
/// evidence IDs, or an arbitrary backend session.
#[tauri::command]
pub async fn api_live_summary_issue_binding_ticket<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, AppState>,
    runtime: State<'_, LiveSummaryRuntimeState>,
    meeting_folder: String,
) -> Result<RecordingSummaryBindingTicket, LiveSummaryFrontendError> {
    configure_recording_runtime(&app, state.inner(), runtime.inner()).await?;
    runtime
        .3
        .issue_binding_ticket(std::path::Path::new(&meeting_folder))
        .await
        .map_err(|error| error.frontend_error())
}

#[tauri::command]
pub async fn api_live_summary_get_preferences(
    state: State<'_, AppState>,
) -> Result<LiveSummaryPreferencesView, LiveSummaryFrontendError> {
    LiveSummaryRepository::load_preferences_view(state.db_manager.pool())
        .await
        .map_err(|_| preferences_storage_error())
}

#[tauri::command]
pub async fn api_live_summary_save_preferences(
    state: State<'_, AppState>,
    template_id: String,
    replacement_prompt: Option<String>,
) -> Result<LiveSummaryPreferencesView, LiveSummaryFrontendError> {
    crate::summary::templates::get_template(&template_id)
        .map_err(|_| invalid_preferences_error())?;
    LiveSummaryRepository::save_preferences(
        state.db_manager.pool(),
        &template_id,
        replacement_prompt.as_deref(),
    )
    .await
    .map_err(preferences_save_error)
}

#[tauri::command]
pub async fn api_live_summary_delete_custom_prompt(
    state: State<'_, AppState>,
) -> Result<LiveSummaryPreferencesView, LiveSummaryFrontendError> {
    LiveSummaryRepository::delete_custom_prompt(state.db_manager.pool())
        .await
        .map_err(|_| preferences_storage_error())
}

/// Trusted Rust-only ingress for canonical transcript events emitted by the
/// recording/transcription backend. It is deliberately not a Tauri command:
/// a renderer must never fabricate the immutable evidence later cited by a
/// summary item.
pub(crate) async fn handle_live_summary_transcript_event<R: Runtime>(
    app: &AppHandle<R>,
    state: &AppState,
    runtime: &LiveSummaryRuntimeState,
    event: TranscriptEvent,
) -> Result<LiveSummaryPublicSnapshot, LiveSummaryFrontendError> {
    let snapshot = runtime
        .0
        .submit_event(state.db_manager.pool(), event)
        .await
        .map_err(|error| error.frontend_error())?;
    emit_snapshot(app, LiveSummaryEventKind::TranscriptAccepted, snapshot)
}

#[tauri::command]
pub async fn api_live_summary_request_final_reconcile<R: Runtime>(
    app: AppHandle<R>,
    runtime: State<'_, LiveSummaryRuntimeState>,
    meeting_id: String,
) -> Result<LiveSummaryPublicSnapshot, LiveSummaryFrontendError> {
    let snapshot = runtime
        .0
        .request_final_reconcile(&meeting_id)
        .await
        .map_err(|error| error.frontend_error())?;
    emit_snapshot(
        &app,
        LiveSummaryEventKind::FinalReconcileRequested,
        snapshot,
    )
}

#[tauri::command]
pub async fn api_live_summary_get_current(
    state: State<'_, AppState>,
    runtime: State<'_, LiveSummaryRuntimeState>,
    meeting_id: String,
) -> Result<LiveSummaryPublicSnapshot, LiveSummaryFrontendError> {
    runtime
        .0
        .get_current(state.db_manager.pool(), &meeting_id)
        .await
        .map_err(|error| error.frontend_error())
}

/// Rust-only completion path for the configured provider adapter.
/// It is deliberately not a Tauri command: provider results must arrive from
/// a trusted Rust adapter, never from WebView input.
pub(crate) async fn handle_live_summary_provider_response<R: Runtime>(
    app: &AppHandle<R>,
    state: &AppState,
    runtime: &LiveSummaryRuntimeState,
    meeting_id: &str,
    response: LiveSummaryProviderResponse,
) -> Result<LiveSummaryPublicSnapshot, LiveSummaryFrontendError> {
    let snapshot = runtime
        .0
        .handle_provider_response(state.db_manager.pool(), meeting_id, response)
        .await
        .map_err(|error| error.frontend_error())?;
    let kind = if snapshot.error.is_some() {
        LiveSummaryEventKind::SummaryFailed
    } else {
        LiveSummaryEventKind::SummaryCommitted
    };
    emit_snapshot(app, kind, snapshot)
}

pub(crate) async fn ensure_provider_dispatcher<R: Runtime>(
    app: &AppHandle<R>,
    runtime: &LiveSummaryRuntimeState,
) {
    let Some(mut receiver) = runtime.2.lock().await.take() else {
        return;
    };
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        while let Some(envelope) = receiver.recv().await {
            let state = app.state::<AppState>();
            let runtime = app.state::<LiveSummaryRuntimeState>();
            match envelope.scope {
                LiveSummaryScope::Meeting(meeting_id) => {
                    if let Err(error) = handle_live_summary_provider_response(
                        &app,
                        state.inner(),
                        runtime.inner(),
                        &meeting_id,
                        envelope.response,
                    )
                    .await
                    {
                        if let Ok(mut snapshot) = runtime
                            .0
                            .get_current(state.db_manager.pool(), &meeting_id)
                            .await
                        {
                            snapshot.lifecycle = super::LiveSummaryLifecycle::Error;
                            snapshot.error = Some(error);
                            let _ =
                                emit_snapshot(&app, LiveSummaryEventKind::SummaryFailed, snapshot);
                        }
                    }
                }
                LiveSummaryScope::RecordingSession(scope_id) => {
                    if let Ok(recording) = runtime
                        .3
                        .handle_provider_response(&scope_id, envelope.response)
                        .await
                    {
                        let kind = if recording.summary.error.is_some() {
                            LiveSummaryEventKind::SummaryFailed
                        } else {
                            LiveSummaryEventKind::SummaryCommitted
                        };
                        let _ = emit_snapshot(&app, kind, recording.summary);
                    }
                }
            }
        }
    });
}

async fn ensure_recording_ingress_dispatcher<R: Runtime>(
    app: &AppHandle<R>,
    runtime: &LiveSummaryRuntimeState,
) {
    let Some(mut receiver) = runtime.4.lock().await.take() else {
        return;
    };
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        while let Some(message) = receiver.recv().await {
            let runtime = app.state::<LiveSummaryRuntimeState>();
            let emitted = match message {
                RecordingSummaryIngress::Transcript {
                    segment,
                    meeting_folder,
                } => runtime
                    .3
                    .accept_segment(segment, meeting_folder)
                    .await
                    .ok()
                    .flatten()
                    .map(|snapshot| (LiveSummaryEventKind::TranscriptAccepted, snapshot.summary)),
                RecordingSummaryIngress::Pending {
                    source_session_id,
                    meeting_folder,
                } => runtime
                    .3
                    .mark_pending(&source_session_id, meeting_folder)
                    .await
                    .ok()
                    .map(|snapshot| {
                        (
                            LiveSummaryEventKind::FinalReconcileRequested,
                            snapshot.summary,
                        )
                    }),
                RecordingSummaryIngress::Barrier(done) => {
                    let _ = done.send(());
                    None
                }
            };
            if let Some((kind, snapshot)) = emitted {
                let _ = emit_snapshot(&app, kind, snapshot);
            }
        }
    });
}

pub(crate) async fn flush_recording_summary_ingress<R: Runtime>(
    app: &AppHandle<R>,
    runtime: &LiveSummaryRuntimeState,
) {
    ensure_recording_ingress_dispatcher(app, runtime).await;
    let (done, wait) = tokio::sync::oneshot::channel();
    if runtime
        .5
        .send(RecordingSummaryIngress::Barrier(done))
        .is_ok()
    {
        let _ = wait.await;
    }
}

async fn configure_recording_runtime<R: Runtime>(
    app: &AppHandle<R>,
    state: &AppState,
    runtime: &LiveSummaryRuntimeState,
) -> Result<(), LiveSummaryFrontendError> {
    let preferences = LiveSummaryRepository::load_runtime_preferences(state.db_manager.pool())
        .await
        .map_err(|_| preferences_storage_error())?;
    let configuration = load_openai_compatible_config(
        state.db_manager.pool(),
        &preferences.template_id,
        preferences
            .custom_prompt
            .as_ref()
            .map(|value| value.expose_secret()),
    )
    .await;
    runtime.1.configure(configuration);
    ensure_provider_dispatcher(app, runtime).await;
    ensure_recording_ingress_dispatcher(app, runtime).await;
    flush_recording_summary_ingress(app, runtime).await;
    runtime
        .3
        .reconfigure(preferences.template_id)
        .await
        .map_err(|error| super::LiveSummaryCoordinatorError::Actor(error).frontend_error())?;
    Ok(())
}

/// Rust-only summary bridge. Recording core has already persisted the durable
/// start anchor; summary configuration remains optional and cannot create it.
pub(crate) async fn prepare_trusted_recording_summary<R: Runtime>(
    app: &AppHandle<R>,
    source_session_id: &str,
    meeting_folder: &std::path::Path,
) -> Result<RecordingSummaryBindingTicket, LiveSummaryFrontendError> {
    let state = app.state::<AppState>();
    let runtime = app.state::<LiveSummaryRuntimeState>();
    configure_recording_runtime(app, state.inner(), runtime.inner()).await?;
    let (ticket, snapshot) = runtime
        .3
        .prepare_session(source_session_id, meeting_folder)
        .await
        .map_err(|error| error.frontend_error())?;
    emit_snapshot(app, LiveSummaryEventKind::SessionStarted, snapshot.summary)?;
    Ok(ticket)
}

/// Recover the one-time in-memory binding from the native transcript log and
/// the durable folder correlation. This is invoked only when the renderer has
/// no handle, which is the normal state after an application-process restart.
pub(crate) async fn recover_trusted_recording_summary<R: Runtime>(
    app: &AppHandle<R>,
    state: &AppState,
    runtime: &LiveSummaryRuntimeState,
    meeting_folder: &std::path::Path,
) -> Result<Option<RecordingSummaryBindingTicket>, LiveSummaryFrontendError> {
    configure_recording_runtime(app, state, runtime).await?;
    if let Ok(ticket) = runtime.3.issue_binding_ticket(meeting_folder).await {
        return Ok(Some(ticket));
    }

    let folder_hash = super::recording_session::recording_folder_hash(meeting_folder)
        .map_err(|error| error.frontend_error())?;
    let durable = RecordingSessionBindingRepository::load_by_folder_hash(
        state.db_manager.pool(),
        &folder_hash,
    )
    .await
    .map_err(|_| recording_binding_storage_error())?;
    let native = match crate::audio::recording_saver::read_recording_transcript_recovery(
        meeting_folder.to_string_lossy().into_owned(),
    )
    .await
    {
        Ok(native) => native,
        Err(_) if durable.is_none() => return Ok(None),
        Err(_) => return Err(recording_binding_recovery_error()),
    };

    let event_sessions = native
        .events
        .iter()
        .filter_map(|segment| segment.session_id.as_deref())
        .filter(|value| !value.trim().is_empty())
        .collect::<HashSet<_>>();
    let source_session_id = match durable.as_ref() {
        Some(binding) => {
            if !event_sessions.is_empty()
                && (event_sessions.len() != 1
                    || !event_sessions.contains(binding.source_session_id.as_str()))
            {
                return Err(recording_binding_recovery_error());
            }
            binding.source_session_id.clone()
        }
        None if event_sessions.len() == 1 => event_sessions
            .into_iter()
            .next()
            .expect("single native session was checked")
            .to_string(),
        None => return Ok(None),
    };

    RecordingSessionBindingRepository::ensure_pending(
        state.db_manager.pool(),
        &source_session_id,
        &folder_hash,
    )
    .await
    .map_err(|_| recording_binding_storage_error())?;
    let (ticket, snapshot) = runtime
        .3
        .recover_pending_session(&source_session_id, meeting_folder, native.events)
        .await
        .map_err(|error| error.frontend_error())?;
    emit_snapshot(
        app,
        LiveSummaryEventKind::FinalReconcileRequested,
        snapshot.summary,
    )?;
    Ok(Some(ticket))
}

/// Rust-only stop fallback. This closes even a transcript-free session by its
/// exact native recording folder; failure never changes recording persistence.
pub(crate) async fn mark_trusted_recording_summary_pending_by_folder<R: Runtime>(
    app: &AppHandle<R>,
    meeting_folder: &std::path::Path,
) -> Result<(), LiveSummaryFrontendError> {
    let runtime = app.state::<LiveSummaryRuntimeState>();
    let snapshot = runtime
        .3
        .mark_pending_by_folder(meeting_folder)
        .await
        .map_err(|error| error.frontend_error())?;
    emit_snapshot(
        app,
        LiveSummaryEventKind::FinalReconcileRequested,
        snapshot.summary,
    )?;
    Ok(())
}

pub(crate) async fn bind_recording_summary_after_save<R: Runtime>(
    app: &AppHandle<R>,
    state: &AppState,
    runtime: &LiveSummaryRuntimeState,
    handle: &str,
    meeting_id: &str,
) -> Result<LiveSummaryPublicSnapshot, LiveSummaryFrontendError> {
    configure_recording_runtime(app, state, runtime).await?;
    if let Some(revision) = runtime
        .3
        .prepare_bound_revision(handle, meeting_id)
        .await
        .map_err(|error| error.frontend_error())?
    {
        let exists: i64 = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM live_summary_revisions WHERE summary_revision_id = ?)",
        )
        .bind(&revision.summary_revision_id)
        .fetch_one(state.db_manager.pool())
        .await
        .map_err(|_| preferences_storage_error())?;
        if exists != 1 {
            LiveSummaryRepository::persist_revision(state.db_manager.pool(), &revision)
                .await
                .map_err(|error| {
                    super::LiveSummaryCoordinatorError::SummaryStore(error).frontend_error()
                })?;
        }
    }

    let preferences = LiveSummaryRepository::load_runtime_preferences(state.db_manager.pool())
        .await
        .map_err(|_| preferences_storage_error())?;
    let _ = runtime
        .0
        .start_session(
            state.db_manager.pool(),
            StartLiveSummarySession {
                meeting_id: meeting_id.to_string(),
                template_id: Some(preferences.template_id),
            },
        )
        .await
        .map_err(|error| error.frontend_error())?;
    let snapshot = runtime
        .0
        .request_final_reconcile(meeting_id)
        .await
        .map_err(|error| error.frontend_error())?;
    runtime
        .3
        .complete_binding(handle, meeting_id)
        .await
        .map_err(|error| error.frontend_error())?;
    emit_snapshot(app, LiveSummaryEventKind::FinalReconcileRequested, snapshot)
}

fn preferences_storage_error() -> LiveSummaryFrontendError {
    LiveSummaryFrontendError {
        code: "live_summary_preferences_storage_failed".to_string(),
        message: "实时总结设置暂时无法保存，请稍后重试。".to_string(),
        retryable: true,
    }
}

fn recording_binding_storage_error() -> LiveSummaryFrontendError {
    LiveSummaryFrontendError {
        code: "live_summary_recording_binding_storage_failed".to_string(),
        message: "录音已保留，但实时总结的持久绑定暂时无法写入。".to_string(),
        retryable: true,
    }
}

fn recording_binding_recovery_error() -> LiveSummaryFrontendError {
    LiveSummaryFrontendError {
        code: "live_summary_recording_recovery_invalid".to_string(),
        message: "原生录音恢复记录与持久会话不一致，已拒绝总结绑定。".to_string(),
        retryable: false,
    }
}

fn invalid_preferences_error() -> LiveSummaryFrontendError {
    LiveSummaryFrontendError {
        code: "live_summary_preferences_invalid".to_string(),
        message: "总结模板不存在，或自定义提示词为空、过长或含无效字符。".to_string(),
        retryable: false,
    }
}

fn preferences_save_error(error: LiveSummaryStoreError) -> LiveSummaryFrontendError {
    if matches!(error, LiveSummaryStoreError::InvalidStoredState(_)) {
        invalid_preferences_error()
    } else {
        preferences_storage_error()
    }
}
