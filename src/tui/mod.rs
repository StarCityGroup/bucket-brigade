use std::io::{self, IsTerminal, Stdout};
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph, Wrap};

use aws_sdk_s3::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_s3::operation::restore_object::RestoreObjectError;

use crate::app::{ActivePane, App, AppMode, MaskEditorField, PendingAction, StorageIntent};
use crate::aws::S3Service;
use crate::mask::ObjectMask;
use crate::models::{RestoreState, StorageClassTier};
use crate::tracker::RestoreTracker;

pub async fn run(app: &mut App, s3: &S3Service, mut tracker: RestoreTracker) -> Result<()> {
    // Verify we have a terminal before trying to initialize TUI
    if !io::stdout().is_terminal() {
        anyhow::bail!(
            "This application requires a terminal to run.\n\
             stdout is not connected to a terminal (TTY).\n\
             Please run this application directly in a terminal."
        );
    }

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.hide_cursor()?;

    app.push_status("Loading buckets…");
    if let Err(err) = refresh_buckets(app, s3).await {
        // Check if this is a credentials error
        let err_msg = format!("{err:#}");
        if err_msg.contains("credentials")
            || err_msg.contains("UnrecognizedClientException")
            || err_msg.contains("InvalidAccessKeyId")
            || err_msg.contains("SignatureDoesNotMatch")
            || err_msg.contains("NoCredentialsError")
        {
            app.set_mode(AppMode::CredentialError);
            app.push_status(&format!("AWS credentials error: {err_msg}"));
        } else {
            app.push_status(&format!("Failed to load buckets: {err:#}"));
        }
    }

    let result = event_loop(&mut terminal, app, s3, &mut tracker).await;
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    s3: &S3Service,
    tracker: &mut RestoreTracker,
) -> Result<()> {
    let mut last_refresh = std::time::Instant::now();
    let refresh_interval = Duration::from_secs(30);

    loop {
        terminal.draw(|frame| draw(frame, app, tracker))?;

        // Check if we should auto-load objects for selected bucket
        if app.pending_bucket_load
            && let Some(last_change) = app.last_bucket_change
            && last_change.elapsed() >= Duration::from_secs(1)
        {
            app.pending_bucket_load = false;
            if let Err(err) = load_objects_for_selection(app, s3).await {
                app.push_status(&format!("Failed to load objects: {err:#}"));
            } else {
                // Automatically switch to Objects pane after successful load
                app.active_pane = ActivePane::Objects;
            }
        }

        // Check if we should lazy-load more objects
        if app.should_load_more()
            && !app.is_loading_objects
            && let Err(err) = load_more_objects(app, s3).await
        {
            app.push_status(&format!("Failed to load more: {err:#}"));
        }

        // Check if it's time to auto-refresh
        if last_refresh.elapsed() >= refresh_interval {
            if !app.objects.is_empty() && app.selected_bucket_name().is_some() {
                // Silently refresh with pagination
                let _ = load_objects_for_selection(app, s3).await;
            }
            last_refresh = std::time::Instant::now();
        }

        if event::poll(Duration::from_millis(200))? {
            match event::read()? {
                Event::Key(key) => {
                    if handle_key_event(key, app, s3, tracker).await? {
                        break;
                    }
                }
                Event::Resize(_, _) => continue,
                _ => continue,
            }
        }
    }
    Ok(())
}

async fn handle_key_event(
    key: KeyEvent,
    app: &mut App,
    s3: &S3Service,
    tracker: &mut RestoreTracker,
) -> Result<bool> {
    if key.kind != KeyEventKind::Press {
        return Ok(false);
    }

    if matches!(key.code, KeyCode::Char('c')) && key.modifiers.contains(KeyModifiers::CONTROL) {
        return Ok(true);
    }

    match app.mode {
        AppMode::CredentialError => {
            // Any key press exits the application
            return Ok(true);
        }
        AppMode::ShowingHelp => {
            if matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('?')) {
                app.set_mode(AppMode::Browsing);
            }
            return Ok(false);
        }
        AppMode::ViewingLog => {
            if matches!(
                key.code,
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('l') | KeyCode::Char('L')
            ) {
                app.set_mode(AppMode::Browsing);
            }
            return Ok(false);
        }
        AppMode::ViewingRestoreRequests => {
            if matches!(
                key.code,
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('t') | KeyCode::Char('T')
            ) {
                app.set_mode(AppMode::Browsing);
            }
            return Ok(false);
        }
        AppMode::EditingMask => {
            handle_mask_editor_keys(key, app);
            return Ok(false);
        }
        AppMode::SelectingStorageClass => {
            handle_storage_class_selector(key, app);
            return Ok(false);
        }
        AppMode::Confirming => {
            handle_confirmation_keys(key, app, s3, tracker).await?;
            return Ok(false);
        }
        AppMode::ShowingProgress => {
            // Ignore all key presses during progress operations
            return Ok(false);
        }
        AppMode::Browsing => {}
    }

    match key.code {
        KeyCode::Char('q') => return Ok(true),
        KeyCode::Tab => {
            app.next_pane();
        }
        KeyCode::BackTab => {
            app.previous_pane();
        }
        KeyCode::Up => move_selection(app, -1),
        KeyCode::Down => move_selection(app, 1),
        KeyCode::Left => {
            if app.active_pane == ActivePane::Buckets {
                cycle_region(app, -1);
            }
        }
        KeyCode::Right => {
            if app.active_pane == ActivePane::Buckets {
                cycle_region(app, 1);
            }
        }
        KeyCode::PageUp => move_selection(app, -5),
        KeyCode::PageDown => move_selection(app, 5),
        KeyCode::Home => jump_selection(app, true),
        KeyCode::End => jump_selection(app, false),
        KeyCode::Char('m') => {
            app.set_mode(AppMode::EditingMask);
            app.focus_mask_field(MaskEditorField::Pattern);
            // Reset cursor position to end of pattern
            app.mask_draft.cursor_pos = app.mask_draft.pattern.len();
            app.push_status(
                "Mask editor active – Type to enter pattern, Tab to switch fields, Enter to apply",
            );
        }
        KeyCode::Char('f') => {
            app.push_status("Refreshing buckets…");
            if let Err(err) = refresh_buckets(app, s3).await {
                app.push_status(&format!("Bucket refresh failed: {err:#}"));
            }
        }
        KeyCode::Char('i') => {
            if let Err(err) = refresh_selected_object(app, s3).await {
                app.push_status(&format!("Inspect failed: {err:#}"));
            }
        }
        KeyCode::Enter => {
            if app.active_pane == ActivePane::Buckets {
                load_objects_for_selection(app, s3).await?;
                // Automatically switch to Objects pane for intuitive navigation
                app.active_pane = ActivePane::Objects;
            }
        }
        KeyCode::Char('s') => {
            if let Err(err) = begin_storage_selection(app, StorageIntent::Transition) {
                app.push_status(&format!("Storage selection unavailable: {err:#}"));
            }
        }
        KeyCode::Char('r') => {
            if let Err(err) = initiate_restore_flow(app) {
                app.push_status(&format!("Cannot request restore: {err:#}"));
            }
        }
        KeyCode::Char('?') => {
            app.set_mode(AppMode::ShowingHelp);
        }
        KeyCode::Char('l') | KeyCode::Char('L') => {
            if matches!(app.mode, AppMode::ViewingLog) {
                app.set_mode(AppMode::Browsing);
            } else {
                app.set_mode(AppMode::ViewingLog);
            }
        }
        KeyCode::Char('t') | KeyCode::Char('T') => {
            if matches!(app.mode, AppMode::ViewingRestoreRequests) {
                app.set_mode(AppMode::Browsing);
            } else {
                app.set_mode(AppMode::ViewingRestoreRequests);
            }
        }
        KeyCode::Esc => {
            if app.active_mask.is_some() {
                app.apply_mask(None);
            }
        }
        _ => {}
    }

    Ok(false)
}

async fn handle_confirmation_keys(
    key: KeyEvent,
    app: &mut App,
    s3: &S3Service,
    tracker: &mut RestoreTracker,
) -> Result<()> {
    match key.code {
        KeyCode::Esc | KeyCode::Char('n') => {
            app.pending_action = None;
            app.set_mode(AppMode::Browsing);
            app.push_status("Cancelled");
        }
        KeyCode::Enter | KeyCode::Char('y') => {
            if let Some(action) = app.pending_action.take() {
                match action {
                    PendingAction::Transition { target_class } => {
                        execute_transition(app, s3, target_class).await?;
                    }
                    PendingAction::Restore { days } => {
                        execute_restore(app, s3, tracker, days).await?;
                    }
                }
            }
            app.set_mode(AppMode::Browsing);
        }
        _ => {}
    }
    Ok(())
}

fn handle_mask_editor_keys(key: KeyEvent, app: &mut App) {
    match key.code {
        KeyCode::Esc => {
            app.set_mode(AppMode::Browsing);
            app.push_status("Mask edit cancelled");
        }
        KeyCode::Enter => {
            if app.mask_draft.pattern.is_empty() {
                app.push_status("Mask pattern cannot be empty");
                return;
            }
            // Generate a name based on the pattern and kind
            let name = format!("{} '{}'", app.mask_draft.kind, app.mask_draft.pattern);
            let mask = ObjectMask {
                name,
                pattern: app.mask_draft.pattern.clone(),
                kind: app.mask_draft.kind.clone(),
                case_sensitive: app.mask_draft.case_sensitive,
                storage_class_filter: app.mask_draft.storage_class_filter.clone(),
            };
            app.apply_mask(Some(mask));
            app.set_mode(AppMode::Browsing);
        }
        KeyCode::Tab => {
            app.next_mask_field();
        }
        KeyCode::BackTab => {
            app.previous_mask_field();
        }
        KeyCode::Backspace => {
            if matches!(app.mask_field, MaskEditorField::Pattern) {
                if app.mask_draft.cursor_pos > 0 {
                    app.mask_draft.pattern.remove(app.mask_draft.cursor_pos - 1);
                    app.mask_draft.cursor_pos -= 1;
                }
            }
        }
        KeyCode::Delete => {
            if matches!(app.mask_field, MaskEditorField::Pattern) {
                if app.mask_draft.cursor_pos < app.mask_draft.pattern.len() {
                    app.mask_draft.pattern.remove(app.mask_draft.cursor_pos);
                }
            }
        }
        KeyCode::Left => match app.mask_field {
            MaskEditorField::Pattern => {
                if app.mask_draft.cursor_pos > 0 {
                    app.mask_draft.cursor_pos -= 1;
                }
            }
            MaskEditorField::Mode => app.cycle_mask_kind_backwards(),
            MaskEditorField::Case => app.toggle_mask_case(),
            MaskEditorField::StorageClass => {
                if app.mask_draft.storage_class_cursor > 0 {
                    app.mask_draft.storage_class_cursor -= 1;
                }
                let all_classes = StorageClassTier::all_for_filter();
                app.mask_draft.storage_class_filter = all_classes
                    .get(app.mask_draft.storage_class_cursor)
                    .and_then(|(_, filter)| filter.clone());
            }
        },
        KeyCode::Right => match app.mask_field {
            MaskEditorField::Pattern => {
                if app.mask_draft.cursor_pos < app.mask_draft.pattern.len() {
                    app.mask_draft.cursor_pos += 1;
                }
            }
            MaskEditorField::Mode => app.cycle_mask_kind(),
            MaskEditorField::Case => app.toggle_mask_case(),
            MaskEditorField::StorageClass => {
                let all_classes = StorageClassTier::all_for_filter();
                if app.mask_draft.storage_class_cursor + 1 < all_classes.len() {
                    app.mask_draft.storage_class_cursor += 1;
                }
                app.mask_draft.storage_class_filter = all_classes
                    .get(app.mask_draft.storage_class_cursor)
                    .and_then(|(_, filter)| filter.clone());
            }
        },
        KeyCode::Home => {
            if matches!(app.mask_field, MaskEditorField::Pattern) {
                app.mask_draft.cursor_pos = 0;
            }
        }
        KeyCode::End => {
            if matches!(app.mask_field, MaskEditorField::Pattern) {
                app.mask_draft.cursor_pos = app.mask_draft.pattern.len();
            }
        }
        KeyCode::Char(' ') => match app.mask_field {
            MaskEditorField::Mode => app.cycle_mask_kind(),
            MaskEditorField::Case => app.toggle_mask_case(),
            MaskEditorField::StorageClass => {
                let all_classes = StorageClassTier::all_for_filter();
                app.mask_draft.storage_class_cursor =
                    (app.mask_draft.storage_class_cursor + 1) % all_classes.len();
                app.mask_draft.storage_class_filter = all_classes
                    .get(app.mask_draft.storage_class_cursor)
                    .and_then(|(_, filter)| filter.clone());
            }
            MaskEditorField::Pattern => {
                app.mask_draft
                    .pattern
                    .insert(app.mask_draft.cursor_pos, ' ');
                app.mask_draft.cursor_pos += 1;
            }
        },
        KeyCode::Char(ch) => {
            if matches!(app.mask_field, MaskEditorField::Pattern) {
                app.mask_draft.pattern.insert(app.mask_draft.cursor_pos, ch);
                app.mask_draft.cursor_pos += 1;
            }
        }
        _ => {}
    }
}

fn handle_storage_class_selector(key: KeyEvent, app: &mut App) {
    match key.code {
        KeyCode::Esc => {
            app.set_mode(AppMode::Browsing);
        }
        KeyCode::Up => {
            if app.storage_class_cursor > 0 {
                app.storage_class_cursor -= 1;
            }
        }
        KeyCode::Down => {
            if app.storage_class_cursor + 1 < StorageClassTier::selectable().len() {
                app.storage_class_cursor += 1;
            }
        }
        KeyCode::Enter => {
            if let Some(selected) = StorageClassTier::selectable().get(app.storage_class_cursor) {
                match app.storage_intent {
                    StorageIntent::Transition => {
                        // Check if objects need restore before transition
                        if app.any_targets_need_restoration() {
                            app.set_mode(AppMode::Browsing);
                            let need_restore = app.count_objects_needing_restore();
                            app.push_status(&format!(
                                "⚠ {} objects require restore before transition. Press 'r' to restore them first.",
                                need_restore
                            ));
                            return;
                        }
                        app.pending_action = Some(PendingAction::Transition {
                            target_class: selected.clone(),
                        });
                        app.set_mode(AppMode::Confirming);
                        app.push_status(&format!(
                            "Confirm transition to {} (press Enter to confirm)",
                            selected.label()
                        ));
                    }
                }
            }
        }
        _ => {}
    }
}

fn begin_storage_selection(app: &mut App, intent: StorageIntent) -> Result<()> {
    match intent {
        StorageIntent::Transition => {
            if app.selected_bucket_name().is_none() {
                anyhow::bail!("Select a bucket first");
            }
            if target_count(app) == 0 {
                anyhow::bail!("Select at least one object (mask or row)");
            }
        }
    }
    app.storage_intent = intent;
    app.storage_class_cursor = 0;
    app.set_mode(AppMode::SelectingStorageClass);
    Ok(())
}

fn initiate_restore_flow(app: &mut App) -> Result<()> {
    if app.selected_bucket_name().is_none() || target_count(app) == 0 {
        anyhow::bail!("Select objects to restore first");
    }

    let need_restore = app.count_objects_needing_restore();
    let already_restoring = app.count_objects_restoring();

    if need_restore == 0 {
        if already_restoring > 0 {
            app.push_status(&format!(
                "{} objects are already being restored",
                already_restoring
            ));
        } else {
            app.push_status("No objects need restore (not Glacier or already restored)");
        }
        return Ok(());
    }

    app.pending_action = Some(PendingAction::Restore { days: 7 });
    app.set_mode(AppMode::Confirming);

    if already_restoring > 0 {
        app.push_status(&format!(
            "Will restore {} objects ({} already restoring will be skipped)",
            need_restore, already_restoring
        ));
    } else {
        app.push_status(&format!(
            "Confirm restore request for {} objects",
            need_restore
        ));
    }
    Ok(())
}

async fn execute_transition(
    app: &mut App,
    s3: &S3Service,
    target_class: StorageClassTier,
) -> Result<()> {
    let bucket = app
        .selected_bucket_name()
        .context("Select a bucket before transitioning")?
        .to_string();
    let keys = target_keys(app);
    if keys.is_empty() {
        app.push_status("No objects selected for transition");
        return Ok(());
    }

    // Initialize progress tracking
    let total = keys.len();
    app.progress = Some(crate::app::ProgressState::new(
        format!("Transitioning to {}", target_class.label()),
        total,
    ));
    app.set_mode(AppMode::ShowingProgress);

    let mut success_count = 0;
    let mut error_count = 0;

    for (index, key) in keys.iter().enumerate() {
        // Update progress
        if let Some(progress) = &mut app.progress {
            progress.update(index + 1, Some(key.clone()));
        }

        // Yield to allow UI updates
        tokio::task::yield_now().await;

        match s3
            .transition_storage_class(&bucket, key, target_class.clone())
            .await
        {
            Ok(_) => {
                success_count += 1;
            }
            Err(err) => {
                error_count += 1;
                app.push_status(&format!("Transition failed for {key}: {err:#}"));
            }
        }
    }

    // Clear progress and return to browsing
    app.progress = None;
    app.set_mode(AppMode::Browsing);

    // Show summary
    if error_count > 0 {
        app.push_status(&format!(
            "Transition complete: {} succeeded, {} failed",
            success_count, error_count
        ));
    } else {
        app.push_status(&format!(
            "Successfully transitioned {} objects to {}",
            success_count,
            target_class.label()
        ));
    }

    load_objects_for_selection(app, s3).await?;
    Ok(())
}

async fn execute_restore(
    app: &mut App,
    s3: &S3Service,
    tracker: &mut RestoreTracker,
    days: i32,
) -> Result<()> {
    let bucket = app
        .selected_bucket_name()
        .context("Select a bucket before restoring")?
        .to_string();

    // Get objects and filter to only those needing restore
    let all_keys = target_keys(app);
    let objects_map: std::collections::HashMap<_, _> = if app.active_mask.is_some() {
        app.filtered_objects
            .iter()
            .map(|o| (o.key.clone(), o))
            .collect()
    } else {
        app.objects.iter().map(|o| (o.key.clone(), o)).collect()
    };

    let mut keys_to_restore = Vec::new();
    let mut already_restoring = 0;
    let mut already_available = 0;

    for key in &all_keys {
        if let Some(obj) = objects_map.get(key) {
            match &obj.restore_state {
                Some(crate::models::RestoreState::InProgress { .. }) => {
                    already_restoring += 1;
                }
                Some(crate::models::RestoreState::Available) => {
                    already_available += 1;
                }
                _ => {
                    // Only restore if it's a Glacier object that needs restore
                    if matches!(
                        obj.storage_class,
                        crate::models::StorageClassTier::GlacierFlexibleRetrieval
                            | crate::models::StorageClassTier::GlacierDeepArchive
                    ) {
                        keys_to_restore.push(key.clone());
                    }
                }
            }
        }
    }

    if already_restoring > 0 {
        app.push_status(&format!(
            "Skipped {} objects already being restored",
            already_restoring
        ));
    }
    if already_available > 0 {
        app.push_status(&format!(
            "Skipped {} objects already restored",
            already_available
        ));
    }

    if keys_to_restore.is_empty() {
        app.push_status("No objects need restore");
        return Ok(());
    }

    // Initialize progress tracking
    let total = keys_to_restore.len();
    app.progress = Some(crate::app::ProgressState::new(
        "Requesting Glacier restore".to_string(),
        total,
    ));
    app.set_mode(AppMode::ShowingProgress);

    let mut restored_keys = Vec::new();
    let mut success_count = 0;
    let mut error_count = 0;

    for (index, key) in keys_to_restore.iter().enumerate() {
        // Update progress
        if let Some(progress) = &mut app.progress {
            progress.update(index + 1, Some(key.clone()));
        }

        // Yield to allow UI updates
        tokio::task::yield_now().await;

        match s3.request_restore(&bucket, key, days).await {
            Ok(_) => {
                success_count += 1;
                // Track the restore request
                tracker.add_request(bucket.clone(), key.clone(), days);
                restored_keys.push(key.clone());
            }
            Err(err) => {
                error_count += 1;
                let detail = describe_restore_error(&err);
                app.push_status(&format!("✗ Restore failed for {key}: {detail}"));
            }
        }
    }

    // Clear progress and return to browsing
    app.progress = None;
    app.set_mode(AppMode::Browsing);

    // Show summary
    if error_count > 0 {
        app.push_status(&format!(
            "Restore requests complete: {} succeeded, {} failed",
            success_count, error_count
        ));
    } else {
        app.push_status(&format!(
            "Successfully requested restore for {} objects",
            success_count
        ));
    }

    // Manually update restore status for successfully restored objects
    // AWS doesn't immediately reflect the status change, so we update it in memory
    for obj in app.objects.iter_mut() {
        if restored_keys.contains(&obj.key) {
            obj.restore_state = Some(crate::models::RestoreState::InProgress { expiry: None });
        }
    }

    // Update filtered objects if a mask is active
    if app.active_mask.is_some() {
        let mask = app.active_mask.clone();
        app.apply_mask(mask);
    }

    Ok(())
}

async fn refresh_buckets(app: &mut App, s3: &S3Service) -> Result<()> {
    let buckets = s3.list_buckets().await?;
    app.set_buckets(buckets);
    Ok(())
}

async fn refresh_selected_object(app: &mut App, s3: &S3Service) -> Result<()> {
    let bucket = app
        .selected_bucket_name()
        .context("Select a bucket first")?
        .to_string();
    let key = app
        .selected_object()
        .map(|obj| obj.key.clone())
        .context("Select an object to inspect")?;
    let refreshed = s3.refresh_object(&bucket, &key).await?;
    if let Some(existing) = app.objects.iter_mut().find(|o| o.key == key) {
        *existing = refreshed.clone();
    }
    if let Some(mask) = &app.active_mask {
        app.filtered_objects = app
            .objects
            .iter()
            .filter(|&obj| mask.matches(&obj.key))
            .cloned()
            .collect();
    }
    app.push_status("Object metadata refreshed");
    Ok(())
}

async fn load_objects_for_selection(app: &mut App, s3: &S3Service) -> Result<()> {
    if let Some(bucket) = app.selected_bucket_name().map(|b| b.to_string()) {
        app.reset_pagination();
        app.is_loading_objects = true;
        app.push_status(&format!("Loading objects from {}...", bucket));

        // Skip full count for now - it can take forever on large buckets
        // We'll show loaded count vs "more available" instead
        app.total_object_count = None;

        // Load first page
        const PAGE_SIZE: i32 = 200;
        match s3
            .list_objects_paginated(&bucket, None, None, PAGE_SIZE)
            .await
        {
            Ok((mut objects, next_token)) => {
                objects.sort_by(|a, b| a.key.cmp(&b.key));
                app.set_objects(objects);
                app.continuation_token = next_token;
                app.apply_mask(app.active_mask.clone());

                let loaded = app.objects.len();
                if app.has_more_objects() {
                    app.push_status(&format!("Loaded {} objects (more available)", loaded));
                } else {
                    app.push_status(&format!("Loaded all {} objects", loaded));
                }

                // Fetch restore status for Glacier objects
                refresh_glacier_restore_status(app, s3, &bucket).await;
            }
            Err(err) => {
                app.push_status(&format!("Failed to load objects: {err:#}"));
            }
        }

        app.is_loading_objects = false;
    }
    Ok(())
}

async fn load_more_objects(app: &mut App, s3: &S3Service) -> Result<()> {
    if app.is_loading_objects || !app.has_more_objects() {
        return Ok(());
    }

    if let Some(bucket) = app.selected_bucket_name().map(|b| b.to_string()) {
        app.is_loading_objects = true;

        const PAGE_SIZE: i32 = 200;
        match s3
            .list_objects_paginated(&bucket, None, app.continuation_token.clone(), PAGE_SIZE)
            .await
        {
            Ok((mut new_objects, next_token)) => {
                new_objects.sort_by(|a, b| a.key.cmp(&b.key));
                app.append_objects(new_objects);
                app.continuation_token = next_token;

                let loaded = app.objects.len();
                if app.has_more_objects() {
                    app.push_status(&format!("Loaded {} objects (more available)...", loaded));
                } else {
                    app.push_status(&format!("Loaded all {} objects", loaded));
                }

                // Fetch restore status for newly loaded Glacier objects
                refresh_glacier_restore_status(app, s3, &bucket).await;
            }
            Err(err) => {
                app.push_status(&format!("Failed to load more: {err:#}"));
            }
        }

        app.is_loading_objects = false;
    }
    Ok(())
}

/// Fetch accurate restore status for Glacier/Deep Archive objects
async fn refresh_glacier_restore_status(app: &mut App, s3: &S3Service, bucket: &str) {
    use crate::models::StorageClassTier;

    // Find all Glacier objects that need restore status
    let glacier_keys: Vec<String> = app
        .objects
        .iter()
        .filter(|obj| {
            matches!(
                obj.storage_class,
                StorageClassTier::GlacierFlexibleRetrieval | StorageClassTier::GlacierDeepArchive
            )
        })
        .map(|obj| obj.key.clone())
        .collect();

    if glacier_keys.is_empty() {
        return;
    }

    // Batch fetch restore status using HeadObject (10 concurrent requests at a time)
    let status_results = s3.batch_refresh_restore_status(bucket, &glacier_keys).await;

    // Update objects with fetched restore status
    for (key, restore_state) in status_results {
        if let Some(obj) = app.objects.iter_mut().find(|o| o.key == key) {
            obj.restore_state = restore_state;
        }
    }

    // Re-apply mask if active to update filtered list
    if app.active_mask.is_some() {
        let mask = app.active_mask.clone();
        app.apply_mask(mask);
    }
}

fn move_selection(app: &mut App, delta: isize) {
    match app.active_pane {
        ActivePane::Buckets => {
            if app.buckets.is_empty() {
                return;
            }
            let len = app.buckets.len() as isize;
            let mut idx = app.selected_bucket as isize + delta;
            if idx < 0 {
                idx = 0;
            }
            if idx >= len {
                idx = len - 1;
            }
            let new_idx = idx as usize;
            if new_idx != app.selected_bucket {
                app.selected_bucket = new_idx;
                app.last_bucket_change = Some(std::time::Instant::now());
                app.pending_bucket_load = true;
            }
        }
        ActivePane::Objects => {
            let len = app.active_objects().len();
            if len == 0 {
                return;
            }
            let len = len as isize;
            let mut idx = app.selected_object as isize + delta;
            if idx < 0 {
                idx = 0;
            }
            if idx >= len {
                idx = len - 1;
            }
            app.selected_object = idx as usize;
        }
        ActivePane::MaskEditor => {}
    }
}

fn jump_selection(app: &mut App, start: bool) {
    match app.active_pane {
        ActivePane::Buckets => {
            if !app.buckets.is_empty() {
                let new_idx = if start { 0 } else { app.buckets.len() - 1 };
                if new_idx != app.selected_bucket {
                    app.selected_bucket = new_idx;
                    app.last_bucket_change = Some(std::time::Instant::now());
                    app.pending_bucket_load = true;
                }
            }
        }
        ActivePane::Objects => {
            if !app.active_objects().is_empty() {
                app.selected_object = if start {
                    0
                } else {
                    app.active_objects().len() - 1
                };
            }
        }
        _ => {}
    }
}

fn cycle_region(app: &mut App, delta: isize) {
    let current_region = app.get_current_region_display();
    let current_idx = app
        .available_regions
        .iter()
        .position(|r| r == &current_region)
        .unwrap_or(0);

    let new_idx =
        (current_idx as isize + delta).rem_euclid(app.available_regions.len() as isize) as usize;

    let new_region = app.available_regions[new_idx].clone();
    let region_to_set = if new_region == "All Regions" {
        None
    } else {
        Some(new_region.clone())
    };

    app.set_region(region_to_set);
    app.active_pane = ActivePane::Buckets; // Ensure focus returns to buckets
    app.push_status(&format!("Region filter: {}", new_region));
}

fn target_count(app: &App) -> usize {
    if app.active_mask.is_some() {
        app.filtered_objects.len()
    } else if app.selected_object < app.objects.len() {
        1
    } else {
        0
    }
}

fn target_keys(app: &App) -> Vec<String> {
    if app.active_mask.is_some() {
        app.filtered_objects.iter().map(|o| o.key.clone()).collect()
    } else {
        app.objects
            .get(app.selected_object)
            .map(|o| vec![o.key.clone()])
            .unwrap_or_default()
    }
}

fn draw(frame: &mut ratatui::Frame, app: &App, tracker: &RestoreTracker) {
    let size = frame.size();

    // Main vertical split: content area, status, command bar
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(10),
            Constraint::Length(4),
            Constraint::Length(3),
        ])
        .split(size);

    // Main content panel: bucket selector, mask, objects, object detail
    let main_panel = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Bucket selector (compact)
            Constraint::Length(5), // Mask panel
            Constraint::Min(10),   // Objects list
            Constraint::Length(8), // Selected object detail
        ])
        .split(vertical[0]);

    draw_bucket_selector(frame, main_panel[0], app);
    draw_mask_panel(frame, main_panel[1], app);
    draw_objects(frame, main_panel[2], app);
    draw_object_detail(frame, main_panel[3], app);
    draw_status(frame, vertical[1], app);
    draw_command_bar(frame, vertical[2]);

    match app.mode {
        AppMode::CredentialError => draw_credential_error_popup(frame),
        AppMode::EditingMask => draw_mask_popup(frame, app),
        AppMode::SelectingStorageClass => draw_storage_popup(frame, app),
        AppMode::Confirming => draw_confirm_popup(frame, app),
        AppMode::ShowingHelp => draw_help_popup(frame),
        AppMode::ViewingLog => draw_log_popup(frame, app),
        AppMode::ViewingRestoreRequests => draw_tracked_requests_popup(frame, tracker),
        AppMode::ShowingProgress => draw_progress_popup(frame, app),
        AppMode::Browsing => {}
    }
}

fn draw_bucket_selector(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let key_style = Style::default()
        .bg(Color::LightCyan)
        .fg(Color::Black)
        .add_modifier(Modifier::BOLD);

    let bucket_name = app.selected_bucket_name().unwrap_or("(no bucket selected)");
    let bucket_info = format!("  ({}/{})  ", app.selected_bucket + 1, app.buckets.len());

    let title_style = Style::default()
        .fg(Color::LightMagenta)
        .add_modifier(Modifier::BOLD);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(highlight_border(app.active_pane == ActivePane::Buckets))
        .style(Style::default().bg(Color::Black).fg(Color::White));

    let text = Line::from(vec![
        Span::styled("Region: ", Style::default().fg(Color::Cyan)),
        Span::styled(
            app.get_current_region_display(),
            Style::default()
                .fg(Color::LightGreen)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled("←", key_style),
        Span::styled("→", key_style),
        Span::raw(" cycle  │  "),
        Span::styled("Bucket: ", Style::default().fg(Color::Cyan)),
        Span::styled(bucket_name, title_style),
        Span::raw(bucket_info),
        Span::styled("↑", key_style),
        Span::styled("↓", key_style),
        Span::raw(" select"),
    ]);

    let para = Paragraph::new(text).block(block);
    frame.render_widget(para, area);
}

fn draw_objects(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let objects = app.active_objects();
    let loaded_count = app.objects.len();
    let total_count = app.total_object_count.unwrap_or(loaded_count);

    let loading_indicator = if app.is_loading_objects {
        " ⟳"
    } else if app.has_more_objects() {
        " +"
    } else {
        ""
    };

    let title = if let Some(mask) = &app.active_mask {
        format!(
            "Objects – mask: {} ({} matches of {} loaded{}){}",
            mask.summary(),
            app.filtered_objects.len(),
            loaded_count,
            if loaded_count < total_count {
                format!(" of {}", total_count)
            } else {
                String::new()
            },
            loading_indicator
        )
    } else {
        format!(
            "Objects (showing {} of {}){}",
            loaded_count, total_count, loading_indicator
        )
    };
    let title_style = Style::default()
        .fg(Color::LightCyan)
        .add_modifier(Modifier::BOLD);
    let block = Block::default()
        .title(Span::styled(title, title_style))
        .borders(Borders::ALL)
        .border_style(highlight_border(app.active_pane == ActivePane::Objects))
        .style(Style::default().bg(Color::Black));

    // Calculate available width for the key column
    // 2 (marker) + 1 (space) + 13 (size) + 1 (space) + 20 (storage) + 1 (space) + 13 (restore) + 2 (borders) = 53
    let fixed_width = 53;
    let key_width = area.width.saturating_sub(fixed_width).max(20) as usize;

    let items: Vec<ListItem> = objects
        .iter()
        .enumerate()
        .map(|(idx, obj)| {
            let is_selected = idx == app.selected_object;
            let marker = if is_selected { "►" } else { " " };
            let marker_style = if is_selected {
                Style::default()
                    .fg(Color::LightYellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let key_style = if is_selected {
                Style::default()
                    .fg(Color::LightGreen)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };

            // Truncate or pad the key to fixed width
            let key_display = if obj.key.len() > key_width {
                format!("{}…", &obj.key[..key_width.saturating_sub(1)])
            } else {
                format!("{:<width$}", obj.key, width = key_width)
            };

            // Format storage class with fixed width
            let storage_label = format!("{:<20}", obj.storage_class.label());

            // Get restore status with more descriptive text
            let (restore_symbol, restore_style) = match &obj.restore_state {
                Some(RestoreState::Available) => (
                    " Restored",
                    Style::default()
                        .fg(Color::LightGreen)
                        .add_modifier(Modifier::BOLD),
                ),
                Some(RestoreState::InProgress { .. }) => (
                    " Restoring",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Some(RestoreState::Expired) => (" Expired", Style::default().fg(Color::Red)),
                None => {
                    // Check if object is in Glacier and needs restore
                    if matches!(
                        obj.storage_class,
                        crate::models::StorageClassTier::GlacierFlexibleRetrieval
                            | crate::models::StorageClassTier::GlacierDeepArchive
                    ) {
                        (
                            " NeedsRestore",
                            Style::default()
                                .fg(Color::Magenta)
                                .add_modifier(Modifier::BOLD),
                        )
                    } else {
                        ("", Style::default().fg(Color::DarkGray))
                    }
                }
            };

            let spans = vec![
                Span::styled(marker.to_string(), marker_style),
                Span::raw(" "),
                Span::styled(key_display, key_style),
                Span::raw(" "),
                Span::styled(format_size(obj.size), Style::default().fg(Color::LightCyan)),
                Span::raw(" "),
                Span::styled(storage_label, storage_class_color(&obj.storage_class)),
                Span::styled(restore_symbol, restore_style),
            ];

            ListItem::new(Line::from(spans))
        })
        .collect();
    let mut state = ListState::default();
    if !objects.is_empty() {
        state.select(Some(app.selected_object.min(objects.len() - 1)));
    }
    let list = List::new(items)
        .highlight_style(Style::default().bg(Color::Blue))
        .block(block);
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_object_detail(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let title_style = Style::default()
        .fg(Color::LightYellow)
        .add_modifier(Modifier::BOLD);
    let block = Block::default()
        .title(Span::styled("Selected object", title_style))
        .borders(Borders::ALL)
        .style(Style::default().bg(Color::Black));
    let lines = if let Some(obj) = app.selected_object() {
        let modified = obj
            .last_modified
            .clone()
            .unwrap_or_else(|| "unknown".into());

        // Match the restore status labels used in the objects list
        let restore = match &obj.restore_state {
            Some(RestoreState::Available) => "Restored".to_string(),
            Some(RestoreState::InProgress { .. }) => "Restoring".to_string(),
            Some(RestoreState::Expired) => "Expired".to_string(),
            None => {
                // Check if object is in Glacier and needs restore
                if matches!(
                    obj.storage_class,
                    crate::models::StorageClassTier::GlacierFlexibleRetrieval
                        | crate::models::StorageClassTier::GlacierDeepArchive
                ) {
                    "NeedsRestore".to_string()
                } else {
                    "N/A".to_string()
                }
            }
        };

        vec![
            Line::from(format!("Key: {}", obj.key)),
            Line::from(format!("Size: {}", format_size(obj.size))),
            Line::from(format!("Storage: {}", obj.storage_class.label())),
            Line::from(format!("Last modified: {}", modified)),
            Line::from(format!("Restore: {}", restore)),
        ]
    } else {
        vec![Line::from("No object selected")]
    };
    let para = Paragraph::new(lines).block(block).wrap(Wrap { trim: true });
    frame.render_widget(para, area);
}

fn draw_mask_panel(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let title_style = Style::default()
        .fg(Color::LightMagenta)
        .add_modifier(Modifier::BOLD);
    let block = Block::default()
        .title(Span::styled("Filter Mask", title_style))
        .borders(Borders::ALL)
        .border_style(highlight_border(app.active_pane == ActivePane::MaskEditor))
        .style(Style::default().bg(Color::Black));

    let content = if let Some(mask) = &app.active_mask {
        let count_style = Style::default()
            .fg(Color::LightYellow)
            .add_modifier(Modifier::BOLD);
        Line::from(vec![
            Span::styled("Active: ", Style::default().fg(Color::Cyan)),
            Span::styled(mask.summary(), Style::default().fg(Color::LightGreen)),
            Span::raw("  "),
            Span::styled(
                format!("({} matches)", app.filtered_objects.len()),
                count_style,
            ),
            Span::raw("  "),
            Span::styled("Esc", Style::default().bg(Color::DarkGray).fg(Color::White)),
            Span::raw(" clear  "),
            Span::styled("m", Style::default().bg(Color::DarkGray).fg(Color::White)),
            Span::raw(" edit"),
        ])
    } else {
        Line::from(vec![
            Span::styled("None. Press ", Style::default().fg(Color::Gray)),
            Span::styled("m", Style::default().bg(Color::LightCyan).fg(Color::Black)),
            Span::styled(" to create a filter mask", Style::default().fg(Color::Gray)),
        ])
    };

    let para = Paragraph::new(content).block(block);
    frame.render_widget(para, area);
}

fn draw_status(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let lines: Vec<Line> = app
        .status
        .iter()
        .rev()
        .map(|msg| Line::from(msg.clone()))
        .collect();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            "Status",
            Style::default()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(Color::Black));
    let para = Paragraph::new(lines).block(block).wrap(Wrap { trim: true });
    frame.render_widget(para, area);
}

fn draw_command_bar(frame: &mut ratatui::Frame, area: Rect) {
    let key_style = Style::default()
        .bg(Color::LightCyan)
        .fg(Color::Black)
        .add_modifier(Modifier::BOLD);
    let help = Line::from(vec![
        Span::styled(" Tab ", key_style),
        Span::raw(" "),
        Span::styled(" m ", key_style),
        Span::raw("ask "),
        Span::styled(" s ", key_style),
        Span::raw("torage "),
        Span::styled(" r ", key_style),
        Span::raw("estore "),
        Span::styled(" i ", key_style),
        Span::raw("nfo "),
        Span::styled(" f ", key_style),
        Span::raw("refresh "),
        Span::styled(" t ", key_style),
        Span::raw("racker "),
        Span::styled(" ? ", key_style),
        Span::raw("help "),
        Span::styled(" l ", key_style),
        Span::raw("og "),
        Span::styled(" q ", key_style),
        Span::raw("uit"),
    ]);
    let block = Block::default()
        .borders(Borders::ALL)
        .style(Style::default().bg(Color::Blue).fg(Color::White));
    let para = Paragraph::new(help).block(block);
    frame.render_widget(para, area);
}

fn draw_mask_popup(frame: &mut ratatui::Frame, app: &App) {
    let area = centered_rect(70, 40, frame.size());
    draw_modal_surface(frame, area);

    let title_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let block = Block::default()
        .title(Span::styled(" Create Object Filter ", title_style))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .style(Style::default().bg(Color::Rgb(20, 20, 30)));

    let label_style = Style::default()
        .fg(Color::LightBlue)
        .add_modifier(Modifier::BOLD);
    let active_style = Style::default()
        .fg(Color::LightYellow)
        .add_modifier(Modifier::BOLD);
    let inactive_style = Style::default().fg(Color::Gray);
    let hint_style = Style::default().fg(Color::DarkGray);

    // Create pattern field with cursor
    let is_pattern_focused = matches!(app.mask_field, MaskEditorField::Pattern);
    let mut pattern_spans = vec![Span::styled("Pattern: ", label_style)];

    if is_pattern_focused {
        // Show cursor in pattern field
        let before_cursor = &app.mask_draft.pattern[..app.mask_draft.cursor_pos];
        let cursor_char = if app.mask_draft.cursor_pos < app.mask_draft.pattern.len() {
            app.mask_draft
                .pattern
                .chars()
                .nth(app.mask_draft.cursor_pos)
                .unwrap()
                .to_string()
        } else {
            " ".to_string()
        };
        let after_cursor = if app.mask_draft.cursor_pos < app.mask_draft.pattern.len() {
            &app.mask_draft.pattern[app.mask_draft.cursor_pos + 1..]
        } else {
            ""
        };

        pattern_spans.push(Span::styled(before_cursor, active_style));
        pattern_spans.push(Span::styled(
            cursor_char,
            Style::default().fg(Color::Black).bg(Color::LightYellow),
        ));
        pattern_spans.push(Span::styled(after_cursor, active_style));
    } else {
        let display = if app.mask_draft.pattern.is_empty() {
            "(empty)"
        } else {
            &app.mask_draft.pattern
        };
        pattern_spans.push(Span::styled(display, inactive_style));
    }

    let text = vec![
        Line::from(""),
        Line::from(pattern_spans),
        Line::from(vec![
            Span::styled("          ", Style::default()),
            Span::styled("↑ Type your filter pattern here", hint_style),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "Match Mode: ",
                if matches!(app.mask_field, MaskEditorField::Mode) {
                    active_style
                } else {
                    label_style
                },
            ),
            Span::styled(
                app.mask_draft.kind.to_string(),
                if matches!(app.mask_field, MaskEditorField::Mode) {
                    active_style
                } else {
                    inactive_style
                },
            ),
            Span::styled("  (use ←/→ or space)", hint_style),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "Case Sensitive: ",
                if matches!(app.mask_field, MaskEditorField::Case) {
                    active_style
                } else {
                    label_style
                },
            ),
            Span::styled(
                if app.mask_draft.case_sensitive {
                    "Yes"
                } else {
                    "No"
                },
                if matches!(app.mask_field, MaskEditorField::Case) {
                    active_style
                } else {
                    inactive_style
                },
            ),
            Span::styled("  (space or ←/→ toggles)", hint_style),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "Storage Class: ",
                if matches!(app.mask_field, MaskEditorField::StorageClass) {
                    active_style
                } else {
                    label_style
                },
            ),
            Span::styled(
                app.mask_draft
                    .storage_class_filter
                    .as_ref()
                    .map(|s| s.label())
                    .unwrap_or("Any"),
                if matches!(app.mask_field, MaskEditorField::StorageClass) {
                    active_style
                } else {
                    inactive_style
                },
            ),
            Span::styled("  (use ←/→ or space)", hint_style),
        ]),
        Line::from(""),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "Tab",
                Style::default()
                    .fg(Color::LightGreen)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" move between fields  ", hint_style),
            Span::styled(
                "Enter",
                Style::default()
                    .fg(Color::LightGreen)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" apply  ", hint_style),
            Span::styled(
                "Esc",
                Style::default()
                    .fg(Color::LightGreen)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" cancel", hint_style),
        ]),
    ];
    let para = Paragraph::new(text).block(block);
    frame.render_widget(para, area);
}

fn draw_storage_popup(frame: &mut ratatui::Frame, app: &App) {
    let area = centered_rect(40, 50, frame.size());
    draw_modal_surface(frame, area);
    let block = Block::default()
        .title("Select storage class (Enter confirm, Esc cancel)")
        .borders(Borders::ALL)
        .style(Style::default().bg(Color::Black));
    let items: Vec<ListItem> = StorageClassTier::selectable()
        .iter()
        .map(|class| ListItem::new(class.label()))
        .collect();
    let mut state = ListState::default();
    state.select(Some(app.storage_class_cursor));
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().fg(Color::Black).bg(Color::Yellow));
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_confirm_popup(frame: &mut ratatui::Frame, app: &App) {
    let area = centered_rect(60, 40, frame.size());
    draw_modal_surface(frame, area);

    let key_style = Style::default()
        .bg(Color::LightYellow)
        .fg(Color::Black)
        .add_modifier(Modifier::BOLD);
    let warn_style = Style::default()
        .fg(Color::LightYellow)
        .add_modifier(Modifier::BOLD);
    let highlight_style = Style::default()
        .fg(Color::LightGreen)
        .add_modifier(Modifier::BOLD);

    let mut lines = Vec::new();

    if let Some(action) = &app.pending_action {
        match action {
            PendingAction::Transition { target_class } => {
                lines.push(Line::from(vec![Span::styled(
                    "Transition Storage Class",
                    warn_style,
                )]));
                lines.push(Line::from(""));
                lines.push(Line::from(vec![
                    Span::raw("  Objects: "),
                    Span::styled(format!("{}", target_count(app)), highlight_style),
                ]));
                lines.push(Line::from(vec![
                    Span::raw("  Target:  "),
                    Span::styled(target_class.label(), highlight_style),
                ]));
            }
            PendingAction::Restore { days } => {
                lines.push(Line::from(vec![Span::styled(
                    "Request Glacier Restore",
                    warn_style,
                )]));
                lines.push(Line::from(""));
                lines.push(Line::from(vec![
                    Span::raw("  Objects:  "),
                    Span::styled(format!("{}", target_count(app)), highlight_style),
                ]));
                lines.push(Line::from(vec![
                    Span::raw("  Duration: "),
                    Span::styled(format!("{} days", days), highlight_style),
                ]));
            }
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(" Enter ", key_style),
        Span::raw(" Confirm   "),
        Span::styled(" Esc ", key_style),
        Span::raw(" Cancel"),
    ]));

    let block = Block::default()
        .title(Span::styled(
            " Confirm Action ",
            Style::default()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .style(Style::default().bg(Color::Black));
    let para = Paragraph::new(lines).block(block);
    frame.render_widget(para, area);
}

fn draw_help_popup(frame: &mut ratatui::Frame) {
    let area = centered_rect(80, 80, frame.size());
    draw_modal_surface(frame, area);
    let title_style = Style::default()
        .fg(Color::LightYellow)
        .add_modifier(Modifier::BOLD);
    let block = Block::default()
        .title(Span::styled(
            "Help & Workflow Guide – Press ? or Esc to close",
            title_style,
        ))
        .borders(Borders::ALL)
        .style(Style::default().bg(Color::Black));

    let key_style = Style::default()
        .fg(Color::LightCyan)
        .add_modifier(Modifier::BOLD);
    let header_style = Style::default()
        .fg(Color::LightGreen)
        .add_modifier(Modifier::BOLD);

    let lines = vec![
        Line::from(vec![Span::styled("BASIC WORKFLOW", header_style)]),
        Line::from("1. Navigate with Tab/Shift+Tab to switch between panes (Buckets, Objects)"),
        Line::from("2. Select a bucket with arrows, press Enter to load its objects"),
        Line::from("3. Create a mask (press 'm') to filter objects by pattern"),
        Line::from("4. Transition objects to different storage classes or request restores"),
        Line::from(""),
        Line::from(vec![Span::styled("NAVIGATION", header_style)]),
        Line::from(vec![
            Span::styled("Tab/Shift+Tab", key_style),
            Span::raw(" - Switch between panes  "),
            Span::styled("↑↓", key_style),
            Span::raw(" - Move selection  "),
            Span::styled("PgUp/PgDn", key_style),
            Span::raw(" - Jump 5 items"),
        ]),
        Line::from(vec![
            Span::styled("Enter", key_style),
            Span::raw(" - Load bucket objects (Buckets pane)"),
        ]),
        Line::from(""),
        Line::from(vec![Span::styled("OBJECT FILTERING (MASKS)", header_style)]),
        Line::from(vec![
            Span::styled("m", key_style),
            Span::raw(" - Open mask editor to create/edit filters"),
        ]),
        Line::from("   • Tab moves between fields: Name → Pattern → Mode → Case"),
        Line::from("   • Match modes: Prefix, Suffix, Contains, Regex (use arrows/space to cycle)"),
        Line::from("   • Enter applies the mask, Esc cancels"),
        Line::from("   • Active masks filter the object list and target all matching objects"),
        Line::from(vec![
            Span::styled("Esc", key_style),
            Span::raw(" - Clear active mask and show all objects"),
        ]),
        Line::from(""),
        Line::from(vec![Span::styled("STORAGE OPERATIONS", header_style)]),
        Line::from(vec![
            Span::styled("s", key_style),
            Span::raw(" - Transition objects to a different storage class"),
        ]),
        Line::from("   • Without mask: transitions the selected object only"),
        Line::from("   • With mask: transitions ALL matching objects"),
        Line::from("   • Press 'o' during confirmation to toggle restore-before-transition"),
        Line::from(vec![
            Span::styled("r", key_style),
            Span::raw(" - Request 7-day Glacier restore for selected/masked objects"),
        ]),
        Line::from(vec![
            Span::styled("i", key_style),
            Span::raw(" - Inspect selected object (refreshes metadata via HeadObject)"),
        ]),
        Line::from(""),
        Line::from(vec![Span::styled("OTHER COMMANDS", header_style)]),
        Line::from(vec![
            Span::styled("l", key_style),
            Span::raw(" - Toggle status log (view full error messages)  "),
            Span::styled("f", key_style),
            Span::raw(" - Refresh bucket list"),
        ]),
        Line::from(vec![
            Span::styled("?", key_style),
            Span::raw(" - Toggle this help screen  "),
            Span::styled("q", key_style),
            Span::raw(" or "),
            Span::styled("Ctrl+C", key_style),
            Span::raw(" - Quit application"),
        ]),
    ];
    let para = Paragraph::new(lines).block(block).wrap(Wrap { trim: true });
    frame.render_widget(para, area);
}

fn draw_log_popup(frame: &mut ratatui::Frame, app: &App) {
    let area = centered_rect(70, 60, frame.size());
    draw_modal_surface(frame, area);
    let block = Block::default()
        .title("Status log – Esc/l/Enter to close")
        .borders(Borders::ALL)
        .style(Style::default().bg(Color::Black));
    let mut lines: Vec<Line> = app
        .status
        .iter()
        .rev()
        .enumerate()
        .map(|(idx, msg)| Line::from(format!("{:>2}. {}", idx + 1, msg)))
        .collect();
    if lines.is_empty() {
        lines.push(Line::from("No status messages yet."));
    }
    let para = Paragraph::new(lines).block(block).wrap(Wrap { trim: true });
    frame.render_widget(para, area);
}

fn draw_tracked_requests_popup(frame: &mut ratatui::Frame, tracker: &RestoreTracker) {
    let area = centered_rect(80, 70, frame.size());
    draw_modal_surface(frame, area);

    let block = Block::default()
        .title("Tracked Restore Requests – Esc/t/Enter to close")
        .borders(Borders::ALL)
        .style(Style::default().bg(Color::Black));

    let requests = tracker.get_all_requests();

    let mut lines: Vec<Line> = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("Bucket", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" | "),
            Span::styled("Object Key", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" | "),
            Span::styled("Status", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" | "),
            Span::styled("Days", Style::default().add_modifier(Modifier::BOLD)),
        ]),
        Line::from(std::iter::repeat('-').take(100).collect::<String>()),
    ];

    if requests.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from("No restore requests tracked yet."));
        lines.push(Line::from(""));
        lines.push(Line::from(
            "Restore requests will appear here after you initiate them.",
        ));
    } else {
        for req in requests {
            let status_text = match &req.current_status {
                RestoreState::InProgress { expiry } => {
                    if let Some(exp) = expiry {
                        format!("In Progress (exp: {})", exp)
                    } else {
                        "In Progress".to_string()
                    }
                }
                RestoreState::Available => "Available".to_string(),
                RestoreState::Expired => "Expired".to_string(),
            };

            let status_style = match &req.current_status {
                RestoreState::InProgress { .. } => Style::default().fg(Color::Yellow),
                RestoreState::Available => Style::default().fg(Color::Green),
                RestoreState::Expired => Style::default().fg(Color::Red),
            };

            lines.push(Line::from(vec![
                Span::raw(format!("{} | ", req.bucket)),
                Span::raw(format!("{} | ", req.key)),
                Span::styled(format!("{} | ", status_text), status_style),
                Span::raw(format!("{} days", req.days)),
            ]));
        }
    }

    let para = Paragraph::new(lines).block(block).wrap(Wrap { trim: true });
    frame.render_widget(para, area);
}

fn draw_progress_popup(frame: &mut ratatui::Frame, app: &App) {
    let area = centered_rect(70, 30, frame.size());
    draw_modal_surface(frame, area);

    let progress = match &app.progress {
        Some(p) => p,
        None => return,
    };

    let title_style = Style::default()
        .fg(Color::LightCyan)
        .add_modifier(Modifier::BOLD);

    let block = Block::default()
        .title(Span::styled(
            format!(" {} ", progress.operation),
            title_style,
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .style(Style::default().bg(Color::Black));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Split into sections
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Progress bar
            Constraint::Length(2), // Counter
            Constraint::Length(2), // Current item
            Constraint::Min(1),    // Padding
        ])
        .split(inner);

    // Progress bar
    let gauge = Gauge::default()
        .gauge_style(
            Style::default()
                .fg(Color::Cyan)
                .bg(Color::Black)
                .add_modifier(Modifier::BOLD),
        )
        .percent(progress.percentage());
    frame.render_widget(gauge, chunks[0]);

    // Counter text
    let counter_text = format!("{} / {} objects", progress.current, progress.total);
    let counter = Paragraph::new(counter_text)
        .style(Style::default().fg(Color::White))
        .alignment(Alignment::Center);
    frame.render_widget(counter, chunks[1]);

    // Current item
    if let Some(ref item) = progress.current_item {
        let item_text = format!("Processing: {}", item);
        let item_para = Paragraph::new(item_text)
            .style(Style::default().fg(Color::Gray))
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true });
        frame.render_widget(item_para, chunks[2]);
    }
}

fn draw_credential_error_popup(frame: &mut ratatui::Frame) {
    let area = centered_rect(70, 50, frame.size());
    draw_modal_surface(frame, area);

    let error_style = Style::default().fg(Color::Red).add_modifier(Modifier::BOLD);
    let title_style = Style::default()
        .fg(Color::LightYellow)
        .add_modifier(Modifier::BOLD);
    let key_style = Style::default()
        .bg(Color::LightYellow)
        .fg(Color::Black)
        .add_modifier(Modifier::BOLD);

    let block = Block::default()
        .title(Span::styled(" AWS Credentials Error ", title_style))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Red))
        .style(Style::default().bg(Color::Black));

    let lines = vec![
        Line::from(""),
        Line::from(vec![Span::styled(
            "⚠ Failed to authenticate with AWS",
            error_style,
        )]),
        Line::from(""),
        Line::from("The application could not access AWS S3. This usually means:"),
        Line::from(""),
        Line::from("  • AWS credentials are not configured"),
        Line::from("  • Credentials have expired (SSO)"),
        Line::from("  • Invalid access key or secret key"),
        Line::from("  • Missing AWS_PROFILE environment variable"),
        Line::from(""),
        Line::from(vec![Span::styled("To fix this:", title_style)]),
        Line::from(""),
        Line::from("  1. Configure AWS credentials in ~/.aws/credentials"),
        Line::from("  2. Set AWS_PROFILE environment variable"),
        Line::from("  3. Run 'aws sso login' if using SSO"),
        Line::from("  4. Set AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY"),
        Line::from(""),
        Line::from("For more information:"),
        Line::from("  https://docs.aws.amazon.com/cli/latest/userguide/cli-configure-files.html"),
        Line::from(""),
        Line::from(""),
        Line::from(vec![
            Span::raw("Press "),
            Span::styled(" any key ", key_style),
            Span::raw(" to exit"),
        ]),
    ];

    let para = Paragraph::new(lines).block(block).wrap(Wrap { trim: true });
    frame.render_widget(para, area);
}

fn draw_modal_surface(frame: &mut ratatui::Frame, area: Rect) {
    frame.render_widget(Clear, area);
    let backdrop = Block::default().style(Style::default().bg(Color::Black));
    frame.render_widget(backdrop, area);

    let canvas = frame.size();
    let shadow_style = Style::default().bg(Color::DarkGray);
    if area.y + area.height < canvas.height {
        let shadow_width = area.width.min(canvas.width.saturating_sub(area.x + 1));
        if shadow_width > 0 {
            let shadow = Rect::new(area.x + 1, area.y + area.height, shadow_width, 1);
            frame.render_widget(Block::default().style(shadow_style), shadow);
        }
    }
    if area.x + area.width < canvas.width {
        let shadow_height = area.height.min(canvas.height.saturating_sub(area.y + 1));
        if shadow_height > 0 {
            let shadow = Rect::new(area.x + area.width, area.y + 1, 1, shadow_height);
            frame.render_widget(Block::default().style(shadow_style), shadow);
        }
    }
}

fn describe_restore_error(err: &anyhow::Error) -> String {
    if let Some(sdk_err) = err.downcast_ref::<SdkError<RestoreObjectError>>() {
        match sdk_err {
            SdkError::ServiceError(err) => {
                let service = err.err();
                let code = service.meta().code().unwrap_or("ServiceError");
                let message = service
                    .message()
                    .map(|m| m.to_string())
                    .unwrap_or_else(|| "no message provided".into());
                let friendly = match code {
                    "NoSuchKey" => {
                        "object was not found (mask may target stale keys or bucket differs)".into()
                    }
                    "InvalidObjectState" => {
                        "object is already being restored or not eligible for this operation".into()
                    }
                    _ => message.clone(),
                };
                if matches!(code, "NoSuchKey" | "InvalidObjectState") {
                    return format!("{code}: {friendly}");
                }
                return format!("{code}: {message}");
            }
            SdkError::DispatchFailure(err) => {
                return format!("network/dispatch failure: {err:?}");
            }
            SdkError::TimeoutError(_) => {
                return "request timed out; please retry".into();
            }
            SdkError::ResponseError(ctx) => {
                return format!("response error: {ctx:?}");
            }
            _ => {}
        }
    }
    format!("{err:#}")
}

fn centered_rect(width_percent: u16, height_percent: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height_percent) / 2),
            Constraint::Percentage(height_percent),
            Constraint::Percentage((100 - height_percent) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_percent) / 2),
            Constraint::Percentage(width_percent),
            Constraint::Percentage((100 - width_percent) / 2),
        ])
        .split(vertical[1])[1]
}

fn highlight_border(active: bool) -> Style {
    if active {
        Style::default()
            .fg(Color::LightYellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

fn format_size(size: i64) -> String {
    const KB: f64 = 1024.0;
    let kb = size as f64 / KB;
    format!("{:>10.2} KB", kb)
}

fn storage_class_color(storage_class: &StorageClassTier) -> Style {
    match storage_class {
        StorageClassTier::Standard => Style::default()
            .fg(Color::LightGreen)
            .add_modifier(Modifier::BOLD),
        StorageClassTier::StandardIa => Style::default()
            .fg(Color::LightYellow)
            .add_modifier(Modifier::BOLD),
        StorageClassTier::OneZoneIa => Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
        StorageClassTier::IntelligentTiering => Style::default()
            .fg(Color::LightMagenta)
            .add_modifier(Modifier::BOLD),
        StorageClassTier::GlacierInstantRetrieval => Style::default()
            .fg(Color::LightCyan)
            .add_modifier(Modifier::BOLD),
        StorageClassTier::GlacierFlexibleRetrieval => Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
        StorageClassTier::GlacierDeepArchive => Style::default()
            .fg(Color::LightBlue)
            .add_modifier(Modifier::BOLD),
        StorageClassTier::ReducedRedundancy => Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::BOLD),
        StorageClassTier::Unknown(_) => Style::default().fg(Color::DarkGray),
    }
}
